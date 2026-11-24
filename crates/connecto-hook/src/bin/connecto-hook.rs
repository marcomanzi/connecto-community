//! Forwards one hook payload to the session's socket, and is the `connecto`
//! command when run under that name (ADR 0016).
//!
//! `connecto-hook claude [--event NAME] [--context]` reads the payload from
//! stdin. `connecto-hook codex-hook --event NAME [--context]` does the same for a
//! Codex hook, and `connecto-hook pi --event NAME [--context]` for the extension
//! Connecto loads into Pi (its `--event Inbox --context` forwards nothing and
//! only prints the inbox), and `connecto-hook opencode ...` alike for the
//! plugin Connecto loads into OpenCode. `connecto-hook antigravity --event NAME` takes an
//! Antigravity CLI hook and always prints JSON (see
//! `connecto_hook::antigravity`). `connecto-hook codex <json>` takes Codex
//! `notify`'s payload from the last argument. A hook always exits 0 and prints at most the inbox: a
//! failure here must never disturb the tool.

use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::fs;
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::{Duration, Instant};

use connecto_hook::bridge::{BRIDGE_SOCKET_ENV, Op, Reply};
use connecto_hook::client::{self, Io};
use connecto_hook::shrink::{Kind, MAX_PAYLOAD, fit};
use connecto_hook::{SOCKET_ENV, TOKEN_ENV, antigravity, codex, encode, transport};

const TIMEOUT: Duration = Duration::from_secs(2);

/// The bytes of an argument or environment value: as given on Unix.
#[cfg(unix)]
fn os_bytes(value: &OsStr) -> Cow<'_, [u8]> {
    use std::os::unix::ffi::OsStrExt;
    Cow::Borrowed(value.as_bytes())
}

/// The bytes of an argument or environment value: UTF-8 on Windows (an
/// unpaired surrogate becomes U+FFFD).
#[cfg(windows)]
fn os_bytes(value: &OsStr) -> Cow<'_, [u8]> {
    match value.to_string_lossy() {
        Cow::Borrowed(text) => Cow::Borrowed(text.as_bytes()),
        Cow::Owned(text) => Cow::Owned(text.into_bytes()),
    }
}

/// Run as `connecto`: the program's file name is exactly `connecto`.
#[cfg(unix)]
fn is_connecto(program: &Path) -> bool {
    program.file_name() == Some(OsStr::new("connecto"))
}

/// Run as `connecto`: `connecto.exe` (or `connecto`), in any case.
#[cfg(windows)]
fn is_connecto(program: &Path) -> bool {
    program.file_name().is_some_and(|name| {
        name.eq_ignore_ascii_case("connecto.exe") || name.eq_ignore_ascii_case("connecto")
    })
}

/// How a hook was registered.
struct Mode {
    notify: bool,
    event: Option<String>,
    context: bool,
    /// Registered with `--holds`: Claude blocks the prompt if this helper
    /// fails or times out (`onFailure: "block"`), so it may hold one for an
    /// operation (ADR 0031).
    holds: bool,
    payload_arg: Option<Vec<u8>>,
    /// Registered by a Codex launch (`codex-hook`).
    codex: bool,
}

fn mode(args: &[OsString]) -> Option<Mode> {
    let name = args.first()?;
    if name == "codex" {
        return Some(Mode {
            notify: true,
            event: None,
            context: false,
            holds: false,
            payload_arg: args
                .last()
                .filter(|_| args.len() > 1)
                .map(|arg| os_bytes(arg).into_owned()),
            codex: false,
        });
    }
    if name != "claude" && name != "codex-hook" && name != "pi" && name != "opencode" {
        return None;
    }
    let mut event = None;
    let mut context = false;
    let mut holds = false;
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "--event" {
            event = rest.next().map(|name| name.to_string_lossy().into_owned());
        } else if arg == "--context" {
            context = true;
        } else if arg == "--holds" {
            holds = true;
        }
    }
    Some(Mode {
        notify: false,
        event,
        context,
        holds,
        payload_arg: None,
        codex: name == "codex-hook",
    })
}

