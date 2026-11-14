//! Wire messages and their length-prefixed binary framing.
//!
//! A frame is `u32` big-endian length, then one tag byte, then the fields. The
//! length counts the tag and the fields. Nothing on the hot path is text: an
//! output chunk costs a 4-byte length, a tag, an 8-byte sequence number and a
//! 4-byte byte count on top of the bytes themselves.
//!
//! Terminals are addressed as [`TerminalAddr`], `(host, terminal)`. On the wire
//! a host only ever names its own terminals (`host` is empty); the client fills
//! in the display name it knows the machine by, so the same types serve a UI
//! that lists terminals of several hosts.

use std::fmt;

/// Bumped when a message changes shape.
pub const PROTOCOL_VERSION: u16 = 1;

/// The most bytes in one frame body (tag and fields). A longer length prefix is
/// refused before anything is buffered.
pub const MAX_FRAME: usize = 256 * 1024;

/// The most terminal bytes in one `Output` or `Input` message.
pub const MAX_DATA: usize = 64 * 1024;

/// The most terminals one list carries.
pub const MAX_LISTED: usize = 64;

/// The longest name or reason string.
pub const MAX_STR: usize = 256;

/// The longest key or signature blob.
pub const MAX_BLOB: usize = 8 * 1024;

/// The largest grid either side may ask for.
pub const MAX_DIM: u16 = 1000;

/// The display name that always means the machine the app runs on.
pub use connecto_core::LOCALHOST;

/// Why a frame could not be read.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum WireError {
    #[error("frame length {0} is not allowed")]
    BadLength(usize),
    #[error("unknown message tag {0}")]
    UnknownTag(u8),
    #[error("message ended early")]
    Truncated,
    #[error("message has {0} bytes left over")]
    Trailing(usize),
    #[error("field is too long")]
    TooLong,
    #[error("text is not valid")]
    BadText,
    #[error("value is out of range")]
    BadValue,
}

/// A terminal on a host: the host's display name and the host's own id for it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct TerminalAddr {
    /// [`LOCALHOST`] for this machine, else the saved display name.
    pub host: String,
    pub terminal: u32,
}

impl TerminalAddr {
    #[must_use]
    pub fn local(terminal: u32) -> Self {
        Self {
            host: LOCALHOST.to_owned(),
            terminal,
        }
    }

    #[must_use]
    pub fn is_local(&self) -> bool {
        self.host == LOCALHOST
    }
}

impl fmt::Display for TerminalAddr {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}:{}", self.host, self.terminal)
    }
}

/// Why a peer was refused. The text a peer sees is this code only, so a probe
/// learns nothing about which key lists exist.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DenyReason {
    /// The key is not allowed, or the signature does not verify.
    NotAuthorized,
    /// A message arrived out of order.
    Protocol,
    /// The peer speaks another protocol version.
    Version,
    /// Another remote peer is connected already.
    Busy,
    /// The host does not accept remote peers now.
    Disabled,
    /// The action is refused while Pause is on.
    Paused,
    /// The terminal does not exist.
    NoSuchTerminal,
}

impl DenyReason {
    const fn code(self) -> u8 {
        match self {
            Self::NotAuthorized => 1,
            Self::Protocol => 2,
            Self::Version => 3,
            Self::Busy => 4,
            Self::Disabled => 5,
            Self::Paused => 6,
            Self::NoSuchTerminal => 7,
        }
    }

    const fn from_code(code: u8) -> Option<Self> {
        Some(match code {
            1 => Self::NotAuthorized,
            2 => Self::Protocol,
            3 => Self::Version,
            4 => Self::Busy,
            5 => Self::Disabled,
            6 => Self::Paused,
            7 => Self::NoSuchTerminal,
            _ => return None,
        })
    }
}

impl fmt::Display for DenyReason {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(match self {
            Self::NotAuthorized => "not authorized",
            Self::Protocol => "protocol error",
            Self::Version => "protocol version not supported",
            Self::Busy => "another remote peer is connected",
            Self::Disabled => "remote access is off on that machine",
            Self::Paused => "paused on that machine",
            Self::NoSuchTerminal => "no such terminal",
        })
    }
}

