//! The `connecto` command: parses one command, sends one request, prints the
//! reply. It has no logic of its own (ADR 0016).
//!
//! Exit codes: 0 success, 1 refused (reason on stderr), 2 usage error, 3 cannot
//! reach Connecto, 4 `result` has no reply to give (state on stdout).

use std::io::{self, Read, Write};
use std::path::Path;
use std::time::Duration;

use serde_json::Value;

use crate::bridge::{Op, Reply, valid_key};
use crate::encode;

pub const EXIT_OK: i32 = 0;
pub const EXIT_REFUSED: i32 = 1;
pub const EXIT_USAGE: i32 = 2;
pub const EXIT_UNREACHABLE: i32 = 3;
pub const EXIT_NO_RESULT: i32 = 4;

/// How long to wait on the socket. The host answers within 30 s.
const WAIT: Duration = Duration::from_secs(40);

const USAGE: &str = "usage:
  connecto list
  connecto send <id> [--ask] [text...]   (text from stdin if none is given)
  connecto result <request>              (after --ask, end your turn: the reply is delivered to you)
  connecto read <id>
  connecto keys <id> <key>...
  connecto watch <id> | unwatch <id>
  connecto add claude|codex|shell [--name <text>] [--model <model>]
  connecto start <id>                    start a stopped terminal
  connecto automation check [path] [--screen <textfile>]
                                        validate an automation.json (offline); with --screen,
                                        test its trigger against a saved screen
  connecto package check [id|path]       validate a package (offline); an id is looked up in
                                        ./packages/<id>, the default is the current folder
  connecto close <id>                    stop a terminal and remove it (it moves to the history)";

/// What the process gives the command.
pub struct Io<'a> {
    pub stdin: &'a mut dyn Read,
    pub stdin_is_tty: bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

fn usage(io: &mut Io<'_>, why: &str) -> i32 {
    let _ = writeln!(io.err, "connecto: {why}\n{USAGE}");
    EXIT_USAGE
}

/// Turn the arguments into a request, or say why not.
pub fn parse_args(args: &[String], io: &mut Io<'_>) -> Result<Op, String> {
    let Some((command, rest)) = args.split_first() else {
        return Err("no command".to_owned());
    };
    let one = |what: &str| -> Result<String, String> {
        match rest {
            [only] => Ok(only.clone()),
            _ => Err(format!("`{command}` takes one {what}")),
        }
    };
    match command.as_str() {
        "list" if rest.is_empty() => Ok(Op::List),
        "send" => {
            let (target, words) = rest.split_first().ok_or("`send` needs a terminal id")?;
            let mut ask = false;
            let mut text: Vec<&str> = Vec::new();
            for word in words {
                if word == "--ask" && text.is_empty() {
                    ask = true;
                } else {
                    text.push(word);
                }
            }
            let text = if text.is_empty() {
                if io.stdin_is_tty {
                    return Err("`send` needs text: give it as arguments or on stdin".to_owned());
                }
                let mut text = String::new();
                io.stdin
                    .read_to_string(&mut text)
                    .map_err(|err| format!("could not read stdin: {err}"))?;
                if text.is_empty() {
                    return Err("`send` got no text".to_owned());
                }
                text
            } else {
                text.join(" ")
            };
            Ok(Op::Send {
                target: target.clone(),
                ask,
                text,
            })
        }
        "keys" => {
            let (target, keys) = rest.split_first().ok_or("`keys` needs a terminal id")?;
            if keys.is_empty() {
                return Err("`keys` needs at least one key".to_owned());
            }
            if let Some(bad) = keys.iter().find(|key| !valid_key(key)) {
                return Err(format!("unknown key {bad:?}"));
            }
            Ok(Op::Keys {
                target: target.clone(),
                keys: keys.to_vec(),
            })
        }
        "read" => Ok(Op::Read {
            target: one("terminal id")?,
        }),
        "result" => Ok(Op::Result {
            request: one("request id")?,
        }),
        "watch" => Ok(Op::Watch {
            target: one("terminal id")?,
        }),
        "unwatch" => Ok(Op::Unwatch {
            target: one("terminal id")?,
        }),
        "add" => {
            let (tool, rest) = rest.split_first().ok_or("`add` takes a tool")?;
            let usage = "`add` takes a tool and optionally `--name <text>` and `--model <model>`";
            let (mut name, mut model) = (None, None);
            for pair in rest.chunks(2) {
                let [flag, value] = pair else {
                    return Err(usage.to_owned());
                };
                let slot = match flag.as_str() {
                    "--name" => &mut name,
                    "--model" => &mut model,
                    _ => return Err(usage.to_owned()),
                };
                // Each option once: a second one is a mistake, not an override.
                if slot.replace(value.clone()).is_some() {
                    return Err(usage.to_owned());
                }
            }
            Ok(Op::Add {
                tool: tool.clone(),
                name,
                model,
            })
        }
        "start" => Ok(Op::Start {
            target: one("terminal id")?,
        }),
        "close" => Ok(Op::Close {
            target: one("terminal id")?,
        }),
        other => Err(format!("unknown command {other:?}")),
    }
}

