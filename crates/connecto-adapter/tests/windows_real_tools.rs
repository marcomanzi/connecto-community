//! The Windows hook shapes against the real CLIs, without a model call and
//! without an account: Claude fires `SessionStart` and `UserPromptSubmit` in
//! `claude -p` before its request fails when it is not logged in, and
//! `codex exec` fires `SessionStart` before its request fails
//! (`docs/windows-port.md` 4.8.2, 4.8.3).
//!
//! Ignored: they need the installed CLIs and a probe program. Run on Windows:
//!
//! ```text
//! $env:CONNECTO_M3_SCRATCH = "<an empty scratch folder>"
//! $env:CONNECTO_M3_PROBE   = "<a folder with ' and a space>\probe.exe"   # logs its argv to probe.log beside it
//! $env:CONNECTO_M3_CLAUDE  = "$env:USERPROFILE\.local\bin\claude.exe"
//! $env:CONNECTO_M3_CODEX   = "<…>\vendor\x86_64-pc-windows-msvc\bin\codex.exe"
//! $env:CONNECTO_M3_TRUST_HOME = "X:\home"   # optional: the trust read's CODEX_HOME
//! cargo test -p connecto-adapter --test windows_real_tools -- --ignored --nocapture --test-threads 1
//! ```
//!
//! Every folder the tools write to is under the scratch folder (their config
//! folders, `HOME`, `USERPROFILE`, `APPDATA`, `LOCALAPPDATA`, `TEMP`), and the
//! environment is cleared, so no credential of the user's can be used.
#![cfg(windows)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::ffi::OsString;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use connecto_adapter::{
    BridgeLaunch, Claude, Codex, LaunchCtx, ToolAdapter, hook_config, hook_trust_state,
};
use connecto_core::AgentMode;
use serde_json::{Value, json};

fn var(name: &str) -> PathBuf {
    std::env::var_os(name)
        .map(PathBuf::from)
        .unwrap_or_else(|| panic!("set {name}; see the file header"))
}

fn system_root() -> OsString {
    std::env::var_os("SystemRoot").unwrap_or_else(|| r"C:\Windows".into())
}

/// Only what Windows itself needs; the rest points into `scratch`.
fn inherited(scratch: &Path) -> Vec<(OsString, OsString)> {
    let root = PathBuf::from(system_root());
    let mut path = OsString::from(root.join("System32"));
    path.push(";");
    path.push(root.as_os_str());
    path.push(";");
    path.push(root.join(r"System32\WindowsPowerShell\v1.0"));
    let home = scratch.join("home");
    vec![
        ("SystemRoot".into(), root.clone().into()),
        ("ComSpec".into(), root.join(r"System32\cmd.exe").into()),
        ("PATHEXT".into(), ".COM;.EXE;.BAT;.CMD;.PS1".into()),
        ("PATH".into(), path),
        ("USERPROFILE".into(), home.clone().into()),
        ("APPDATA".into(), home.join(r"AppData\Roaming").into()),
        ("LOCALAPPDATA".into(), home.join(r"AppData\Local").into()),
        ("TEMP".into(), home.join("tmp-should-be-replaced").into()),
    ]
}

fn ctx(scratch: &Path, entry: PathBuf, project: &Path, profile: &str) -> LaunchCtx {
    for dir in [
        scratch.join("home").join(r"AppData\Roaming"),
        scratch.join("home").join(r"AppData\Local"),
        scratch.join(profile),
        scratch.join(r"run\tmp"),
        project.to_owned(),
    ] {
        std::fs::create_dir_all(dir).unwrap();
    }
    LaunchCtx {
        entry,
        cli_version: None,
        hook_binary: var("CONNECTO_M3_PROBE"),
        project: project.to_owned(),
        profile: scratch.join(profile),
        system_config: false,
        session_tmp: scratch.join(r"run\tmp"),
        run: scratch.join("run"),
        home: scratch.join("home"),
        inherited: inherited(scratch),
        pi: Default::default(),
        claude: Default::default(),
        claude_cuts: Default::default(),
        session_id: "11111111-2222-4333-8444-555555555555".to_owned(),
        resume: None,
        hook_socket: r"\\.\pipe\connecto-hook-m3-probe".into(),
        hook_token: "token".to_owned(),
        bridge: BridgeLaunch {
            socket: r"\\.\pipe\connecto-bridge-m3-probe".into(),
            terminal: "probe".to_owned(),
            bin_dir: scratch.join("bin"),
            text: None,
            hooks: true,
            trust: None,
        },
        package_text: None,
        initial_prompt: Some("say ok".to_owned()),
        session_name: None,
        model: None,
        resume_note: None,
        utility: None,
        agent_mode: AgentMode::Prompt,
    }
}

fn probe_log() -> PathBuf {
    var("CONNECTO_M3_PROBE").with_file_name("probe.log")
}

/// Run to the end, or kill after `limit`.
fn run(mut command: Command, limit: Duration) {
    let mut child = command
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let start = Instant::now();
    while child.try_wait().unwrap().is_none() {
        if start.elapsed() > limit {
            let _ = child.kill();
            break;
        }
        std::thread::sleep(Duration::from_millis(200));
    }
    let _ = child.wait();
}