/// What the host says about one terminal in a list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TerminalInfo {
    pub id: u32,
    pub name: String,
    /// 0 unknown, 1 Claude, 2 Codex, 3 shell.
    pub tool: u8,
    pub running: bool,
    /// The host's status dot, as `TermActivity` in declaration order.
    pub activity: u8,
    pub cols: u16,
    pub rows: u16,
}

/// A change to the host's terminals that a peer asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ManageOp {
    /// A new terminal of `tool` (1 Claude, 2 Codex), in the host's active
    /// project. It appears in the list once it has started.
    Add {
        tool: u8,
        name: Option<String>,
    },
    Close {
        terminal: u32,
    },
    Restart {
        terminal: u32,
    },
    /// `None` clears the name.
    Rename {
        terminal: u32,
        name: Option<String>,
    },
}

/// How a terminal's process stands.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RemoteStatus {
    Running,
    Exited { code: u32, signal: Option<String> },
    Unknown,
}

/// Every message either side sends.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Message {
    // Control stream, before and during auth.
    Hello {
        version: u16,
    },
    Challenge {
        nonce: [u8; 32],
    },
    AuthResponse {
        /// The key in OpenSSH wire form.
        public_key: Vec<u8>,
        /// An SSHSIG, armored.
        signature: Vec<u8>,
    },
    AuthOk,
    Denied(DenyReason),
    /// One step of the pairing protocol (see `pairing`). Opaque here.
    Pair(Vec<u8>),
    // Control stream, after auth.
    ListTerminals,
    TerminalList(Vec<TerminalInfo>),
    Bye,
    /// A request to change the host's terminals; answered by `ManageResult`
    /// with the same `req`.
    Manage {
        req: u32,
        op: ManageOp,
    },
    ManageResult {
        req: u32,
        /// The new terminal's id when it is known, or why it was refused.
        outcome: Result<Option<u32>, String>,
    },
    // Terminal stream, client to host.
    Attach {
        terminal: u32,
        /// The next byte the client wants; 0 for a first attach.
        resume_seq: u64,
        cols: u16,
        rows: u16,
    },
    Input(Vec<u8>),
    Resize {
        cols: u16,
        rows: u16,
    },
    /// Everything below `seq` is applied, so the host may drop it.
    Ack {
        seq: u64,
    },
    // Terminal stream, host to client.
    Attached {
        /// The sequence number of the first byte that follows.
        start_seq: u64,
        /// True when the host could not resume and a `Resync` follows.
        resync: bool,
        /// The terminal's grid on the host.
        cols: u16,
        rows: u16,
    },
    Output {
        seq: u64,
        bytes: Vec<u8>,
    },
    /// Replace the screen with `repaint`, after which output continues at `seq`.
    Resync {
        seq: u64,
        cols: u16,
        rows: u16,
        repaint: Vec<u8>,
    },
    /// The host's grid changed.
    Size {
        cols: u16,
        rows: u16,
    },
    Status(RemoteStatus),
}

const TAG_HELLO: u8 = 1;
const TAG_CHALLENGE: u8 = 2;
const TAG_AUTH_RESPONSE: u8 = 3;
const TAG_AUTH_OK: u8 = 4;
const TAG_DENIED: u8 = 5;
const TAG_LIST: u8 = 6;
const TAG_TERMINAL_LIST: u8 = 7;
const TAG_BYE: u8 = 8;
const TAG_MANAGE: u8 = 9;
const TAG_MANAGE_RESULT: u8 = 10;
const TAG_ATTACH: u8 = 16;
const TAG_INPUT: u8 = 17;
const TAG_RESIZE: u8 = 18;
const TAG_ACK: u8 = 19;
const TAG_ATTACHED: u8 = 20;
const TAG_OUTPUT: u8 = 21;
const TAG_RESYNC: u8 = 22;
const TAG_STATUS: u8 = 23;
const TAG_SIZE: u8 = 24;
const TAG_PAIR: u8 = 11;

struct Put(Vec<u8>);

