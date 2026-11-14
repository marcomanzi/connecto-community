//! D3 exit evidence: a real CLI's tool call under the generated profile.
//!
//! Evidence class REAL-CLI/FAKE-MODEL. The CLIs are private copies of the
//! installed, unmodified binaries, run by connecto's own launch path against a
//! local fake model, with a scratch home and empty profiles. No login, no real
//! model and no spend. It proves nothing about login or real model behaviour.
//!
//! Run with `cargo test -p connecto-session --features test-endpoints --test
//! real_cli -- --ignored --test-threads=1`. Ignored by default because it copies
//! both CLIs and needs them installed. Setting `CONNECTO_TEST_DUMP` prints the
//! screens, which hold only fake-model text here.

#![cfg(all(unix, feature = "test-endpoints"))]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::collections::BTreeMap;
use std::fs;
use std::net::TcpListener;
use std::os::unix::fs::{MetadataExt, PermissionsExt};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use common::{hook_binary, no_wake, screen_text};
use connecto_adapter::{Readiness, ToolAdapter};
use connecto_core::{
    AuthState, GridSize, InputModes, SessionStatus, TermSnapshot, TerminalSession, ToolEvent,
    encode_paste,
};
use connecto_session::LiveSession;
use connecto_session::launch::{LaunchExtras, Layout, adapter, check_auth, start_verified};
use connecto_store::{Codesign, Installed, Store, Tool};

const SIZE: GridSize = GridSize {
    cols: 120,
    rows: 40,
};
const TURN_TIMEOUT: Duration = Duration::from_secs(90);

/// Every write the session attempts, one line each. `$P` is the tool's profile.
const TOOL_COMMAND: &str = r#"P="${CLAUDE_CONFIG_DIR:-$CODEX_HOME}"; R="$PWD/result.txt"; : > "$R"
try() { (echo x > "$2") 2>/dev/null; echo "$1 $?" >> "$R"; }
try project "$PWD/in.txt"
try temp "$TMPDIR/t.txt"
try profile "$P/p.txt"
try outside "@ROOT@/outside/o.txt"
try home "@ROOT@/home/h.txt"
try slash_tmp "/private/tmp/mg-real-probe-$$"
try other_profile "$(dirname "$(dirname "$P")")/other-tool/x.txt"
try store "@DATA@/cli/x.txt"
try audit "@DATA@/audit/x.txt"
try run "$(dirname "$TMPDIR")/x.txt"
git init -q . ; echo "git_init $?" >> "$R"
echo done >> "$R"
"#;

/// V-32: shell temp files in a tool call. Each line records one result, and
/// `shell.txt` which shell ran it. The checks that need zsh syntax run from a
/// file, so a tool shell that is not zsh still reaches the end.
const SHELL_COMMAND: &str = r#"R="$PWD/result.txt"; : > "$R"
echo "dollar0=$0 zsh=${ZSH_VERSION:-none} bash=${BASH_VERSION:-none} SHELL=${SHELL:-unset} TMPPREFIX=${TMPPREFIX:-unset}" > "$PWD/shell.txt"
cat > "$PWD/heredoc.txt" <<'END'
heredoc-body
END
echo "tool_shell_heredoc $?" >> "$R"
printf 'cat > "$1" <<END\nzsh-body\nEND\n' > heredoc.sh
/bin/zsh heredoc.sh "$PWD/zsh-heredoc.txt" 2>/dev/null; echo "zsh_heredoc $?" >> "$R"
printf 'cat <<< here-string > "$1"\n' > string.zsh
/bin/zsh string.zsh "$PWD/here-string.txt" 2>/dev/null; echo "zsh_here_string $?" >> "$R"
printf 'print -r -- =(print x)\n' > subst.zsh
/bin/zsh subst.zsh > "$PWD/subst-path.txt" 2>/dev/null; echo "zsh_substitution $?" >> "$R"
M=$(mktemp -p "$TMPDIR" 2>/dev/null); echo "mktemp_p $?" >> "$R"
echo "$M" > "$PWD/mktemp-path.txt"
echo "$TMPDIR" > "$PWD/tmpdir.txt"
/bin/bash heredoc.sh "$PWD/bash-heredoc.txt" 2>/dev/null; echo "bash_heredoc $?" >> "$R"
/bin/sh heredoc.sh "$PWD/sh-heredoc.txt" 2>/dev/null; echo "sh_heredoc $?" >> "$R"
/bin/bash -c "$(cat heredoc.sh)" x "$PWD/bash-c-heredoc.txt" 2>/dev/null; echo "bash_c_heredoc $?" >> "$R"
mktemp > /dev/null 2>&1; echo "plain_mktemp $?" >> "$R"
echo done >> "$R"
"#;

