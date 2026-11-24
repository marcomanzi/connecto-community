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
/// How long `secret run` waits: the program's own limit (10 minutes) and a
/// little more.
const SECRET_RUN_WAIT: Duration = Duration::from_secs(11 * 60);

const USAGE: &str = "usage:
  connecto list
  connecto projects                      the projects Connecto knows, and their terminals
  connecto send <id> [--ask] [text...]   (text from stdin if none is given)
  connecto result <request>              (after --ask, end your turn: the reply is delivered to you)
  connecto read <id>
  connecto keys <id> <key>...
  connecto watch <id> | unwatch <id>
  connecto add claude|codex|pi|antigravity|opencode|shell [--name <text>]
                                        [--model <model>] [--effort <level>]
  connecto start <id>                    start a stopped terminal
  connecto agents [--roles]              the coding CLIs this project can start now (allowed,
                                        installed, signed in); with --roles, which one takes
                                        the coder, reviewer and second-opinion roles, and how
  --project <name|path>                  on list, read, send, result, watch and unwatch: act on
                                        another project's terminals. Only when the user or the
                                        task asks for it (a request to check or fix something
                                        in that project counts); never implied otherwise
  connecto automation check [path] [--screen <textfile>]
                                        validate an automation.json (offline); with --screen,
                                        test its trigger against a saved screen
  connecto package check [id|path]       validate a package (offline); an id is looked up in
                                        ./packages/<id>, the default is the current folder
  connecto close <id>                    stop a terminal and remove it (it moves to the history)
  connecto tasks <id>                    the background work its agent left running, and for how long
  connecto tasks <id> --stop [task]      stop its background shells, or only that one
  connecto send-file <path>              offer a file to the machines viewing this one over
                                        Connecto's remote link, to download (at most 64 MB)
  connecto memory brief|search|read|room|status [...]
                                        read the project memory (see `connecto memory help`)
  connecto memory propose|flag [...] <<'EOF'
                                        leave a note for later sessions (text on stdin)
  connecto op list                       the project's operations: ready-made scripts for routine requests
  connecto op show <name>                what one operation does, its arguments and its script
  connecto op run <name> [args...] [--yes]
                                        run an approved operation here (--yes once the user agreed)
  connecto op llm [--model haiku|sonnet] <<'EOF'
                                        ask a small model (for an operation's script); prints the reply
  connecto works help|show|databases|link|unlink|publish|test|job [...] [--db <name>]
                                        set up this project's Agentic Works (see `connecto works help`)
  connecto works ask '<question>'        an Agentic Works job's agent asks the user about its job;
                                        end your turn: a notice says when `connecto works answer` has it
  connecto secret list                   the secrets this terminal can use, by name (never their values)
  connecto secret request <NAME> --reason '<why>' [--kind token|password|ssh-key]
        [--http <https://base> [--header H] [--prefix P]] [--command </path> --env VAR] [--ssh]
        [--agentic-works]
                                        ask the user for a secret; a notice tells you the answer.
                                        The reason is one plain sentence the user can judge:
                                        what you will do with it and where
  connecto secret url <NAME>             the address to send web requests to (Connecto adds the secret)
  connecto secret run <NAME> -- <program> [args]
                                        run an allowed program with the secret, output hidden of it
  connecto secret ssh <NAME>             SSH_AUTH_SOCK and the public key for an SSH key";

/// What a `send` with no text is told, with the two forms that work.
fn send_needs_text(target: &str) -> String {
    format!(
        "`send` got no text, so nothing was sent. Put the message in quotes after the id: \
         connecto send {target} --ask 'your message'. For several lines use a heredoc, \
         with the text on the lines after the command and EOF alone on the last line."
    )
}

/// The `connecto memory` usage, one line per verb.
const MEMORY_USAGE: &str = "usage:
  connecto memory brief [--budget N]
  connecto memory search <text> [--room R] [--scope S] [--all] [--budget N]
  connecto memory read <id|room/slug> [--budget N]
  connecto memory room <room> [--budget N]
  connecto memory status [--budget N]
  connecto memory propose [--type integrazione|correzione|contraddizione|task] [--about <id>] [--room R] [--scope S] <<'EOF'
  connecto memory flag <id> [--type contraddizione|correzione] <<'EOF'
Budgets are in tokens, estimated as characters / 4.";

/// Longest `propose` / `flag` text, in bytes: `connecto_core::memory::MAX_NOTE_BYTES`.
/// A longer text is refused, never cut.
pub const MEMORY_TEXT_MAX: usize = 8 * 1024;

/// The `connecto op` usage.
const OP_USAGE: &str = "usage:
  connecto op list
  connecto op show <name>
  connecto op run <name> [args...] [--yes]
  connecto op llm [--model haiku|sonnet] <<'EOF'
  ...
  EOF";

/// Longest `op llm` prompt, in bytes. A longer one is refused, never cut.
pub const OP_LLM_MAX: usize = 32 * 1024;
/// How long `op llm` waits: the app's own limit (120 s) and a little more.
const OP_LLM_WAIT: Duration = Duration::from_secs(150);

/// What an older app answers to an operation it does not know.
const UNKNOWN_OP: &str = "unknown op";

/// What the process gives the command.
pub struct Io<'a> {
    pub stdin: &'a mut dyn Read,
    pub stdin_is_tty: bool,
    pub out: &'a mut dyn Write,
    pub err: &'a mut dyn Write,
}

fn usage(io: &mut Io<'_>, why: &str) -> i32 {
    // The reason comes first and again last: an agent's tool output is often
    // shown cut to its last lines, and the usage alone does not say what was
    // wrong.
    let _ = writeln!(io.err, "connecto: {why}\n{USAGE}\nconnecto: {why}");
    EXIT_USAGE
}

/// The commands `--project` works with (ADR 0035).
const ACROSS_PROJECTS: &[&str] = &["list", "read", "send", "result", "watch", "unwatch"];

/// A `--project` value that reads as a relative path (`./src`, `apps/web`)
/// made absolute from the folder `connecto` runs in: the app runs elsewhere,
/// and would otherwise read it from its own folder. The words are joined
/// as written, `..` included, and the result resolved by the filesystem when
/// it exists, so `link/../dest` goes where the link leads, as `cd` would. A
/// name, an absolute path and `~/...` are kept as given.
fn anchored(value: &str, cwd: Option<&std::path::Path>) -> String {
    let path = std::path::Path::new(value);
    let looks_like_path = value.contains('/') || value.contains(std::path::MAIN_SEPARATOR);
    match cwd {
        Some(cwd) if looks_like_path && path.is_relative() && !value.starts_with('~') => {
            let joined = cwd.join(path);
            std::fs::canonicalize(&joined)
                .unwrap_or(joined)
                .display()
                .to_string()
        }
        _ => value.to_owned(),
    }
}

/// Take `--project <name|path>` out of a command's words. For `send` it may
/// come before the id or among the flags before the text, never inside the
/// text; for the other commands that take it, anywhere. A command that does
/// not take it is refused when the flag is there, so nothing meant for
/// another project is done in this one.
fn take_project(command: &str, rest: &[String]) -> Result<(Vec<String>, Option<String>), String> {
    const FLAG: &str = "--project";
    let not_here = || {
        format!(
            "`{command}` works only in this project. Across projects only list, read, send,              result, watch and unwatch work (with `--project <name|path>`); see `connecto projects`."
        )
    };
    let mut project: Option<String> = None;
    let mut take = |value: Option<&String>| -> Result<(), String> {
        let value = value
            .filter(|value| !value.trim().is_empty())
            .ok_or("`--project` needs a project name or path (see `connecto projects`)")?;
        let value = anchored(value, std::env::current_dir().ok().as_deref());
        if project.replace(value).is_some() {
            return Err("`--project` may be given once".to_owned());
        }
        Ok(())
    };
    if !ACROSS_PROJECTS.contains(&command) {
        // Anywhere among its words, up to a `--` after which the words are
        // the payload's own (`secret run NAME -- program --project x`).
        let flagged = rest
            .iter()
            .take_while(|word| *word != "--")
            .any(|word| word == FLAG);
        return if flagged {
            Err(not_here())
        } else {
            Ok((rest.to_vec(), None))
        };
    }
    let mut kept = Vec::new();
    if command == "send" {
        let mut words = rest.iter();
        let mut seen_id = false;
        while let Some(word) = words.next() {
            if word == FLAG {
                take(words.next())?;
                continue;
            }
            if seen_id && word != "--ask" {
                // The text starts here, and runs to the end.
                kept.push(word.clone());
                kept.extend(words.cloned());
                break;
            }
            seen_id = true;
            kept.push(word.clone());
        }
    } else {
        let mut words = rest.iter();
        while let Some(word) = words.next() {
            if word == FLAG {
                take(words.next())?;
            } else {
                kept.push(word.clone());
            }
        }
    }
    Ok((kept, project))
}

/// Turn the arguments into a request, or say why not.
pub fn parse_args(args: &[String], io: &mut Io<'_>) -> Result<Op, String> {
    let Some((command, rest)) = args.split_first() else {
        return Err("no command".to_owned());
    };
    let (rest, project) = take_project(command, rest)?;
    let op = parse_command(command, &rest, io)?;
    Ok(match project {
        Some(project) => Op::InProject {
            project,
            op: Box::new(op),
        },
        None => op,
    })
}

fn parse_command(command: &String, rest: &[String], io: &mut Io<'_>) -> Result<Op, String> {
    let one = |what: &str| -> Result<String, String> {
        match rest {
            [only] => Ok(only.clone()),
            _ => Err(format!("`{command}` takes one {what}")),
        }
    };
    match command.as_str() {
        "list" if rest.is_empty() => Ok(Op::List),
        "projects" if rest.is_empty() => Ok(Op::Projects),
        "agents" => match rest {
            [] => Ok(Op::Agents { roles: false }),
            [flag] if flag == "--roles" => Ok(Op::Agents { roles: true }),
            _ => Err("`agents` takes only `--roles`".to_owned()),
        },
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
                    return Err(send_needs_text(target));
                }
                let mut text = String::new();
                io.stdin
                    .read_to_string(&mut text)
                    .map_err(|err| format!("could not read stdin: {err}"))?;
                if text.trim().is_empty() {
                    return Err(send_needs_text(target));
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
            let usage = "`add` takes a tool and optionally `--name <text>`, `--model <model>` and `--effort <level>`";
            let (mut name, mut model, mut effort) = (None, None, None);
            for pair in rest.chunks(2) {
                let [flag, value] = pair else {
                    return Err(usage.to_owned());
                };
                let slot = match flag.as_str() {
                    "--name" => &mut name,
                    "--model" => &mut model,
                    "--effort" => &mut effort,
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
                effort,
            })
        }
        "start" => Ok(Op::Start {
            target: one("terminal id")?,
        }),
        "close" => Ok(Op::Close {
            target: one("terminal id")?,
        }),
        "tasks" => {
            let usage = "`tasks` takes a terminal id and optionally `--stop [task]`";
            match rest {
                [target] => Ok(Op::Tasks {
                    target: target.clone(),
                    stop: false,
                    task: None,
                }),
                [target, flag, task @ ..] if flag == "--stop" && task.len() <= 1 => Ok(Op::Tasks {
                    target: target.clone(),
                    stop: true,
                    task: task.first().cloned(),
                }),
                _ => Err(usage.to_owned()),
            }
        }
        "memory" => memory_args(rest, io),
        "op" => op_args(rest, io),
        "secret" => secret_args(rest),
        "send-file" => match rest {
            [path] if !path.is_empty() => {
                let path = std::path::Path::new(path);
                let full = if path.is_absolute() {
                    path.to_path_buf()
                } else {
                    std::env::current_dir()
                        .map_err(|_| "the current folder cannot be read".to_owned())?
                        .join(path)
                };
                Ok(Op::SendFile {
                    path: full.to_string_lossy().into_owned(),
                })
            }
            _ => Err("`send-file` takes one file path".to_owned()),
        },
        "works" => works_args(rest, io),
        other => Err(format!("unknown command {other:?}")),
    }
}