impl Put {
    fn u8(&mut self, v: u8) {
        self.0.push(v);
    }
    fn u16(&mut self, v: u16) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u32(&mut self, v: u32) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn u64(&mut self, v: u64) {
        self.0.extend_from_slice(&v.to_be_bytes());
    }
    fn blob(&mut self, v: &[u8]) {
        // Lengths are checked against MAX_FRAME on decode; a local value cannot
        // exceed u32 because frames this large are refused when sent.
        self.u32(u32::try_from(v.len()).unwrap_or(u32::MAX));
        self.0.extend_from_slice(v);
    }
    fn str(&mut self, v: &str) {
        self.blob(v.as_bytes());
    }
}

fn opt_str(p: &mut Put, v: Option<&str>) {
    match v {
        Some(v) => {
            p.u8(1);
            p.str(v);
        }
        None => p.u8(0),
    }
}

struct Take<'a>(&'a [u8]);

impl<'a> Take<'a> {
    fn bytes(&mut self, n: usize) -> Result<&'a [u8], WireError> {
        if self.0.len() < n {
            return Err(WireError::Truncated);
        }
        let (head, rest) = self.0.split_at(n);
        self.0 = rest;
        Ok(head)
    }
    fn u8(&mut self) -> Result<u8, WireError> {
        Ok(self.bytes(1)?.first().copied().unwrap_or(0))
    }
    fn u16(&mut self) -> Result<u16, WireError> {
        let mut a = [0; 2];
        a.copy_from_slice(self.bytes(2)?);
        Ok(u16::from_be_bytes(a))
    }
    fn u32(&mut self) -> Result<u32, WireError> {
        let mut a = [0; 4];
        a.copy_from_slice(self.bytes(4)?);
        Ok(u32::from_be_bytes(a))
    }
    fn u64(&mut self) -> Result<u64, WireError> {
        let mut a = [0; 8];
        a.copy_from_slice(self.bytes(8)?);
        Ok(u64::from_be_bytes(a))
    }
    fn blob(&mut self, max: usize) -> Result<Vec<u8>, WireError> {
        let len = usize::try_from(self.u32()?).map_err(|_| WireError::TooLong)?;
        if len > max {
            return Err(WireError::TooLong);
        }
        Ok(self.bytes(len)?.to_vec())
    }
    fn str(&mut self) -> Result<String, WireError> {
        String::from_utf8(self.blob(MAX_STR)?).map_err(|_| WireError::BadText)
    }
    fn opt_str(&mut self) -> Result<Option<String>, WireError> {
        if self.u8()? == 0 {
            Ok(None)
        } else {
            Ok(Some(self.str()?))
        }
    }
    fn dims(&mut self) -> Result<(u16, u16), WireError> {
        let (cols, rows) = (self.u16()?, self.u16()?);
        if cols == 0 || rows == 0 || cols > MAX_DIM || rows > MAX_DIM {
            return Err(WireError::BadValue);
        }
        Ok((cols, rows))
    }
    fn done(&self) -> Result<(), WireError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(WireError::Trailing(self.0.len()))
        }
    }
}

