//! The bridge text against the installed Codex: Connecto's launch must add it
//! to the `developer_instructions` the user configured, not replace them, in a
//! new session and in a resumed one. The launch's own `-c` values and
//! environment are used, with Connecto's real hook helper (built here) and a
//! hook trust read from `codex app-server`, as preflight reads it. A stand-in
//! Responses endpoint records every request Codex sends and answers with a
//! short reply, so no account or model is used.
//!
//! Ignored: it needs `codex` on `PATH` (checked with 0.167.0). Run with
//!
//! ```text
//! cargo test -p connecto-adapter --test codex_real_bridge -- --ignored --nocapture
//! ```
#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::TcpListener;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use connecto_adapter::{
    BridgeLaunch, Codex, LaunchCtx, LaunchSpec, ToolAdapter, hook_config, hook_trust_state,
};
use connecto_core::AgentMode;
use serde_json::{Value, json};

/// The user's own instructions, in the profile's `config.toml`.
const USER_TEXT: &str = "USER-DEV-INSTRUCTIONS-7Q";
/// Connecto's bridge text. Long, with the characters TOML, JSON and a shell
/// would each treat specially, and starting as every real one does (the
/// hook knows Connecto's messages in a thread by that start).
fn bridge_text() -> String {
    let mut text = String::from(
        "You are running in terminal \"codex-9\" of Connecto. BRIDGE-TEXT-3K\n\
         Use `connecto list`; quote it 'like this' \\ $HOME %PATH% <tag> & done.\n",
    );
    while text.len() < 12_000 {
        text.push_str("  connecto send <id> --ask 'message'   ask another agent — ünïcode ✓\n");
    }
    text.push_str("BRIDGE-END-3K");
    text
}

/// The hook helper, built from this workspace.
fn helper() -> PathBuf {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let status = Command::new(cargo)
        .args(["build", "-q", "-p", "connecto-hook"])
        .status()
        .unwrap();
    assert!(status.success());
    let test_exe = std::env::current_exe().unwrap();
    test_exe
        .ancestors()
        .skip(1)
        .map(|dir| dir.join("connecto-hook"))
        .find(|path| path.is_file())
        .expect("the hook helper next to the profile's binaries")
}

/// The `hooks.state` value that trusts the hooks for `helper`, read the way
/// preflight reads it: `hooks/list` from `codex app-server` in an empty home.
fn trust(helper: &Path, scratch: &Path) -> String {
    let home = scratch.join("trust-home");
    std::fs::create_dir_all(&home).unwrap();
    let mut command = Command::new("codex");
    for value in hook_config(helper).unwrap() {
        command.arg("-c").arg(value);
    }
    let mut child = command
        .args(["app-server", "--listen", "stdio://"])
        .env("CODEX_HOME", &home)
        .current_dir(&home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("codex on PATH");
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut answer = |id: u64| -> Value {
        for line in lines.by_ref() {
            let value: Value = serde_json::from_str(&line.unwrap()).unwrap_or(Value::Null);
            if value["id"] == id {
                return value;
            }
        }
        panic!("codex app-server ended");
    };
    let mut send = |value: Value| {
        writeln!(stdin, "{value}").unwrap();
        stdin.flush().unwrap();
    };
    send(json!({"id": 1, "method": "initialize",
        "params": {"clientInfo": {"name": "connecto", "version": "0"}}}));
    answer(1);
    send(json!({"method": "initialized"}));
    send(json!({"id": 2, "method": "hooks/list", "params": {"cwds": [home]}}));
    let reply = answer(2);
    let _ = child.kill();
    let _ = child.wait();
    let listed: Vec<(&str, Option<&str>)> = reply
        .pointer("/result/data/0/hooks")
        .and_then(Value::as_array)
        .unwrap()
        .iter()
        .filter_map(|hook| Some((hook["key"].as_str()?, hook["currentHash"].as_str())))
        .collect();
    hook_trust_state(&listed).unwrap()
}

/// A Responses endpoint that hands back every request body and answers each
/// with a one-line reply.
fn stand_in() -> (u16, mpsc::Receiver<String>) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for stream in listener.incoming() {
            let Ok(mut stream) = stream else { continue };
            let mut reader = BufReader::new(stream.try_clone().unwrap());
            let mut first = String::new();
            let _ = reader.read_line(&mut first);
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
            if !first.starts_with("POST") || !first.contains("responses") {
                let _ = stream.write_all(
                    b"HTTP/1.1 200 OK\r\ncontent-type: application/json\r\ncontent-length: 23\r\nconnection: close\r\n\r\n{\"data\":[],\"models\":[]}",
                );
                continue;
            }
            let _ = tx.send(String::from_utf8_lossy(&body).into_owned());
            let item = json!({"type": "message", "role": "assistant",
                "content": [{"type": "output_text", "text": "ok"}]});
            let usage = json!({"input_tokens": 5, "input_tokens_details": null,
                "output_tokens": 1, "output_tokens_details": null, "total_tokens": 6});
            let mut sse = String::new();
            for (name, data) in [
                (
                    "response.created",
                    json!({"type": "response.created", "response": {"id": "resp_1"}}),
                ),
                (
                    "response.output_item.done",
                    json!({"type": "response.output_item.done", "item": item}),
                ),
                (
                    "response.completed",
                    json!({"type": "response.completed",
                    "response": {"id": "resp_1", "usage": usage}}),
                ),
            ] {
                sse.push_str(&format!("event: {name}\ndata: {data}\n\n"));
            }
            let _ = stream.write_all(
                format!(
                    "HTTP/1.1 200 OK\r\ncontent-type: text/event-stream\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{sse}",
                    sse.len()
                )
                .as_bytes(),
            );
        }
    });
    (port, rx)
}