/// Send `op` to the bridge at `socket` (on Windows, a named pipe).
pub fn request(socket: &Path, op: &Op) -> io::Result<Reply> {
    let payload = serde_json::to_vec(&op.to_json()).map_err(io::Error::other)?;
    let frame = encode(b"", &payload).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "request over the frame limit")
    })?;
    let mut stream = crate::transport::connect(socket, WAIT)?;
    stream.write_all(&frame)?;
    Reply::read_from(&mut stream)
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}

fn table(header: &Value) -> String {
    let this = text(header, "self");
    let mut notes: Vec<String> = Vec::new();
    let mut rows = vec![[
        "ID".to_owned(),
        "TOOL".to_owned(),
        "STATE".to_owned(),
        "SECONDS".to_owned(),
        "LAST".to_owned(),
        "MARKS".to_owned(),
    ]];
    for terminal in header
        .get("terminals")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice)
    {
        let id = text(terminal, "id");
        let mut marks = Vec::new();
        if id == this {
            marks.push("self");
        }
        if terminal.get("watched").and_then(Value::as_bool) == Some(true) {
            marks.push("watched");
        }
        if text(terminal, "bridge") == "off" {
            marks.push("bridge off");
        }
        let reason = text(terminal, "bridge_reason").to_owned();
        let warning = text(terminal, "warning").to_owned();
        let exit = text(terminal, "exit").to_owned();
        if !warning.is_empty() {
            marks.push("not full power");
        }
        if !exit.is_empty() {
            marks.push(&exit);
        }
        notes.extend(
            [(id, reason), (id, warning)]
                .into_iter()
                .filter(|(_, why)| !why.is_empty())
                .map(|(id, why)| format!("{id}: {why}")),
        );
        rows.push([
            id.to_owned(),
            text(terminal, "tool").to_owned(),
            text(terminal, "state").to_owned(),
            terminal
                .get("seconds")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_owned(), |seconds| seconds.to_string()),
            terminal
                .get("last_event")
                .and_then(Value::as_str)
                .unwrap_or("-")
                .to_owned(),
            marks.join(", "),
        ]);
    }
    let widths: Vec<usize> = (0..6)
        .map(|col| rows.iter().map(|row| row[col].len()).max().unwrap_or(0))
        .collect();
    let mut out = String::new();
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    for note in notes {
        out.push_str(&note);
        out.push('\n');
    }
    if header.get("paused").and_then(Value::as_bool) == Some(true) {
        out.push_str("Connecto is paused by the user.\n");
    }
    out
}