impl Message {
    /// The frame for this message: length prefix, tag, fields.
    ///
    /// # Errors
    /// `BadLength` when the body would exceed [`MAX_FRAME`].
    pub fn encode(&self) -> Result<Vec<u8>, WireError> {
        let mut p = Put(vec![0, 0, 0, 0]);
        match self {
            Self::Hello { version } => {
                p.u8(TAG_HELLO);
                p.u16(*version);
            }
            Self::Challenge { nonce } => {
                p.u8(TAG_CHALLENGE);
                p.0.extend_from_slice(nonce);
            }
            Self::AuthResponse {
                public_key,
                signature,
            } => {
                p.u8(TAG_AUTH_RESPONSE);
                p.blob(public_key);
                p.blob(signature);
            }
            Self::Pair(bytes) => {
                p.u8(TAG_PAIR);
                p.blob(bytes);
            }
            Self::AuthOk => p.u8(TAG_AUTH_OK),
            Self::Denied(reason) => {
                p.u8(TAG_DENIED);
                p.u8(reason.code());
            }
            Self::ListTerminals => p.u8(TAG_LIST),
            Self::TerminalList(list) => {
                p.u8(TAG_TERMINAL_LIST);
                p.u16(u16::try_from(list.len()).map_err(|_| WireError::TooLong)?);
                for t in list {
                    p.u32(t.id);
                    p.str(&t.name);
                    p.u8(t.tool);
                    p.u8(u8::from(t.running));
                    p.u8(t.activity);
                    p.u16(t.cols);
                    p.u16(t.rows);
                }
            }
            Self::Bye => p.u8(TAG_BYE),
            Self::Manage { req, op } => {
                p.u8(TAG_MANAGE);
                p.u32(*req);
                match op {
                    ManageOp::Add { tool, name } => {
                        p.u8(1);
                        p.u8(*tool);
                        opt_str(&mut p, name.as_deref());
                    }
                    ManageOp::Close { terminal } => {
                        p.u8(2);
                        p.u32(*terminal);
                    }
                    ManageOp::Restart { terminal } => {
                        p.u8(3);
                        p.u32(*terminal);
                    }
                    ManageOp::Rename { terminal, name } => {
                        p.u8(4);
                        p.u32(*terminal);
                        opt_str(&mut p, name.as_deref());
                    }
                }
            }
            Self::ManageResult { req, outcome } => {
                p.u8(TAG_MANAGE_RESULT);
                p.u32(*req);
                match outcome {
                    Ok(id) => {
                        p.u8(1);
                        match id {
                            Some(id) => {
                                p.u8(1);
                                p.u32(*id);
                            }
                            None => p.u8(0),
                        }
                    }
                    Err(why) => {
                        p.u8(0);
                        p.str(why.get(..why.len().min(MAX_STR)).unwrap_or(""));
                    }
                }
            }
            Self::Attach {
                terminal,
                resume_seq,
                cols,
                rows,
            } => {
                p.u8(TAG_ATTACH);
                p.u32(*terminal);
                p.u64(*resume_seq);
                p.u16(*cols);
                p.u16(*rows);
            }
            Self::Input(bytes) => {
                p.u8(TAG_INPUT);
                p.blob(bytes);
            }
            Self::Resize { cols, rows } => {
                p.u8(TAG_RESIZE);
                p.u16(*cols);
                p.u16(*rows);
            }
            Self::Ack { seq } => {
                p.u8(TAG_ACK);
                p.u64(*seq);
            }
            Self::Attached {
                start_seq,
                resync,
                cols,
                rows,
            } => {
                p.u8(TAG_ATTACHED);
                p.u64(*start_seq);
                p.u8(u8::from(*resync));
                p.u16(*cols);
                p.u16(*rows);
            }
            Self::Size { cols, rows } => {
                p.u8(TAG_SIZE);
                p.u16(*cols);
                p.u16(*rows);
            }
            Self::Output { seq, bytes } => {
                p.u8(TAG_OUTPUT);
                p.u64(*seq);
                p.blob(bytes);
            }
            Self::Resync {
                seq,
                cols,
                rows,
                repaint,
            } => {
                p.u8(TAG_RESYNC);
                p.u64(*seq);
                p.u16(*cols);
                p.u16(*rows);
                p.blob(repaint);
            }
            Self::Status(status) => {
                p.u8(TAG_STATUS);
                match status {
                    RemoteStatus::Running => p.u8(0),
                    RemoteStatus::Exited { code, signal } => {
                        p.u8(1);
                        p.u32(*code);
                        p.u8(u8::from(signal.is_some()));
                        if let Some(signal) = signal {
                            p.str(signal);
                        }
                    }
                    RemoteStatus::Unknown => p.u8(2),
                }
            }
        }
        let body = p.0.len() - 4;
        if body > MAX_FRAME {
            return Err(WireError::BadLength(body));
        }
        let len = u32::try_from(body).map_err(|_| WireError::BadLength(body))?;
        p.0.splice(0..4, len.to_be_bytes());
        Ok(p.0)
    }

