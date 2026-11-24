//! The bridge text and the package recap against the installed Pi: Connecto's
//! launch adds them to the prompt Pi resolved itself, so the
//! `APPEND_SYSTEM.md` Pi loads (the project's in a trusted project, the
//! profile's in an untrusted one) stays, in a new session and a resumed one.
//! The launch's own arguments are used. A stand-in OpenAI-compatible endpoint
//! records the request Pi sends and fails it, so no account or model is used.
//!
//! Ignored: it needs `pi` on `PATH`, or `PI_BIN` naming another one (checked
//! with 1.5.2, which has prompt sections, and 0.85.1, which has none). Run
//! with
//!
//! ```text
//! cargo test -p connecto-adapter --test pi_real_recap -- --ignored --nocapture
//! PI_BIN=/path/to/pi cargo test -p connecto-adapter --test pi_real_recap -- --ignored
//! ```
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use connecto_adapter::{BridgeLaunch, LaunchCtx, Pi, ToolAdapter};
use connecto_core::AgentMode;

const SESSION: &str = "11111111-2222-4333-8444-555555555555";

fn ctx(
    scratch: &Path,
    project: &Path,
    recap: Option<&str>,
    bridge: Option<&str>,
    resume: bool,
) -> LaunchCtx {
    LaunchCtx {
        entry: "pi".into(),
        cli_version: None,
        // Reports go nowhere: the helper is `true`.
        hook_binary: "/usr/bin/true".into(),
        project: project.to_owned(),
        profile: scratch.join("agent"),
        system_config: false,
        session_tmp: scratch.join("run/tmp"),
        run: scratch.join("run"),
        home: scratch.join("home"),
        inherited: Vec::new(),
        pi: Default::default(),
        claude: Default::default(),
        claude_cuts: Default::default(),
        session_id: SESSION.to_owned(),
        resume: resume.then(|| SESSION.to_owned()),
        hook_socket: scratch.join("hook.sock"),
        hook_token: "token".to_owned(),
        bridge: BridgeLaunch {
            socket: scratch.join("bridge.sock"),
            terminal: "pi-1".to_owned(),
            bin_dir: scratch.join("bin"),
            text: bridge.map(str::to_owned),
            hooks: false,
            trust: None,
        },
        package_text: recap.map(str::to_owned),
        initial_prompt: None,
        session_name: None,
        model: None,
        effort: None,
        resume_note: None,
        utility: None,
        agent_mode: AgentMode::Prompt,
        #[cfg(feature = "test-endpoints")]
        fake_model: None,
    }
}

/// Answer every request with a failure, handing its body back.
fn stand_in() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut length = 0usize;
            loop {
                let mut line = String::new();
                if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
                    break;
                }
                if let Some(value) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                    length = value.trim().parse().unwrap_or(0);
                }
            }
            let mut body = vec![0; length];
            let _ = reader.read_exact(&mut body);
            let _ = tx.send(String::from_utf8_lossy(&body).into_owned());
            let _ = stream.write_all(
                b"HTTP/1.1 500 Internal Server Error\r\ncontent-length: 2\r\nconnection: close\r\n\r\n{}",
            );
        }
    });
    (port, rx)
}

/// One scratch profile and project, with both `APPEND_SYSTEM.md` files and
/// an AGENTS.md, for one or more `pi -p` runs.
struct Scene {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    bodies: mpsc::Receiver<String>,
}