/// Print a reply and give the exit code.
pub fn show(op: &Op, reply: &Reply, io: &mut Io<'_>) -> i32 {
    let header = &reply.header;
    if !reply.is_ok() {
        let mut reason = text(header, "reason").to_owned();
        if let Some(id) = header.get("id").and_then(Value::as_str) {
            reason.push_str(&format!(" (request {id})"));
        }
        let _ = writeln!(io.err, "connecto: {reason} [{}]", text(header, "code"));
        return EXIT_REFUSED;
    }
    let _ = match op {
        Op::List => write!(io.out, "{}", table(header)),
        Op::Send {
            ask: true, target, ..
        } => writeln!(
            io.out,
            "{} {}. {target}'s reply will be delivered to you automatically when it finishes. \
             End your turn now; do not poll `connecto result`.",
            text(header, "id"),
            text(header, "state"),
        ),
        Op::Send { .. } | Op::Keys { .. } => {
            writeln!(io.out, "{} {}", text(header, "id"), text(header, "state"))
        }
        Op::Read { .. } => {
            if header.get("history_full").and_then(Value::as_bool) == Some(true) {
                let _ = writeln!(
                    io.err,
                    "connecto: older lines have scrolled out of the 10,000-line history; this is not everything"
                );
            }
            io.out.write_all(&reply.body)
        }
        Op::Result { .. } => {
            if text(header, "state") == "ok" {
                io.out.write_all(&reply.body)
            } else {
                let reason = text(header, "reason");
                let line = if reason.is_empty() {
                    text(header, "state").to_owned()
                } else {
                    format!("{}: {reason}", text(header, "state"))
                };
                let _ = writeln!(io.out, "{line}");
                return EXIT_NO_RESULT;
            }
        }
        Op::Watch { .. } | Op::Unwatch { .. } => {
            let verb = if header.get("watching").and_then(Value::as_bool) == Some(true) {
                "watching"
            } else {
                "not watching"
            };
            writeln!(io.out, "{verb} {}", text(header, "target"))
        }
        Op::Close { .. } => {
            let id = text(header, "id");
            let unsaved = if header.get("saved").and_then(Value::as_bool) == Some(false) {
                " (the list could not be saved yet)"
            } else {
                ""
            };
            match text(header, "removal") {
                "done" => writeln!(io.out, "{id} stopped and removed{unsaved}"),
                "pending" => {
                    let note = match text(header, "reason") {
                        "" => String::new(),
                        why => format!(" ({why})"),
                    };
                    writeln!(
                        io.out,
                        "{id} is stopping and will be removed{note}{unsaved}"
                    )
                }
                "kept" => writeln!(
                    io.out,
                    "{id} stopped, but not removed: {}{unsaved}",
                    text(header, "reason")
                ),
                _ => writeln!(io.out, "{id} {}{unsaved}", text(header, "state")),
            }
        }
        Op::Add { .. } | Op::Start { .. } => {
            let unsaved = if header.get("saved").and_then(Value::as_bool) == Some(false) {
                " (not saved yet)"
            } else {
                ""
            };
            writeln!(
                io.out,
                "{} {}{unsaved}",
                text(header, "id"),
                text(header, "state")
            )
        }
        Op::Drain { .. } => io.out.write_all(&reply.body),
    };
    EXIT_OK
}