/// Read stdin up to the limit, then drain the rest. False when it was cut.
fn read_stdin() -> Option<(Vec<u8>, bool)> {
    let mut payload = Vec::new();
    let limit = u64::try_from(MAX_PAYLOAD).ok()?;
    let mut stdin = std::io::stdin();
    (&mut stdin).take(limit).read_to_end(&mut payload).ok()?;
    let complete = std::io::copy(&mut stdin, &mut std::io::sink()).is_ok_and(|extra| extra == 0);
    Some((payload, complete))
}

fn forward(mode: &Mode, payload: Vec<u8>, complete: bool) -> Option<()> {
    let token = env::var_os(TOKEN_ENV)?;
    let kind = if mode.notify {
        Kind::Notify
    } else {
        Kind::Hook(mode.event.as_deref())
    };
    let token = os_bytes(&token);
    let payload = fit(kind, &payload, token.len(), complete)?;
    let frame = encode(&token, &payload)?;
    let socket = env::var_os(SOCKET_ENV)?;
    let mut stream = transport::connect(Path::new(&socket), TIMEOUT).ok()?;
    stream.write_all(&frame).ok()?;
    // Stay alive until the server has checked who we are.
    stream.read_exact(&mut [0_u8; 1]).ok()
}

/// How long a drain that carries a prompt may take: the app may ask Jev
/// whether the prompt matches a project operation (ADR 0031).
const PROMPT_DRAIN_TIMEOUT: Duration = Duration::from_secs(6);

/// How long the hook waits for the app to confirm an intercept (the app
/// gives the shell less, `INTERCEPT_READY` in `ops_host`).
const INTERCEPT_WAIT: Duration = Duration::from_secs(10);

/// How long the hook waits on `go`. The app answers it at once (it types
/// the run from a thread); the helper waits only so the bridge can still
/// see who sent it, and is done long before Claude's hook timeout.
const GO_WAIT: Duration = Duration::from_secs(2);

/// Past this, counted from the hook's start, go is not sent (the prompt is
/// stopped and nothing runs). The whole intercept stays far inside Claude's
/// hook timeout, set to [`CLAUDE_PROMPT_HOOK_TIMEOUT_S`] by the adapter.
const GO_BY: Duration = Duration::from_secs(25);

/// The `UserPromptSubmit` timeout the Claude adapter sets (seconds).
const CLAUDE_PROMPT_HOOK_TIMEOUT_S: u64 = 60;
const _: () = assert!(GO_BY.as_secs() + GO_WAIT.as_secs() < CLAUDE_PROMPT_HOOK_TIMEOUT_S);

/// The prompt in a prompt hook's payload, if it has one.
fn prompt_of(payload: &[u8]) -> Option<String> {
    let value: serde_json::Value = serde_json::from_slice(payload).ok()?;
    value
        .get("prompt")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned)
}

/// Ask the host for this terminal's inbox: its reply, unprinted. With the
/// prompt, the host may add a line about a matching operation, or answer
/// with an intercept to confirm (see [`drain`]).
fn drained_reply(event: &str, prompt: Option<String>, holds: bool) -> Option<Reply> {
    let socket = PathBuf::from(env::var_os(BRIDGE_SOCKET_ENV)?);
    drained_reply_at(&socket, event, prompt, holds)
}

fn drained_reply_at(
    socket: &Path,
    event: &str,
    prompt: Option<String>,
    holds: bool,
) -> Option<Reply> {
    let (frame, with_prompt) = drain_frame(event, prompt, holds)?;
    let wait = if with_prompt {
        PROMPT_DRAIN_TIMEOUT
    } else {
        TIMEOUT
    };
    let mut stream = transport::connect(socket, wait).ok()?;
    stream.write_all(&frame).ok()?;
    let reply = Reply::read_from(&mut stream).ok()?;
    reply.is_ok().then_some(reply)
}

/// The inbox text alone, for a tool that cannot be answered with a block:
/// an intercept answer is never printed as text.
fn drained(event: &str, prompt: Option<String>) -> Option<Vec<u8>> {
    let reply = drained_reply(event, prompt, false)?;
    if reply.header.get("intercept").is_some() || reply.body.is_empty() {
        return None;
    }
    Some(reply.body)
}