/// Longest `works` definition or prompt, in bytes. A longer one is refused,
/// never cut.
pub const WORKS_TEXT_MAX: usize = 128 * 1024;
/// Longest service account key file `works key-import` reads.
pub const WORKS_KEY_MAX: usize = 64 * 1024;

/// Read a regular file, at most `max` bytes: a longer one, a folder, a
/// device or a pipe is refused, never read in part.
fn read_bounded(path: &str, max: usize) -> Result<String, String> {
    let regular = |meta: &std::fs::Metadata| meta.is_file();
    let meta = std::fs::metadata(path).map_err(|err| format!("could not read {path}: {err}"))?;
    if !regular(&meta) {
        return Err(format!("{path} is not a regular file"));
    }
    let file = std::fs::File::open(path).map_err(|err| format!("could not read {path}: {err}"))?;
    // Checked again on what was opened: the name may have changed since.
    if !file.metadata().is_ok_and(|meta| regular(&meta)) {
        return Err(format!("{path} is not a regular file"));
    }
    let mut bytes = Vec::new();
    let limit = u64::try_from(max + 1).unwrap_or(u64::MAX);
    Read::take(file, limit)
        .read_to_end(&mut bytes)
        .map_err(|err| format!("could not read {path}: {err}"))?;
    if bytes.len() > max {
        return Err(format!(
            "{path} is over {} KiB: it is refused, not cut.",
            max / 1024
        ));
    }
    String::from_utf8(bytes).map_err(|_| format!("{path} is not UTF-8 text"))
}

/// `connecto works <verb> [args]`. The app checks the verb and its options
/// and answers `help`; here only the text is read, as this terminal may read
/// it: a definition (`publish`, `check`) from `--file <path>` or stdin, a
/// prompt (`test`) from `--prompt <text>` or stdin, and the key file of
/// `key-import`.
fn works_args(rest: &[String], io: &mut Io<'_>) -> Result<Op, String> {
    let (verb, args) = match rest.split_first() {
        Some((verb, args)) => (verb.clone(), args.to_vec()),
        None => ("help".to_owned(), Vec::new()),
    };
    if verb == "key-import" {
        let [path] = args.as_slice() else {
            return Err("`works key-import` takes the key file's path".to_owned());
        };
        let text = read_bounded(path, WORKS_KEY_MAX)?;
        return Ok(Op::Works {
            verb,
            args,
            text: Some(text),
        });
    }
    let flag = match verb.as_str() {
        "publish" | "check" => Some("--file"),
        "test" => Some("--prompt"),
        // A job's question: as words, or on stdin when none is given.
        "ask" if args.first().is_none_or(|a| a == "--question") => Some("--question"),
        _ => None,
    };
    let mut kept = Vec::new();
    let mut given = None;
    if let Some(flag) = flag {
        let mut words = args.into_iter();
        while let Some(word) = words.next() {
            if word == flag {
                let value = words.next().ok_or(format!("{flag} needs a value"))?;
                if given.replace(value).is_some() {
                    return Err(format!("{flag} is given twice"));
                }
            } else {
                kept.push(word);
            }
        }
    } else {
        kept = args;
    }
    let text = match (flag, given) {
        (None, _) => None,
        (Some("--file"), Some(path)) => Some(read_bounded(&path, WORKS_TEXT_MAX)?),
        (Some(_), Some(prompt)) => {
            if prompt.len() > WORKS_TEXT_MAX {
                return Err("the prompt is over 128 KiB: it is refused, not cut.".to_owned());
            }
            Some(prompt)
        }
        (Some(flag), None) => {
            if io.stdin_is_tty {
                return Err(format!(
                    "`works {verb}` takes its text with {flag} or on stdin, for example:\n  connecto works {verb} ... <<'EOF'\n  ...\n  EOF"
                ));
            }
            let mut bytes = Vec::new();
            let limit = u64::try_from(WORKS_TEXT_MAX + 1).unwrap_or(u64::MAX);
            Read::take(&mut *io.stdin, limit)
                .read_to_end(&mut bytes)
                .map_err(|err| format!("could not read stdin: {err}"))?;
            if bytes.len() > WORKS_TEXT_MAX {
                return Err("the text is over 128 KiB: it is refused, not cut.".to_owned());
            }
            Some(String::from_utf8_lossy(&bytes).into_owned())
        }
    };
    if text.as_deref().is_some_and(|t| t.trim().is_empty()) {
        return Err(format!("`works {verb}` got no text. Nothing was done."));
    }
    Ok(Op::Works {
        verb,
        args: kept,
        text,
    })
}