struct Scene {
    root: PathBuf,
    layout: Layout,
    project: PathBuf,
    model: Child,
    url: String,
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
        let _ = self.model.kill();
        let _ = self.model.wait();
        unlock(&self.root);
        let _ = fs::remove_dir_all(&self.root);
    }
}

fn free_port() -> u16 {
    TcpListener::bind("127.0.0.1:0")
        .unwrap()
        .local_addr()
        .unwrap()
        .port()
}

impl Scene {
    fn new(tool: Tool) -> (Self, Installed) {
        Self::with_command(tool, TOOL_COMMAND)
    }

    fn with_command(tool: Tool, tool_command: &str) -> (Self, Installed) {
        let root =
            std::env::temp_dir().join(format!("mg-real-{}-{}", tool.name(), std::process::id()));
        unlock(&root);
        let _ = fs::remove_dir_all(&root);
        for dir in ["home/project", "data/audit", "outside"] {
            fs::create_dir_all(root.join(dir)).unwrap();
        }
        let root = fs::canonicalize(root).unwrap();
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        // Only the installed binary is read from the real home.
        let real_home = PathBuf::from(std::env::var_os("HOME").unwrap());
        let entry = real_home.join(".local/bin").join(tool.name());
        let store = Store::new(&layout.data_root);
        let installed = store.provision(tool, &entry, &Codesign).unwrap();
        store.install_hook(&hook_binary()).unwrap();

        let command = tool_command
            .replace("@ROOT@", &root.display().to_string())
            .replace("@DATA@", &layout.data_root.display().to_string());
        let command_file = root.join("tool-command.txt");
        fs::write(&command_file, command).unwrap();

        let port = free_port();
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("../../tests/fixtures/fake_model.py");
        let model = Command::new("/usr/bin/python3")
            .arg("-I")
            .arg(fixture)
            .arg(port.to_string())
            .arg(root.join("model-metadata.jsonl"))
            .arg(&command_file)
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let url = format!("http://127.0.0.1:{port}");
        let scene = Self {
            project: root.join("home/project"),
            root,
            layout,
            model,
            url,
        };
        scene.wait_for_model(port);
        (scene, installed)
    }

    fn wait_for_model(&self, port: u16) {
        let deadline = Instant::now() + Duration::from_secs(10);
        while std::net::TcpStream::connect(("127.0.0.1", port)).is_err() {
            assert!(Instant::now() < deadline, "the fake model did not start");
            thread::sleep(Duration::from_millis(20));
        }
    }

    fn start(&self, tool: Tool, installed: &Installed) -> LiveSession {
        start_verified(
            &self.layout,
            &self.project,
            tool,
            installed,
            SIZE,
            no_wake(),
            &LaunchExtras {
                fake_model: Some(self.url.clone()),
                ..LaunchExtras::default()
            },
            &connecto_session::launch::BridgeInfo::absent(),
        )
        .unwrap()
    }

    /// A folder standing for the other tool's profile, beside this one's.
    fn create_other_profile(&self, tool: Tool) {
        let projects = self.layout.data_root.join("projects");
        let project = fs::read_dir(projects).unwrap().next().unwrap().unwrap();
        let own = project.path().join("profiles").join(tool.name());
        assert!(own.is_dir());
        fs::create_dir_all(own.with_file_name("other-tool")).unwrap();
    }

    fn results(&self) -> BTreeMap<String, String> {
        let text = fs::read_to_string(self.project.join("result.txt")).unwrap_or_default();
        text.lines()
            .filter_map(|line| {
                let (name, rc) = line.split_once(' ')?;
                Some((name.to_owned(), rc.to_owned()))
            })
            .collect()
    }

