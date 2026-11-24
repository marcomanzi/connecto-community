//! The bridge's wire format and the `connecto` command (ADR 0016).
//!
//! A request is a hook frame (`encode`) with an empty token and a JSON payload.
//! A reply is a big-endian `u32` length, a JSON header, and then a body until
//! end of stream. Both sides are built from the types here.

use std::io::{self, Read, Write};

use serde_json::{Value, json};

/// The op of a `connecto add` that carries `--effort`.
pub const ADD_EFFORT_OP: &str = "add_effort";

/// The wire version both sides speak.
pub const WIRE_VERSION: u64 = 1;

/// Environment variable naming the bridge socket.
pub const BRIDGE_SOCKET_ENV: &str = "CONNECTO_BRIDGE_SOCK";

/// Longest header a reply may carry.
pub const MAX_HEADER: usize = 1024 * 1024;

/// The drain's field saying the helper may hold a prompt (see
/// [`Op::Drain`]).
const BLOCKS_ON_HOOK_FAILURE: &str = "blocks_on_hook_failure";

/// The op that carries another op into another project (ADR 0035). An op
/// of its own, so an older app refuses it as unknown instead of ignoring the
/// project and acting on its own.
pub const IN_PROJECT_OP: &str = "in_project";

/// One request. Long text arrives in `Send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    List,
    /// `connecto projects`: the projects this Connecto knows (ADR 0035).
    Projects,
    /// `connecto agents [--roles]`: the coding CLIs this project can start
    /// now and, with `roles`, which one takes each role of a team (ADR 0045).
    Agents {
        roles: bool,
    },
    /// `--project <name|path>`: `op` acts on that project's terminals. Only
    /// list, read, send, result, watch and unwatch are allowed; the app
    /// refuses any other.
    InProject {
        project: String,
        op: Box<Op>,
    },
    Send {
        target: String,
        ask: bool,
        text: String,
    },
    Keys {
        target: String,
        keys: Vec<String>,
    },
    Read {
        target: String,
    },
    Result {
        request: String,
    },
    Watch {
        target: String,
    },
    Unwatch {
        target: String,
    },
    Add {
        tool: String,
        /// A name for the new terminal, from `--name`.
        name: Option<String>,
        /// The model the new terminal runs on, from `--model`.
        model: Option<String>,
        /// Its reasoning effort, from `--effort`.
        effort: Option<String>,
    },
    Start {
        target: String,
    },
    Close {
        target: String,
    },
    /// `connecto tasks <id> [--stop [task]]`: the background work a terminal's
    /// agent left running; with `stop`, stop its background shells, or only
    /// `task`.
    Tasks {
        target: String,
        stop: bool,
        task: Option<String>,
    },
    Drain {
        event: String,
        /// The prompt being submitted, for project operations (ADR 0031).
        /// An older app ignores it.
        prompt: Option<String>,
        /// This hook can take an intercept: confirm it, print the block,
        /// then say go, and Claude blocks the prompt if the hook fails or
        /// times out (`onFailure: "block"`, set with `--holds`). On the
        /// wire as `blocks_on_hook_failure`; an earlier `holds` field, sent
        /// by helpers that could not promise the latter, is ignored.
        /// Without it the app never answers with an intercept.
        holds: bool,
    },
    /// `connecto memory <verb> [args]`: the verb word and the arguments as
    /// given, checked by the app; `text` is stdin for `propose` and `flag`.
    Memory {
        verb: String,
        args: Vec<String>,
        text: Option<String>,
    },
    /// `connecto op <verb> [args]`: the project's operations (ADR 0031);
    /// `text` is stdin for `llm`.
    Ops {
        verb: String,
        args: Vec<String>,
        text: Option<String>,
    },
    /// `connecto send-file <path>`: offer a file to the machines viewing this
    /// one over Connecto's remote link. `path` is absolute (the command
    /// resolves it against the folder it was typed in).
    SendFile {
        path: String,
    },
    /// `connecto works <verb> [args]`: the project's Agentic Works; `text`
    /// is what the command read for it (a definition, a prompt, a key file).
    Works {
        verb: String,
        args: Vec<String>,
        text: Option<String>,
    },
    /// `connecto secret <verb> [args]`: the vault, by name only. No reply to
    /// it ever holds a secret's value.
    Secret {
        verb: String,
        args: Vec<String>,
        /// The folder the command was typed in, for `run`.
        cwd: Option<String>,
    },
}

