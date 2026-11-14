//! The bridge's wire format and the `connecto` command (ADR 0016).
//!
//! A request is a hook frame (`encode`) with an empty token and a JSON payload.
//! A reply is a big-endian `u32` length, a JSON header, and then a body until
//! end of stream. Both sides are built from the types here.

use std::io::{self, Read, Write};

use serde_json::{Value, json};

/// The wire version both sides speak.
pub const WIRE_VERSION: u64 = 1;

/// Environment variable naming the bridge socket.
pub const BRIDGE_SOCKET_ENV: &str = "CONNECTO_BRIDGE_SOCK";

/// Longest header a reply may carry.
pub const MAX_HEADER: usize = 1024 * 1024;

/// One request. Long text arrives in `Send`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Op {
    List,
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
    },
    Start {
        target: String,
    },
    Close {
        target: String,
    },
    Drain {
        event: String,
    },
}

/// Why a request could not be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BadRequest {
    Version,
    Malformed(String),
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
            Self::Send { target, ask, text } => {
                json!({ "op": "send", "target": target, "ask": ask, "text": text })
            }
            Self::Keys { target, keys } => json!({ "op": "keys", "target": target, "keys": keys }),
            Self::Read { target } => json!({ "op": "read", "target": target }),
            Self::Result { request } => json!({ "op": "result", "request": request }),
            Self::Watch { target } => json!({ "op": "watch", "target": target }),
            Self::Unwatch { target } => json!({ "op": "unwatch", "target": target }),
            Self::Add { tool, name, model } => {
                let mut value = json!({ "op": "add", "tool": tool });
                if let Some(name) = name {
                    value["name"] = json!(name);
                }
                if let Some(model) = model {
                    value["model"] = json!(model);
                }
                value
            }
            Self::Start { target } => json!({ "op": "start", "target": target }),
            Self::Close { target } => json!({ "op": "close", "target": target }),
            Self::Drain { event } => json!({ "op": "drain", "event": event }),
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
            "add" => {
                let optional = |field: &str| match value.get(field) {
                    None | Some(Value::Null) => Ok(None),
                    Some(Value::String(text)) => Ok(Some(text.clone())),
                    Some(_) => Err(BadRequest::Malformed(format!("`{field}` is not text"))),
                };
                Self::Add {
                    tool: string(&value, "tool")?,
                    name: optional("name")?,
                    model: optional("model")?,
                }
            }
            "start" => Self::Start {
                target: string(&value, "target")?,
            },
            "close" => Self::Close {
                target: string(&value, "target")?,
            },
            "drain" => Self::Drain {
                event: string(&value, "event")?,
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

    #[test]
    fn every_op_round_trips() {
        let ops = [
            Op::List,
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
            },
            Op::Add {
                tool: "codex".into(),
                name: Some("review the diff".into()),
                model: Some("gpt-6.1-sol".into()),
            },
            Op::Start { target: "a".into() },
            Op::Close { target: "a".into() },
            Op::Drain {
                event: "UserPromptSubmit".into(),
            },
        ];
        for op in ops {
            let bytes = serde_json::to_vec(&op.to_json()).unwrap();
            assert_eq!(Op::parse(&bytes).unwrap(), op);
        }
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