/// `connecto secret <verb> [args]`. The app checks the verb and its options;
/// here `run` resolves its program to a full path the way this terminal's
/// shell would, and says the folder it was typed in.
fn secret_args(rest: &[String]) -> Result<Op, String> {
    let (verb, args) = rest
        .split_first()
        .ok_or("`secret` needs a command: `connecto secret list`")?;
    if verb != "run" {
        return Ok(Op::Secret {
            verb: verb.clone(),
            args: args.to_vec(),
            cwd: None,
        });
    }
    let usage = "`secret run` takes a secret's name, `--`, then the program and its arguments";
    let [name, dashes, program, program_args @ ..] = args else {
        return Err(usage.to_owned());
    };
    if dashes != "--" {
        return Err(usage.to_owned());
    }
    let cwd = std::env::current_dir().ok();
    let program = resolve_program(program, cwd.as_deref())
        .ok_or_else(|| format!("{program} was not found on PATH"))?;
    let mut out = vec![name.clone(), "--".to_owned(), program];
    out.extend(program_args.iter().cloned());
    Ok(Op::Secret {
        verb: verb.clone(),
        args: out,
        cwd: cwd.map(|dir| dir.to_string_lossy().into_owned()),
    })
}

/// `program` as a full path: as given when it has a `/`, else the first
/// match on `PATH`.
fn resolve_program(program: &str, cwd: Option<&Path>) -> Option<String> {
    if program.contains('/') {
        let path = Path::new(program);
        let full = if path.is_absolute() {
            path.to_path_buf()
        } else {
            cwd?.join(path)
        };
        return full.is_file().then(|| full.to_string_lossy().into_owned());
    }
    let paths = std::env::var_os("PATH")?;
    std::env::split_paths(&paths)
        .map(|dir| dir.join(program))
        .find(|candidate| candidate.is_file())
        .map(|found| found.to_string_lossy().into_owned())
}

/// `connecto op <verb> [args]`. The app checks the verb and its arguments;
/// here only the stdin prompt of `llm` is read, and refused when it is over
/// [`OP_LLM_MAX`] rather than cut.
fn op_args(rest: &[String], io: &mut Io<'_>) -> Result<Op, String> {
    let Some((verb, args)) = rest.split_first() else {
        return Err(format!("`op` needs a command\n{OP_USAGE}"));
    };
    if verb == "help" || verb == "--help" {
        return Err(OP_USAGE.to_owned());
    }
    let text = if verb == "llm" {
        if io.stdin_is_tty {
            return Err(
                "`op llm` takes its prompt on stdin, for example:\n  connecto op llm <<'EOF'\n  ...\n  EOF"
                    .to_owned(),
            );
        }
        let mut bytes = Vec::new();
        let limit = u64::try_from(OP_LLM_MAX + 1).unwrap_or(u64::MAX);
        Read::take(&mut *io.stdin, limit)
            .read_to_end(&mut bytes)
            .map_err(|err| format!("could not read stdin: {err}"))?;
        if bytes.len() > OP_LLM_MAX {
            return Err("the prompt is over 32 KiB: it is refused, not cut.".to_owned());
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.trim().is_empty() {
            return Err("`op llm` got no prompt on stdin.".to_owned());
        }
        Some(text)
    } else {
        None
    };
    Ok(Op::Ops {
        verb: verb.clone(),
        args: args.to_vec(),
        text,
    })
}

/// `connecto memory <verb> [args]`. The app checks the verb and its options;
/// here only the stdin text of `propose` and `flag` is read, and refused when
/// it is over [`MEMORY_TEXT_MAX`] rather than cut.
fn memory_args(rest: &[String], io: &mut Io<'_>) -> Result<Op, String> {
    let Some((verb, args)) = rest.split_first() else {
        return Err(format!("`memory` needs a command\n{MEMORY_USAGE}"));
    };
    if verb == "help" || verb == "--help" {
        return Err(MEMORY_USAGE.to_owned());
    }
    let text = if verb == "propose" || verb == "flag" {
        if io.stdin_is_tty {
            return Err(format!(
                "`memory {verb}` takes its text on stdin, for example:\n  connecto memory {verb} <<'EOF'\n  ...\n  EOF"
            ));
        }
        let mut bytes = Vec::new();
        let limit = u64::try_from(MEMORY_TEXT_MAX + 1).unwrap_or(u64::MAX);
        Read::take(&mut *io.stdin, limit)
            .read_to_end(&mut bytes)
            .map_err(|err| format!("could not read stdin: {err}"))?;
        if bytes.len() > MEMORY_TEXT_MAX {
            return Err(format!(
                "the text is over 8 KiB: it is refused, not cut. Nothing was stored. Split it into shorter notes with `memory {verb}`."
            ));
        }
        let text = String::from_utf8_lossy(&bytes).into_owned();
        if text.trim().is_empty() {
            return Err(format!(
                "`memory {verb}` got no text on stdin. Nothing was stored."
            ));
        }
        Some(text)
    } else {
        None
    };
    Ok(Op::Memory {
        verb: verb.clone(),
        args: args.to_vec(),
        text,
    })
}

/// Send `op` to the bridge at `socket` (on Windows, a named pipe).
pub fn request(socket: &Path, op: &Op) -> io::Result<Reply> {
    let payload = serde_json::to_vec(&op.to_json()).map_err(io::Error::other)?;
    let frame = encode(b"", &payload).ok_or_else(|| {
        io::Error::new(io::ErrorKind::InvalidInput, "request over the frame limit")
    })?;
    // A program run with a secret may take as long as its own limit.
    let wait = match op {
        Op::Secret { verb, .. } if verb == "run" => SECRET_RUN_WAIT,
        Op::Ops { verb, .. } if verb == "llm" => OP_LLM_WAIT,
        _ => WAIT,
    };
    let mut stream = crate::transport::connect(socket, wait)?;
    stream.write_all(&frame)?;
    Reply::read_from(&mut stream)
}

fn text<'a>(value: &'a Value, field: &str) -> &'a str {
    value.get(field).and_then(Value::as_str).unwrap_or("")
}

/// `connecto tasks`: one row per task, or a line saying there is none.
fn tasks_table(header: &Value) -> String {
    let id = text(header, "id");
    let tasks = header
        .get("tasks")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if tasks.is_empty() {
        return format!("{id} has no background work running.\n");
    }
    let mut rows = vec![[
        "TASK".to_owned(),
        "TYPE".to_owned(),
        "AGE".to_owned(),
        "RUNS".to_owned(),
    ]];
    for task in tasks {
        let mut age = text(task, "age").to_owned();
        if task.get("long").and_then(Value::as_bool) == Some(true) {
            age.push_str(" (long)");
        }
        rows.push([
            text(task, "id").to_owned(),
            text(task, "type").to_owned(),
            age,
            text(task, "label").to_owned(),
        ]);
    }
    let widths: Vec<usize> = (0..4)
        .map(|col| {
            rows.iter()
                .map(|row| row[col].chars().count())
                .max()
                .unwrap_or(0)
        })
        .collect();
    let mut out = format!("{id} {}\n", text(header, "state"));
    for row in &rows {
        let line: Vec<String> = row
            .iter()
            .zip(&widths)
            .map(|(cell, width)| format!("{cell:<width$}"))
            .collect();
        out.push_str(line.join("  ").trim_end());
        out.push('\n');
    }
    if tasks.iter().any(|task| text(task, "type") == "shell") {
        out.push_str(&format!(
            "Stop the shells with: connecto tasks {id} --stop [task]\n"
        ));
    }
    out
}

/// A list row's mark for background work: how many tasks, and the oldest's age.
fn background_mark(terminal: &Value) -> Option<String> {
    let tasks = terminal.get("background").and_then(Value::as_array)?;
    let oldest = tasks.first()?;
    let count = if tasks.len() == 1 {
        "1 task".to_owned()
    } else {
        format!("{} tasks", tasks.len())
    };
    let long = if oldest.get("long").and_then(Value::as_bool) == Some(true) {
        ", long-running"
    } else {
        ""
    };
    Some(format!("{count}, oldest {}{long}", text(oldest, "age")))
}