    /// Read one message body (tag and fields, without the length prefix).
    ///
    /// # Errors
    /// Any [`WireError`] for a malformed body.
    pub fn decode(body: &[u8]) -> Result<Self, WireError> {
        let mut t = Take(body);
        let tag = t.u8()?;
        let message = match tag {
            TAG_HELLO => Self::Hello { version: t.u16()? },
            TAG_CHALLENGE => {
                let mut nonce = [0; 32];
                nonce.copy_from_slice(t.bytes(32)?);
                Self::Challenge { nonce }
            }
            TAG_AUTH_RESPONSE => Self::AuthResponse {
                public_key: t.blob(MAX_BLOB)?,
                signature: t.blob(MAX_BLOB)?,
            },
            TAG_PAIR => Self::Pair(t.blob(MAX_BLOB)?),
            TAG_AUTH_OK => Self::AuthOk,
            TAG_DENIED => Self::Denied(DenyReason::from_code(t.u8()?).ok_or(WireError::BadValue)?),
            TAG_LIST => Self::ListTerminals,
            TAG_TERMINAL_LIST => {
                let count = usize::from(t.u16()?);
                if count > MAX_LISTED {
                    return Err(WireError::TooLong);
                }
                let mut list = Vec::with_capacity(count);
                for _ in 0..count {
                    list.push(TerminalInfo {
                        id: t.u32()?,
                        name: t.str()?,
                        tool: t.u8()?,
                        running: t.u8()? != 0,
                        activity: t.u8()?,
                        cols: t.u16()?,
                        rows: t.u16()?,
                    });
                }
                Self::TerminalList(list)
            }
            TAG_BYE => Self::Bye,
            TAG_MANAGE => {
                let req = t.u32()?;
                let op = match t.u8()? {
                    1 => ManageOp::Add {
                        tool: t.u8()?,
                        name: t.opt_str()?,
                    },
                    2 => ManageOp::Close { terminal: t.u32()? },
                    3 => ManageOp::Restart { terminal: t.u32()? },
                    4 => ManageOp::Rename {
                        terminal: t.u32()?,
                        name: t.opt_str()?,
                    },
                    _ => return Err(WireError::BadValue),
                };
                Self::Manage { req, op }
            }
            TAG_MANAGE_RESULT => {
                let req = t.u32()?;
                let outcome = if t.u8()? == 1 {
                    Ok(if t.u8()? == 1 { Some(t.u32()?) } else { None })
                } else {
                    Err(t.str()?)
                };
                Self::ManageResult { req, outcome }
            }
            TAG_ATTACH => {
                let terminal = t.u32()?;
                let resume_seq = t.u64()?;
                let (cols, rows) = t.dims()?;
                Self::Attach {
                    terminal,
                    resume_seq,
                    cols,
                    rows,
                }
            }
            TAG_INPUT => Self::Input(t.blob(MAX_DATA)?),
            TAG_RESIZE => {
                let (cols, rows) = t.dims()?;
                Self::Resize { cols, rows }
            }
            TAG_ACK => Self::Ack { seq: t.u64()? },
            TAG_ATTACHED => {
                let start_seq = t.u64()?;
                let resync = t.u8()? != 0;
                let (cols, rows) = t.dims()?;
                Self::Attached {
                    start_seq,
                    resync,
                    cols,
                    rows,
                }
            }
            TAG_SIZE => {
                let (cols, rows) = t.dims()?;
                Self::Size { cols, rows }
            }
            TAG_OUTPUT => Self::Output {
                seq: t.u64()?,
                bytes: t.blob(MAX_DATA)?,
            },
            TAG_RESYNC => {
                let seq = t.u64()?;
                let (cols, rows) = t.dims()?;
                Self::Resync {
                    seq,
                    cols,
                    rows,
                    repaint: t.blob(MAX_FRAME - 64)?,
                }
            }
            TAG_STATUS => Self::Status(match t.u8()? {
                0 => RemoteStatus::Running,
                1 => {
                    let code = t.u32()?;
                    let signal = if t.u8()? == 0 { None } else { Some(t.str()?) };
                    RemoteStatus::Exited { code, signal }
                }
                2 => RemoteStatus::Unknown,
                _ => return Err(WireError::BadValue),
            }),
            other => return Err(WireError::UnknownTag(other)),
        };
        t.done()?;
        Ok(message)
    }
}