impl Scene {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().to_owned();
        let project = root.join("proj");
        for sub in [
            root.join("agent"),
            root.join("run/tmp"),
            root.join("home"),
            project.join(".pi"),
        ] {
            std::fs::create_dir_all(sub).unwrap();
        }
        std::fs::write(root.join("agent/APPEND_SYSTEM.md"), "PROFILE-APPEND\n").unwrap();
        std::fs::write(project.join(".pi/APPEND_SYSTEM.md"), "PROJECT-APPEND\n").unwrap();
        std::fs::write(project.join("AGENTS.md"), "AGENTS-MD-TEXT\n").unwrap();
        let (port, bodies) = stand_in();
        std::fs::write(
            root.join("agent/models.json"),
            format!(
                r#"{{"providers":{{"fake":{{"baseUrl":"http://127.0.0.1:{port}/v1","api":"openai-completions","apiKey":"x","models":[{{"id":"m"}}]}}}}}}"#
            ),
        )
        .unwrap();
        Self {
            _dir: dir,
            root,
            project,
            bodies,
        }
    }

    /// The system prompt Pi sends for one `pi -p` run with the launch's own
    /// arguments (without the TUI mode, which `-p` has none of).
    fn sent_prompt(
        &self,
        trusted: bool,
        recap: Option<&str>,
        bridge: Option<&str>,
        resume: bool,
    ) -> String {
        let spec = Pi.launch(&ctx(&self.root, &self.project, recap, bridge, resume));
        for (path, body) in &spec.files {
            std::fs::write(path, body).unwrap();
        }
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let at = args.iter().position(|a| a == "--tui-mode").unwrap();
        let mut command = Command::new(std::env::var_os("PI_BIN").unwrap_or_else(|| "pi".into()));
        command
            .current_dir(&self.project)
            .env("PI_CODING_AGENT_DIR", self.root.join("agent"))
            .args(["-p", "--model", "fake/m"])
            .args(&args[..at])
            .args(&args[at + 2..]);
        if trusted {
            command.arg("--approve");
        }
        let status = command
            .arg("hi")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .expect("pi on PATH");
        let _ = status;
        let body = self
            .bodies
            .recv_timeout(Duration::from_secs(60))
            .expect("Pi sent a request");
        // Pi may retry the failed request: only the first counts.
        std::thread::sleep(Duration::from_millis(200));
        while self.bodies.try_recv().is_ok() {}
        let request: serde_json::Value = serde_json::from_str(&body).unwrap();
        request["messages"]
            .as_array()
            .unwrap()
            .iter()
            .filter(|m| m["role"] == "system" || m["role"] == "developer")
            .map(|m| match m["content"].as_str() {
                Some(text) => text.to_owned(),
                None => m["content"].to_string(),
            })
            .collect::<Vec<_>>()
            .join("\n")
    }
}

fn sent_prompt(trusted: bool, recap: Option<&str>) -> String {
    Scene::new().sent_prompt(trusted, recap, None, false)
}

#[test]
#[ignore = "needs the installed pi"]
fn the_recap_is_added_and_append_system_md_stays() {
    let recap = "RECAP-X: end with a status block";
    for (trusted, kept, dropped) in [
        (true, "PROJECT-APPEND", "PROFILE-APPEND"),
        (false, "PROFILE-APPEND", "PROJECT-APPEND"),
    ] {
        let prompt = sent_prompt(trusted, Some(recap));
        assert!(prompt.contains(kept), "trusted={trusted}: {prompt}");
        assert!(!prompt.contains(dropped), "trusted={trusted}");
        assert!(prompt.contains("AGENTS-MD-TEXT"), "trusted={trusted}");
        assert!(prompt.contains("RECAP-X"), "trusted={trusted}");
        assert!(
            prompt.contains("connecto-status-recap"),
            "trusted={trusted}"
        );
        // Without the package: Pi's own prompt, unchanged.
        let plain = sent_prompt(trusted, None);
        assert!(
            plain.contains(kept) && !plain.contains("RECAP-X"),
            "trusted={trusted}"
        );
    }
}

/// The bridge text is added beside the `APPEND_SYSTEM.md` Pi loads, before
/// the recap, in a new session and again when it is resumed; quotes,
/// backslashes and line breaks arrive as they were.
#[test]
#[ignore = "needs the installed pi"]
fn the_bridge_text_is_added_and_append_system_md_stays() {
    let bridge = "BRIDGE-X: you are terminal \"pi-1\".\nRun `connecto list` \\ $HOME ünï";
    let recap = "RECAP-X: end with a status block";
    for (trusted, kept, dropped) in [
        (true, "PROJECT-APPEND", "PROFILE-APPEND"),
        (false, "PROFILE-APPEND", "PROJECT-APPEND"),
    ] {
        let scene = Scene::new();
        for resume in [false, true] {
            let label = format!("trusted={trusted} resume={resume}");
            let prompt = scene.sent_prompt(trusted, Some(recap), Some(bridge), resume);
            assert!(prompt.contains(kept), "{label}: {prompt}");
            assert!(!prompt.contains(dropped), "{label}");
            assert!(prompt.contains("AGENTS-MD-TEXT"), "{label}");
            let at = prompt
                .find(bridge)
                .unwrap_or_else(|| panic!("{label}: {prompt}"));
            let recap_at = prompt.find("RECAP-X").expect(&label);
            assert!(at < recap_at, "{label}: the bridge text comes first");
        }
    }
}