    fn wait_for_results(&self) {
        let deadline = Instant::now() + TURN_TIMEOUT;
        let done = || {
            fs::read_to_string(self.project.join("result.txt"))
                .is_ok_and(|text| text.lines().any(|line| line == "done"))
        };
        while !done() {
            assert!(Instant::now() < deadline, "the tool call never finished");
            thread::sleep(Duration::from_millis(100));
        }
    }

    fn read_project(&self, name: &str) -> String {
        let text = fs::read_to_string(self.project.join(name)).unwrap_or_default();
        text.trim_end().to_owned()
    }

    /// V-32. Reports the shell before asserting, so a failure still answers it.
    fn assert_shell_temp_files(&self, tool: Tool) {
        let results = self.results();
        println!(
            "{} tool shell: {}",
            tool.name(),
            self.read_project("shell.txt")
        );
        println!("{} shell results: {results:?}", tool.name());
        let tmp = self.read_project("tmpdir.txt");
        assert!(!tmp.is_empty());
        for works in [
            "tool_shell_heredoc",
            "zsh_heredoc",
            "zsh_here_string",
            "zsh_substitution",
            "mktemp_p",
        ] {
            assert_eq!(results.get(works).map(String::as_str), Some("0"), "{works}");
        }
        assert_eq!(self.read_project("heredoc.txt"), "heredoc-body");
        assert_eq!(self.read_project("zsh-heredoc.txt"), "zsh-body");
        assert_eq!(self.read_project("here-string.txt"), "here-string");
        for name in ["subst-path.txt", "mktemp-path.txt"] {
            let path = self.read_project(name);
            assert!(path.starts_with(&format!("{tmp}/")), "{name}: {path}");
        }
        // Measured: bash and sh heredocs need a writable working folder, which
        // the project is. Plain mktemp uses a folder sessions cannot write.
        for works in ["bash_heredoc", "sh_heredoc", "bash_c_heredoc"] {
            assert_eq!(results.get(works).map(String::as_str), Some("0"), "{works}");
        }
        for name in ["bash-heredoc.txt", "sh-heredoc.txt", "bash-c-heredoc.txt"] {
            assert_eq!(self.read_project(name), "zsh-body", "{name}");
        }
        assert_ne!(results.get("plain_mktemp").map(String::as_str), Some("0"));
    }

