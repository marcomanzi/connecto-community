//! Forwards one hook payload to the session's socket, and is the `connecto`
//! command when run under that name (ADR 0016).
//!
//! `connecto-hook claude [--event NAME] [--context]` reads the payload from
//! stdin. `connecto-hook codex-hook --event NAME [--context]` does the same for a
//! Codex hook. `connecto-hook codex <json>` takes Codex `notify`'s payload from
//! the last argument. A hook always exits 0 and prints at most the inbox: a
//! failure here must never disturb the tool.

use std::borrow::Cow;
use std::env;
use std::ffi::{OsStr, OsString};
use std::io::{IsTerminal, Read, Write};
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use connecto_hook::bridge::{BRIDGE_SOCKET_ENV, Op, Reply};
use connecto_hook::client::{self, Io};
use connecto_hook::shrink::{Kind, MAX_PAYLOAD, fit};
use connecto_hook::{SOCKET_ENV, TOKEN_ENV, encode, transport};

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
    payload_arg: Option<Vec<u8>>,
}

fn mode(args: &[OsString]) -> Option<Mode> {
    let name = args.first()?;
    if name == "codex" {
        return Some(Mode {
            notify: true,
            event: None,
            context: false,
            payload_arg: args
                .last()
                .filter(|_| args.len() > 1)
                .map(|arg| os_bytes(arg).into_owned()),
        });
    }
    if name != "claude" && name != "codex-hook" {
        return None;
    }
    let mut event = None;
    let mut context = false;
    let mut rest = args.iter().skip(1);
    while let Some(arg) = rest.next() {
        if arg == "--event" {
            event = rest.next().map(|name| name.to_string_lossy().into_owned());
        } else if arg == "--context" {
            context = true;
        }
    }
    Some(Mode {
        notify: false,
        event,
        context,
        payload_arg: None,
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

/// Ask the host for this terminal's inbox, and print it.
fn drain(event: &str) -> Option<()> {
    let socket = PathBuf::from(env::var_os(BRIDGE_SOCKET_ENV)?);
    let payload = serde_json::to_vec(
        &Op::Drain {
            event: event.to_owned(),
        }
        .to_json(),
    )
    .ok()?;
    let frame = encode(b"", &payload)?;
    let mut stream = transport::connect(&socket, TIMEOUT).ok()?;
    stream.write_all(&frame).ok()?;
    let reply = Reply::read_from(&mut stream).ok()?;
    if reply.is_ok() && !reply.body.is_empty() {
        let mut out = std::io::stdout();
        out.write_all(&reply.body).ok()?;
        out.flush().ok()?;
    }
    Some(())
}

fn hook(args: &[OsString]) {
    let Some(mode) = mode(args) else { return };
    let (payload, complete) = match &mode.payload_arg {
        Some(arg) => (arg.clone(), true),
        None if mode.notify => return,
        None => match read_stdin() {
            Some(read) => read,
            None => return,
        },
    };
    let _ = forward(&mode, payload, complete);
    if mode.context && mode.event.as_deref() == Some("UserPromptSubmit") {
        let _ = drain("UserPromptSubmit");
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

fn main() -> ExitCode {
    let mut args = env::args_os();
    let program = args.next().unwrap_or_default();
    let rest: Vec<OsString> = args.collect();
    if is_connecto(Path::new(&program)) {
        return connecto(&rest);
    }
    hook(&rest);
    ExitCode::SUCCESS
}