/// Why a request could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadRequest {
    Version,
    Malformed(String),
}

/// An optional list of text (absent is empty).
fn text_list(value: &Value, field: &str) -> Result<Vec<String>, BadRequest> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(Vec::new()),
        Some(Value::Array(items)) => items
            .iter()
            .map(|item| item.as_str().map(str::to_owned))
            .collect::<Option<Vec<_>>>()
            .ok_or_else(|| BadRequest::Malformed(format!("`{field}` is not a list of text"))),
        Some(_) => Err(BadRequest::Malformed(format!(
            "`{field}` is not a list of text"
        ))),
    }
}

/// An optional text field (absent or null is None).
fn optional_text(value: &Value, field: &str) -> Result<Option<String>, BadRequest> {
    match value.get(field) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(text)) => Ok(Some(text.clone())),
        Some(_) => Err(BadRequest::Malformed(format!("`{field}` is not text"))),
    }
}

fn string(value: &Value, field: &str) -> Result<String, BadRequest> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| BadRequest::Malformed(format!("`{field}` is missing or not text")))
}

impl Op {
    /// The request payload.
    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut value = match self {
            Self::List => json!({ "op": "list" }),
            Self::Projects => json!({ "op": "projects" }),
            Self::Agents { roles } => json!({ "op": "agents", "roles": roles }),
            Self::InProject { project, op } => {
                json!({ "op": IN_PROJECT_OP, "project": project, "inner": op.to_json() })
            }
            Self::Send { target, ask, text } => {
                json!({ "op": "send", "target": target, "ask": ask, "text": text })
            }
            Self::Keys { target, keys } => json!({ "op": "keys", "target": target, "keys": keys }),
            Self::Read { target } => json!({ "op": "read", "target": target }),
            Self::Result { request } => json!({ "op": "result", "request": request }),
            Self::Watch { target } => json!({ "op": "watch", "target": target }),
            Self::Unwatch { target } => json!({ "op": "unwatch", "target": target }),
            Self::Add {
                tool,
                name,
                model,
                effort,
            } => {
                // An add with an effort has an op of its own, so an older app,
                // which would drop the field and launch at the default
                // effort, refuses it as an unknown op instead.
                let op = if effort.is_some() {
                    ADD_EFFORT_OP
                } else {
                    "add"
                };
                let mut value = json!({ "op": op, "tool": tool });
                if let Some(name) = name {
                    value["name"] = json!(name);
                }
                if let Some(model) = model {
                    value["model"] = json!(model);
                }
                if let Some(effort) = effort {
                    value["effort"] = json!(effort);
                }
                value
            }
            Self::Start { target } => json!({ "op": "start", "target": target }),
            Self::Close { target } => json!({ "op": "close", "target": target }),
            Self::Tasks { target, stop, task } => {
                let mut value = json!({ "op": "tasks", "target": target, "stop": stop });
                if let Some(task) = task {
                    value["task"] = json!(task);
                }
                value
            }
            Self::Drain {
                event,
                prompt,
                holds,
            } => {
                let mut value = json!({ "op": "drain", "event": event });
                if let Some(prompt) = prompt {
                    value["prompt"] = json!(prompt);
                }
                if *holds {
                    value[BLOCKS_ON_HOOK_FAILURE] = json!(true);
                }
                value
            }
            Self::SendFile { path } => json!({ "op": "send_file", "path": path }),
            Self::Memory { verb, args, text } => {
                let mut value = json!({ "op": "memory", "verb": verb, "args": args });
                if let Some(text) = text {
                    value["text"] = json!(text);
                }
                value
            }
            Self::Ops { verb, args, text } => {
                let mut value = json!({ "op": "ops", "verb": verb, "args": args });
                if let Some(text) = text {
                    value["text"] = json!(text);
                }
                value
            }
            Self::Works { verb, args, text } => {
                let mut value = json!({ "op": "works", "verb": verb, "args": args });
                if let Some(text) = text {
                    value["text"] = json!(text);
                }
                value
            }
            Self::Secret { verb, args, cwd } => {
                let mut value = json!({ "op": "secret", "verb": verb, "args": args });
                if let Some(cwd) = cwd {
                    value["cwd"] = json!(cwd);
                }
                value
            }
        };
        value["v"] = json!(WIRE_VERSION);
        value
    }

    /// Read a request. Unknown fields are ignored. A missing or wrongly typed
    /// field is an error.
    pub fn parse(payload: &[u8]) -> Result<Self, BadRequest> {
        let value: Value = serde_json::from_slice(payload)
            .map_err(|err| BadRequest::Malformed(format!("not JSON: {err}")))?;
        if value.get("v").and_then(Value::as_u64) != Some(WIRE_VERSION) {
            return Err(BadRequest::Version);
        }
        let op = string(&value, "op")?;
        Ok(match op.as_str() {
            "list" => Self::List,
            "projects" => Self::Projects,
            "agents" => Self::Agents {
                roles: match value.get("roles") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(roles)) => *roles,
                    Some(_) => {
                        return Err(BadRequest::Malformed("`roles` is not true or false".into()));
                    }
                },
            },
            IN_PROJECT_OP => {
                let project = string(&value, "project")?;
                let inner = value
                    .get("inner")
                    .ok_or_else(|| BadRequest::Malformed("`inner` is missing".into()))?;
                let bytes = serde_json::to_vec(inner)
                    .map_err(|err| BadRequest::Malformed(format!("`inner`: {err}")))?;
                let inner = Self::parse(&bytes)?;
                if matches!(inner, Self::InProject { .. }) {
                    return Err(BadRequest::Malformed(
                        "`in_project` cannot carry another `in_project`".into(),
                    ));
                }
                Self::InProject {
                    project,
                    op: Box::new(inner),
                }
            }
            "send" => Self::Send {
                target: string(&value, "target")?,
                ask: match value.get("ask") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(ask)) => *ask,
                    Some(_) => {
                        return Err(BadRequest::Malformed("`ask` is not true or false".into()));
                    }
                },
                text: string(&value, "text")?,
            },
            "keys" => Self::Keys {
                target: string(&value, "target")?,
                keys: value
                    .get("keys")
                    .and_then(Value::as_array)
                    .and_then(|keys| {
                        keys.iter()
                            .map(|key| key.as_str().map(str::to_owned))
                            .collect::<Option<Vec<_>>>()
                    })
                    .filter(|keys| !keys.is_empty())
                    .ok_or_else(|| BadRequest::Malformed("`keys` is not a list of keys".into()))?,
            },
            "read" => Self::Read {
                target: string(&value, "target")?,
            },
            "result" => Self::Result {
                request: string(&value, "request")?,
            },
            "watch" => Self::Watch {
                target: string(&value, "target")?,
            },
            "unwatch" => Self::Unwatch {
                target: string(&value, "target")?,
            },
            "add" | ADD_EFFORT_OP => {
                let optional = |field: &str| match value.get(field) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(text)) => Ok(Some(text.clone())),
                    Some(_) => Err(BadRequest::Malformed(format!("`{field}` is not text"))),
                };
                let add = Self::Add {
                    tool: string(&value, "tool")?,
                    name: optional("name")?,
                    model: optional("model")?,
                    effort: optional("effort")?,
                };
                if op == ADD_EFFORT_OP && matches!(add, Self::Add { effort: None, .. }) {
                    return Err(BadRequest::Malformed(format!(
                        "`{ADD_EFFORT_OP}` takes an `effort`"
                    )));
                }
                add
            }
            "start" => Self::Start {
                target: string(&value, "target")?,
            },
            "close" => Self::Close {
                target: string(&value, "target")?,
            },
            "tasks" => Self::Tasks {
                target: string(&value, "target")?,
                stop: match value.get("stop") {
                    None | Some(Value::Null) => false,
                    Some(Value::Bool(stop)) => *stop,
                    Some(_) => {
                        return Err(BadRequest::Malformed("`stop` is not true or false".into()));
                    }
                },
                task: match value.get("task") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(task)) => Some(task.clone()),
                    Some(_) => return Err(BadRequest::Malformed("`task` is not text".into())),
                },
            },
            "drain" => Self::Drain {
                event: string(&value, "event")?,
                prompt: match value.get("prompt") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(prompt)) => Some(prompt.clone()),
                    Some(_) => return Err(BadRequest::Malformed("`prompt` is not text".into())),
                },
                holds: value.get(BLOCKS_ON_HOOK_FAILURE).and_then(Value::as_bool) == Some(true),
            },
            "ops" => Self::Ops {
                verb: string(&value, "verb")?,
                args: text_list(&value, "args")?,
                text: match value.get("text") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(_) => return Err(BadRequest::Malformed("`text` is not text".into())),
                },
            },
            "send_file" => Self::SendFile {
                path: string(&value, "path")?,
            },
            "works" => Self::Works {
                verb: string(&value, "verb")?,
                args: text_list(&value, "args")?,
                text: optional_text(&value, "text")?,
            },
            "secret" => Self::Secret {
                verb: string(&value, "verb")?,
                args: text_list(&value, "args")?,
                cwd: match value.get("cwd") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(cwd)) => Some(cwd.clone()),
                    Some(_) => return Err(BadRequest::Malformed("`cwd` is not text".into())),
                },
            },
            "memory" => Self::Memory {
                verb: string(&value, "verb")?,
                args: match value.get("args") {
                    None | Some(Value::Null) => Vec::new(),
                    Some(Value::Array(args)) => args
                        .iter()
                        .map(|arg| arg.as_str().map(str::to_owned))
                        .collect::<Option<Vec<_>>>()
                        .ok_or_else(|| {
                            BadRequest::Malformed("`args` is not a list of text".into())
                        })?,
                    Some(_) => {
                        return Err(BadRequest::Malformed("`args` is not a list of text".into()));
                    }
                },
                text: match value.get("text") {
                    None | Some(Value::Null) => None,
                    Some(Value::String(text)) => Some(text.clone()),
                    Some(_) => return Err(BadRequest::Malformed("`text` is not text".into())),
                },
            },
            other => return Err(BadRequest::Malformed(format!("unknown op {other:?}"))),
        })
    }
}