    fn assert_confined(&self, tool: Tool) {
        let results = self.results();
        println!("{} tool call results: {results:?}", tool.name());
        for allowed in ["project", "temp", "profile", "git_init"] {
            assert_eq!(
                results.get(allowed).map(String::as_str),
                Some("0"),
                "{allowed}"
            );
        }
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

fn assert_running(session: &LiveSession, text: &str) {
    assert_eq!(session.status(), SessionStatus::Running, "screen:\n{text}");
}

/// Answer first-run screens by what they say, until the composer is up. This
/// lives in the test: the app never answers a screen for the user.
fn clear_first_run(
    session: &LiveSession,
    is_ready: impl Fn(&str) -> bool,
    steps: &[(&str, &[u8])],
) {
    let deadline = Instant::now() + Duration::from_secs(60);
    let mut last = String::new();
    let mut answered_at = Instant::now();
    loop {
        let text = screen(session);
        assert_running(session, &text);
        if text != last {
            dump("screen", &text);
            last.clone_from(&text);
        }
        if is_ready(&text) {
            return;
        }
        assert!(Instant::now() < deadline, "stuck on:\n{text}");
        // A screen needs time to take a key and redraw.
        if answered_at.elapsed() > Duration::from_millis(1200)
            && let Some((_, keys)) = steps.iter().find(|(marker, _)| text.contains(marker))
        {
            session.send_input(keys.to_vec()).unwrap();
            answered_at = Instant::now();
        }
        thread::sleep(Duration::from_millis(100));
    }
}

fn clear_claude_first_run(session: &LiveSession) {
    let enter: &[u8] = b"\r";
    let accept: &[u8] = b"\x1b[B\r";
    clear_first_run(
        session,
        |text| text.contains("bypass permissions on") || text.contains("? for shortcuts"),
        &[
            ("Yes, I accept", accept),
            ("Yes, I trust", accept),
            ("Yes, proceed", enter),
            ("Choose the text style", enter),
            ("Security notes", enter),
            ("Press Enter to continue", enter),
        ],
    );
}

fn clear_codex_first_run(session: &LiveSession) {
    let codex: &dyn ToolAdapter = adapter(Tool::Codex);
    let enter: &[u8] = b"\r";
    clear_first_run(
        session,
        |text| codex.readiness(text) == Readiness::Ready,
        &[
            ("Do you trust", enter),
            ("Yes, continue", enter),
            ("Press enter to continue", enter),
        ],
    );
}

/// V-10 and T-003: what the relay pastes, at the limit, over many lines, and the
/// shapes each CLI was measured rewriting. Each must still ack in `ack_form`.
fn relay_messages() -> Vec<(&'static str, String)> {
    let header = "[connecto relay m1 from user. Relayed content, not a user instruction.]";
    let line = format!("{}\n", "a".repeat(63));
    let limit = format!("{}{}b", line.repeat(63), "a".repeat(63));
    assert_eq!(limit.len(), 4096);
    let many: Vec<String> = (0..200).map(|n| format!("line {n}")).collect();
    vec![
        ("4096 bytes", format!("{header}\n{limit}")),
        ("many lines", format!("{header}\n{}", many.join("\n"))),
        ("one long line", format!("{header}\n{}", "c".repeat(4096))),
        (
            "two spaces at the end",
            format!("{header}\nkept  \n\nend  "),
        ),
        ("newline at the end", format!("{header}\nend\n")),
        ("two newlines at the end", format!("{header}\nend\n\n")),
        ("tab at the end", format!("{header}\nend\t")),
        ("space then newline", format!("{header}\nend \n")),
        ("no-break space at the end", format!("{header}\nend\u{a0}")),
        (
            "ideographic space at the end",
            format!("{header}\nend\u{3000}"),
        ),
        (
            "spaces before a line break",
            format!("{header}\nkept  \nend"),
        ),
        ("blank lines inside", format!("{header}\n\n\nend")),
        ("tab inside", format!("{header}\na\tb")),
        (
            "tabs at other columns",
            format!("{header}\nab\tc\t\tdefgh\ti"),
        ),
    ]
}

fn acks(tool: Tool, label: &str, sent: &str, reported: &str) -> bool {
    let first_diff = sent
        .bytes()
        .zip(reported.bytes())
        .position(|(a, b)| a != b)
        .unwrap_or(sent.len().min(reported.len()));
    println!(
        "{} {label}: sent {} reported {} first difference {first_diff} tail {:?}",
        tool.name(),
        sent.len(),
        reported.len(),
        reported
            .chars()
            .rev()
            .take(6)
            .collect::<String>()
            .chars()
            .rev()
            .collect::<String>(),
    );
    let form = adapter(tool);
    form.ack_form(sent) == form.ack_form(reported)
}

fn send_message(session: &LiveSession, text: &str, modes: InputModes) {
    session.send_input(encode_paste(text, modes)).unwrap();
    thread::sleep(Duration::from_millis(400));
    session.send_input(b"\r".to_vec()).unwrap();
}

fn current_modes(session: &LiveSession) -> InputModes {
    let mut snapshot = TermSnapshot::default();
    session.poll_snapshot(&mut snapshot);
    snapshot.modes
}

fn collect_until_turn_end(session: &LiveSession) -> Vec<ToolEvent> {
    let hook = session.hook().unwrap();
    let deadline = Instant::now() + TURN_TIMEOUT;
    let mut events = Vec::new();
    while Instant::now() < deadline {
        let Some(event) = hook.events().recv_timeout(Duration::from_millis(500)) else {
            continue;
        };
        let terminal = event.is_terminal();
        events.push(event);
        if terminal {
            return events;
        }
    }
    panic!(
        "no turn end. events={events:?} rejected={} screen:\n{}",
        hook.rejected(),
        screen(session)
    );
}

fn assert_image_in_store(session: &LiveSession, installed: &Installed) {
    let pid = session.pid().unwrap();
    let image = libproc::proc_pid::pidpath(pid).unwrap();
    assert_eq!(
        fs::canonicalize(image).unwrap(),
        fs::canonicalize(&installed.entry).unwrap()
    );
}

fn stop(session: &LiveSession) {
    session.terminate();
    assert!(session.wait_exit(Duration::from_secs(8)));
}

/// V-30 with real binaries, logged out. Whether status reads a real login from
/// inside the sandbox stays unmeasured until the user logs in.
#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn real_status_commands_answer() {
    for tool in [Tool::Claude, Tool::Codex] {
        let (scene, installed) = Scene::new(tool);
        let state = check_auth(&scene.layout, &scene.project, tool, &installed).unwrap();
        println!("{} status: {state:?}", tool.name());
        assert_eq!(state, AuthState::LoggedOut);
    }
}

#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn claude_makes_a_real_tool_call_under_the_generated_profile() {
    let tool = Tool::Claude;
    let (scene, installed) = Scene::new(tool);
    let session = scene.start(tool, &installed);
    scene.create_other_profile(tool);