/// Ask the host for this terminal's inbox, and print it.
///
/// An intercept takes three steps, so the prompt is never both sent to the
/// agent and run: `op intercept <ticket>` (the app checks everything again
/// and prepares a ready shell, typing nothing), then the block is printed,
/// then `op go <ticket>`, which the app answers at once before typing the
/// run. Every step has a short
/// limit, so the helper is done long before Claude's hook timeout: Claude
/// never discards a block after the run started. If the first step fails,
/// nothing is printed and the prompt goes to the agent. If this helper
/// stops between the block and go, the prompt is stopped and nothing runs;
/// the app says in the Operations dialog that the operation did not start.
fn drain(event: &str, prompt: Option<String>, holds: bool) -> Option<()> {
    let socket = PathBuf::from(env::var_os(BRIDGE_SOCKET_ENV)?);
    drain_at(
        &socket,
        event,
        prompt,
        holds,
        &mut std::io::stdout(),
        STARTED.get().copied().unwrap_or_else(Instant::now),
    )
}

fn drain_at(
    socket: &Path,
    event: &str,
    prompt: Option<String>,
    holds: bool,
    out: &mut impl Write,
    started: Instant,
) -> Option<()> {
    let reply = drained_reply_at(socket, event, prompt, holds)?;
    let ticket = reply
        .header
        .get("intercept")
        .and_then(serde_json::Value::as_str)
        .map(str::to_owned);
    let frame = |verb: &str, ticket: &str| {
        let op = Op::Ops {
            verb: verb.to_owned(),
            args: vec![ticket.to_owned()],
            text: None,
        };
        encode(b"", &serde_json::to_vec(&op.to_json()).ok()?)
    };
    if let Some(ticket) = &ticket {
        let confirmed = frame("intercept", ticket).is_some_and(|f| {
            transport::connect(socket, INTERCEPT_WAIT)
                .and_then(|mut s| {
                    s.write_all(&f)?;
                    Reply::read_from(&mut s)
                })
                .is_ok_and(|r| r.is_ok())
        });
        if !confirmed {
            return None;
        }
    }
    if reply.body.is_empty() {
        return None;
    }
    out.write_all(&reply.body).ok()?;
    out.flush().ok()?;
    if let Some(ticket) = &ticket
        && let Some(f) = frame("go", ticket)
    {
        send_go(socket, &f, started);
    }
    Some(())
}

/// Hand `go` over, only while the deadline holds: the connection waits at
/// most until [`GO_BY`] (a blocking connect is run on its own thread and
/// abandoned when late), and the deadline is checked again just before the
/// write, the one step that starts the run. The answer changes nothing:
/// the block is printed either way.
fn send_go(socket: &Path, frame: &[u8], started: Instant) {
    let left = || GO_BY.saturating_sub(started.elapsed());
    if left().is_zero() {
        return;
    }
    let (tx, rx) = std::sync::mpsc::channel();
    let path = socket.to_path_buf();
    let wait = left().min(GO_WAIT);
    let spawned = std::thread::Builder::new()
        .name("go".into())
        .spawn(move || {
            let _ = tx.send(transport::connect(&path, wait));
        });
    if spawned.is_err() {
        return;
    }
    let Ok(Ok(mut stream)) = rx.recv_timeout(wait) else {
        return;
    };
    if left().is_zero() {
        return;
    }
    if stream.write_all(frame).is_ok() {
        let _ = Reply::read_from(&mut stream);
    }
}

/// The drain request, with the prompt when it fits in a frame. A prompt too
/// long for one is left out whole (never cut: a cut could drop a condition),
/// so the inbox still comes. Says whether the prompt went with it.
fn drain_frame(event: &str, prompt: Option<String>, holds: bool) -> Option<(Vec<u8>, bool)> {
    let build = |prompt: Option<String>| {
        let holds = holds && prompt.is_some();
        let payload = serde_json::to_vec(
            &Op::Drain {
                event: event.to_owned(),
                prompt,
                holds,
            }
            .to_json(),
        )
        .ok()?;
        encode(b"", &payload)
    };
    if prompt.is_some()
        && let Some(frame) = build(prompt)
    {
        return Some((frame, true));
    }
    build(None).map(|frame| (frame, false))
}

/// How long a `Stop` waits for the transcript to hold the turn's reply.
const REPLY_WAIT: Duration = Duration::from_millis(1500);
const REPLY_POLL: Duration = Duration::from_millis(100);

