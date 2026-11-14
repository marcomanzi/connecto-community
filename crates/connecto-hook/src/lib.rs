//! The hook channel's wire format.
//!
//! A frame is a big-endian `u32` body length, then a body of one token-length
//! byte, the token, and the tool's raw payload. The server answers with one
//! byte, so the sender stays alive while its process ancestry is checked.

pub mod automation;
pub mod bridge;
pub mod client;
pub mod name;
pub mod package;
pub mod shrink;
pub mod transport;

use std::io::{self, Read};

/// Largest accepted body.
pub const MAX_FRAME: usize = 256 * 1024;

/// The byte a server sends once it has taken a frame.
pub const ACK: u8 = 1;

/// Environment variable naming the session's socket.
pub const SOCKET_ENV: &str = "CONNECTO_HOOK_SOCK";

/// Environment variable holding the session's token.
pub const TOKEN_ENV: &str = "CONNECTO_HOOK_TOKEN";

/// A decoded frame body.
#[derive(Debug, PartialEq, Eq)]
pub struct Frame<'a> {
    pub token: &'a [u8],
    pub payload: &'a [u8],
}

/// Encode a frame, or `None` when it is over the limit or the token is too long.
#[must_use]
pub fn encode(token: &[u8], payload: &[u8]) -> Option<Vec<u8>> {
    let token_len = u8::try_from(token.len()).ok()?;
    let body_len = 1 + token.len() + payload.len();
    if body_len > MAX_FRAME {
        return None;
    }
    let mut frame = Vec::with_capacity(4 + body_len);
    frame.extend_from_slice(&u32::try_from(body_len).ok()?.to_be_bytes());
    frame.push(token_len);
    frame.extend_from_slice(token);
    frame.extend_from_slice(payload);
    Some(frame)
}

/// Split a body into token and payload, or `None` when it is malformed.
#[must_use]
pub fn decode(body: &[u8]) -> Option<Frame<'_>> {
    let (len, rest) = body.split_first()?;
    let split = usize::from(*len);
    (split <= rest.len()).then(|| {
        let (token, payload) = rest.split_at(split);
        Frame { token, payload }
    })
}

/// Read one body. A declared length over the limit is refused unread.
pub fn read_body(reader: &mut impl Read) -> io::Result<Vec<u8>> {
    let mut len = [0_u8; 4];
    reader.read_exact(&mut len)?;
    let len = usize::try_from(u32::from_be_bytes(len)).unwrap_or(usize::MAX);
    if len > MAX_FRAME {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "frame over limit",
        ));
    }
    let mut body = vec![0_u8; len];
    reader.read_exact(&mut body)?;
    Ok(body)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_frame_survives_the_round_trip() {
        let frame = encode(b"token", b"{\"a\":1}").unwrap();
        let body = read_body(&mut frame.as_slice()).unwrap();
        assert_eq!(
            decode(&body),
            Some(Frame {
                token: b"token",
                payload: b"{\"a\":1}"
            })
        );
    }

    #[test]
    fn an_oversized_payload_is_not_encoded() {
        assert!(encode(b"t", &vec![0; MAX_FRAME]).is_none());
        assert!(encode(&[0; 256], b"x").is_none());
    }

    #[test]
    fn an_oversized_declared_length_is_refused_before_reading() {
        let declared = u32::try_from(MAX_FRAME + 1).unwrap().to_be_bytes();
        assert!(read_body(&mut declared.as_slice()).is_err());
    }

    #[test]
    fn a_truncated_or_malformed_frame_is_an_error() {
        assert!(read_body(&mut [0_u8, 0, 0, 9, 1].as_slice()).is_err());
        assert_eq!(decode(&[]), None);
        assert_eq!(decode(&[5, b'a']), None);
    }
}