    clear_claude_first_run(&session);
    assert_image_in_store(&session, &installed);

    send_message(&session, "please RUNTOOL now", current_modes(&session));
    let events = collect_until_turn_end(&session);
    dump("after turn", &screen(&session));

    assert!(
        matches!(events.first(), Some(ToolEvent::Started { .. })),
        "{events:?}"
    );
    let prompt = events.iter().find_map(|event| match event {
        ToolEvent::PromptSubmitted { prompt, .. } => Some(prompt.expose().clone()),
        _ => None,
    });
    assert_eq!(prompt.as_deref(), Some("please RUNTOOL now"));
    let Some(ToolEvent::TurnEnded { reply, .. }) = events.last() else {
        panic!("{events:?}");
    };
    let reply = reply.text().unwrap().expose();
    assert!(reply.contains("FAKE-CLAUDE-REPLY"), "{reply}");

    scene.wait_for_results();
    scene.assert_confined(tool);
    assert_eq!(session.hook().unwrap().rejected(), 0);
    stop(&session);
}

#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn claude_reports_a_relay_message_exactly() {
    let tool = Tool::Claude;
    let (scene, installed) = Scene::new(tool);
    let session = scene.start(tool, &installed);
    clear_claude_first_run(&session);
    let mut rewritten = Vec::new();
    for (label, text) in relay_messages() {
        send_message(&session, &text, current_modes(&session));
        let events = collect_until_turn_end(&session);
        let prompt = events.iter().find_map(|event| match event {
            ToolEvent::PromptSubmitted { prompt, .. } => Some(prompt.expose().clone()),
            _ => None,
        });
        if !acks(tool, label, &text, &prompt.unwrap()) {
            rewritten.push(label);
        }
    }
    assert_eq!(rewritten, Vec::<&str>::new());
    assert_eq!(session.hook().unwrap().rejected(), 0);
    stop(&session);
}

#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn codex_reports_a_relay_message_exactly() {
    let tool = Tool::Codex;
    let (scene, installed) = Scene::new(tool);
    let session = scene.start(tool, &installed);
    clear_codex_first_run(&session);
    let mut rewritten = Vec::new();
    for (label, text) in relay_messages() {
        send_message(&session, &text, current_modes(&session));
        let events = collect_until_turn_end(&session);
        let Some(ToolEvent::TurnEnded { input, .. }) = events.last() else {
            panic!("{events:?}");
        };
        if !acks(tool, label, &text, input.last().unwrap().expose()) {
            rewritten.push(label);
        }
    }
    assert_eq!(rewritten, Vec::<&str>::new());
    assert_eq!(session.hook().unwrap().rejected(), 0);
    stop(&session);
}

