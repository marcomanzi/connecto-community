//! The built-in status line scripts against stored expected output.
//!
//! `tests/fixtures/statusline/` and `tests/fixtures/subagent/` hold, per case,
//! the JSON Claude Code would send (`<case>.in`) and the exact bytes the
//! script prints (`<case>.out`). The same cases check both forms:
//!
//! - on Windows, the PowerShell twins (`statusline.ps1`,
//!   `subagent-statusline.ps1`), run the way Connecto runs them, with Windows
//!   PowerShell 5.1 and, when it is installed, PowerShell 7;
//! - on a Mac (any Unix with `bash` and `jq`), the `.sh` originals, run the way
//!   the built-in `settings.json` runs them (`bash statusline.sh`,
//!   `sh subagent-statusline.sh`).
//!
//! So a Mac proves the expected output is the originals', and Windows proves
//! the twins print the same. Without the tools the run is skipped (and says
//! so). The expected output was first derived on Windows, where `jq` is
//! missing: for `statusline.sh` by feeding its real bash and awk code (Git for
//! Windows) the line its jq program prints, as read off the program; for
//! `subagent-statusline.sh` by a separate reading of its jq program, written
//! in another language (Python; redone for the right-aligned rows of 0.1.6,
//! when the cases `cost`, `start_times`, `widths`, `tiers`, `controls` and
//! `wide` were added). Until a Mac has run this test, those readings are what
//! the expected output rests on. The running time depends on the clock, so it
//! has its own test instead of a stored case.
//!
//! The first Mac run showed that jq (1.7.1, as macOS ships it) does not catch
//! an error on the left of `//`: `.model.display_name` of a string `model`
//! emptied the whole line, so `statusline.sh` now writes
//! `.model.display_name?`. The cases `effort_string`, `usage_number` and
//! `cost_number` (whole line empty) and `model_array` (`?` gives the
//! fallback) were derived on Windows from that finding; a Mac run confirms
//! them.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::fs;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::process::{Command, Output, Stdio};
use std::sync::Mutex;
use std::sync::atomic::{AtomicUsize, Ordering};

fn crate_dir() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
}

/// The script of the `claude` variant (the `both` copy is the same file).
fn script(name: &str) -> PathBuf {
    crate_dir()
        .join("builtin")
        .join("claude")
        .join(".claude")
        .join("scripts")
        .join(name)
}

struct Case {
    kind: &'static str,
    name: String,
    input: Vec<u8>,
    expected: Vec<u8>,
}

fn cases(kind: &'static str) -> Vec<Case> {
    let dir = crate_dir().join("tests").join("fixtures").join(kind);
    let mut names: Vec<String> = fs::read_dir(&dir)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .filter_map(|n| n.strip_suffix(".in").map(str::to_owned))
        .collect();
    names.sort();
    assert!(names.len() >= 10, "{kind}: too few cases");
    names
        .into_iter()
        .map(|name| Case {
            kind,
            input: fs::read(dir.join(format!("{name}.in"))).unwrap(),
            expected: fs::read(dir.join(format!("{name}.out")))
                .unwrap_or_else(|e| panic!("{kind}/{name}.out: {e}")),
            name,
        })
        .collect()
}

/// Run `program args` with `input` on stdin.
fn run(program: &Path, args: &[&std::ffi::OsStr], input: &[u8]) -> Output {
    let mut child = Command::new(program)
        .args(args)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .unwrap_or_else(|e| panic!("{}: {e}", program.display()));
    let mut stdin = child.stdin.take().unwrap();
    let input = input.to_vec();
    let writer = std::thread::spawn(move || {
        // A script may stop reading early; that is not a failure here.
        let _ = stdin.write_all(&input);
    });
    let out = child.wait_with_output().unwrap();
    writer.join().unwrap();
    out
}