/// This terminal's helper state for Antigravity, in the session's temporary
/// folder, named after its hook channel (one per attempt).
fn antigravity_state_file() -> Option<PathBuf> {
    let socket = env::var_os(SOCKET_ENV)?;
    let name: String = Path::new(&socket)
        .file_name()?
        .to_string_lossy()
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect();
    Some(env::temp_dir().join(format!("connecto-antigravity-{name}.json")))
}

/// The reply of the turn that just stopped, once the transcript holds it.
fn antigravity_reply(transcript: &Path) -> antigravity::Lookup {
    let started = std::time::Instant::now();
    loop {
        let found = antigravity::transcript_reply(transcript);
        // A cut reply waits for the full transcript, written just after.
        if matches!(found, antigravity::Lookup::Reply(_)) || started.elapsed() >= REPLY_WAIT {
            return found;
        }
        std::thread::sleep(REPLY_POLL);
    }
}

/// Codex's `SessionStart`: hand the model the bridge text and the built-in
/// package's recap (compiled into this helper), as the launch set them. On a
/// resume, not when the thread's latest message from Connecto is the same.
fn codex_session_context(payload: &[u8]) {
    let flag = env::var(codex::RECAP_ENV).ok();
    let bridge = env::var(codex::BRIDGE_TEXT_ENV).ok();
    let value: serde_json::Value = serde_json::from_slice(payload).unwrap_or_default();
    let source = value["source"].as_str();
    let latest = (source == Some("resume"))
        .then(|| value["transcript_path"].as_str())
        .flatten()
        .and_then(|path| codex::latest_connecto_message(Path::new(path)));
    let start = codex::SessionStart {
        recap_flag: flag.as_deref(),
        bridge: bridge.as_deref(),
        source,
        latest: latest.as_deref(),
    };
    if let Some(body) = start.output() {
        let mut out = std::io::stdout();
        let _ = out.write_all(&body);
        let _ = out.flush();
    }
}

/// One run of an Antigravity hook. Returns the system message to hand the
/// model, if any.
fn antigravity_hook(event: &str) -> Option<String> {
    // Outside a Connecto terminal: nothing to report to. In another tool's
    // terminal (an `agy` run by a Codex agent inherits its hook channel):
    // nothing that is this terminal's to report.
    env::var_os(TOKEN_ENV)?;
    env::var_os(antigravity::TERMINAL_ENV)?;
    let (payload, complete) = read_stdin()?;
    let state_file = antigravity_state_file()?;
    let mut state = fs::read_to_string(&state_file)
        .map(|text| antigravity::State::parse(&text))
        .unwrap_or_default();
    let value: serde_json::Value = serde_json::from_slice(&payload).ok().filter(|_| complete)?;
    let action = antigravity::action(event, &value, &mut state);
    let _ = fs::write(&state_file, state.to_json());
    let send = |payload: &serde_json::Value| {
        let mode = Mode {
            notify: false,
            event: payload["hook_event_name"].as_str().map(str::to_owned),
            context: false,
            holds: false,
            payload_arg: None,
            codex: false,
        };
        let bytes = serde_json::to_vec(payload).unwrap_or_default();
        let _ = forward(&mode, bytes, true);
    };
    match action {
        antigravity::Action::Nothing => None,
        antigravity::Action::Started(payload) => {
            send(&payload);
            let file = env::var_os(antigravity::LAUNCH_TEXT_ENV)?;
            fs::read_to_string(file).ok()
        }
        antigravity::Action::Prompt(payload) => {
            send(&payload);
            env::var_os(antigravity::INBOX_ENV)?;
            let prompt = payload
                .get("prompt")
                .and_then(serde_json::Value::as_str)
                .map(str::to_owned);
            antigravity::inbox_text(&drained("UserPromptSubmit", prompt)?)
        }
        antigravity::Action::Stopped {
            mut payload,
            transcript,
        } => {
            if let Some(transcript) = transcript {
                antigravity::with_reply(&mut payload, antigravity_reply(Path::new(&transcript)));
            }
            send(&payload);
            None
        }
    }
}