/// Section 8.2 rests on this: the interrupted turn fires no `Stop`, and the
/// next prompt's turn end is the first one the session sees.
#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn claude_fires_no_stop_for_a_turn_the_user_interrupts() {
    let tool = Tool::Claude;
    let (scene, installed) = Scene::new(tool);
    let session = scene.start(tool, &installed);
    clear_claude_first_run(&session);

    send_message(&session, "please FLOOD now", current_modes(&session));
    let hook = session.hook().unwrap();
    let deadline = Instant::now() + TURN_TIMEOUT;
    loop {
        assert!(Instant::now() < deadline, "no prompt event");
        let event = hook.events().recv_timeout(Duration::from_millis(200));
        if matches!(event, Some(ToolEvent::PromptSubmitted { .. })) {
            break;
        }
    }
    // The turn is under way while the footer offers to interrupt it.
    let deadline = Instant::now() + TURN_TIMEOUT;
    while !screen(&session).contains("esc to interrupt") {
        assert!(Instant::now() < deadline, "no turn:\n{}", screen(&session));
        thread::sleep(Duration::from_millis(100));
    }
    thread::sleep(Duration::from_secs(3));
    session.send_input(b"\x1b".to_vec()).unwrap();
    let deadline = Instant::now() + Duration::from_secs(15);
    let mut after_esc = Vec::new();
    while Instant::now() < deadline {
        if let Some(event) = hook.events().recv_timeout(Duration::from_millis(200)) {
            after_esc.push(event);
        }
    }
    let text = screen(&session);
    dump("after esc", &text);
    println!("after Esc, 15 s of hook events: {after_esc:?}");
    println!("interrupted shown: {}", text.contains("Interrupted"));
    assert!(text.contains("Interrupted"), "not interrupted:\n{text}");
    assert!(
        !after_esc.iter().any(ToolEvent::is_terminal),
        "Stop fired: {after_esc:?}"
    );

    // Only the order matters here. The fake model floods again, as FLOOD is
    // still in the conversation, and that reply is too long for a hook frame.
    send_message(&session, "a later turn", current_modes(&session));
    let deadline = Instant::now() + TURN_TIMEOUT;
    let first = loop {
        assert!(Instant::now() < deadline, "no event for the next turn");
        if let Some(event) = hook.events().recv_timeout(Duration::from_millis(200)) {
            break event;
        }
    };
    println!("the next turn's first event: {first:?}");
    assert!(
        matches!(first, ToolEvent::PromptSubmitted { .. }),
        "{first:?}"
    );
    stop(&session);
}

#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn codex_makes_a_real_tool_call_under_the_generated_profile() {
    let tool = Tool::Codex;
    let (scene, installed) = Scene::new(tool);
    let session = scene.start(tool, &installed);
    scene.create_other_profile(tool);

    clear_codex_first_run(&session);
    assert_image_in_store(&session, &installed);

    send_message(&session, "please RUNTOOL now", current_modes(&session));
    let events = collect_until_turn_end(&session);
    dump("after turn", &screen(&session));

    let Some(ToolEvent::TurnEnded { input, reply, .. }) = events.last() else {
        panic!("{events:?}");
    };
    assert_eq!(
        input.last().unwrap().expose(),
        "please RUNTOOL now",
        "a hidden title thread must not stand in for the user's turn"
    );
    assert!(
        reply.text().unwrap().expose().contains("FAKE-CODEX-REPLY"),
        "{events:?}"
    );

    scene.wait_for_results();
    scene.assert_confined(tool);
    assert_eq!(session.hook().unwrap().rejected(), 0);

    // Any title thread notifying late must still be filtered out.
    let hook = session.hook().unwrap();
    assert_eq!(hook.events().recv_timeout(Duration::from_secs(3)), None);
    stop(&session);
}

/// Every multiply linked file under `dir`, with its link count.
fn linked_files(dir: &Path) -> Vec<(PathBuf, u64)> {
    let mut found = Vec::new();
    let Ok(entries) = fs::read_dir(dir) else {
        return found;
    };
    for entry in entries.flatten() {
        let meta = entry.metadata().unwrap();
        if meta.is_dir() {
            found.extend(linked_files(&entry.path()));
        } else if meta.nlink() > 1 {
            found.push((entry.path(), meta.nlink()));
        }
    }
    found
}