/// A reply: a header, and for `read`, `result` and `drain` a body.
#[derive(Debug, Clone, PartialEq)]
pub struct Reply {
    pub header: Value,
    pub body: Vec<u8>,
}

impl Reply {
    #[must_use]
    pub fn ok(mut header: Value) -> Self {
        header["v"] = json!(WIRE_VERSION);
        header["ok"] = json!(true);
        Self {
            header,
            body: Vec::new(),
        }
    }

    #[must_use]
    pub fn with_body(mut self, body: Vec<u8>) -> Self {
        self.body = body;
        self
    }

    /// A refusal. `id` is present when a request record survives it.
    #[must_use]
    pub fn refused(code: &str, reason: &str, id: Option<&str>) -> Self {
        let mut header = json!({ "v": WIRE_VERSION, "ok": false, "code": code, "reason": reason });
        if let Some(id) = id {
            header["id"] = json!(id);
        }
        Self {
            header,
            body: Vec::new(),
        }
    }

    #[must_use]
    pub fn is_ok(&self) -> bool {
        self.header.get("ok").and_then(Value::as_bool) == Some(true)
    }

    /// Write the reply. The body goes after the header, unbounded.
    pub fn write_to(&self, out: &mut impl Write) -> io::Result<()> {
        let header = serde_json::to_vec(&self.header).map_err(io::Error::other)?;
        let len = u32::try_from(header.len()).map_err(io::Error::other)?;
        out.write_all(&len.to_be_bytes())?;
        out.write_all(&header)?;
        out.write_all(&self.body)?;
        out.flush()
    }