/// `connecto automation check [path]`: offline validation of an automation file.
/// The path may be the file or its folder; the default is `automation.json` in
/// the current folder. Works anywhere, with no socket.
fn automation(args: &[String], io: &mut Io<'_>) -> i32 {
    use crate::automation::{FILE_NAME, check_report, trigger_report};
    // `check [path] [--screen <file>]`
    let mut rest: Vec<&String> = match args.split_first() {
        Some((check, rest)) if check == "check" => rest.iter().collect(),
        _ => Vec::new(),
    };
    let mut screen_file = None;
    if let Some(at) = rest.iter().position(|a| *a == "--screen") {
        if at + 1 >= rest.len() {
            return usage(io, "`--screen` needs a file");
        }
        screen_file = Some(rest.remove(at + 1).clone());
        rest.remove(at);
    }
    if args.first().map(String::as_str) != Some("check") || rest.len() > 1 {
        return usage(
            io,
            "usage: connecto automation check [path] [--screen <textfile>]",
        );
    }
    let path = rest
        .first()
        .map_or_else(|| ".".to_owned(), |p| (*p).clone());
    let screen = match &screen_file {
        Some(file) => match std::fs::read_to_string(file) {
            Ok(text) => Some(text),
            Err(err) => {
                let _ = writeln!(io.err, "{file}: cannot read: {err}");
                return EXIT_REFUSED;
            }
        },
        None => None,
    };
    let given = Path::new(&path);
    // A folder with its own automation.json is one automation; the
    // Automations folder (with none of its own) holds many, one per subfolder.
    let files: Vec<std::path::PathBuf> = if given.is_dir() && !given.join(FILE_NAME).is_file() {
        let mut found: Vec<_> = std::fs::read_dir(given)
            .into_iter()
            .flatten()
            .flatten()
            .map(|entry| entry.path().join(FILE_NAME))
            .filter(|file| file.is_file())
            .collect();
        found.sort();
        if found.is_empty() {
            let _ = writeln!(io.err, "{path}: no {FILE_NAME} here or in a subfolder");
            return EXIT_REFUSED;
        }
        found
    } else if given.is_dir() {
        vec![given.join(FILE_NAME)]
    } else {
        vec![given.to_owned()]
    };
    let mut all_ok = true;
    for file in files {
        let text = std::fs::read_to_string(&file).map_err(|err| err.to_string());
        let shown = file.display().to_string();
        let (ok, report) = match (&screen, text) {
            // Offline: does the trigger match this saved screen? No Jev call.
            (Some(screen), Ok(text)) => trigger_report(&shown, &text, screen),
            (_, text) => check_report(&shown, text),
        };
        all_ok &= ok;
        let _ = if ok {
            io.out.write_all(report.as_bytes())
        } else {
            io.err.write_all(report.as_bytes())
        };
    }
    if all_ok { EXIT_OK } else { EXIT_REFUSED }
}

/// `connecto package check [id|path]`: offline validation of a package folder.
/// An existing folder is used as given; otherwise the argument is an id looked
/// up in `./packages/<id>` (the session runs in Connecto Home) and `./<id>`.
fn package(args: &[String], io: &mut Io<'_>) -> i32 {
    if args.first().map(String::as_str) != Some("check") || args.len() > 2 {
        return usage(io, "usage: connecto package check [id|path]");
    }
    let given = args.get(1).map_or(".", String::as_str);
    let candidates = [
        Path::new(given).to_owned(),
        Path::new("packages").join(given),
    ];
    let Some(dir) = candidates
        .iter()
        .find(|dir| dir.join(crate::package::FILE_NAME).is_file())
    else {
        let _ = writeln!(
            io.err,
            "{given}: no {} here or in ./packages/{given}",
            crate::package::FILE_NAME
        );
        return EXIT_REFUSED;
    };
    // A relative path such as `.` has no folder name to check as an id.
    let dir = std::fs::canonicalize(dir).unwrap_or_else(|_| dir.clone());
    let (ok, report) = crate::package::check_dir(&dir);
    let _ = if ok {
        io.out.write_all(report.as_bytes())
    } else {
        io.err.write_all(report.as_bytes())
    };
    if ok { EXIT_OK } else { EXIT_REFUSED }
}