/// Reassembles frames from a byte stream that delivers them in any chunking.
#[derive(Debug, Default)]
pub struct FrameDecoder {
    buf: Vec<u8>,
    failed: bool,
}

impl FrameDecoder {
    pub fn push(&mut self, bytes: &[u8]) {
        if !self.failed {
            self.buf.extend_from_slice(bytes);
        }
    }

    /// The next whole message, `Ok(None)` when more bytes are needed. After an
    /// error the decoder stays failed: a stream with a bad frame is closed.
    ///
    /// # Errors
    /// Any [`WireError`]; the length prefix is checked before the body arrives.
    pub fn next_message(&mut self) -> Result<Option<Message>, WireError> {
        if self.failed {
            return Err(WireError::BadValue);
        }
        let Some(prefix) = self.buf.get(..4) else {
            return Ok(None);
        };
        let mut a = [0; 4];
        a.copy_from_slice(prefix);
        let len = usize::try_from(u32::from_be_bytes(a)).unwrap_or(usize::MAX);
        if len == 0 || len > MAX_FRAME {
            self.failed = true;
            return Err(WireError::BadLength(len));
        }
        let Some(body) = self.buf.get(4..4 + len) else {
            return Ok(None);
        };
        let result = Message::decode(body);
        match result {
            Ok(message) => {
                self.buf.drain(..4 + len);
                Ok(Some(message))
            }
            Err(err) => {
                self.failed = true;
                Err(err)
            }
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    fn samples() -> Vec<Message> {
        vec![
            Message::Hello { version: 1 },
            Message::Challenge { nonce: [7; 32] },
            Message::AuthResponse {
                public_key: vec![1, 2, 3],
                signature: vec![4; 600],
            },
            Message::AuthOk,
            Message::Pair(vec![1, 2, 3]),
            Message::Denied(DenyReason::Busy),
            Message::ListTerminals,
            Message::TerminalList(vec![TerminalInfo {
                id: 3,
                name: "claude-1".into(),
                tool: 1,
                running: true,
                activity: 5,
                cols: 120,
                rows: 40,
            }]),
            Message::Bye,
            Message::Manage {
                req: 7,
                op: ManageOp::Add {
                    tool: 2,
                    name: Some("review".into()),
                },
            },
            Message::Manage {
                req: 8,
                op: ManageOp::Close { terminal: 3 },
            },
            Message::Manage {
                req: 9,
                op: ManageOp::Restart { terminal: 3 },
            },
            Message::Manage {
                req: 10,
                op: ManageOp::Rename {
                    terminal: 3,
                    name: None,
                },
            },
            Message::ManageResult {
                req: 7,
                outcome: Ok(Some(12)),
            },
            Message::ManageResult {
                req: 8,
                outcome: Ok(None),
            },
            Message::ManageResult {
                req: 9,
                outcome: Err("paused on that machine".into()),
            },
            Message::Attach {
                terminal: 2,
                resume_seq: 99,
                cols: 80,
                rows: 24,
            },
            Message::Input(b"ls\r".to_vec()),
            Message::Resize { cols: 90, rows: 30 },
            Message::Ack { seq: u64::MAX },
            Message::Attached {
                start_seq: 5,
                resync: true,
                cols: 80,
                rows: 24,
            },
            Message::Size {
                cols: 100,
                rows: 50,
            },
            Message::Output {
                seq: 1 << 40,
                bytes: vec![0xff; 1000],
            },
            Message::Resync {
                seq: 12,
                cols: 80,
                rows: 24,
                repaint: vec![b'x'; 5000],
            },
            Message::Status(RemoteStatus::Exited {
                code: 1,
                signal: Some("SIGTERM".into()),
            }),
            Message::Status(RemoteStatus::Running),
            Message::Status(RemoteStatus::Unknown),
        ]
    }

    #[test]
    fn every_message_round_trips() {
        for message in samples() {
            let frame = message.encode().unwrap();
            let mut decoder = FrameDecoder::default();
            decoder.push(&frame);
            assert_eq!(decoder.next_message().unwrap(), Some(message));
            assert_eq!(decoder.next_message().unwrap(), None);
        }
    }

    #[test]
    fn frames_survive_one_byte_chunks_and_back_to_back_delivery() {
        let all = samples();
        let mut stream = Vec::new();
        for m in &all {
            stream.extend(m.encode().unwrap());
        }
        let mut decoder = FrameDecoder::default();
        let mut got = Vec::new();
        for byte in &stream {
            decoder.push(std::slice::from_ref(byte));
            while let Some(m) = decoder.next_message().unwrap() {
                got.push(m);
            }
        }
        assert_eq!(got, all);
    }

    #[test]
    fn an_oversized_length_is_refused_before_the_body_arrives() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes());
        assert_eq!(
            decoder.next_message(),
            Err(WireError::BadLength(MAX_FRAME + 1))
        );
        // It stays failed.
        decoder.push(&Message::AuthOk.encode().unwrap());
        assert!(decoder.next_message().is_err());
    }