struct Scene {
    _dir: tempfile::TempDir,
    root: PathBuf,
    project: PathBuf,
    helper: PathBuf,
    trust: String,
    bodies: mpsc::Receiver<String>,
}

impl Scene {
    fn new() -> Self {
        let dir = tempfile::tempdir().unwrap();
        // The real folder: Codex resolves `/var` to `/private/var`.
        let root = dir.path().canonicalize().unwrap();
        let project = root.join("proj");
        for sub in ["profile", "run/tmp", "home", "proj"] {
            std::fs::create_dir_all(root.join(sub)).unwrap();
        }
        let (port, bodies) = stand_in();
        std::fs::write(
            root.join("profile/config.toml"),
            format!(
                "developer_instructions = \"{USER_TEXT}\"\n\
                 model_provider = \"fake\"\n\
                 model = \"gpt-fake\"\n\
                 [model_providers.fake]\n\
                 name = \"fake\"\n\
                 base_url = \"http://127.0.0.1:{port}/v1\"\n\
                 wire_api = \"responses\"\n\
                 env_key = \"FAKE_KEY\"\n"
            ),
        )
        .unwrap();
        let helper = helper();
        let trust = trust(&helper, &root);
        Self {
            _dir: dir,
            root,
            project,
            helper,
            trust,
            bodies,
        }
    }

    fn ctx(&self, resume: Option<&str>) -> LaunchCtx {
        self.ctx_with(resume, &bridge_text())
    }