fn table(header: &Value) -> String {
    let this = text(header, "self");
    let mut notes: Vec<String> = Vec::new();
    let mut rows = vec![[
        "ID".to_owned(),
        "TOOL".to_owned(),
        "STATE".to_owned(),
        "CONTEXT".to_owned(),
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
        // A terminal an agent started belongs to that agent.
        let owner = match text(terminal, "owner") {
            "" => String::new(),
            owner if owner == this => "yours".to_owned(),
            owner => format!("belongs to {owner}"),
        };
        if !owner.is_empty() {
            marks.push(&owner);
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
        let background = background_mark(terminal).unwrap_or_default();
        if !background.is_empty() {
            marks.push(&background);
            notes.push(format!(
                "{id}: background work running; `connecto tasks {id}` lists it"
            ));
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
            // The share of its context window the agent has used, as its
            // status line shows it; "-" when it shows none.
            terminal
                .get("context")
                .and_then(Value::as_u64)
                .map_or_else(|| "-".to_owned(), |used| format!("{used}%")),
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
    let widths: Vec<usize> = (0..7)
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

/// `connecto projects` as a table, then one line per open project naming
/// its terminals.
fn projects_table(header: &Value) -> String {
    let projects = header
        .get("projects")
        .and_then(Value::as_array)
        .map_or(&[][..], Vec::as_slice);
    if projects.is_empty() {
        return "No projects.\n".to_owned();
    }
    let mut rows = vec![[
        "NAME".to_owned(),
        "STATE".to_owned(),
        "TERMINALS".to_owned(),
        "PATH".to_owned(),
    ]];
    let mut lines = Vec::new();
    for project in projects {
        let name = text(project, "name");
        let state = if project.get("this").and_then(Value::as_bool) == Some(true) {
            "this project"
        } else if project.get("open").and_then(Value::as_bool) == Some(true) {
            "open"
        } else {
            "closed"
        };
        let terminals = project
            .get("terminals")
            .and_then(Value::as_array)
            .map_or(&[][..], Vec::as_slice);
        let count = project
            .get("count")
            .and_then(Value::as_u64)
            .map_or_else(|| terminals.len().to_string(), |count| count.to_string());
        rows.push([
            name.to_owned(),
            state.to_owned(),
            count,
            text(project, "path").to_owned(),
        ]);
        if !terminals.is_empty() {
            let shown: Vec<String> = terminals
                .iter()
                .map(|terminal| {
                    let owner = match text(terminal, "owner") {
                        "" => String::new(),
                        owner => format!(", belongs to {owner}"),
                    };
                    format!(
                        "{} ({}, {}{owner})",
                        text(terminal, "id"),
                        text(terminal, "tool"),
                        text(terminal, "state")
                    )
                })
                .collect();
            lines.push(format!("{name}: {}", shown.join(", ")));
        }
    }
    let widths: Vec<usize> = (0..4)
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
    for line in lines {
        out.push_str(&line);
        out.push('\n');
    }
    out
}

/// Print a reply and give the exit code.
pub fn show(op: &Op, reply: &Reply, io: &mut Io<'_>) -> i32 {
    let header = &reply.header;
    if matches!(op, Op::InProject { .. } | Op::Projects)
        && !reply.is_ok()
        && text(header, "code") == "bad_request"
        && text(header, "reason").starts_with(UNKNOWN_OP)
    {
        let _ = writeln!(
            io.err,
            "connecto: this Connecto app is older than this command and cannot reach other projects. \
             Nothing was done. Update Connecto (or restart it, if it was just updated)."
        );
        return EXIT_REFUSED;
    }
    if let Op::InProject { op, .. } = op {
        if reply.is_ok() && matches!(**op, Op::List) {
            let _ = writeln!(io.out, "Terminals of project {}:", text(header, "project"));
        }
        return show(op, reply, io);
    }
    if !reply.is_ok() {
        if matches!(op, Op::Secret { .. })
            && text(header, "code") == "bad_request"
            && text(header, "reason").starts_with(UNKNOWN_OP)
        {
            let _ = writeln!(
                io.err,
                "connecto: this Connecto app is older than this command and has no secrets. \
                 Update Connecto (or restart it, if it was just updated) to use `connecto secret`."
            );
            return EXIT_REFUSED;
        }
        if matches!(
            op,
            Op::Add {
                effort: Some(_),
                ..
            }
        ) && text(header, "code") == "bad_request"
            && text(header, "reason").starts_with(UNKNOWN_OP)
        {
            let _ = writeln!(
                io.err,
                "connecto: this Connecto app is older than this command and cannot set an effort. \
                 Update Connecto (or restart it, if it was just updated), or leave out `--effort`."
            );
            return EXIT_REFUSED;
        }
        if matches!(op, Op::Agents { .. })
            && text(header, "code") == "bad_request"
            && text(header, "reason").starts_with(UNKNOWN_OP)
        {
            let _ = writeln!(
                io.err,
                "connecto: this Connecto app is older than this command and cannot list its agents. \
                 Update Connecto (or restart it, if it was just updated). Until then, check with \
                 `connecto list` and `connecto add`, which says which CLIs the project allows."
            );
            return EXIT_REFUSED;
        }
        if matches!(op, Op::Ops { .. })
            && text(header, "code") == "bad_request"
            && text(header, "reason").starts_with(UNKNOWN_OP)
        {
            let _ = writeln!(
                io.err,
                "connecto: this Connecto app is older than this command and has no project operations. \
                 Update Connecto (or restart it, if it was just updated) to use `connecto op`."
            );
            return EXIT_REFUSED;
        }
        if matches!(op, Op::Works { .. })
            && text(header, "code") == "bad_request"
            && text(header, "reason").starts_with(UNKNOWN_OP)
        {
            let _ = writeln!(
                io.err,
                "connecto: this Connecto app is older than this command and cannot set up Agentic Works. \
                 Update Connecto (or restart it, if it was just updated) to use `connecto works`."
            );
            return EXIT_REFUSED;
        }
        if matches!(op, Op::Memory { .. })
            && text(header, "code") == "bad_request"
            && text(header, "reason").starts_with(UNKNOWN_OP)
        {
            let _ = writeln!(
                io.err,
                "connecto: this Connecto app is older than this command and has no project memory. \
                 Update Connecto (or restart it, if it was just updated) to use `connecto memory`."
            );
            return EXIT_REFUSED;
        }
        let mut reason = text(header, "reason").to_owned();
        if let Some(id) = header.get("id").and_then(Value::as_str) {
            reason.push_str(&format!(" (request {id})"));
        }
        let _ = writeln!(io.err, "connecto: {reason} [{}]", text(header, "code"));
        return EXIT_REFUSED;
    }
    // A send that waits for the user to finish typing in the target says so.
    if matches!(op, Op::Send { .. })
        && let Some(note) = header.get("note").and_then(Value::as_str)
    {
        let _ = writeln!(io.out, "{note}");
    }
    let _ = match op {
        Op::List => write!(io.out, "{}", table(header)),
        Op::Projects => write!(io.out, "{}", projects_table(header)),
        Op::InProject { .. } => Ok(()),
        Op::Send {
            ask: true, target, ..
        } => {
            // Across projects the target is named with its project.
            let target = match text(header, "project") {
                "" => target.clone(),
                project => format!("{target} (project {project})"),
            };
            writeln!(
                io.out,
                "{} {}. {target}'s reply will be delivered to you automatically when it finishes. \
                 End your turn now; do not poll `connecto result`.",
                text(header, "id"),
                text(header, "state"),
            )
        }
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
        Op::Tasks { stop: false, .. } => write!(io.out, "{}", tasks_table(header)),
        Op::Tasks { stop: true, .. } | Op::SendFile { .. } => {
            writeln!(io.out, "{}", text(header, "note"))
        }
        Op::Secret { verb, .. } if verb == "run" => return show_run(header, &reply.body, io),
        Op::Drain { .. }
        | Op::Agents { .. }
        | Op::Memory { .. }
        | Op::Secret { .. }
        | Op::Ops { .. }
        | Op::Works { .. } => io.out.write_all(&reply.body),
    };
    EXIT_OK
}

/// What the app answered to `op run` of a script: the program to run here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpExec {
    pub run: String,
    pub slug: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: String,
    pub env: Vec<(String, String)>,
    pub timeout: Duration,
}

impl OpExec {
    /// # Errors
    ///
    /// The header is not an exec answer.
    pub fn from_header(header: &Value) -> Result<Self, String> {
        let exec = header.get("exec").ok_or("no exec")?;
        let field = |name: &str| {
            exec.get(name)
                .and_then(Value::as_str)
                .map(str::to_owned)
                .ok_or_else(|| format!("exec has no `{name}`"))
        };
        let strings = |value: Option<&Value>| -> Option<Vec<String>> {
            value?
                .as_array()?
                .iter()
                .map(|v| v.as_str().map(str::to_owned))
                .collect()
        };
        let env = exec
            .get("env")
            .and_then(Value::as_array)
            .map(|pairs| {
                pairs
                    .iter()
                    .filter_map(|pair| {
                        let pair = strings(Some(pair))?;
                        match pair.as_slice() {
                            [k, v] => Some((k.clone(), v.clone())),
                            _ => None,
                        }
                    })
                    .collect()
            })
            .unwrap_or_default();
        Ok(Self {
            run: field("run")?,
            slug: field("slug")?,
            program: field("program")?,
            args: strings(exec.get("args")).ok_or("exec has no `args`")?,
            cwd: field("cwd")?,
            env,
            timeout: Duration::from_secs(
                exec.get("timeout_s")
                    .and_then(Value::as_u64)
                    .unwrap_or(600)
                    .clamp(1, 3600),
            ),
        })
    }
}

/// An operation's process: on Unix its own process group, which gets the
/// terminal while it runs (so Ctrl-C and typed input reach it), the signals
/// this command receives, and on a timeout TERM then KILL for the whole
/// group. Processes it detached into a session of their own (`setsid`) are
/// left alone, as are those still running after a normal end.
mod group {
    use std::process::{Child, Command};
    use std::time::{Duration, Instant};

    use super::OP_POLL;

    pub(super) struct Running {
        child: Child,
        #[cfg(unix)]
        unix: unix::Group,
    }

    pub(super) fn spawn(command: &mut Command) -> std::io::Result<Running> {
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            command.process_group(0);
            let child = command.spawn()?;
            let unix = unix::Group::take(child.id());
            Ok(Running { child, unix })
        }
        #[cfg(not(unix))]
        {
            Ok(Running {
                child: command.spawn()?,
            })
        }
    }

    impl Running {
        /// Wait for the end, or stop it at `timeout`. The exit code and the
        /// outcome word.
        pub(super) fn wait(&mut self, timeout: Duration) -> (i32, String) {
            let started = Instant::now();
            let out = loop {
                match self.child.try_wait() {
                    Ok(Some(status)) => {
                        break match status.code() {
                            Some(code) => (code, format!("exit {code}")),
                            None => (128, "killed by a signal".to_owned()),
                        };
                    }
                    Ok(None) if started.elapsed() >= timeout => {
                        self.stop();
                        break (124, "timeout".to_owned());
                    }
                    Ok(None) => std::thread::sleep(OP_POLL),
                    Err(err) => break (super::EXIT_REFUSED, format!("lost: {err}")),
                }
            };
            #[cfg(unix)]
            self.unix.give_back();
            out
        }

        fn stop(&mut self) {
            #[cfg(unix)]
            {
                self.unix.signal(nix::sys::signal::Signal::SIGTERM);
                let deadline = Instant::now() + Duration::from_secs(2);
                while Instant::now() < deadline && !self.unix.empty() {
                    let _ = self.child.try_wait();
                    std::thread::sleep(OP_POLL);
                }
                self.unix.signal(nix::sys::signal::Signal::SIGKILL);
            }
            let _ = self.child.kill();
            let _ = self.child.wait();
        }
    }

    #[cfg(unix)]
    mod unix {
        use std::io::IsTerminal;
        use std::sync::Arc;
        use std::sync::atomic::{AtomicBool, Ordering};

        use nix::sys::signal::{SigSet, Signal, killpg};
        use nix::unistd::{Pid, getpgrp, tcgetpgrp, tcsetpgrp};

        pub(super) struct Group {
            pgid: Pid,
            /// The terminal was handed to the group: give it back after.
            foreground: bool,
            forwarding: Option<signal_hook::iterator::Handle>,
            done: Arc<AtomicBool>,
        }

        impl Group {
            pub(super) fn take(pid: u32) -> Self {
                let pgid = Pid::from_raw(i32::try_from(pid).unwrap_or(i32::MAX));
                let stdin = std::io::stdin();
                let foreground =
                    stdin.is_terminal() && tcgetpgrp(&stdin).is_ok_and(|fg| fg == getpgrp());
                if foreground {
                    // A process outside the foreground group that sets it
                    // gets SIGTTOU unless the signal is blocked.
                    let _ = SigSet::from(Signal::SIGTTOU).thread_block();
                    let _ = tcsetpgrp(&stdin, pgid);
                    // A first read before the handoff stopped it (SIGTTIN):
                    // now that it has the terminal, let it go on.
                    let _ = killpg(pgid, Signal::SIGCONT);
                }
                let done = Arc::new(AtomicBool::new(false));
                let forwarding = signal_hook::iterator::Signals::new([
                    signal_hook::consts::SIGINT,
                    signal_hook::consts::SIGTERM,
                    signal_hook::consts::SIGHUP,
                    signal_hook::consts::SIGQUIT,
                ])
                .ok()
                .and_then(|mut signals| {
                    let handle = signals.handle();
                    let done = Arc::clone(&done);
                    std::thread::Builder::new()
                        .name("op-signals".into())
                        .spawn(move || {
                            for number in signals.forever() {
                                if done.load(Ordering::SeqCst) {
                                    break;
                                }
                                if let Ok(signal) = Signal::try_from(number) {
                                    let _ = killpg(pgid, signal);
                                }
                            }
                        })
                        .ok()
                        .map(|_| handle)
                });
                Self {
                    pgid,
                    foreground,
                    forwarding,
                    done,
                }
            }

            pub(super) fn signal(&self, signal: Signal) {
                let _ = killpg(self.pgid, signal);
            }

            /// No process is left in the group.
            pub(super) fn empty(&self) -> bool {
                killpg(self.pgid, None).is_err()
            }

            pub(super) fn give_back(&mut self) {
                self.done.store(true, Ordering::SeqCst);
                if let Some(handle) = self.forwarding.take() {
                    handle.close();
                }
                if self.foreground {
                    self.foreground = false;
                    let _ = tcsetpgrp(std::io::stdin(), getpgrp());
                }
            }
        }

        impl Drop for Group {
            fn drop(&mut self) {
                self.give_back();
            }
        }
    }
}

/// How often a running operation is checked for its end.
const OP_POLL: Duration = Duration::from_millis(100);

/// Run an operation's script here, with this terminal's output, stop it at
/// its timeout, and tell the app how it ended. Returns its exit code (124
/// when it ran out of time, as `timeout` says).
fn run_op(header: &Value, socket: &Path, io: &mut Io<'_>) -> i32 {
    let exec = match OpExec::from_header(header) {
        Ok(exec) => exec,
        Err(why) => {
            let _ = writeln!(io.err, "connecto: unexpected answer from Connecto: {why}");
            return EXIT_REFUSED;
        }
    };
    let _ = writeln!(
        io.err,
        "connecto: running operation {} (run {})",
        exec.slug, exec.run
    );
    let _ = io.err.flush();
    let started = std::time::Instant::now();
    let mut command = std::process::Command::new(&exec.program);
    command
        .args(&exec.args)
        .current_dir(&exec.cwd)
        .envs(exec.env.iter().map(|(k, v)| (k, v)));
    let (code, outcome) = match group::spawn(&mut command) {
        Err(err) => (EXIT_REFUSED, format!("not started: {err}")),
        Ok(mut running) => running.wait(exec.timeout),
    };
    let elapsed = started.elapsed();
    let done = Op::Ops {
        verb: "done".to_owned(),
        args: vec![
            exec.run.clone(),
            outcome.clone(),
            elapsed.as_millis().to_string(),
        ],
        text: None,
    };
    let recorded = request(socket, &done).is_ok_and(|reply| reply.is_ok());
    let _ = writeln!(
        io.err,
        "connecto: operation {} finished ({outcome}, {:.1}s){}",
        exec.slug,
        elapsed.as_secs_f64(),
        if recorded {
            ""
        } else {
            "; Connecto could not record it"
        }
    );
    code
}

/// What a program run with a secret printed, each stream to its own, and its
/// exit code (124 when it ran out of time, as `timeout` says).
fn show_run(header: &Value, body: &[u8], io: &mut Io<'_>) -> i32 {
    let split = header
        .get("stdout_len")
        .and_then(Value::as_u64)
        .and_then(|n| usize::try_from(n).ok())
        .unwrap_or(body.len())
        .min(body.len());
    let (out, err) = body.split_at(split);
    let _ = io.out.write_all(out);
    let _ = io.err.write_all(err);
    if header.get("timed_out").and_then(Value::as_bool) == Some(true) {
        let _ = writeln!(
            io.err,
            "connecto: the program ran out of time and was stopped"
        );
        return 124;
    }
    header
        .get("code")
        .and_then(Value::as_i64)
        .and_then(|code| i32::try_from(code).ok())
        .unwrap_or(1)
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
        Ok(reply)
            if reply.is_ok()
                && matches!(&op, Op::Ops { verb, .. } if verb == "run")
                && reply.header.get("exec").is_some() =>
        {
            run_op(&reply.header, socket, io)
        }
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
    fn works_reads_a_definition_or_a_prompt_and_nothing_else() {
        let works = |list: &[&str], stdin: &str, tty: bool| match parse(list, stdin, tty) {
            Ok(Op::Works {
                verb, args, text, ..
            }) => Ok((verb, args, text)),
            Ok(other) => Err(format!("not a works op: {other:?}")),
            Err(why) => Err(why),
        };
        assert_eq!(
            works(&["works"], "", true),
            Ok(("help".into(), Vec::new(), None))
        );
        assert_eq!(
            works(&["works", "link", "--secret", "DB", "--create"], "", true),
            Ok(("link".into(), words(&["--secret", "DB", "--create"]), None))
        );
        assert_eq!(
            works(&["works", "publish", "summary"], "{\"a\":1}", false),
            Ok((
                "publish".into(),
                words(&["summary"]),
                Some("{\"a\":1}".into())
            ))
        );
        assert!(works(&["works", "publish", "summary"], "", true).is_err());
        assert!(works(&["works", "publish", "summary"], " \n", false).is_err());
        // A job's question: as words (stdin is not read), with --question,
        // or on stdin.
        assert_eq!(
            works(&["works", "ask", "Which", "season?"], "", false),
            Ok(("ask".into(), words(&["Which", "season?"]), None))
        );
        assert_eq!(
            works(&["works", "ask", "--question", "Which season?"], "", true),
            Ok(("ask".into(), Vec::new(), Some("Which season?".into())))
        );
        assert_eq!(
            works(&["works", "ask"], "Which season?\n", false),
            Ok(("ask".into(), Vec::new(), Some("Which season?\n".into())))
        );
        assert!(works(&["works", "ask"], "", true).is_err());
        assert_eq!(
            works(
                &[
                    "works",
                    "test",
                    "summary",
                    "--prompt",
                    "the README",
                    "--cli",
                    "claude"
                ],
                "",
                true
            ),
            Ok((
                "test".into(),
                words(&["summary", "--cli", "claude"]),
                Some("the README".into())
            ))
        );
        assert!(works(&["works", "test", "s", "--prompt"], "", true).is_err());
        let long = "x".repeat(WORKS_TEXT_MAX + 1);
        assert!(works(&["works", "test", "s", "--prompt", &long], "", true).is_err());
        let dir = std::env::temp_dir().join(format!("works-def-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let def = dir.join("def.json");
        std::fs::write(&def, "{}").unwrap();
        let path = def.to_string_lossy().into_owned();
        assert_eq!(
            works(&["works", "check", "--file", &path], "", true),
            Ok(("check".into(), Vec::new(), Some("{}".into())))
        );
        // The key file is read here, with this terminal's own rights, and
        // sent once; a folder or a file over the limit is not read.
        assert_eq!(
            works(&["works", "key-import", &path], "", true),
            Ok(("key-import".into(), words(&[&path]), Some("{}".into())))
        );
        let folder = dir.to_string_lossy().into_owned();
        assert!(works(&["works", "key-import", &folder], "", true).is_err());
        assert!(works(&["works", "check", "--file", &folder], "", true).is_err());
        let big = dir.join("big.json");
        std::fs::write(&big, vec![b' '; WORKS_KEY_MAX + 1]).unwrap();
        let big = big.to_string_lossy().into_owned();
        assert!(works(&["works", "key-import", &big], "", true).is_err());
        assert!(works(&["works", "key-import"], "", true).is_err());
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn op_takes_its_verb_and_llm_reads_stdin() {
        assert_eq!(
            parse(&["op", "run", "deploy", "prod", "--yes"], "", true),
            Ok(Op::Ops {
                verb: "run".into(),
                args: vec!["deploy".into(), "prod".into(), "--yes".into()],
                text: None,
            })
        );
        assert_eq!(
            parse(&["op", "llm"], "Write a commit message.", false),
            Ok(Op::Ops {
                verb: "llm".into(),
                args: Vec::new(),
                text: Some("Write a commit message.".into()),
            })
        );
        assert!(
            parse(&["op", "llm"], "", true).is_err(),
            "a tty has no prompt"
        );
        assert!(parse(&["op", "llm"], "  \n", false).is_err());
        let over = "x".repeat(OP_LLM_MAX + 1);
        assert!(parse(&["op", "llm"], &over, false).is_err());
        assert!(parse(&["op"], "", true).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn a_timeout_stops_the_scripts_children_too() {
        let dir = std::env::temp_dir().join(format!("connecto-op-tree-{}", std::process::id()));
        let _ = std::fs::create_dir_all(&dir);
        let marker = dir.join("survived");
        let _ = std::fs::remove_file(&marker);
        // Children started while it stops are in the group too.
        let script = format!(
            "trap '(sleep 3; touch \"{0}\") &' TERM; (sleep 3; touch '{0}') & wait; wait",
            marker.display()
        );
        let mut command = std::process::Command::new("/bin/bash");
        command.args(["-c", &script]);
        let mut running = group::spawn(&mut command).unwrap();
        let (code, outcome) = running.wait(Duration::from_millis(300));
        assert_eq!((code, outcome.as_str()), (124, "timeout"));
        std::thread::sleep(Duration::from_secs(4));
        assert!(!marker.exists(), "a child outlived the timeout");
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn an_op_exec_header_reads_back() {
        let header = json!({"exec": {
            "run": "r1", "slug": "tests", "program": "/bin/bash",
            "args": ["-c", "true", "tests"], "cwd": "/p",
            "env": [["CONNECTO_OP", "tests"], ["bad"]], "timeout_s": 99999
        }});
        let exec = OpExec::from_header(&header).unwrap();
        assert_eq!(exec.env, [("CONNECTO_OP".to_owned(), "tests".to_owned())]);
        assert_eq!(exec.timeout.as_secs(), 3600, "clamped");
        assert!(OpExec::from_header(&json!({"exec": {"run": "r"}})).is_err());
    }

    #[test]
    fn add_takes_a_tool_and_an_optional_name() {
        assert_eq!(
            parse(&["add", "claude"], "", true),
            Ok(Op::Add {
                tool: "claude".into(),
                name: None,
                model: None,
                effort: None
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
                model: Some("opus".into()),
                effort: None
            })
        );
        assert_eq!(
            parse(&["add", "codex", "--name", "review the diff"], "", true),
            Ok(Op::Add {
                tool: "codex".into(),
                name: Some("review the diff".into()),
                model: None,
                effort: None
            })
        );
        assert_eq!(
            parse(
                &[
                    "add",
                    "codex",
                    "--model",
                    "gpt-6.2-astra",
                    "--effort",
                    "medium"
                ],
                "",
                true
            ),
            Ok(Op::Add {
                tool: "codex".into(),
                name: None,
                model: Some("gpt-6.2-astra".into()),
                effort: Some("medium".into())
            })
        );
        for bad in [
            &["add"][..],
            &["add", "claude", "--name"],
            &["add", "claude", "name"],
            &["add", "claude", "--name", "a", "b"],
            &["add", "claude", "--model"],
            &["add", "claude", "--model", "a", "--model", "b"],
            &["add", "claude", "--effort"],
            &["add", "claude", "--effort", "low", "--effort", "high"],
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
        assert_eq!(
            parse(&["tasks", "claude-2"], "", true),
            Ok(Op::Tasks {
                target: "claude-2".into(),
                stop: false,
                task: None
            })
        );
        assert_eq!(
            parse(&["tasks", "claude-2", "--stop", "b1"], "", true),
            Ok(Op::Tasks {
                target: "claude-2".into(),
                stop: true,
                task: Some("b1".into())
            })
        );
        assert_eq!(
            parse(&["tasks", "claude-2", "--stop"], "", true),
            Ok(Op::Tasks {
                target: "claude-2".into(),
                stop: true,
                task: None
            })
        );
        for bad in [
            &["tasks"][..],
            &["tasks", "a", "b"],
            &["tasks", "a", "--stop", "b1", "b2"],
        ] {
            assert!(parse(bad, "", true).is_err(), "{bad:?}");
        }
        assert!(parse(&["nope"], "", true).is_err());
        assert!(parse(&["watch"], "", true).is_err());
        assert!(parse(&[], "", true).is_err());
    }

    #[test]
    fn agents_takes_only_roles() {
        assert_eq!(
            parse(&["agents"], "", true),
            Ok(Op::Agents { roles: false })
        );
        assert_eq!(
            parse(&["agents", "--roles"], "", true),
            Ok(Op::Agents { roles: true })
        );
        for bad in [&["agents", "--json"][..], &["agents", "--roles", "x"]] {
            assert!(parse(bad, "", true).is_err(), "{bad:?}");
        }
        // Not across projects: another project's CLIs are its own to choose.
        assert!(parse(&["agents", "--project", "harbor"], "", true).is_err());
    }

    /// An agent's tool output is often cut to its last lines: a send with no
    /// text ends with the reason and the form that works.
    #[test]
    fn project_is_taken_only_by_the_commands_that_cross_projects() {
        let inside = |project: &str, op: Op| Op::InProject {
            project: project.into(),
            op: Box::new(op),
        };
        assert_eq!(parse(&["projects"], "", true), Ok(Op::Projects));
        assert_eq!(
            parse(&["list", "--project", "harbor"], "", true),
            Ok(inside("harbor", Op::List))
        );
        assert_eq!(
            parse(&["read", "--project", "/w/harbor", "claude-12"], "", true),
            Ok(inside(
                "/w/harbor",
                Op::Read {
                    target: "claude-12".into()
                }
            ))
        );
        assert_eq!(
            parse(&["watch", "claude-12", "--project", "harbor"], "", true),
            Ok(inside(
                "harbor",
                Op::Watch {
                    target: "claude-12".into()
                }
            ))
        );
        assert_eq!(
            parse(&["result", "r4", "--project", "harbor"], "", true),
            Ok(inside(
                "harbor",
                Op::Result {
                    request: "r4".into()
                }
            ))
        );
        let ask = |text: &str| Op::Send {
            target: "claude-12".into(),
            ask: true,
            text: text.into(),
        };
        // Before the id, or among the flags before the text.
        assert_eq!(
            parse(
                &["send", "--project", "harbor", "claude-12", "--ask", "hi"],
                "",
                true
            ),
            Ok(inside("harbor", ask("hi")))
        );
        assert_eq!(
            parse(
                &["send", "claude-12", "--ask", "--project", "harbor", "hi"],
                "",
                true
            ),
            Ok(inside("harbor", ask("hi")))
        );
        // Inside the text it is text.
        assert_eq!(
            parse(
                &["send", "claude-12", "--ask", "use", "--project", "x"],
                "",
                true
            ),
            Ok(ask("use --project x"))
        );
        // After `--` the words are the payload's own.
        assert!(matches!(
            parse(
                &["secret", "run", "K", "--", "/bin/echo", "--project", "x"],
                "",
                true
            ),
            Ok(Op::Secret { .. })
        ));
        // Without the flag, nothing changes.
        assert_eq!(parse(&["list"], "", true), Ok(Op::List));
        assert!(parse(&["list", "--project"], "", true).is_err());
        assert!(parse(&["list", "--project", "a", "--project", "b"], "", true).is_err());
        for refused in [
            &["keys", "claude-12", "esc", "--project", "harbor"][..],
            &["add", "claude", "--project", "harbor"],
            &["start", "--project", "harbor", "claude-12"],
            &["close", "claude-12", "--project", "harbor"],
            &["tasks", "claude-12", "--stop", "--project", "harbor"],
            &["memory", "--project", "harbor", "brief"],
            &["secret", "--project", "harbor", "list"],
            &["op", "--project", "harbor", "list"],
            &["works", "--project", "harbor", "help"],
            &["op", "list", "--project", "beta"],
            &["op", "run", "deploy", "--project", "beta"],
            &["memory", "search", "x", "--project", "beta"],
            &["send-file", "--project"],
        ] {
            let why = parse(refused, "", true).unwrap_err();
            assert!(
                why.contains("works only in this project"),
                "{refused:?}: {why}"
            );
        }
    }

    #[test]
    fn projects_print_as_a_table_and_an_older_app_says_so() {
        let header = json!({ "projects": [
            { "name": "connecto", "path": "/w/connecto", "open": true, "this": true,
              "terminals": [{ "id": "claude-55", "tool": "claude", "state": "working" }] },
            { "name": "harbor", "path": "/w/harbor", "open": true,
              "terminals": [{ "id": "claude-12", "tool": "claude", "state": "idle" },
                            { "id": "codex-3", "tool": "codex", "state": "idle", "owner": "claude-12" }] },
            { "name": "old", "path": "/w/old", "open": false, "count": 2, "terminals": [] },
        ]});
        let (code, out, _) = shown(&Op::Projects, header, b"");
        assert_eq!(code, EXIT_OK);
        assert!(out.lines().next().unwrap().starts_with("NAME"), "{out}");
        assert!(out.contains("this project"), "{out}");
        assert!(out.contains("closed"), "{out}");
        assert!(
            out.contains(
                "harbor: claude-12 (claude, idle), codex-3 (codex, idle, belongs to claude-12)"
            ),
            "{out}"
        );
        let op = Op::InProject {
            project: "harbor".into(),
            op: Box::new(Op::List),
        };
        let mut out = Vec::new();
        let mut err = Vec::new();
        let mut stdin: &[u8] = b"";
        let mut io = Io {
            stdin: &mut stdin,
            stdin_is_tty: true,
            out: &mut out,
            err: &mut err,
        };
        let reply = Reply::refused("bad_request", "unknown op \"in_project\"", None);
        assert_eq!(show(&op, &reply, &mut io), EXIT_REFUSED);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("cannot reach other projects"), "{err}");
    }

    #[test]
    fn a_send_with_no_text_ends_with_why_and_how() {
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut stdin = std::io::empty();
        let mut io = Io {
            stdin: &mut stdin,
            stdin_is_tty: false,
            out: &mut out,
            err: &mut err,
        };
        let code = run(&words(&["send", "pi-15", "--ask"]), None, &mut io);
        assert_eq!(code, EXIT_USAGE);
        let err = String::from_utf8(err).unwrap();
        let last = err.trim_end().lines().last().unwrap();
        assert!(
            last.contains("nothing was sent")
                && last.contains("connecto send pi-15 --ask 'your message'"),
            "{err}"
        );
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
        let (code, out, _) = shown(
            &Op::Send {
                target: "a".into(),
                ask: false,
                text: String::new(),
            },
            json!({ "id": "r13", "state": "waiting", "note": "The user is typing in a." }),
            b"",
        );
        assert_eq!(
            (code, out.as_str()),
            (0, "The user is typing in a.\nr13 waiting\n")
        );

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
    fn memory_commands_carry_their_words_and_read_stdin_only_to_propose_or_flag() {
        assert_eq!(
            parse(
                &["memory", "search", "release", "--room", "officina"],
                "ignored",
                false
            ),
            Ok(Op::Memory {
                verb: "search".into(),
                args: words(&["release", "--room", "officina"]),
                text: None,
            })
        );
        assert_eq!(
            parse(&["memory", "brief"], "", true),
            Ok(Op::Memory {
                verb: "brief".into(),
                args: Vec::new(),
                text: None,
            })
        );
        assert_eq!(
            parse(
                &["memory", "propose", "--type", "task"],
                "Ship it.\n",
                false
            ),
            Ok(Op::Memory {
                verb: "propose".into(),
                args: words(&["--type", "task"]),
                text: Some("Ship it.\n".into()),
            })
        );
        let exact = "x".repeat(MEMORY_TEXT_MAX);
        assert!(matches!(
            parse(&["memory", "flag", "m-abcdefgh"], &exact, false),
            Ok(Op::Memory { text: Some(ref t), .. }) if t.len() == MEMORY_TEXT_MAX
        ));
        let over = "x".repeat(MEMORY_TEXT_MAX + 1);
        let refused = parse(&["memory", "propose"], &over, false).unwrap_err();
        assert!(refused.contains("refused, not cut"), "{refused}");
        assert!(parse(&["memory", "propose"], "  \n", false).is_err());
        assert!(
            parse(&["memory", "propose"], "text", true).is_err(),
            "a TTY"
        );
        assert!(parse(&["memory"], "", true).is_err());
    }

    #[test]
    fn an_older_app_that_does_not_know_memory_gets_a_clear_message() {
        let op = Op::Memory {
            verb: "brief".into(),
            args: Vec::new(),
            text: None,
        };
        let reply = Reply::refused("bad_request", "unknown op \"memory\"", None);
        let mut stdin = &b""[..];
        let (mut out, mut err) = (Vec::new(), Vec::new());
        let mut io = Io {
            stdin: &mut stdin,
            stdin_is_tty: true,
            out: &mut out,
            err: &mut err,
        };
        assert_eq!(show(&op, &reply, &mut io), EXIT_REFUSED);
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("older than this command"), "{err}");
        // The body of an answer is printed as it is.
        let (code, out, _) = shown(&op, json!({}), b"[connecto memory] notes\n");
        assert_eq!((code, out.as_str()), (0, "[connecto memory] notes\n"));
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
    }

    /// A terminal in `background` keeps its state word; a mark gives how many
    /// tasks run and the oldest's age, and a note says where the list is.
    #[test]
    fn the_list_marks_background_work_with_the_oldest_age() {
        let (_, out, _) = shown(
            &Op::List,
            json!({ "self": "claude", "paused": false, "terminals": [
                { "id": "claude", "tool": "claude", "state": "working", "bridge": "on" },
                { "id": "claude-2", "tool": "claude", "state": "background", "seconds": 40,
                  "bridge": "on", "background": [
                    { "id": "b1", "type": "shell", "label": "python3 grind.py", "seconds": 8400,
                      "age": "2 h 20 min", "long": true },
                    { "id": "a2", "type": "subagent", "label": "Review", "seconds": 60,
                      "age": "1 min", "long": false },
                ] },
            ] }),
            b"",
        );
        let row = out
            .lines()
            .find(|line| line.starts_with("claude-2"))
            .unwrap();
        assert!(row.contains("background"), "{out}");
        assert!(
            row.contains("2 tasks, oldest 2 h 20 min, long-running"),
            "{out}"
        );
        assert!(
            out.contains("claude-2: background work running; `connecto tasks claude-2` lists it"),
            "{out}"
        );
        let first = out
            .lines()
            .find(|line| line.starts_with("claude "))
            .unwrap();
        assert!(!first.contains("task"), "{out}");
    }

    #[test]
    fn tasks_are_a_table_and_a_stop_prints_its_note() {
        let op = Op::Tasks {
            target: "claude-2".into(),
            stop: false,
            task: None,
        };
        let (code, out, _) = shown(
            &op,
            json!({ "id": "claude-2", "state": "background", "tasks": [
                { "id": "b1", "type": "shell", "label": "python3 grind.py big.log",
                  "seconds": 8400, "age": "2 h 20 min", "long": true },
                { "id": "a2", "type": "subagent", "label": "Review the diff",
                  "seconds": 60, "age": "1 min", "long": false },
            ] }),
            b"",
        );
        assert_eq!(code, 0);
        let lines: Vec<&str> = out.lines().collect();
        assert_eq!(lines[0], "claude-2 background");
        assert!(
            lines[1].starts_with("TASK") && lines[1].contains("RUNS"),
            "{out}"
        );
        assert!(
            lines[2].starts_with("b1") && lines[2].contains("2 h 20 min (long)"),
            "{out}"
        );
        assert!(lines[2].ends_with("python3 grind.py big.log"), "{out}");
        assert!(
            lines[3].starts_with("a2") && lines[3].contains("subagent"),
            "{out}"
        );
        assert!(out.contains("connecto tasks claude-2 --stop"), "{out}");
        let (_, out, _) = shown(
            &op,
            json!({ "id": "claude-2", "state": "idle", "tasks": [] }),
            b"",
        );
        assert_eq!(out, "claude-2 has no background work running.\n");
        let stop = Op::Tasks {
            target: "claude-2".into(),
            stop: true,
            task: None,
        };
        let (code, out, _) = shown(
            &stop,
            json!({ "id": "claude-2", "note": "Stopping 1 background shell." }),
            b"",
        );
        assert_eq!((code, out.as_str()), (0, "Stopping 1 background shell.\n"));
    }

    #[test]
    fn the_list_says_whose_a_started_terminal_is() {
        let (_, out, _) = shown(
            &Op::List,
            json!({ "self": "pi", "paused": false, "terminals": [
                { "id": "pi", "tool": "pi", "state": "working", "bridge": "on" },
                { "id": "pi-2", "tool": "pi", "state": "idle", "owner": "pi", "bridge": "on" },
                { "id": "pi-3", "tool": "pi", "state": "idle", "owner": "claude", "bridge": "on" },
            ] }),
            b"",
        );
        let row = |id: &str| {
            out.lines()
                .find(|line| line.starts_with(id))
                .unwrap()
                .to_owned()
        };
        assert!(row("pi-2").contains("yours"), "{out}");
        assert!(row("pi-3").contains("belongs to claude"), "{out}");
        assert!(!row("pi ").contains("yours"), "{out}");
        assert!(out.lines().next().unwrap().starts_with("ID"));
    }

    #[test]
    fn the_list_shows_context_use_or_a_dash() {
        let (_, out, _) = shown(
            &Op::List,
            json!({ "self": "claude", "paused": false, "terminals": [
                { "id": "claude-2", "tool": "claude", "state": "idle", "context": 42, "bridge": "on" },
                { "id": "codex-3", "tool": "codex", "state": "idle", "context": null, "bridge": "on" },
            ] }),
            b"",
        );
        let cells = |id: &str| -> Vec<String> {
            out.lines()
                .find(|line| line.starts_with(id))
                .unwrap()
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        };
        assert!(
            out.lines().next().unwrap().contains("STATE  CONTEXT"),
            "{out}"
        );
        assert_eq!(cells("claude-2")[3], "42%", "{out}");
        assert_eq!(cells("codex-3")[3], "-", "{out}");
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

    /// A relative `--project` path names a folder from where `connecto`
    /// runs, not from where the app runs: two commands in different folders
    /// name different projects with the same words.
    #[test]
    fn a_relative_project_path_is_made_absolute_from_the_commands_folder() {
        use std::path::Path;
        let from = |cwd: &str, value: &str| anchored(value, Some(Path::new(cwd)));
        // Folders that do not exist are joined as written.
        assert_eq!(from("/w/one", "./src"), "/w/one/./src");
        assert_eq!(from("/w/two", "./src"), "/w/two/./src");
        assert_eq!(from("/w/one", "apps/web"), "/w/one/apps/web");
        // A name, an absolute path and a home path are kept as given.
        assert_eq!(from("/w/one", "src"), "src");
        assert_eq!(from("/w/one", "/x/app"), "/x/app");
        assert_eq!(from("/w/one", "~/app"), "~/app");
        assert_eq!(anchored("./src", None), "./src");
    }

    /// `link/../dest`, where `link` leads to `peer/child`: the folder is
    /// `peer/dest`, as the filesystem resolves it, not `caller/dest`, which
    /// exists too and is what taking `..` away by the words alone gives.
    #[cfg(unix)]
    #[test]
    fn a_relative_project_path_follows_a_link_before_going_up() {
        let root = std::env::temp_dir().join(format!("mg-anchor-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let (caller, peer) = (root.join("caller"), root.join("peer"));
        for dir in [caller.join("dest"), peer.join("child"), peer.join("dest")] {
            std::fs::create_dir_all(dir).unwrap();
        }
        std::os::unix::fs::symlink(peer.join("child"), caller.join("link")).unwrap();
        let got = anchored("link/../dest", Some(&caller));
        let peer_dest = std::fs::canonicalize(peer.join("dest")).unwrap();
        let _ = std::fs::remove_dir_all(&root);
        assert_eq!(got, peer_dest.display().to_string());
    }
}