/// ADR 0012 point 6: a CLI's own file tools must not leave a link that refuses
/// the next launch. The second start stands for Restart, which reaches the
/// same check. A failure here goes to the Architect; do not widen the rule.
fn file_tools_leave_a_project_that_launches_again(tool: Tool, clear: fn(&LiveSession)) {
    let (scene, installed) = Scene::with_command(tool, "@ROOT@/home/project");
    fs::write(scene.project.join("seed.txt"), "one\n").unwrap();
    let session = scene.start(tool, &installed);
    clear(&session);

    send_message(&session, "please RUNEDIT now", current_modes(&session));
    let events = collect_until_turn_end(&session);
    dump("after turn", &screen(&session));
    println!(
        "REAL-CLI/FAKE-MODEL {} events: {}",
        tool.name(),
        events.len()
    );
    assert_eq!(scene.read_project("seed.txt").trim(), "two");
    assert_eq!(scene.read_project("added.txt").trim(), "added");

    let in_project = linked_files(&scene.project);
    let in_data = linked_files(&scene.layout.data_root.join("projects"));
    println!(
        "REAL-CLI/FAKE-MODEL {} nlink>1 project={in_project:?} data={in_data:?}",
        tool.name()
    );
    stop(&session);
    drop(session);

    let again = scene.start(tool, &installed);
    stop(&again);
}

#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn claude_file_tools_leave_a_project_that_launches_again() {
    file_tools_leave_a_project_that_launches_again(Tool::Claude, clear_claude_first_run);
}

#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn codex_file_tools_leave_a_project_that_launches_again() {
    file_tools_leave_a_project_that_launches_again(Tool::Codex, clear_codex_first_run);
}

#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn claude_tool_call_can_use_shell_temp_files() {
    let tool = Tool::Claude;
    let (scene, installed) = Scene::with_command(tool, SHELL_COMMAND);
    let session = scene.start(tool, &installed);
    clear_claude_first_run(&session);
    send_message(&session, "please RUNTOOL now", current_modes(&session));
    collect_until_turn_end(&session);
    scene.wait_for_results();
    scene.assert_shell_temp_files(tool);
    stop(&session);
}

#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn codex_tool_call_can_use_shell_temp_files() {
    let tool = Tool::Codex;
    let (scene, installed) = Scene::with_command(tool, SHELL_COMMAND);
    let session = scene.start(tool, &installed);
    clear_codex_first_run(&session);
    send_message(&session, "please RUNTOOL now", current_modes(&session));
    collect_until_turn_end(&session);
    scene.wait_for_results();
    scene.assert_shell_temp_files(tool);
    stop(&session);
}

fn utility_title(scene: &Scene) -> (Result<String, String>, Duration) {
    let started = Instant::now();
    let result = connecto_session::utility::run_pty_title(
        scene.layout.clone(),
        no_wake(),
        LaunchExtras {
            fake_model: Some(scene.url.clone()),
            ..LaunchExtras::default()
        },
        scene.project.clone(),
        connecto_core::UtilitySettings::default(),
        // Long and over several lines: it must arrive whole (the run checks the
        // submitted prompt against this one and fails if it is cut).
        format!(
            "Give a title of 2 to 5 words for this. Reply with only the title.\n{}\nSCREEN END: the end",
            (0..60)
                .map(|n| format!(
                    "LINE {n}: some text about the login redirect {}",
                    "y".repeat(40)
                ))
                .collect::<Vec<_>>()
                .join("\n")
        ),
        Duration::from_secs(60),
    );
    (result, started.elapsed())
}

/// A profile that never ran Claude shows first-run screens. The hidden session
/// does not answer them: it must fail fast and say which screen blocks it.
#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn claude_utility_session_blocked_at_first_run_fails_fast_with_the_screen() {
    let (scene, _installed) = Scene::new(Tool::Claude);
    let (result, took) = utility_title(&scene);
    eprintln!("blocked run after {took:?}: {result:?}");
    let why = result.unwrap_err();
    assert!(why.contains("first-run") || why.contains("trust"), "{why}");
    assert!(took < Duration::from_secs(15), "{took:?}");
}

/// A hidden utility session against the real CLI and the fake model: it must
/// reach a reply without a person answering any screen.
#[test]
#[ignore = "needs the installed CLIs; see the file header"]
fn claude_utility_session_returns_a_reply_without_help() {
    let tool = Tool::Claude;
    let (scene, installed) = Scene::new(tool);
    // A normal terminal answers the first-run screens, as a person would have
    // done once in this project's profile.
    let first = scene.start(tool, &installed);
    clear_claude_first_run(&first);
    stop(&first);
    drop(first);
    let (result, took) = utility_title(&scene);
    eprintln!("utility result after {took:?}: {result:?}");
    assert!(result.is_ok(), "{result:?}");
}