    fn ctx_with(&self, resume: Option<&str>, bridge: &str) -> LaunchCtx {
        let r = &self.root;
        LaunchCtx {
            entry: "codex".into(),
            cli_version: Some("0.167.0".to_owned()),
            hook_binary: self.helper.clone(),
            project: self.project.clone(),
            profile: r.join("profile"),
            system_config: false,
            session_tmp: r.join("run/tmp"),
            run: r.join("run"),
            home: r.join("home"),
            inherited: vec![("PATH".into(), std::env::var_os("PATH").unwrap())],
            pi: Default::default(),
            claude: Default::default(),
            claude_cuts: Default::default(),
            session_id: "11111111-2222-4333-8444-555555555555".to_owned(),
            resume: resume.map(str::to_owned),
            // No Connecto listens here: the helper's reports go nowhere.
            hook_socket: r.join("no-hook.sock"),
            hook_token: "token".to_owned(),
            bridge: BridgeLaunch {
                socket: r.join("no-bridge.sock"),
                terminal: "codex-9".to_owned(),
                bin_dir: r.join("bin"),
                text: Some(bridge.to_owned()),
                hooks: true,
                trust: Some(self.trust.clone()),
            },
            package_text: Some("RECAP".to_owned()),
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

    /// Run `codex exec` (or `codex exec resume <id>`) with the launch's `-c`
    /// values and environment. Returns the thread id and the first request
    /// Codex sent.
    fn run(&self, spec: &LaunchSpec, resume: Option<&str>, prompt: &str) -> (String, Value) {
        let args: Vec<String> = spec
            .args
            .iter()
            .map(|a| a.to_string_lossy().into_owned())
            .collect();
        let mut command = Command::new("codex");
        command.arg("exec");
        for pair in args.windows(2) {
            if pair[0] == "-c" {
                command.arg("-c").arg(&pair[1]);
            }
        }
        command.args(["--json", "--skip-git-repo-check", "-C"]);
        command.arg(&self.project);
        if let Some(id) = resume {
            command.args(["resume", id]);
        }
        let out = command
            .arg(prompt)
            .env_clear()
            .envs(spec.env.iter().map(|(k, v)| (k.clone(), v.clone())))
            .env("FAKE_KEY", "fake-local-model")
            .current_dir(&self.project)
            .stdin(Stdio::null())
            .output()
            .expect("codex on PATH");
        let stdout = String::from_utf8_lossy(&out.stdout);
        let thread = stdout
            .lines()
            .filter_map(|line| serde_json::from_str::<Value>(line).ok())
            .find_map(|event| event["thread_id"].as_str().map(str::to_owned))
            .unwrap_or_else(|| {
                panic!(
                    "no thread id: {stdout}\n{}",
                    String::from_utf8_lossy(&out.stderr)
                )
            });
        let body = self
            .bodies
            .recv_timeout(Duration::from_secs(5))
            .expect("Codex sent a request");
        while self.bodies.try_recv().is_ok() {}
        (thread, serde_json::from_str(&body).unwrap())
    }
}

/// The developer messages of a request, as text.
fn developer(request: &Value) -> Vec<String> {
    let mut texts: Vec<String> = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .filter(|item| item["role"] == "developer" || item["role"] == "system")
        .map(ToString::to_string)
        .collect();
    texts.extend(request["instructions"].as_str().map(str::to_owned));
    texts
}

fn assert_both(request: &Value, label: &str) {
    let texts = developer(request).join("\n");
    assert!(
        texts.contains(USER_TEXT),
        "{label}: the user's own instructions are gone"
    );
    // Compared after JSON decoding, so the escaping must have held.
    let decoded: Vec<String> = request["input"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part["text"].as_str().map(str::to_owned))
        .collect();
    let seen: Vec<String> = decoded
        .iter()
        .map(|text| {
            let head: String = text.chars().take(80).collect();
            format!("{} bytes: {head:?}", text.len())
        })
        .collect();
    assert!(
        decoded.iter().any(|text| text.contains(&bridge_text())),
        "{label}: the bridge text did not arrive whole; the request had {seen:#?}"
    );
    assert!(
        decoded
            .iter()
            .any(|text| text.contains("**Waiting on user**")),
        "{label}: the package recap did not arrive"
    );
}

/// How many times `needle` is in the request's texts, decoded.
fn copies(request: &Value, needle: &str) -> usize {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| {
            part["text"]
                .as_str()
                .map(|text| text.matches(needle).count())
        })
        .sum()
}

/// The texts of Connecto's messages in a request, in order.
fn connecto_texts(request: &Value) -> Vec<String> {
    request["input"]
        .as_array()
        .unwrap()
        .iter()
        .flat_map(|item| item["content"].as_array().cloned().unwrap_or_default())
        .filter_map(|part| part["text"].as_str().map(str::to_owned))
        .filter(|text| text.contains("BRIDGE-TEXT-3K"))
        .collect()
}

#[test]
#[ignore = "needs the installed codex"]
fn the_bridge_text_is_added_to_the_users_developer_instructions() {
    let scene = Scene::new();
    let spec = Codex.launch(&scene.ctx(None));
    let (thread, first) = scene.run(&spec, None, "hello");
    assert_both(&first, "new session");

    // Each resume runs the hook again; what the thread holds is not added
    // again (Codex keeps every earlier answer in the thread).
    for round in 1..=2 {
        let spec = Codex.launch(&scene.ctx(Some(&thread)));
        let (_, resumed) = scene.run(&spec, Some(&thread), "again");
        let label = format!("resume {round}");
        assert_both(&resumed, &label);
        assert_eq!(copies(&resumed, &bridge_text()), 1, "{label}");
        assert_eq!(copies(&resumed, "**Waiting on user**"), 1, "{label}");
        assert_eq!(copies(&resumed, USER_TEXT), 1, "{label}");
    }
}

/// Project memory turned on, then off: the bridge text changes with it. The
/// resume after memory is turned off must end with the text without memory,
/// although the thread still holds that same text from its start; the next
/// resume with no change adds nothing.
#[test]
#[ignore = "needs the installed codex"]
fn a_capability_turned_off_is_what_the_latest_instructions_say() {
    let scene = Scene::new();
    let base = bridge_text();
    let memory = format!("{base}\n\nThis project has a project memory: MEMORY-ON");
    let spec = Codex.launch(&scene.ctx_with(None, &base));
    let (thread, first) = scene.run(&spec, None, "hello");
    let mut seen = connecto_texts(&first).len();
    assert_eq!(seen, 1);
    for (round, text, has_memory) in [
        ("memory on", &memory, true),
        ("memory off", &base, false),
        ("no change", &base, false),
    ] {
        let spec = Codex.launch(&scene.ctx_with(Some(&thread), text));
        let (_, resumed) = scene.run(&spec, Some(&thread), "again");
        assert_both(&resumed, round);
        let texts = connecto_texts(&resumed);
        let latest = texts.last().unwrap();
        assert_eq!(latest.contains("MEMORY-ON"), has_memory, "{round}");
        if round == "no change" {
            assert_eq!(texts.len(), seen, "{round}: nothing added");
        } else {
            assert_eq!(texts.len(), seen + 1, "{round}: one message added");
            assert!(
                latest.starts_with(connecto_hook_note()),
                "{round}: {latest:.80}"
            );
        }
        seen = texts.len();
    }
}

/// The line a resume puts in front of a changed message
/// (`connecto_hook::codex::REPLACES_NOTE`; the adapter's tests do not
/// depend on the hook crate).
fn connecto_hook_note() -> &'static str {
    "These Connecto instructions replace any given earlier in this conversation."
}