    /// Read a whole reply: the header, then the body until end of stream.
    pub fn read_from(input: &mut impl Read) -> io::Result<Self> {
        let mut len = [0_u8; 4];
        input.read_exact(&mut len)?;
        let len = usize::try_from(u32::from_be_bytes(len)).unwrap_or(usize::MAX);
        if len > MAX_HEADER {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "header too large",
            ));
        }
        let mut header = vec![0_u8; len];
        input.read_exact(&mut header)?;
        let header = serde_json::from_slice(&header)
            .map_err(|err| io::Error::new(io::ErrorKind::InvalidData, err))?;
        let mut body = Vec::new();
        input.read_to_end(&mut body)?;
        Ok(Self { header, body })
    }
}

/// The bytes a key sends, or `None` for a key that is not allowed. This is
/// the one list of key names: the named keys below, `f1` to `f12`, `ctrl-` and
/// `alt-` with a letter, and any single printable character.
#[must_use]
pub fn key_bytes(key: &str) -> Option<Vec<u8>> {
    let named = |bytes: &[u8]| Some(bytes.to_vec());
    match key {
        "enter" => named(b"\r"),
        "esc" => named(b"\x1b"),
        "tab" => named(b"\t"),
        "space" => named(b" "),
        "backspace" => named(b"\x7f"),
        "delete" => named(b"\x1b[3~"),
        "up" => named(b"\x1b[A"),
        "down" => named(b"\x1b[B"),
        "right" => named(b"\x1b[C"),
        "left" => named(b"\x1b[D"),
        "home" => named(b"\x1b[H"),
        "end" => named(b"\x1b[F"),
        "pageup" => named(b"\x1b[5~"),
        "pagedown" => named(b"\x1b[6~"),
        "insert" => named(b"\x1b[2~"),
        "shift-tab" => named(b"\x1b[Z"),
        _ => {
            if let Some(number) = key.strip_prefix('f').and_then(|n| n.parse::<u8>().ok())
                && (1..=12).contains(&number)
            {
                let code: &[u8] = match number {
                    1 => b"\x1bOP",
                    2 => b"\x1bOQ",
                    3 => b"\x1bOR",
                    4 => b"\x1bOS",
                    5 => b"\x1b[15~",
                    6 => b"\x1b[17~",
                    7 => b"\x1b[18~",
                    8 => b"\x1b[19~",
                    9 => b"\x1b[20~",
                    10 => b"\x1b[21~",
                    11 => b"\x1b[23~",
                    _ => b"\x1b[24~",
                };
                return named(code);
            }
            if let Some(letter) = key.strip_prefix("ctrl-").and_then(single_letter) {
                return Some(vec![letter.to_ascii_lowercase() as u8 - b'a' + 1]);
            }
            if let Some(letter) = key.strip_prefix("alt-").and_then(single_letter) {
                return Some(vec![0x1b, letter as u8]);
            }
            let mut chars = key.chars();
            match (chars.next(), chars.next()) {
                (Some(ch), None) if !ch.is_control() => Some(ch.to_string().into_bytes()),
                _ => None,
            }
        }
    }
}