    #[test]
    fn a_zero_length_frame_is_refused() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&[0, 0, 0, 0]);
        assert_eq!(decoder.next_message(), Err(WireError::BadLength(0)));
    }

    #[test]
    fn an_unknown_tag_is_refused() {
        let mut decoder = FrameDecoder::default();
        decoder.push(&[0, 0, 0, 1, 250]);
        assert_eq!(decoder.next_message(), Err(WireError::UnknownTag(250)));
    }

    #[test]
    fn truncated_and_trailing_bodies_are_refused() {
        assert_eq!(Message::decode(&[TAG_ACK, 0, 0]), Err(WireError::Truncated));
        assert_eq!(
            Message::decode(&[TAG_AUTH_OK, 9]),
            Err(WireError::Trailing(1))
        );
        assert_eq!(Message::decode(&[]), Err(WireError::Truncated));
    }

    #[test]
    fn a_blob_longer_than_its_limit_is_refused_without_allocating_it() {
        let mut body = vec![TAG_INPUT];
        body.extend((MAX_DATA as u32 + 1).to_be_bytes());
        assert_eq!(Message::decode(&body), Err(WireError::TooLong));
    }

    #[test]
    fn a_list_over_the_cap_and_bad_dimensions_are_refused() {
        let mut body = vec![TAG_TERMINAL_LIST];
        body.extend(((MAX_LISTED + 1) as u16).to_be_bytes());
        assert_eq!(Message::decode(&body), Err(WireError::TooLong));
        let zero = Message::Resize { cols: 0, rows: 5 }.encode().unwrap();
        assert_eq!(Message::decode(&zero[4..]), Err(WireError::BadValue));
        let huge = Message::Resize {
            cols: MAX_DIM + 1,
            rows: 5,
        }
        .encode()
        .unwrap();
        assert_eq!(Message::decode(&huge[4..]), Err(WireError::BadValue));
    }

    #[test]
    fn an_unknown_deny_code_and_invalid_utf8_are_refused() {
        assert_eq!(Message::decode(&[TAG_DENIED, 99]), Err(WireError::BadValue));
        let mut body = vec![TAG_TERMINAL_LIST, 0, 1];
        body.extend(1_u32.to_be_bytes());
        body.extend(2_u32.to_be_bytes());
        body.extend([0xff, 0xfe]);
        body.extend([0, 1, 0, 0, 80, 0, 24]);
        assert_eq!(Message::decode(&body), Err(WireError::BadText));
    }

    #[test]
    fn a_message_too_big_to_send_is_refused() {
        let big = Message::Resync {
            seq: 0,
            cols: 80,
            rows: 24,
            repaint: vec![0; MAX_FRAME],
        };
        assert!(big.encode().is_err());
    }

    #[test]
    fn terminal_addresses_name_a_host_and_localhost_is_local() {
        let local = TerminalAddr::local(4);
        assert!(local.is_local());
        assert_eq!(local.to_string(), "localhost:4");
        let remote = TerminalAddr {
            host: "build-box".into(),
            terminal: 4,
        };
        assert!(!remote.is_local());
        assert_ne!(local, remote);
    }
}