/// `connecto-hook antigravity --event NAME`: always answers with JSON, as
/// Antigravity reads every hook's answer.
fn antigravity(args: &[OsString]) {
    let mut event = None;
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "--event" {
            event = rest.next().map(|name| name.to_string_lossy().into_owned());
        }
    }
    let message = event.as_deref().and_then(antigravity_hook);
    let mut out = std::io::stdout();
    let _ = out.write_all(antigravity::answer(message.as_deref()).as_bytes());
    let _ = out.flush();
}

/// Pi's extension asks for the inbox by itself, before a run starts: Pi has no
/// hook whose output joins the prompt.
const INBOX_EVENT: &str = "Inbox";

fn hook(args: &[OsString]) {
    if args.first().is_some_and(|name| name == "antigravity") {
        antigravity(args);
        return;
    }
    let Some(mode) = mode(args) else { return };
    if mode.event.as_deref() == Some(INBOX_EVENT) {
        if mode.context {
            // Pi's extension writes `{"prompt": ...}` on stdin.
            let prompt = read_stdin()
                .filter(|(_, complete)| *complete)
                .and_then(|(payload, _)| prompt_of(&payload));
            if let Some(body) = drained("UserPromptSubmit", prompt) {
                let mut out = std::io::stdout();
                let _ = out.write_all(&body);
                let _ = out.flush();
            }
        }
        return;
    }
    let (payload, complete) = match &mode.payload_arg {
        Some(arg) => (arg.clone(), true),
        None if mode.notify => return,
        None => match read_stdin() {
            Some(read) => read,
            None => return,
        },
    };
    let prompt = (mode.context && complete)
        .then(|| prompt_of(&payload))
        .flatten();
    let codex_start = mode.codex && mode.event.as_deref() == Some("SessionStart");
    let start_payload = (codex_start && complete).then(|| payload.clone());
    let _ = forward(&mode, payload, complete);
    if codex_start {
        codex_session_context(start_payload.as_deref().unwrap_or_default());
    }
    if mode.context && mode.event.as_deref() == Some("UserPromptSubmit") {
        let _ = drain("UserPromptSubmit", prompt, mode.holds);
    }
}

fn connecto(args: &[OsString]) -> ExitCode {
    let args: Vec<String> = args
        .iter()
        .map(|arg| arg.to_string_lossy().into_owned())
        .collect();
    let socket = env::var_os(BRIDGE_SOCKET_ENV).map(PathBuf::from);
    let mut stdin = std::io::stdin();
    let is_tty = stdin.is_terminal();
    let (mut out, mut err) = (std::io::stdout(), std::io::stderr());
    let mut io = Io {
        stdin: &mut stdin,
        stdin_is_tty: is_tty,
        out: &mut out,
        err: &mut err,
    };
    let code = client::run(&args, socket.as_deref().map(Path::new), &mut io);
    ExitCode::from(u8::try_from(code).unwrap_or(1))
}

/// When this helper started: Claude's hook timeout runs from about then,
/// so the intercept's deadline ([`GO_BY`]) does too, not from the drain.
static STARTED: std::sync::OnceLock<Instant> = std::sync::OnceLock::new();