/// Bytes as text a person can compare: ESC as `\e`, other controls escaped.
fn shown(bytes: &[u8]) -> String {
    String::from_utf8_lossy(bytes)
        .chars()
        .map(|c| match c {
            '\u{1b}' => "\\e".to_owned(),
            '\n' => "\\n\n".to_owned(),
            c if c.is_control() => c.escape_default().to_string(),
            c => c.to_string(),
        })
        .collect()
}

/// Run every case through `command(case)` on a few threads; returns what
/// failed, as readable text. `quiet`: anything on stderr is a failure too.
fn check(all: &[Case], command: &(dyn Fn(&Case) -> Output + Sync), quiet: bool) -> Vec<String> {
    let next = AtomicUsize::new(0);
    let failed = Mutex::new(Vec::new());
    std::thread::scope(|scope| {
        for _ in 0..4 {
            scope.spawn(|| {
                while let Some(case) = all.get(next.fetch_add(1, Ordering::Relaxed)) {
                    let out = command(case);
                    let mut why = Vec::new();
                    if out.stdout != case.expected {
                        why.push(format!(
                            "stdout:\n{}\nexpected:\n{}",
                            shown(&out.stdout),
                            shown(&case.expected)
                        ));
                    }
                    if out.status.code() != Some(0) {
                        why.push(format!("exit status {:?}", out.status));
                    }
                    if quiet && !out.stderr.is_empty() {
                        why.push(format!("stderr: {}", String::from_utf8_lossy(&out.stderr)));
                    }
                    if !why.is_empty() {
                        failed.lock().unwrap().push(format!(
                            "{}/{}: {}",
                            case.kind,
                            case.name,
                            why.join("\n")
                        ));
                    }
                }
            });
        }
    });
    failed.into_inner().unwrap()
}

fn all_cases() -> Vec<Case> {
    let mut all = cases("statusline");
    all.extend(cases("subagent"));
    all
}

/// The two copies of each script (`claude` and `both` variants) are one file,
/// and the twins are plain ASCII with LF line ends and no `#!` (Windows
/// PowerShell 5.1 reads a file without a BOM in the ANSI code page; a `#!`
/// would make the Windows build take the twin for an executable).
#[test]
fn the_scripts_are_the_same_in_both_variants_and_the_twins_are_plain_ascii() {
    for name in [
        "statusline.sh",
        "subagent-statusline.sh",
        "statusline.ps1",
        "subagent-statusline.ps1",
    ] {
        let claude = fs::read(script(name)).unwrap();
        let both = fs::read(crate_dir().join("builtin/both/.claude/scripts").join(name)).unwrap();
        assert!(claude == both, "{name}: the claude and both copies differ");
        if name.ends_with(".ps1") {
            assert!(claude.is_ascii(), "{name} is not ASCII");
            assert!(!claude.contains(&b'\r'), "{name} has CR line ends");
            assert!(!claude.starts_with(b"#!"), "{name} starts with #!");
        }
    }
}