#[test]
#[ignore = "needs the installed CLIs and a probe; see the file header"]
fn claude_runs_the_exec_form_hooks_with_their_arguments_verbatim() {
    let scratch = var("CONNECTO_M3_SCRATCH").join("claude");
    let project = scratch.join("proj dir");
    let ctx = ctx(
        &scratch,
        var("CONNECTO_M3_CLAUDE"),
        &project,
        "claude-config",
    );
    let spec = Claude.launch(&ctx);
    for (path, body) in &spec.files {
        std::fs::write(path, body).unwrap();
        println!("{}:\n{}", path.display(), String::from_utf8_lossy(body));
    }
    let _ = std::fs::remove_file(probe_log());
    let mut command = Command::new(&spec.program);
    command
        .arg("-p")
        .args(&spec.args)
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)));
    run(command, Duration::from_secs(90));
    let log = std::fs::read_to_string(probe_log()).unwrap_or_default();
    println!("probe.log:\n{log}");
    assert!(
        log.contains(r"ARGS=claude|--event|SessionStart SOCK=\\.\pipe\connecto-hook-m3-probe"),
        "{log}"
    );
    assert!(
        log.contains("ARGS=claude|--event|UserPromptSubmit|--context SOCK="),
        "{log}"
    );
}

/// `hooks/list` from `codex app-server`, as the session layer reads it.
fn listed_hooks(codex: &Path, values: &[String], home: &Path) -> Vec<(String, Option<String>)> {
    std::fs::create_dir_all(home).unwrap();
    let mut command = Command::new(codex);
    for value in values {
        command.arg("-c").arg(value);
    }
    let mut child = command
        .args(["app-server", "--listen", "stdio://"])
        .env_clear()
        .env("SystemRoot", system_root())
        .env("CODEX_HOME", home)
        .env("HOME", home)
        .current_dir(home)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut stdin = child.stdin.take().unwrap();
    let mut lines = BufReader::new(child.stdout.take().unwrap()).lines();
    let mut wait_for = |id: u64| loop {
        let line = lines.next().expect("app-server ended").unwrap();
        if let Ok(value) = serde_json::from_str::<Value>(&line)
            && value.get("id").and_then(Value::as_u64) == Some(id)
        {
            return value;
        }
    };
    let init = json!({"id":1,"method":"initialize","params":{"clientInfo":{"name":"connecto","version":"0"}}});
    writeln!(stdin, "{init}").unwrap();
    wait_for(1);
    writeln!(stdin, "{}", json!({"method":"initialized"})).unwrap();
    writeln!(
        stdin,
        "{}",
        json!({"id":2,"method":"hooks/list","params":{"cwds":[home]}})
    )
    .unwrap();
    let reply = wait_for(2);
    let _ = child.kill();
    let _ = child.wait();
    reply
        .pointer("/result/data/0/hooks")
        .and_then(Value::as_array)
        .expect("no hooks")
        .iter()
        .filter_map(|hook| {
            Some((
                hook.get("key")?.as_str()?.to_owned(),
                hook.get("currentHash")
                    .and_then(Value::as_str)
                    .map(str::to_owned),
            ))
        })
        .collect()
}

#[test]
#[ignore = "needs the installed CLIs and a probe; see the file header"]
fn codex_trusts_and_runs_the_powershell_hooks() {
    let scratch = var("CONNECTO_M3_SCRATCH").join("codex");
    let codex = var("CONNECTO_M3_CODEX");
    let probe = var("CONNECTO_M3_PROBE");
    let values = hook_config(&probe).unwrap();
    // `CONNECTO_M3_TRUST_HOME` puts the trust read's `CODEX_HOME` elsewhere,
    // e.g. on another drive, to see which drive letter the keys carry.
    let trust_home = std::env::var_os("CONNECTO_M3_TRUST_HOME")
        .map_or_else(|| scratch.join("trust-home"), PathBuf::from);
    let listed = listed_hooks(&codex, &values, &trust_home);
    println!("listed: {listed:?}");
    let borrowed: Vec<(&str, Option<&str>)> = listed
        .iter()
        .map(|(key, hash)| (key.as_str(), hash.as_deref()))
        .collect();
    let trust = hook_trust_state(&borrowed).unwrap();
    println!("trust: {trust}");

    let project = scratch.join("proj dir");
    let mut ctx = ctx(&scratch, codex.clone(), &project, "codex-home");
    ctx.bridge.trust = Some(trust.clone());
    // The launch's environment and hook values; `codex exec` instead of the
    // TUI, with the same `-c` values.
    let spec = Codex.launch(&ctx);
    let words: Vec<String> = spec
        .args
        .iter()
        .map(|a| a.to_string_lossy().into_owned())
        .collect();
    let mut config: Vec<&String> = Vec::new();
    for pair in words.windows(2) {
        if pair[0] == "-c" && (pair[1].starts_with("hooks.") || pair[1].starts_with("notify=")) {
            config.push(&pair[1]);
        }
    }
    assert!(config.iter().any(|c| **c == trust), "{words:?}");
    let _ = std::fs::remove_file(probe_log());
    let mut command = Command::new(&spec.program);
    command.arg("exec").arg("--skip-git-repo-check");
    for value in config {
        command.arg("-c").arg(value);
    }
    command
        .arg("-C")
        .arg(&project)
        .arg("say ok")
        .current_dir(&spec.cwd)
        .env_clear()
        .envs(spec.env.iter().map(|(k, v)| (k, v)));
    run(command, Duration::from_secs(90));
    let log = std::fs::read_to_string(probe_log()).unwrap_or_default();
    println!("probe.log:\n{log}");
    assert!(
        log.contains(r"ARGS=codex-hook|--event|SessionStart SOCK=\\.\pipe\connecto-hook-m3-probe"),
        "{log}"
    );
}