/// Run the command. `socket` is `CONNECTO_BRIDGE_SOCK`.
pub fn run(args: &[String], socket: Option<&Path>, io: &mut Io<'_>) -> i32 {
    if args.first().map(String::as_str) == Some("automation") {
        return automation(&args[1..], io);
    }
    if args.first().map(String::as_str) == Some("package") {
        return package(&args[1..], io);
    }
    let op = match parse_args(args, io) {
        Ok(op) => op,
        Err(why) => return usage(io, &why),
    };
    let Some(socket) = socket else {
        let _ = writeln!(
            io.err,
            "connecto: CONNECTO_BRIDGE_SOCK is not set. This command works only inside a Connecto terminal."
        );
        return EXIT_UNREACHABLE;
    };
    match request(socket, &op) {
        Ok(reply) => show(&op, &reply, io),
        Err(err) => {
            let _ = writeln!(io.err, "connecto: cannot reach Connecto ({err})");
            EXIT_UNREACHABLE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use serde_json::json;

    use super::*;

    fn words(list: &[&str]) -> Vec<String> {
        list.iter().map(|word| (*word).to_owned()).collect()
    }

    fn parse(list: &[&str], stdin: &str, tty: bool) -> Result<Op, String> {
        let mut stdin = stdin.as_bytes();
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut io = Io {
            stdin: &mut stdin,
            stdin_is_tty: tty,
            out: &mut out,
            err: &mut err,
        };
        parse_args(&words(list), &mut io)
    }

    #[test]
    fn add_takes_a_tool_and_an_optional_name() {
        assert_eq!(
            parse(&["add", "claude"], "", true),
            Ok(Op::Add {
                tool: "claude".into(),
                name: None,
                model: None
            })
        );
        assert_eq!(
            parse(
                &["add", "claude", "--model", "opus", "--name", "review"],
                "",
                true
            ),
            Ok(Op::Add {
                tool: "claude".into(),
                name: Some("review".into()),
                model: Some("opus".into())
            })
        );
        assert_eq!(
            parse(&["add", "codex", "--name", "review the diff"], "", true),
            Ok(Op::Add {
                tool: "codex".into(),
                name: Some("review the diff".into()),
                model: None
            })
        );
        for bad in [
            &["add"][..],
            &["add", "claude", "--name"],
            &["add", "claude", "name"],
            &["add", "claude", "--name", "a", "b"],
            &["add", "claude", "--model"],
            &["add", "claude", "--model", "a", "--model", "b"],
        ] {
            assert!(parse(bad, "", true).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn commands_parse_and_text_comes_from_arguments_or_stdin() {
        assert_eq!(parse(&["list"], "", true), Ok(Op::List));
        assert_eq!(
            parse(&["send", "codex", "--ask", "hello", "there"], "", true),
            Ok(Op::Send {
                target: "codex".into(),
                ask: true,
                text: "hello there".into()
            })
        );
        assert_eq!(
            parse(&["send", "codex"], "long\ntext\n", false),
            Ok(Op::Send {
                target: "codex".into(),
                ask: false,
                text: "long\ntext\n".into()
            })
        );
        assert!(
            parse(&["send", "codex"], "", true).is_err(),
            "a TTY and no text"
        );
        assert!(parse(&["send", "codex"], "", false).is_err(), "empty stdin");
        assert!(parse(&["keys", "a", "bogus-key"], "", true).is_err());
        assert!(parse(&["keys", "a"], "", true).is_err());
        assert!(parse(&["nope"], "", true).is_err());
        assert!(parse(&["watch"], "", true).is_err());
        assert!(parse(&[], "", true).is_err());
    }

    fn shown(op: &Op, header: Value, body: &[u8]) -> (i32, String, String) {
        let reply = Reply::ok(header).with_body(body.to_vec());
        let mut stdin = &b""[..];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let code = {
            let mut io = Io {
                stdin: &mut stdin,
                stdin_is_tty: true,
                out: &mut out,
                err: &mut err,
            };
            show(op, &reply, &mut io)
        };
        (
            code,
            String::from_utf8(out).unwrap(),
            String::from_utf8(err).unwrap(),
        )
    }

    #[test]
    fn replies_print_as_the_spec_says_and_exit_with_the_right_code() {
        let (code, out, _) = shown(
            &Op::Send {
                target: "a".into(),
                ask: false,
                text: String::new(),
            },
            json!({ "id": "r12", "state": "typed" }),
            b"",
        );
        assert_eq!((code, out.as_str()), (0, "r12 typed\n"));

        let result = Op::Result {
            request: "r1".into(),
        };
        let (code, out, _) = shown(&result, json!({ "state": "ok" }), b"the reply");
        assert_eq!((code, out.as_str()), (0, "the reply"));
        let (code, out, _) = shown(
            &result,
            json!({ "state": "unmatched", "reason": "no evidence" }),
            b"",
        );
        assert_eq!((code, out.as_str()), (4, "unmatched: no evidence\n"));

        let (code, out, err) = shown(
            &Op::Read { target: "a".into() },
            json!({ "history_full": true }),
            b"screen",
        );
        assert_eq!((code, out.as_str()), (0, "screen"));
        assert!(err.contains("scrolled out"));

        let (_, out, _) = shown(
            &Op::Watch { target: "a".into() },
            json!({ "target": "codex-2", "watching": true }),
            b"",
        );
        assert_eq!(out, "watching codex-2\n");
    }

    #[test]
    fn package_check_takes_a_folder_and_validates_it_offline() {
        let root = std::env::temp_dir().join(format!("mg-pkg-cli-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let pkg = root.join("team");
        std::fs::create_dir_all(pkg.join(".claude/commands")).unwrap();
        std::fs::write(pkg.join(".claude/commands/go.md"), "go").unwrap();
        std::fs::write(pkg.join("package.json"), r#"{"name":"Team"}"#).unwrap();
        let check = |args: &[&str]| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut stdin = std::io::empty();
            let mut io = Io {
                stdin: &mut stdin,
                stdin_is_tty: false,
                out: &mut out,
                err: &mut err,
            };
            let code = run(&words(args), None, &mut io);
            (
                code,
                String::from_utf8(out).unwrap() + &String::from_utf8(err).unwrap(),
            )
        };
        let path = pkg.to_str().unwrap();
        assert_eq!(check(&["package", "check", path]).0, EXIT_OK);
        assert_eq!(
            check(&["package", "check", "/nonexistent/x"]).0,
            EXIT_REFUSED
        );
        assert_eq!(check(&["package", "nope"]).0, EXIT_USAGE);
        std::fs::write(pkg.join("package.json"), "{").unwrap();
        let (code, text) = check(&["package", "check", path]);
        assert_eq!(code, EXIT_REFUSED);
        assert!(text.contains("not valid JSON"), "{text}");
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn automation_check_takes_a_file_one_folder_or_the_whole_automations_folder() {
        use crate::automation::{Agent, Definition};
        let root = std::env::temp_dir().join(format!("mg-check-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        for name in ["a", "b"] {
            std::fs::create_dir_all(root.join(name)).unwrap();
        }
        let mut good = Definition::template("Good", Agent::Claude);
        good.question = crate::automation::Question::Noul {
            instructions: "Is it a prompt?".into(),
            yes: None,
            no: None,
        };
        good.matches = "true".into();
        good.trigger = vec!["Do you want to proceed".into()];
        std::fs::write(root.join("a/automation.json"), good.to_json().to_string()).unwrap();
        let check = |target: &std::path::Path| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut stdin = std::io::empty();
            let mut io = Io {
                stdin: &mut stdin,
                stdin_is_tty: false,
                out: &mut out,
                err: &mut err,
            };
            let args = words(&["automation", "check", target.to_str().unwrap()]);
            (run(&args, None, &mut io), String::from_utf8(err).unwrap())
        };
        assert_eq!(check(&root.join("a/automation.json")).0, EXIT_OK);
        assert_eq!(check(&root.join("a")).0, EXIT_OK);
        // The folder above: every automation in it, and a broken one fails it.
        assert_eq!(check(&root).0, EXIT_OK);
        // --screen: offline, whether the trigger matches a saved screen.
        let screen = root.join("screen.txt");
        std::fs::write(
            &screen,
            "\u{2502} DO YOU WANT\n\u{2502} TO PROCEED? \u{2502}",
        )
        .unwrap();
        let with_screen = |target: &std::path::Path, file: &std::path::Path| {
            let (mut out, mut err) = (Vec::new(), Vec::new());
            let mut stdin = std::io::empty();
            let mut io = Io {
                stdin: &mut stdin,
                stdin_is_tty: false,
                out: &mut out,
                err: &mut err,
            };
            let args = words(&[
                "automation",
                "check",
                target.to_str().unwrap(),
                "--screen",
                file.to_str().unwrap(),
            ]);
            (
                run(&args, None, &mut io),
                String::from_utf8(out).unwrap() + &String::from_utf8(err).unwrap(),
            )
        };
        let (code, text) = with_screen(&root.join("a"), &screen);
        assert_eq!(code, EXIT_OK, "{text}");
        assert!(text.contains("matches"));
        std::fs::write(&screen, "something else").unwrap();
        let (code, text) = with_screen(&root.join("a"), &screen);
        assert_eq!(code, EXIT_REFUSED, "{text}");
        assert!(text.contains("does not match"));
        assert_eq!(
            with_screen(&root.join("a"), &root.join("missing.txt")).0,
            EXIT_REFUSED
        );
        std::fs::write(root.join("b/automation.json"), "{").unwrap();
        let (code, err) = check(&root);
        assert_eq!(code, EXIT_REFUSED);
        let broken = Path::new("b").join("automation.json");
        assert!(err.contains(&*broken.to_string_lossy()), "{err}");
        assert_eq!(check(&root.join("missing")).0, EXIT_REFUSED);
        let _ = std::fs::remove_dir_all(root);
    }

    #[test]
    fn the_list_is_a_table_with_marks() {
        let (code, out, _) = shown(
            &Op::List,
            json!({ "self": "claude", "paused": false, "terminals": [
                { "id": "claude", "tool": "claude", "state": "working", "seconds": 3,
                  "last_event": null, "watched": false, "bridge": "on" },
                { "id": "codex-2", "tool": "codex", "state": "idle", "seconds": 12,
                  "last_event": "finished", "watched": true, "bridge": "off" },
            ] }),
            b"",
        );
        assert_eq!(code, 0);
        assert!(out.contains("claude") && out.contains("self"));
        assert!(out.contains("watched, bridge off"), "{out}");
        assert!(out.lines().next().unwrap().starts_with("ID"));
    }

    #[test]
    fn a_refusal_prints_the_reason_with_the_request_id_and_exits_1() {
        let reply = Reply::refused(
            "paused_before_enter",
            "The text may be in the box.",
            Some("r9"),
        );
        let mut stdin = &b""[..];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut io = Io {
            stdin: &mut stdin,
            stdin_is_tty: true,
            out: &mut out,
            err: &mut err,
        };
        let code = show(&Op::List, &reply, &mut io);
        assert_eq!(code, 1);
        let err = String::from_utf8(err).unwrap();
        assert!(
            err.contains("request r9") && err.contains("paused_before_enter"),
            "{err}"
        );
    }

    #[test]
    fn an_unreachable_socket_exits_3_and_a_missing_one_too() {
        let mut stdin = &b""[..];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut io = Io {
            stdin: &mut stdin,
            stdin_is_tty: true,
            out: &mut out,
            err: &mut err,
        };
        let args = words(&["list"]);
        assert_eq!(
            run(&args, Some(Path::new("/nonexistent/bridge.sock")), &mut io),
            3
        );
        assert_eq!(run(&args, None, &mut io), 3);
        assert_eq!(run(&words(&["bogus"]), None, &mut io), 2);
    }
}
