//! What one person gives another so a machine can be added: its identity, and
//! how to reach it.
//!
//! `connecto:<64 hex>` then optionally `?relay=<url>` and `&addr=<ip:port>`
//! (repeated). The fingerprint shown beside it is a short digest of the
//! identity, for comparing by eye before the first connection is trusted.

use std::net::SocketAddr;

use sha2::{Digest, Sha256};

use crate::transport::EndpointId;

const PREFIX: &str = "connecto:";

/// A parsed address.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Ticket {
    pub id: EndpointId,
    pub relay: Option<String>,
    pub addrs: Vec<SocketAddr>,
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|b| format!("{b:02x}")).collect()
}

fn unhex(s: &str) -> Option<EndpointId> {
    if s.len() != 64 || !s.is_ascii() {
        return None;
    }
    let mut id = [0_u8; 32];
    for (i, slot) in id.iter_mut().enumerate() {
        *slot = u8::from_str_radix(s.get(2 * i..2 * i + 2)?, 16).ok()?;
    }
    Some(id)
}

/// `xxxx-xxxx-xxxx-xxxx-xxxx`: 80 bits of SHA-256 of the identity.
#[must_use]
pub fn fingerprint(id: &EndpointId) -> String {
    let digest = Sha256::digest(id);
    let text = hex(digest.get(..10).unwrap_or_default());
    text.as_bytes()
        .chunks(4)
        .map(|c| String::from_utf8_lossy(c).into_owned())
        .collect::<Vec<_>>()
        .join("-")
}

impl Ticket {
    /// # Errors
    /// With what is wrong, in words for the person who pasted it.
    pub fn parse(text: &str) -> Result<Self, String> {
        let text = text.trim();
        let rest = text
            .strip_prefix(PREFIX)
            .ok_or("an address starts with \"connecto:\"")?;
        let (id_text, query) = rest.split_once('?').unwrap_or((rest, ""));
        let id = unhex(id_text).ok_or("the machine id is not 64 hex digits")?;
        let mut ticket = Self {
            id,
            relay: None,
            addrs: Vec::new(),
        };
        for pair in query.split('&').filter(|p| !p.is_empty()) {
            match pair.split_once('=') {
                Some(("relay", url))
                    if url.starts_with("https://") || url.starts_with("http://") =>
                {
                    ticket.relay = Some(url.to_owned());
                }
                Some(("relay", _)) => return Err("the relay address must be a URL".to_owned()),
                Some(("addr", addr)) => ticket.addrs.push(
                    addr.parse()
                        .map_err(|_| format!("\"{addr}\" is not an address"))?,
                ),
                _ => return Err(format!("unknown part \"{pair}\"")),
            }
        }
        Ok(ticket)
    }

    #[must_use]
    pub fn to_text(&self) -> String {
        let mut out = format!("{PREFIX}{}", hex(&self.id));
        let mut sep = '?';
        if let Some(relay) = &self.relay {
            out.push(sep);
            out.push_str(&format!("relay={relay}"));
            sep = '&';
        }
        for addr in &self.addrs {
            out.push(sep);
            out.push_str(&format!("addr={addr}"));
            sep = '&';
        }
        out
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_ticket_round_trips_with_and_without_hints() {
        let bare = Ticket {
            id: [7; 32],
            relay: None,
            addrs: vec![],
        };
        assert_eq!(Ticket::parse(&bare.to_text()).unwrap(), bare);
        let full = Ticket {
            id: [9; 32],
            relay: Some("https://staging.example/".into()),
            addrs: vec!["10.0.0.2:4000".parse().unwrap(), "[::1]:5".parse().unwrap()],
        };
        assert_eq!(
            Ticket::parse(&format!("  {} \n", full.to_text())).unwrap(),
            full
        );
    }

    #[test]
    fn bad_tickets_say_what_is_wrong() {
        assert!(Ticket::parse("hello").unwrap_err().contains("connecto:"));
        assert!(
            Ticket::parse("connecto:abcd")
                .unwrap_err()
                .contains("64 hex")
        );
        let id = "11".repeat(32);
        assert!(Ticket::parse(&format!("connecto:{id}?relay=ftp://x")).is_err());
        assert!(Ticket::parse(&format!("connecto:{id}?addr=nope")).is_err());
        assert!(Ticket::parse(&format!("connecto:{id}?bogus=1")).is_err());
    }

    #[test]
    fn the_fingerprint_is_stable_and_differs_per_machine() {
        let a = fingerprint(&[1; 32]);
        assert_eq!(a, fingerprint(&[1; 32]));
        assert_ne!(a, fingerprint(&[2; 32]));
        assert_eq!(a.len(), 24);
        assert_eq!(a.matches('-').count(), 4);
    }
}