fn single_letter(text: &str) -> Option<char> {
    let mut chars = text.chars();
    match (chars.next(), chars.next()) {
        (Some(ch), None) if ch.is_ascii_alphabetic() => Some(ch),
        _ => None,
    }
}

/// Whether `key` is a name the command knows. Used by the client to refuse
/// early, and by the server, which checks again before anything is sent.
#[must_use]
pub fn valid_key(key: &str) -> bool {
    key_bytes(key).is_some()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// An add with an effort travels as its own op, so an older app refuses
    /// it rather than launching at the default effort; without one it stays
    /// the plain `add` every app knows.
    #[test]
    fn an_add_with_an_effort_has_an_op_of_its_own() {
        let add = |effort: Option<&str>| Op::Add {
            tool: "codex".into(),
            name: None,
            model: Some("gpt-6.2-astra".into()),
            effort: effort.map(str::to_owned),
        };
        assert_eq!(add(None).to_json()["op"], "add");
        assert_eq!(add(Some("medium")).to_json()["op"], ADD_EFFORT_OP);
        let bare = json!({ "v": WIRE_VERSION, "op": ADD_EFFORT_OP, "tool": "codex" });
        assert!(Op::parse(bare.to_string().as_bytes()).is_err());
    }

    /// A helper from before the gate sent `holds` whatever Claude would do on
    /// a hook failure: the app reads it as a drain that cannot hold, so the
    /// session gets Suggest.
    #[test]
    fn a_legacy_holds_field_does_not_let_a_drain_hold() {
        let legacy = json!({ "v": WIRE_VERSION, "op": "drain", "event": "UserPromptSubmit",
            "prompt": "run the tests", "holds": true });
        assert!(matches!(
            Op::parse(legacy.to_string().as_bytes()).unwrap(),
            Op::Drain { holds: false, .. }
        ));
        let gated = Op::Drain {
            event: "UserPromptSubmit".into(),
            prompt: Some("run the tests".into()),
            holds: true,
        }
        .to_json();
        assert_eq!(gated["blocks_on_hook_failure"], true);
        assert!(gated.get("holds").is_none());
    }

    #[test]
    fn every_op_round_trips() {
        let ops = [
            Op::List,
            Op::Projects,
            Op::Agents { roles: false },
            Op::Agents { roles: true },
            Op::InProject {
                project: "harbor app".into(),
                op: Box::new(Op::Send {
                    target: "claude-12".into(),
                    ask: true,
                    text: "hi".into(),
                }),
            },
            Op::Send {
                target: "codex-2".into(),
                ask: true,
                text: "hi\nthere".into(),
            },
            Op::Keys {
                target: "claude".into(),
                keys: vec!["esc".into(), "down".into()],
            },
            Op::Read { target: "a".into() },
            Op::Result {
                request: "r12".into(),
            },
            Op::Watch { target: "a".into() },
            Op::Unwatch { target: "a".into() },
            Op::Add {
                tool: "claude".into(),
                name: None,
                model: None,
                effort: None,
            },
            Op::Add {
                tool: "codex".into(),
                name: Some("review the diff".into()),
                model: Some("gpt-6.2-astra".into()),
                effort: Some("medium".into()),
            },
            Op::Start { target: "a".into() },
            Op::Close { target: "a".into() },
            Op::Tasks {
                target: "a".into(),
                stop: false,
                task: None,
            },
            Op::Tasks {
                target: "a".into(),
                stop: true,
                task: Some("b1".into()),
            },
            Op::Drain {
                event: "UserPromptSubmit".into(),
                prompt: None,
                holds: false,
            },
            Op::Drain {
                event: "UserPromptSubmit".into(),
                prompt: Some("deploy".into()),
                holds: true,
            },
            Op::SendFile {
                path: "/tmp/report.html".into(),
            },
            Op::Memory {
                verb: "brief".into(),
                args: Vec::new(),
                text: None,
            },
            Op::Memory {
                verb: "search".into(),
                args: vec!["release".into(), "--room".into(), "officina".into()],
                text: None,
            },
            Op::Memory {
                verb: "propose".into(),
                args: vec!["--type".into(), "task".into()],
                text: Some("We chose SQLite.\nBecause.".into()),
            },
            Op::Ops {
                verb: "run".into(),
                args: vec!["deploy".into(), "--yes".into()],
                text: None,
            },
            Op::Ops {
                verb: "llm".into(),
                args: Vec::new(),
                text: Some("Write a commit message.".into()),
            },
            Op::Secret {
                verb: "list".into(),
                args: Vec::new(),
                cwd: None,
            },
            Op::Works {
                verb: "show".into(),
                args: Vec::new(),
                text: None,
            },
            Op::Works {
                verb: "publish".into(),
                args: vec!["summary".into()],
                text: Some("{}".into()),
            },
            Op::Secret {
                verb: "run".into(),
                args: vec!["GITHUB".into(), "--".into(), "/usr/bin/gh".into()],
                cwd: Some("/work/p".into()),
            },
        ];
        for op in ops {
            let bytes = serde_json::to_vec(&op.to_json()).unwrap();
            assert_eq!(Op::parse(&bytes).unwrap(), op);
        }
    }

    #[test]
    fn a_project_op_is_an_op_of_its_own_and_carries_one_level() {
        let op = Op::InProject {
            project: "p".into(),
            op: Box::new(Op::List),
        };
        let value = op.to_json();
        // An older app does not know this op, so it refuses instead of
        // listing its own project.
        assert_eq!(value["op"], IN_PROJECT_OP);
        let nested = json!({
            "v": WIRE_VERSION, "op": IN_PROJECT_OP, "project": "a",
            "inner": { "v": WIRE_VERSION, "op": IN_PROJECT_OP, "project": "b",
                       "inner": { "v": WIRE_VERSION, "op": "list" } }
        });
        assert!(matches!(
            Op::parse(&serde_json::to_vec(&nested).unwrap()),
            Err(BadRequest::Malformed(_))
        ));
        let no_inner = json!({ "v": WIRE_VERSION, "op": IN_PROJECT_OP, "project": "a" });
        assert!(Op::parse(&serde_json::to_vec(&no_inner).unwrap()).is_err());
    }

    #[test]
    fn a_bad_request_is_told_from_a_wrong_version() {
        assert_eq!(
            Op::parse(br#"{"v":2,"op":"list"}"#),
            Err(BadRequest::Version)
        );
        assert_eq!(Op::parse(br#"{"op":"list"}"#), Err(BadRequest::Version));
        assert!(matches!(
            Op::parse(br#"{"v":1,"op":"send","target":"a"}"#),
            Err(BadRequest::Malformed(_))
        ));
        assert!(matches!(
            Op::parse(br#"{"v":1,"op":"send","target":"a","text":"x","ask":"yes"}"#),
            Err(BadRequest::Malformed(_))
        ));
        assert!(matches!(
            Op::parse(br#"{"v":1,"op":"keys","target":"a","keys":[]}"#),
            Err(BadRequest::Malformed(_))
        ));
        assert!(matches!(Op::parse(b"nope"), Err(BadRequest::Malformed(_))));
    }

    #[test]
    fn unknown_fields_are_ignored_and_ask_defaults_to_false() {
        let op = Op::parse(br#"{"v":1,"op":"send","target":"a","text":"x","extra":[1]}"#).unwrap();
        assert_eq!(
            op,
            Op::Send {
                target: "a".into(),
                ask: false,
                text: "x".into()
            }
        );
    }

    #[test]
    fn a_memory_request_checks_its_fields_and_ignores_unknown_ones() {
        assert_eq!(
            Op::parse(br#"{"v":1,"op":"memory","verb":"status","future":true}"#).unwrap(),
            Op::Memory {
                verb: "status".into(),
                args: Vec::new(),
                text: None
            }
        );
        for bad in [
            &br#"{"v":1,"op":"memory"}"#[..],
            br#"{"v":1,"op":"memory","verb":"read","args":"m-x"}"#,
            br#"{"v":1,"op":"memory","verb":"read","args":[1]}"#,
            br#"{"v":1,"op":"memory","verb":"propose","text":7}"#,
        ] {
            assert!(
                matches!(Op::parse(bad), Err(BadRequest::Malformed(_))),
                "{}",
                String::from_utf8_lossy(bad)
            );
        }
    }

    #[test]
    fn replies_round_trip_with_and_without_a_body() {
        let reply = Reply::ok(json!({ "history_full": false })).with_body(b"screen text".to_vec());
        let mut bytes = Vec::new();
        reply.write_to(&mut bytes).unwrap();
        assert_eq!(Reply::read_from(&mut bytes.as_slice()).unwrap(), reply);

        let refused = Reply::refused("paused", "Connecto is paused", Some("r3"));
        let mut bytes = Vec::new();
        refused.write_to(&mut bytes).unwrap();
        let back = Reply::read_from(&mut bytes.as_slice()).unwrap();
        assert!(!back.is_ok());
        assert_eq!(back.header["id"], "r3");
        assert_eq!(back.header["v"], 1);
    }

    #[test]
    fn keys_are_the_listed_ones_and_nothing_else() {
        for key in [
            "enter",
            "esc",
            "tab",
            "space",
            "backspace",
            "delete",
            "up",
            "down",
            "left",
            "right",
            "home",
            "end",
            "pageup",
            "pagedown",
            "insert",
            "f1",
            "f12",
            "shift-tab",
            "ctrl-c",
            "alt-x",
            "y",
            "7",
            "é",
        ] {
            assert!(valid_key(key), "{key}");
        }
        for key in [
            "f13", "f0", "ctrl-", "ctrl-1", "alt-ab", "ab", "", "\n", "super-x", "Enter",
        ] {
            assert!(!valid_key(key), "{key:?}");
        }
        assert_eq!(key_bytes("ctrl-c"), Some(vec![3]));
        assert_eq!(key_bytes("alt-x"), Some(vec![0x1b, b'x']));
        assert_eq!(key_bytes("down"), Some(b"\x1b[B".to_vec()));
    }
}