/// The PowerShell twins print the expected bytes, exit 0 and write nothing
/// to stderr, on Windows PowerShell 5.1 and on PowerShell 7 when present.
#[cfg(windows)]
#[test]
fn the_powershell_twins_print_the_expected_output() {
    let root = std::env::var_os("SystemRoot").expect("SystemRoot");
    let mut shells = vec![
        PathBuf::from(root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe"),
    ];
    let pwsh = Command::new("pwsh")
        .args(["-NoProfile", "-NonInteractive", "-Command", "exit 0"])
        .output();
    if pwsh.is_ok_and(|o| o.status.success()) {
        shells.push(PathBuf::from("pwsh"));
    } else {
        eprintln!("PowerShell 7 (pwsh) not found: only Windows PowerShell 5.1 is checked");
    }
    let all = all_cases();
    let mut failed = Vec::new();
    for shell in &shells {
        let command = |case: &Case| {
            let path = script(match case.kind {
                "statusline" => "statusline.ps1",
                _ => "subagent-statusline.ps1",
            });
            run(
                shell,
                &[
                    "-NoProfile".as_ref(),
                    "-NonInteractive".as_ref(),
                    "-ExecutionPolicy".as_ref(),
                    "Bypass".as_ref(),
                    "-File".as_ref(),
                    path.as_os_str(),
                ],
                &case.input,
            )
        };
        for why in check(&all, &command, true) {
            failed.push(format!("{}: {why}", shell.display()));
        }
    }
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}

/// The subagent script as this platform runs it, on `input`: the twin with
/// Windows PowerShell on Windows; the original with `sh` elsewhere, or `None`
/// without `jq`.
fn run_subagent_here(input: &[u8]) -> Option<Output> {
    if cfg!(windows) {
        let root = std::env::var_os("SystemRoot").expect("SystemRoot");
        let shell = PathBuf::from(root)
            .join("System32")
            .join("WindowsPowerShell")
            .join("v1.0")
            .join("powershell.exe");
        let path = script("subagent-statusline.ps1");
        Some(run(
            &shell,
            &[
                "-NoProfile".as_ref(),
                "-NonInteractive".as_ref(),
                "-ExecutionPolicy".as_ref(),
                "Bypass".as_ref(),
                "-File".as_ref(),
                path.as_os_str(),
            ],
            input,
        ))
    } else {
        let jq = Command::new("jq").arg("--version").output();
        if !jq.is_ok_and(|o| o.status.success()) {
            eprintln!("skipped: jq is not on PATH");
            return None;
        }
        let path = script("subagent-statusline.sh");
        Some(run(Path::new("sh"), &[path.as_os_str()], input))
    }
}

/// A running task shows how long it has run, before its tokens (time
/// dependent, so not a stored case): started 75 s ago reads `1m 15s` (or
/// `1m 16s` on a slow run). A finished one shows no time.
#[test]
fn a_running_subagent_shows_its_running_time() {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let start = now - 75_000;
    let input = format!(
        r#"{{"columns":80,"tasks":[{{"id":"r","name":"runner","tokenCount":172000,"contextWindowSize":1000000,"status":"running","startTime":{start}}},{{"id":"d","name":"done","tokenCount":1,"status":"completed","startTime":{start}}}]}}"#
    );
    let Some(out) = run_subagent_here(input.as_bytes()) else {
        return;
    };
    let text = String::from_utf8(out.stdout).unwrap();
    let rows: Vec<&str> = text.lines().collect();
    assert_eq!(rows.len(), 2, "{text}");
    assert!(
        rows[0].contains("\\u001b[90m1m 15s\\u001b[0m  \\u001b[33m172k 17%")
            || rows[0].contains("\\u001b[90m1m 16s\\u001b[0m  \\u001b[33m172k 17%"),
        "{}",
        rows[0]
    );
    assert!(!rows[1].contains("m 1"), "{}", rows[1]);
}

/// The `.sh` originals print the expected bytes, run as the built-in
/// `settings.json` runs them. Needs `bash` and `jq` (a Mac with jq: macOS 15
/// ships `/usr/bin/jq`, else Homebrew's).
#[cfg(unix)]
#[test]
fn the_shell_originals_print_the_expected_output() {
    let found = |tool: &str| {
        Command::new(tool)
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    };
    if !found("bash") || !found("jq") {
        eprintln!("skipped: bash or jq is not on PATH, so the .sh originals cannot run here");
        return;
    }
    let all = all_cases();
    let command = |case: &Case| {
        let (shell, file) = match case.kind {
            "statusline" => ("bash", "statusline.sh"),
            _ => ("sh", "subagent-statusline.sh"),
        };
        let path = script(file);
        run(Path::new(shell), &[path.as_os_str()], &case.input)
    };
    // jq reports bad input on stderr; only stdout and the exit code count.
    let failed = check(&all, &command, false);
    assert!(failed.is_empty(), "{}", failed.join("\n\n"));
}