fn main() -> ExitCode {
    STARTED.get_or_init(Instant::now);
    let mut args = env::args_os();
    let program = args.next().unwrap_or_default();
    let rest: Vec<OsString> = args.collect();
    if is_connecto(Path::new(&program)) {
        return connecto(&rest);
    }
    hook(&rest);
    ExitCode::SUCCESS
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_prompt_too_long_for_the_frame_is_left_out_and_the_inbox_still_drains() {
        let (_, with) = drain_frame("UserPromptSubmit", Some("deploy".into()), true).unwrap();
        assert!(with);
        let huge = "x".repeat(2 * connecto_hook::MAX_FRAME);
        let (frame, with) = drain_frame("UserPromptSubmit", Some(huge), true).unwrap();
        assert!(!with, "left out");
        assert!(frame.len() < 1024);
        let (_, with) = drain_frame("UserPromptSubmit", None, true).unwrap();
        assert!(!with);
    }

    /// A fake bridge: answers each connection with `replies[i]` after
    /// `delays[i]`, and records the verbs it was sent.
    #[cfg(unix)]
    fn fake_bridge(
        dir: &Path,
        replies: Vec<(Reply, Duration)>,
    ) -> (PathBuf, std::sync::mpsc::Receiver<String>) {
        let path = dir.join("bridge.sock");
        let listener = std::os::unix::net::UnixListener::bind(&path).unwrap();
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            for (reply, delay) in replies {
                let Ok((mut stream, _)) = listener.accept() else {
                    return;
                };
                let body = connecto_hook::read_body(&mut stream).unwrap();
                let frame = connecto_hook::decode(&body).unwrap();
                let op: serde_json::Value = serde_json::from_slice(frame.payload).unwrap();
                let verb = op["verb"].as_str().unwrap_or("drain").to_owned();
                let _ = tx.send(verb);
                let tx_reply = tx.clone();
                std::thread::spawn(move || {
                    std::thread::sleep(delay);
                    let _ = reply.write_to(&mut stream);
                    drop(tx_reply);
                });
            }
        });
        (path, rx)
    }

    #[cfg(unix)]
    #[test]
    fn a_late_answer_to_go_does_not_hold_the_hook() {
        let dir = tempfile::Builder::new()
            .prefix("ch")
            .tempdir_in("/tmp")
            .unwrap();
        let held = Reply::ok(serde_json::json!({ "intercept": "t1" }))
            .with_body(b"{\"decision\":\"block\"}".to_vec());
        let (socket, verbs) = fake_bridge(
            dir.path(),
            vec![
                (held, Duration::ZERO),
                (Reply::ok(serde_json::json!({})), Duration::ZERO),
                // go answered long after any hook limit.
                (Reply::ok(serde_json::json!({})), Duration::from_secs(30)),
            ],
        );
        let mut out = Vec::new();
        let started = Instant::now();
        let done = drain_at(
            &socket,
            "UserPromptSubmit",
            Some("run the tests".into()),
            true,
            &mut out,
            started,
        );
        assert!(done.is_some());
        assert!(
            started.elapsed() < GO_WAIT + Duration::from_secs(1),
            "{:?}",
            started.elapsed()
        );
        assert_eq!(out, b"{\"decision\":\"block\"}");
        let sent: Vec<String> = verbs.try_iter().collect();
        assert_eq!(sent, ["drain", "intercept", "go"]);
    }

    #[cfg(unix)]
    #[test]
    fn go_is_not_sent_past_its_deadline_or_after_a_refusal() {
        let dir = tempfile::Builder::new()
            .prefix("ch")
            .tempdir_in("/tmp")
            .unwrap();
        let held = || {
            Reply::ok(serde_json::json!({ "intercept": "t1" }))
                .with_body(b"{\"decision\":\"block\"}".to_vec())
        };
        // Refused confirmation: nothing printed, no go.
        let (socket, verbs) = fake_bridge(
            dir.path(),
            vec![
                (held(), Duration::ZERO),
                (Reply::refused("changed", "no", None), Duration::ZERO),
            ],
        );
        let mut out = Vec::new();
        assert!(
            drain_at(
                &socket,
                "UserPromptSubmit",
                Some("x".into()),
                true,
                &mut out,
                Instant::now()
            )
            .is_none()
        );
        assert!(out.is_empty());
        assert_eq!(verbs.try_iter().collect::<Vec<_>>(), ["drain", "intercept"]);
        // Past the deadline: the block is printed (the prompt stops), go is not sent.
        let dir = tempfile::Builder::new()
            .prefix("ch")
            .tempdir_in("/tmp")
            .unwrap();
        let (socket, verbs) = fake_bridge(
            dir.path(),
            vec![
                (held(), Duration::ZERO),
                (Reply::ok(serde_json::json!({})), Duration::ZERO),
            ],
        );
        let mut out = Vec::new();
        let long_ago = Instant::now().checked_sub(GO_BY).unwrap();
        assert!(
            drain_at(
                &socket,
                "UserPromptSubmit",
                Some("x".into()),
                true,
                &mut out,
                long_ago
            )
            .is_some()
        );
        assert!(!out.is_empty());
        std::thread::sleep(Duration::from_millis(100));
        assert_eq!(verbs.try_iter().collect::<Vec<_>>(), ["drain", "intercept"]);
    }
}
