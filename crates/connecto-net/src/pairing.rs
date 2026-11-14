//! Pairing two machines with an eight-digit code each, so nobody copies an
//! address or a public key (ADR 0022, which replaces the one-sided flow of
//! ADR 0017).
//!
//! Each machine shows its own code. Each person types the other machine's code
//! into their own machine, in either order. Once a machine has both codes it
//! derives everything from the pair, in a canonical order (the lower code
//! first), so both machines compute the same values:
//!
//! - a key `K` from the two codes (Argon2id, the slow step on purpose),
//! - a rendezvous endpoint id `R` from `K`, which the machine holding the
//!   higher code listens on and the one holding the lower code dials,
//! - a SPAKE2 password from `K`.
//!
//! So the roles need no choice: the codes decide them. A machine that has both
//! codes but whose peer has not entered this machine's code yet waits, and the
//! dialing side retries until the listener is up or its code expires.
//!
//! Over the connection to `R` the two run SPAKE2 with key confirmation, bound
//! to `R` and to the TLS session. Then each side sends, under the key, its
//! device identity, its address, and a fresh per-pair SSH public key for the
//! other side to authorize. Each side names the other machine (locally), and
//! then the two exchange a commit MAC. A side writes its record only after it
//! has its own name and the other side's commit, so a pairing that fails
//! before that point leaves nothing behind on either machine.
//!
//! What this protects, and what it does not. A party without the codes cannot
//! reach `R`, since it is derived from both codes; a party with one code must
//! guess the other among a million, each guess an Argon2 evaluation, and a
//! listener is open for two minutes and accepts one connection. The codes are
//! the secret: whoever holds both during their lifetime can pair.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use argon2::{Algorithm, Argon2, Params, Version};
use hmac::{Hmac, KeyInit, Mac};
use sha2::Sha256;
use spake2::{Ed25519Group, Identity, Password, Spake2};
use zeroize::Zeroizing;

use iroh::{PublicKey, SecretKey, Signature};

use crate::auth::checked_key_line;

use crate::transport::{Connection, EndpointId, NetError, Stream};
use crate::wire::Message;

/// How many digits a code has: the one constant, shared with the UI.
pub const CODE_DIGITS: usize = connecto_core::PAIRING_CODE_DIGITS;

/// The number of digits, in words, for the error text.
const fn digit_words() -> &'static str {
    match CODE_DIGITS {
        6 => "six",
        7 => "seven",
        8 => "eight",
        _ => "required",
    }
}

/// How long a code works, from the moment it is shown.
pub const CODE_LIFETIME: Duration = Duration::from_secs(120);

/// How long one protocol step may take.
const STEP_TIMEOUT: Duration = Duration::from_secs(10);

/// How long a connected machine waits for its user to name the other one, and
/// the other machine to name this one.
pub const NAMING_WAIT: Duration = Duration::from_secs(120);

/// How long a side waits for the other's final word once its own is sent.
/// Short on purpose: nothing can be cancelled by then, so the wait is the only
/// way out, and it must not be long.
pub const COMMIT_WAIT: Duration = Duration::from_secs(20);

/// How often a wait checks whether the user has cancelled.
const POLL: Duration = Duration::from_millis(200);

/// Fixed and public: it only separates this use of the function from others.
const KDF_SALT: &[u8] = b"pairing-mutual/v2 salt";

const MAX_NAME: usize = 64;
const MAX_KEY_LINE: usize = 2048;
const MAX_REASON: usize = 200;
const MAX_RELAY: usize = 512;
const MAX_ADDR: usize = 64;
const MAX_ADDRS: usize = 16;

const DIALER_ID: &[u8] = b"connecto/pairing/dialer/v2";
const LISTENER_ID: &[u8] = b"connecto/pairing/listener/v2";

/// Steps of the exchange, the first byte of each `Message::Pair`.
const STEP_PAKE_A: u8 = 1;
const STEP_PAKE_B: u8 = 2;
const STEP_CONFIRM_DIALER: u8 = 3;
const STEP_CONFIRM_LISTENER: u8 = 4;
const STEP_HELLO_DIALER: u8 = 5;
const STEP_HELLO_LISTENER: u8 = 6;
/// The intent to keep the pairing: sent after the record is staged.
const STEP_COMMIT: u8 = 7;
/// The final word: sent once the intents are exchanged and the commit point is
/// passed. A side keeps the pairing only when the other side's final arrives.
const STEP_FINAL: u8 = 8;
/// A side that will not go on sends this, with its reason, instead of its next step.
const STEP_REFUSED: u8 = 0xFF;

#[derive(Debug, thiserror::Error)]
pub enum PairError {
    #[error("Enter the {0} digits shown on the other machine.")]
    BadCode(&'static str),
    #[error("That is this machine's own code. Enter the code shown on the other machine.")]
    SameCode,
    #[error("That is this machine. Pair with the other machine.")]
    SelfPairing,
    #[error("the key could not be made: {0}")]
    Kdf(String),
    #[error("{0}")]
    Net(#[from] NetError),
    #[error("the other machine did not follow the pairing protocol")]
    Protocol,
    #[error("the codes did not match, or the connection was changed; start pairing again")]
    CodeMismatch,
    #[error("the other machine refused: {0}")]
    Refused(String),
    #[error("the other machine did not answer in time")]
    Timeout,
    #[error("the other machine stopped pairing")]
    Stopped,
    #[error("the pairing was cancelled here")]
    Cancelled,
    #[error("the other machine could not prove that it is the machine it names")]
    Unproven,
}

/// A stream that closes before the codes are confirmed means the other side
/// did not accept the codes: it dropped the connection rather than go on.
fn closed_before_confirm(err: PairError) -> PairError {
    match err {
        PairError::Net(NetError::Closed) => PairError::CodeMismatch,
        other => other,
    }
}

/// A stream that closes after the codes are confirmed means the other side stopped.
fn closed_after_confirm(err: PairError) -> PairError {
    match err {
        PairError::Net(NetError::Closed) => PairError::Stopped,
        other => other,
    }
}

/// A pairing code. Its text is never printed: `Debug` hides it.
#[derive(Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct Code(String);

impl std::fmt::Debug for Code {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Code(<hidden>)")
    }
}

impl Code {
    /// A code chosen uniformly at random.
    ///
    /// # Errors
    /// When the system has no randomness.
    pub fn generate() -> Result<Self, PairError> {
        loop {
            let mut bytes = [0_u8; 4];
            getrandom::fill(&mut bytes).map_err(|e| PairError::Kdf(e.to_string()))?;
            let n = u64::from(u32::from_le_bytes(bytes));
            // Throw away the top of the range, so every code is equally likely.
            let modulus = 10_u64.pow(u32::try_from(CODE_DIGITS).unwrap_or(0));
            let limit = (u64::from(u32::MAX) + 1) / modulus * modulus;
            if n < limit {
                return Ok(Self(format!(
                    "{:0width$}",
                    n % modulus,
                    width = CODE_DIGITS
                )));
            }
        }
    }

    /// What a person typed: spaces and dashes are ignored.
    ///
    /// # Errors
    /// `BadCode` unless it is the right number of digits.
    pub fn parse(text: &str) -> Result<Self, PairError> {
        let digits: String = text
            .chars()
            .filter(|c| !c.is_whitespace() && *c != '-')
            .collect();
        if digits.len() == CODE_DIGITS && digits.chars().all(|c| c.is_ascii_digit()) {
            Ok(Self(digits))
        } else {
            Err(PairError::BadCode(digit_words()))
        }
    }

    /// `123 456`, for showing.
    #[must_use]
    pub fn display(&self) -> String {
        let half = CODE_DIGITS / 2;
        format!("{} {}", &self.0[..half], &self.0[half..])
    }

    fn bytes(&self) -> &[u8] {
        self.0.as_bytes()
    }
}

/// The cost of turning the two codes into a key.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct KdfParams {
    pub memory_kib: u32,
    pub iterations: u32,
}

impl KdfParams {
    /// About 0.7 s on an M-series Mac in a release build (measured: 256 MiB
    /// with 3, 6 and 8 passes took 0.28, 0.56 and 0.72 s), and 256 MiB of memory
    /// for each guess. A debug build is about fifteen times slower.
    pub const PRODUCTION: Self = Self {
        memory_kib: 256 * 1024,
        iterations: 8,
    };

    /// For tests that are about everything but the cost.
    pub const FAST: Self = Self {
        memory_kib: 8,
        iterations: 1,
    };
}

/// What the two codes give both machines.
#[derive(Clone, PartialEq, Eq)]
pub struct PairingKeys {
    /// The secret of the rendezvous endpoint `R`.
    pub rendezvous_secret: Zeroizing<[u8; 32]>,
    /// The SPAKE2 password.
    pub pake_password: Zeroizing<[u8; 32]>,
}

impl std::fmt::Debug for PairingKeys {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("PairingKeys(<hidden>)")
    }
}

/// Both codes, in the one order both machines use: the lower first.
fn canonical<'a>(a: &'a Code, b: &'a Code) -> (&'a Code, &'a Code) {
    if a <= b { (a, b) } else { (b, a) }
}

/// The keys of a pairing. The same two codes give the same keys on both
/// machines, whichever was typed first.
///
/// # Errors
/// When the parameters are refused.
pub fn derive_keys(mine: &Code, peer: &Code, params: KdfParams) -> Result<PairingKeys, PairError> {
    let (lo, hi) = canonical(mine, peer);
    let mut password = Zeroizing::new(lo.bytes().to_vec());
    password.push(b'|');
    password.extend_from_slice(hi.bytes());
    let argon = Argon2::new(
        Algorithm::Argon2id,
        Version::V0x13,
        Params::new(params.memory_kib, params.iterations, 1, Some(32))
            .map_err(|e| PairError::Kdf(e.to_string()))?,
    );
    let mut key = Zeroizing::new([0_u8; 32]);
    argon
        .hash_password_into(&password, KDF_SALT, &mut key[..])
        .map_err(|e| PairError::Kdf(e.to_string()))?;
    Ok(PairingKeys {
        rendezvous_secret: Zeroizing::new(mac(&key[..], b"rendezvous", &[])),
        pake_password: Zeroizing::new(mac(&key[..], b"pake", &[])),
    })
}

/// Which side listens and which dials. The machine with the higher code
/// listens, so both decide the same without asking each other.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    Listen,
    Dial,
}

/// The role of a machine that holds `mine` and was given `peer`.
///
/// # Errors
/// `SameCode` when the two codes are equal: the roles could not be told apart.
pub fn role_for(mine: &Code, peer: &Code) -> Result<Role, PairError> {
    match mine.cmp(peer) {
        std::cmp::Ordering::Greater => Ok(Role::Listen),
        std::cmp::Ordering::Less => Ok(Role::Dial),
        std::cmp::Ordering::Equal => Err(PairError::SameCode),
    }
}

/// What a machine tells the other one about itself, under the pairing key.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Offer {
    /// The name this machine goes by (`device_name`).
    pub device: String,
    /// The machine's real endpoint id: the one its sessions use.
    pub id: EndpointId,
    pub relay: Option<String>,
    pub addrs: Vec<SocketAddr>,
    /// The OpenSSH public key this machine generated for the other to allow.
    pub key_line: String,
}

impl Offer {
    fn encode(&self) -> Result<Vec<u8>, PairError> {
        if self.device.len() > MAX_NAME || self.key_line.len() > MAX_KEY_LINE {
            return Err(PairError::Protocol);
        }
        let mut out = Vec::new();
        put_str(&mut out, &self.device);
        out.extend_from_slice(&self.id);
        put_str(&mut out, self.relay.as_deref().unwrap_or(""));
        let addrs = &self.addrs[..self.addrs.len().min(MAX_ADDRS)];
        out.extend_from_slice(&u16::try_from(addrs.len()).unwrap_or(0).to_be_bytes());
        for addr in addrs {
            put_str(&mut out, &addr.to_string());
        }
        put_str(&mut out, &self.key_line);
        Ok(out)
    }

    fn decode(bytes: &[u8]) -> Result<Self, PairError> {
        let mut cur = Cursor(bytes);
        let device = cur.str(MAX_NAME)?;
        let id: EndpointId = cur.take(32)?.try_into().map_err(|_| PairError::Protocol)?;
        let relay = cur.str(MAX_RELAY)?;
        if !relay.is_empty() && !relay_ok(&relay) {
            return Err(PairError::Protocol);
        }
        let count = usize::from(u16::from_be_bytes(
            cur.take(2)?.try_into().map_err(|_| PairError::Protocol)?,
        ));
        if count > MAX_ADDRS {
            return Err(PairError::Protocol);
        }
        let mut addrs = Vec::with_capacity(count);
        for _ in 0..count {
            let text = cur.str(MAX_ADDR)?;
            // Only a socket address, so nothing in it can reach the host list as text.
            addrs.push(text.parse().map_err(|_| PairError::Protocol)?);
        }
        let key_line = cur.str(MAX_KEY_LINE)?;
        cur.done()?;
        if !device.chars().all(|c| !c.is_control())
            || checked_key_line(&key_line).ok().as_deref() != Some(key_line.as_str())
        {
            return Err(PairError::Protocol);
        }
        Ok(Self {
            device,
            id,
            relay: (!relay.is_empty()).then_some(relay),
            addrs,
            key_line,
        })
    }
}

/// A pairing that has reached the commit: both sides have shown the codes,
/// and each has the other's [`Offer`]. Nothing is saved yet.
pub struct Established {
    stream: Box<dyn Stream>,
    key: Zeroizing<Vec<u8>>,
    transcript: Vec<u8>,
    hellos: Vec<u8>,
    role: Role,
    /// The other machine's offer.
    pub peer: Offer,
}

impl std::fmt::Debug for Established {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Established")
            .field("role", &self.role)
            .field("peer", &self.peer)
            .finish_non_exhaustive()
    }
}

type HmacSha256 = Hmac<Sha256>;

fn mac(key: &[u8], label: &[u8], parts: &[&[u8]]) -> [u8; 32] {
    // HMAC takes a key of any length, so this does not fail; if it did, the
    // zero tag would fail every check.
    let Ok(mut m) = HmacSha256::new_from_slice(key) else {
        return [0; 32];
    };
    m.update(label);
    for part in parts {
        m.update(&(part.len() as u32).to_be_bytes());
        m.update(part);
    }
    m.finalize().into_bytes().into()
}

fn mac_ok(key: &[u8], label: &[u8], parts: &[&[u8]], tag: &[u8]) -> bool {
    let Ok(mut m) = HmacSha256::new_from_slice(key) else {
        return false;
    };
    m.update(label);
    for part in parts {
        m.update(&(part.len() as u32).to_be_bytes());
        m.update(part);
    }
    m.verify_slice(tag).is_ok()
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    out.extend_from_slice(&u16::try_from(s.len()).unwrap_or(u16::MAX).to_be_bytes());
    out.extend_from_slice(s.as_bytes());
}

struct Cursor<'a>(&'a [u8]);

impl<'a> Cursor<'a> {
    fn take(&mut self, n: usize) -> Result<&'a [u8], PairError> {
        if self.0.len() < n {
            return Err(PairError::Protocol);
        }
        let (a, b) = self.0.split_at(n);
        self.0 = b;
        Ok(a)
    }
    fn str(&mut self, max: usize) -> Result<String, PairError> {
        let len = usize::from(u16::from_be_bytes(
            self.take(2)?.try_into().map_err(|_| PairError::Protocol)?,
        ));
        if len > max {
            return Err(PairError::Protocol);
        }
        String::from_utf8(self.take(len)?.to_vec()).map_err(|_| PairError::Protocol)
    }
    fn done(&self) -> Result<(), PairError> {
        if self.0.is_empty() {
            Ok(())
        } else {
            Err(PairError::Protocol)
        }
    }
}

/// The body of the next step, or the other side's refusal.
fn recv(stream: &dyn Stream, step: u8, wait: Duration) -> Result<Vec<u8>, PairError> {
    match stream.recv(wait)? {
        Some(Message::Pair(bytes)) if bytes.first() == Some(&step) => Ok(bytes[1..].to_vec()),
        Some(Message::Pair(bytes)) if bytes.first() == Some(&STEP_REFUSED) => Err(
            PairError::Refused(String::from_utf8_lossy(&bytes[1..]).into_owned()),
        ),
        Some(_) => Err(PairError::Protocol),
        None => Err(PairError::Timeout),
    }
}

fn send(stream: &dyn Stream, step: u8, body: &[u8]) -> Result<(), PairError> {
    let mut frame = vec![step];
    frame.extend_from_slice(body);
    stream.send(&Message::Pair(frame))?;
    Ok(())
}

/// Tell the other side why this one stops. Best effort: the stream may be gone.
fn refuse(stream: &dyn Stream, why: &str) {
    let text: String = why.chars().take(MAX_REASON).collect();
    let _ = send(stream, STEP_REFUSED, text.as_bytes());
}

/// Everything both sides MAC: the rendezvous id, the TLS session and the two
/// PAKE messages.
fn transcript(rendezvous: &EndpointId, binding: &[u8; 32], msg_a: &[u8], msg_b: &[u8]) -> Vec<u8> {
    let mut t = Vec::new();
    for part in [rendezvous.as_slice(), binding.as_slice(), msg_a, msg_b] {
        t.extend_from_slice(&(part.len() as u32).to_be_bytes());
        t.extend_from_slice(part);
    }
    t
}

fn split_tag(bytes: &[u8]) -> Result<(&[u8], &[u8]), PairError> {
    if bytes.len() < 32 {
        return Err(PairError::Protocol);
    }
    Ok(bytes.split_at(bytes.len() - 32))
}

/// A relay address a host list can hold: http or https, printable, no spaces.
fn relay_ok(url: &str) -> bool {
    url.len() <= MAX_RELAY
        && (url.starts_with("https://") || url.starts_with("http://"))
        && url.bytes().all(|b| b.is_ascii_graphic())
}

/// What the signer of a hello signs: the role, this pairing's transcript and
/// the offer. Length-prefixed, so no two different inputs give the same text.
fn identity_text(role: &[u8], transcript: &[u8], offer: &[u8]) -> Vec<u8> {
    let mut out = b"connecto/pairing/identity/v1".to_vec();
    for part in [role, transcript, offer] {
        out.extend_from_slice(&(part.len() as u32).to_be_bytes());
        out.extend_from_slice(part);
    }
    out
}

/// This side's hello: its offer, then its signature over the offer and the
/// transcript with its endpoint key. The signature proves that the machine
/// named in the offer is the one sending it; the codes alone cannot.
fn signed_hello(identity: &[u8; 32], role: &[u8], transcript: &[u8], offer: &[u8]) -> Vec<u8> {
    let signature = SecretKey::from_bytes(identity)
        .sign(&identity_text(role, transcript, offer))
        .to_bytes();
    let mut out = offer.to_vec();
    out.extend_from_slice(&signature);
    out
}

/// The other side's hello, checked: its offer must decode, and its signature
/// must verify against the endpoint id the offer names.
fn open_hello(body: &[u8], role: &[u8], transcript: &[u8]) -> Result<Offer, PairError> {
    if body.len() < 64 {
        return Err(PairError::Protocol);
    }
    let (offer_bytes, signature) = body.split_at(body.len() - 64);
    let offer = Offer::decode(offer_bytes)?;
    let key = PublicKey::from_bytes(&offer.id).map_err(|_| PairError::Unproven)?;
    let signature: [u8; 64] = signature.try_into().map_err(|_| PairError::Protocol)?;
    key.verify(
        &identity_text(role, transcript, offer_bytes),
        &Signature::from_bytes(&signature),
    )
    .map_err(|_| PairError::Unproven)?;
    Ok(offer)
}

/// Run the key exchange on `conn`, the connection to the rendezvous endpoint:
/// SPAKE2, key confirmation, then each side's [`Offer`] in both directions,
/// each signed with the endpoint key of the machine it names.
///
/// The listening side checks that the dialing side's offer names the machine
/// the connection came from (the transport's own identity). Both sides check
/// every offer's signature, so a machine holding the codes cannot claim an
/// id it does not hold the key for, and both refuse a machine that offers its
/// own id.
///
/// # Errors
/// Any [`PairError`]. On an error the other side is told where it can be.
pub fn handshake(
    conn: &dyn Connection,
    role: Role,
    keys: &PairingKeys,
    mine: &Offer,
    rendezvous: &EndpointId,
    identity: &[u8; 32],
) -> Result<Established, PairError> {
    let password = Password::new(&keys.pake_password[..]);
    let dialer_id = Identity::new(DIALER_ID);
    let listener_id = Identity::new(LISTENER_ID);
    match role {
        Role::Dial => {
            let stream = conn.open_stream()?;
            let binding = conn.channel_binding()?;
            let (state, msg_a) =
                Spake2::<Ed25519Group>::start_a(&password, &dialer_id, &listener_id);
            send(&*stream, STEP_PAKE_A, &msg_a)?;
            let msg_b = recv(&*stream, STEP_PAKE_B, STEP_TIMEOUT).map_err(closed_before_confirm)?;
            let key = Zeroizing::new(state.finish(&msg_b).map_err(|_| PairError::CodeMismatch)?);
            let t = transcript(rendezvous, &binding, &msg_a, &msg_b);
            send(
                &*stream,
                STEP_CONFIRM_DIALER,
                &mac(&key, b"dialer-confirm", &[&t]),
            )?;
            let confirm = recv(&*stream, STEP_CONFIRM_LISTENER, STEP_TIMEOUT)
                .map_err(closed_before_confirm)?;
            if !mac_ok(&key, b"listener-confirm", &[&t], &confirm) {
                return Err(PairError::CodeMismatch);
            }
            let hello = signed_hello(identity, b"dialer", &t, &mine.encode()?);
            let tag = mac(&key, b"dialer-hello", &[&t, &hello]);
            let mut body = hello.clone();
            body.extend_from_slice(&tag);
            send(&*stream, STEP_HELLO_DIALER, &body)?;
            let reply =
                recv(&*stream, STEP_HELLO_LISTENER, STEP_TIMEOUT).map_err(closed_after_confirm)?;
            let (theirs, tag) = split_tag(&reply)?;
            if !mac_ok(&key, b"listener-hello", &[&t, theirs], tag) {
                return Err(PairError::CodeMismatch);
            }
            let peer = open_hello(theirs, b"listener", &t).inspect_err(|_| {
                refuse(&*stream, "could not prove its identity");
            })?;
            if peer.id == mine.id {
                refuse(&*stream, "that is the same machine");
                return Err(PairError::SelfPairing);
            }
            let mut hellos = hello;
            hellos.extend_from_slice(theirs);
            Ok(Established {
                stream,
                key,
                transcript: t,
                hellos,
                role,
                peer,
            })
        }
        Role::Listen => {
            let stream = conn
                .accept_stream(STEP_TIMEOUT)?
                .ok_or(PairError::Timeout)?;
            let binding = conn.channel_binding()?;
            let client_id = conn.remote_id();
            let msg_a = recv(&*stream, STEP_PAKE_A, STEP_TIMEOUT).map_err(closed_before_confirm)?;
            let (state, msg_b) =
                Spake2::<Ed25519Group>::start_b(&password, &dialer_id, &listener_id);
            send(&*stream, STEP_PAKE_B, &msg_b)?;
            let key = Zeroizing::new(state.finish(&msg_a).map_err(|_| PairError::CodeMismatch)?);
            let t = transcript(rendezvous, &binding, &msg_a, &msg_b);
            let confirm =
                recv(&*stream, STEP_CONFIRM_DIALER, STEP_TIMEOUT).map_err(closed_before_confirm)?;
            if !mac_ok(&key, b"dialer-confirm", &[&t], &confirm) {
                return Err(PairError::CodeMismatch);
            }
            send(
                &*stream,
                STEP_CONFIRM_LISTENER,
                &mac(&key, b"listener-confirm", &[&t]),
            )?;
            let reply =
                recv(&*stream, STEP_HELLO_DIALER, STEP_TIMEOUT).map_err(closed_after_confirm)?;
            let (theirs, tag) = split_tag(&reply)?;
            if !mac_ok(&key, b"dialer-hello", &[&t, theirs], tag) {
                return Err(PairError::CodeMismatch);
            }
            let peer = open_hello(theirs, b"dialer", &t).inspect_err(|_| {
                refuse(&*stream, "could not prove its identity");
            })?;
            if peer.id != client_id {
                refuse(
                    &*stream,
                    "the connection and the offer name different machines",
                );
                return Err(PairError::Protocol);
            }
            if peer.id == mine.id {
                refuse(&*stream, "that is the same machine");
                return Err(PairError::SelfPairing);
            }
            let hello = signed_hello(identity, b"listener", &t, &mine.encode()?);
            let tag = mac(&key, b"listener-hello", &[&t, &hello]);
            let mut body = hello.clone();
            body.extend_from_slice(&tag);
            send(&*stream, STEP_HELLO_LISTENER, &body)?;
            let mut hellos = theirs.to_vec();
            hellos.extend_from_slice(&hello);
            Ok(Established {
                stream,
                key,
                transcript: t,
                hellos,
                role,
                peer,
            })
        }
    }
}

/// The labels of each side's intent and final MACs, as (mine, theirs).
type Labels = (&'static [u8], &'static [u8]);

fn labels(role: Role) -> (Labels, Labels) {
    match role {
        Role::Dial => (
            (b"dialer-intent", b"listener-intent"),
            (b"dialer-final", b"listener-final"),
        ),
        Role::Listen => (
            (b"listener-intent", b"dialer-intent"),
            (b"listener-final", b"dialer-final"),
        ),
    }
}

/// The next message of `step`, checking for a cancel every [`POLL`]. A refusal
/// from the other side is its reason; `Cancelled` when this side cancels.
fn recv_cancellable(
    stream: &dyn Stream,
    step: u8,
    total: Duration,
    cancel: &AtomicBool,
) -> Result<Vec<u8>, PairError> {
    let deadline = Instant::now() + total;
    loop {
        if cancel.load(Ordering::SeqCst) {
            return Err(PairError::Cancelled);
        }
        if Instant::now() >= deadline {
            return Err(PairError::Timeout);
        }
        match stream.recv(POLL)? {
            Some(Message::Pair(bytes)) if bytes.first() == Some(&step) => {
                return Ok(bytes[1..].to_vec());
            }
            Some(Message::Pair(bytes)) if bytes.first() == Some(&STEP_REFUSED) => {
                return Err(PairError::Refused(
                    String::from_utf8_lossy(&bytes[1..]).into_owned(),
                ));
            }
            Some(_) => return Err(PairError::Protocol),
            None => {}
        }
    }
}

impl Established {
    /// Stage one: this side's intent to keep the pairing (or its refusal, with
    /// `decision` an `Err`), and the other side's intent. Until this side sends
    /// its final word, either side may cancel: a cancel here sends the other
    /// side a refusal, and nothing is kept on either side.
    ///
    /// # Errors
    /// `Refused` when the other side will not keep it, `Cancelled` when this
    /// side cancels (the refusal is sent first), `Stopped` when the other side
    /// is gone, `CodeMismatch` for an intent that does not check out.
    pub fn exchange_intent(
        &self,
        cancel: &AtomicBool,
        decision: Result<(), String>,
    ) -> Result<(), PairError> {
        if let Err(why) = decision {
            refuse(&*self.stream, &why);
            return Err(PairError::Refused(why));
        }
        if cancel.load(Ordering::SeqCst) {
            refuse(&*self.stream, "cancelled");
            return Err(PairError::Cancelled);
        }
        let ((mine, theirs), _) = labels(self.role);
        send(
            &*self.stream,
            STEP_COMMIT,
            &mac(&self.key, mine, &[&self.transcript, &self.hellos]),
        )?;
        let got = match recv_cancellable(&*self.stream, STEP_COMMIT, NAMING_WAIT, cancel) {
            Ok(got) => got,
            Err(PairError::Cancelled) => {
                refuse(&*self.stream, "cancelled");
                return Err(PairError::Cancelled);
            }
            Err(PairError::Net(err)) => return Err(closed_after_confirm(PairError::Net(err))),
            Err(other) => return Err(other),
        };
        if !mac_ok(&self.key, theirs, &[&self.transcript, &self.hellos], &got) {
            return Err(PairError::CodeMismatch);
        }
        Ok(())
    }

    /// Tell the other side this one will not keep the pairing. Used when this
    /// side's record cannot be kept any more (the cancel came too late for the
    /// commit point, say).
    pub fn decline(&self, why: &str) {
        refuse(&*self.stream, why);
    }

    /// Stage two: this side's final word, sent at the commit point, and the
    /// other side's. Only when both have been sent and received is the pairing
    /// kept on both sides. Waits at most [`COMMIT_WAIT`].
    ///
    /// # Errors
    /// `Refused`, `Stopped`, `Timeout` or `CodeMismatch`; in each case the
    /// pairing is not kept here.
    pub fn finish(&self) -> Result<(), PairError> {
        let (_, (mine, theirs)) = labels(self.role);
        send(
            &*self.stream,
            STEP_FINAL,
            &mac(&self.key, mine, &[&self.transcript, &self.hellos]),
        )?;
        let got = recv(&*self.stream, STEP_FINAL, COMMIT_WAIT).map_err(closed_after_confirm)?;
        if !mac_ok(&self.key, theirs, &[&self.transcript, &self.hellos], &got) {
            return Err(PairError::CodeMismatch);
        }
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::thread;

    use super::*;
    use crate::transport::loopback;

    /// A machine's endpoint secret, and the id its key gives.
    fn identity(seed: u8) -> ([u8; 32], EndpointId) {
        let secret = [seed; 32];
        (secret, *SecretKey::from_bytes(&secret).public().as_bytes())
    }

    /// One machine taking part: its keys, its offer and its endpoint secret.
    struct Party {
        keys: PairingKeys,
        offer: Offer,
        secret: [u8; 32],
    }

    fn party(keys: &PairingKeys, seed: u8, device: &str, tag: u8) -> Party {
        let (secret, id) = identity(seed);
        Party {
            keys: keys.clone(),
            offer: offer(device, id, tag),
            secret,
        }
    }

    fn code(s: &str) -> Code {
        Code::parse(s).unwrap()
    }

    fn offer(device: &str, id: EndpointId, tag: u8) -> Offer {
        Offer {
            device: device.into(),
            id,
            relay: Some("https://relay.example".into()),
            addrs: vec![format!("10.0.0.{tag}:4000").parse().unwrap()],
            key_line: real_key_line(device),
        }
    }

    /// A real OpenSSH public key line, as a machine sends it.
    fn real_key_line(comment: &str) -> String {
        let key = ssh_key::PrivateKey::random(
            &mut ssh_key::rand_core::OsRng,
            ssh_key::Algorithm::Ed25519,
        )
        .unwrap();
        let mut public = key.public_key().clone();
        public.set_comment(comment);
        public.to_openssh().unwrap()
    }

    fn keys_for(a: &str, b: &str) -> PairingKeys {
        derive_keys(&code(a), &code(b), KdfParams::FAST).unwrap()
    }

    /// Both ends of a loopback pair. `tls_dialer` and `tls_listener` are the
    /// ids the transport reports for each end (the listener sees the dialer's).
    fn run(
        dialer: &Party,
        listener: &Party,
        tls_dialer: EndpointId,
        tls_listener: EndpointId,
    ) -> (
        Result<Established, PairError>,
        Result<Established, PairError>,
    ) {
        let (client, host) = loopback::pair(tls_dialer, tls_listener, BINDING);
        let rendezvous = [9; 32];
        thread::scope(|s| {
            let listening = s.spawn(|| {
                handshake(
                    &host,
                    Role::Listen,
                    &listener.keys,
                    &listener.offer,
                    &rendezvous,
                    &listener.secret,
                )
            });
            let dialing = handshake(
                &client,
                Role::Dial,
                &dialer.keys,
                &dialer.offer,
                &rendezvous,
                &dialer.secret,
            );
            (listening.join().unwrap(), dialing)
        })
    }

    const BINDING: [u8; 32] = [7; 32];

    #[test]
    fn the_codes_decide_the_roles_and_equal_codes_have_none() {
        assert_eq!(
            role_for(&code("50000000"), &code("40000000")).unwrap(),
            Role::Listen
        );
        assert_eq!(
            role_for(&code("40000000"), &code("50000000")).unwrap(),
            Role::Dial
        );
        assert!(matches!(
            role_for(&code("12345678"), &code("12345678")),
            Err(PairError::SameCode)
        ));
    }

    #[test]
    fn both_machines_derive_the_same_keys_in_either_order_and_other_codes_give_others() {
        let a = keys_for("12345678", "87654321");
        assert_eq!(a, keys_for("87654321", "12345678"));
        assert_ne!(a, keys_for("12345678", "87654322"));
        assert_ne!(*a.rendezvous_secret, *a.pake_password);
        assert!(!format!("{a:?}").contains("12345678"));
    }

    #[test]
    fn codes_have_the_configured_digits_and_are_parsed_forgivingly() {
        for _ in 0..50 {
            let c = Code::generate().unwrap();
            assert_eq!(c.0.len(), CODE_DIGITS);
            assert!(c.0.chars().all(|ch| ch.is_ascii_digit()));
        }
        assert_eq!(code("1234 5678"), code("12345678"));
        assert_eq!(code("12-34-56-78"), code("12345678"));
        for bad in ["12345", "1234567", "123456789", "1234567a", "", "١٢٣٤٥٦٧٨"] {
            assert!(Code::parse(bad).is_err(), "{bad}");
        }
        assert_eq!(code("00420000").display(), "0042 0000");
        let wrong_len = Code::parse("1234567").unwrap_err();
        assert_eq!(
            wrong_len.to_string(),
            "Enter the eight digits shown on the other machine."
        );
        assert!(!format!("{:?}", code("12345678")).contains("12345678"));
    }

    #[test]
    fn two_machines_with_the_same_codes_exchange_offers_and_both_keep() {
        let keys = keys_for("12345678", "87654321");
        let d = party(&keys, 1, "laptop", 1);
        let l = party(&keys, 2, "studio", 2);
        let (listened, dialed) = run(&d, &l, d.offer.id, l.offer.id);
        let listened = listened.unwrap();
        let dialed = dialed.unwrap();
        assert_eq!(listened.peer, d.offer);
        assert_eq!(dialed.peer, l.offer);
        let never = AtomicBool::new(false);
        thread::scope(|s| {
            let a = s.spawn(|| {
                listened.exchange_intent(&never, Ok(()))?;
                listened.finish()
            });
            let b = dialed
                .exchange_intent(&never, Ok(()))
                .and_then(|()| dialed.finish());
            assert!(b.is_ok(), "{b:?}");
            assert!(a.join().unwrap().is_ok());
        });
    }

    #[test]
    fn a_cancel_before_the_final_word_makes_the_other_side_refuse_and_keep_nothing() {
        let keys = keys_for("12345678", "87654321");
        let d = party(&keys, 1, "laptop", 1);
        let l = party(&keys, 2, "studio", 2);
        let (listened, dialed) = run(&d, &l, d.offer.id, l.offer.id);
        let listened = listened.unwrap();
        let dialed = dialed.unwrap();
        let cancelled = AtomicBool::new(true);
        let never = AtomicBool::new(false);
        thread::scope(|s| {
            let a = s.spawn(|| listened.exchange_intent(&cancelled, Ok(())));
            let b = dialed.exchange_intent(&never, Ok(()));
            assert!(matches!(a.join().unwrap(), Err(PairError::Cancelled)));
            assert!(
                matches!(&b, Err(PairError::Refused(why)) if why == "cancelled"),
                "{b:?}"
            );
        });
    }

    #[test]
    fn a_side_that_cancels_while_the_other_waits_for_its_intent_is_seen_as_gone() {
        let keys = keys_for("12345678", "87654321");
        let d = party(&keys, 1, "laptop", 1);
        let l = party(&keys, 2, "studio", 2);
        let (listened, dialed) = run(&d, &l, d.offer.id, l.offer.id);
        let listened = listened.unwrap();
        let dialed = dialed.unwrap();
        let cancelled = AtomicBool::new(false);
        let never = AtomicBool::new(false);
        thread::scope(|s| {
            // The dialer is cancelled while it waits; its wait then ends at once.
            let b = s.spawn(|| dialed.exchange_intent(&cancelled, Ok(())));
            // The listener never intends to keep: it is dropped as if it vanished.
            thread::sleep(Duration::from_millis(300));
            cancelled.store(true, Ordering::SeqCst);
            assert!(matches!(b.join().unwrap(), Err(PairError::Cancelled)));
            drop(listened);
            let _ = never;
        });
    }

    #[test]
    fn a_different_code_on_one_side_fails_both_and_offers_nothing() {
        let d = party(&keys_for("12345678", "87654321"), 1, "laptop", 1);
        let l = party(&keys_for("12345678", "87654322"), 2, "studio", 2);
        let (listened, dialed) = run(&d, &l, d.offer.id, l.offer.id);
        assert!(
            matches!(listened, Err(PairError::CodeMismatch)),
            "{listened:?}"
        );
        assert!(matches!(dialed, Err(PairError::CodeMismatch)), "{dialed:?}");
    }

    #[test]
    fn a_connection_with_another_tls_binding_is_not_accepted() {
        struct OtherBinding<'a>(&'a dyn Connection);
        impl Connection for OtherBinding<'_> {
            fn local_id(&self) -> EndpointId {
                self.0.local_id()
            }
            fn remote_id(&self) -> EndpointId {
                self.0.remote_id()
            }
            fn channel_binding(&self) -> Result<[u8; 32], NetError> {
                Ok([0xee; 32])
            }
            fn open_stream(&self) -> Result<Box<dyn Stream>, NetError> {
                self.0.open_stream()
            }
            fn accept_stream(&self, t: Duration) -> Result<Option<Box<dyn Stream>>, NetError> {
                self.0.accept_stream(t)
            }
            fn close(&self) {
                self.0.close();
            }
        }
        let keys = keys_for("12345678", "87654321");
        let d = party(&keys, 1, "laptop", 1);
        let l = party(&keys, 2, "studio", 2);
        let (client, host) = loopback::pair(d.offer.id, l.offer.id, BINDING);
        let rendezvous = [9; 32];
        let (listened, dialed) = thread::scope(|s| {
            let listening = s.spawn(|| {
                handshake(
                    &OtherBinding(&host),
                    Role::Listen,
                    &keys,
                    &l.offer,
                    &rendezvous,
                    &l.secret,
                )
            });
            let dialing = handshake(&client, Role::Dial, &keys, &d.offer, &rendezvous, &d.secret);
            (listening.join().unwrap(), dialing)
        });
        assert!(
            matches!(listened, Err(PairError::CodeMismatch)),
            "{listened:?}"
        );
        assert!(dialed.is_err());
    }

    #[test]
    fn the_same_machine_on_both_ends_is_refused() {
        let keys = keys_for("12345678", "87654321");
        let d = party(&keys, 1, "laptop", 1);
        let same = party(&keys, 1, "laptop again", 3);
        let (listened, dialed) = run(&d, &same, d.offer.id, same.offer.id);
        assert!(
            matches!(listened, Err(PairError::SelfPairing)),
            "{listened:?}"
        );
        assert!(matches!(dialed, Err(PairError::Refused(_))), "{dialed:?}");
    }

    #[test]
    fn a_listener_that_claims_another_machines_id_is_rejected_by_the_dialer() {
        // The attacker holds both codes, so the exchange completes, but it has
        // not the key of the machine X it names.
        let keys = keys_for("12345678", "87654321");
        let dialer = party(&keys, 1, "laptop", 1);
        let victim_id = identity(2).1;
        let attacker = Party {
            keys: keys.clone(),
            offer: offer("studio", victim_id, 2),
            secret: identity(9).0,
        };
        let (_listened, dialed) = run(&dialer, &attacker, dialer.offer.id, victim_id);
        assert!(matches!(dialed, Err(PairError::Unproven)), "{dialed:?}");
    }

    #[test]
    fn a_dialer_that_claims_another_machines_id_is_rejected_by_the_listener() {
        // The connection comes from X's endpoint in this test, so the transport
        // identity agrees with the offer and only the signature can catch it.
        let keys = keys_for("12345678", "87654321");
        let victim_id = identity(1).1;
        let attacker = Party {
            keys: keys.clone(),
            offer: offer("laptop", victim_id, 1),
            secret: identity(9).0,
        };
        let listener = party(&keys, 2, "studio", 2);
        let (listened, _dialed) = run(&attacker, &listener, victim_id, listener.offer.id);
        assert!(matches!(listened, Err(PairError::Unproven)), "{listened:?}");
    }

    #[test]
    fn a_refusal_at_the_commit_reaches_the_other_side_and_saves_nothing_there() {
        let keys = keys_for("12345678", "87654321");
        let d = party(&keys, 1, "laptop", 1);
        let l = party(&keys, 2, "studio", 2);
        let (listened, dialed) = run(&d, &l, d.offer.id, l.offer.id);
        let listened = listened.unwrap();
        let dialed = dialed.unwrap();
        let never = AtomicBool::new(false);
        thread::scope(|s| {
            let a = s.spawn(|| listened.exchange_intent(&never, Err("Pause is on".into())));
            let b = dialed.exchange_intent(&never, Ok(()));
            assert!(matches!(a.join().unwrap(), Err(PairError::Refused(_))));
            assert!(
                matches!(&b, Err(PairError::Refused(why)) if why.contains("Pause")),
                "{b:?}"
            );
        });
    }

    /// Raw offer bytes, so a hostile peer's values can be sent as they are.
    fn raw_offer(device: &str, relay: &str, addrs: &[&str], key_line: &str) -> Vec<u8> {
        let mut out = Vec::new();
        put_str(&mut out, device);
        out.extend_from_slice(&[2; 32]);
        put_str(&mut out, relay);
        out.extend_from_slice(&u16::try_from(addrs.len()).unwrap().to_be_bytes());
        for addr in addrs {
            put_str(&mut out, addr);
        }
        put_str(&mut out, key_line);
        out
    }

    #[test]
    fn a_hostile_relay_or_address_or_key_is_refused_at_decode() {
        let good = real_key_line("laptop");
        assert!(
            Offer::decode(&raw_offer(
                "laptop",
                "https://ok.example",
                &["10.0.0.1:4000"],
                &good
            ))
            .is_ok()
        );
        for relay in [
            "https://a.example\tfake-host",
            "https://a.example\n10.0.0.9:1",
            "https://a.example /x",
            "ftp://a.example",
            "-",
            "javascript:alert(1)",
        ] {
            let bytes = raw_offer("laptop", relay, &["10.0.0.1:4000"], &good);
            assert!(
                matches!(Offer::decode(&bytes), Err(PairError::Protocol)),
                "{relay:?}"
            );
        }
        for addr in [
            "10.0.0.1:4000\tx",
            "10.0.0.1:4000,10.0.0.2:1",
            "host.example:4000",
            " 10.0.0.1:4000",
        ] {
            let bytes = raw_offer("laptop", "https://ok.example", &[addr], &good);
            assert!(
                matches!(Offer::decode(&bytes), Err(PairError::Protocol)),
                "{addr:?}"
            );
        }
        let bad_key = raw_offer(
            "laptop",
            "https://ok.example",
            &[],
            "ssh-ed25519 AAAA\nhost-line",
        );
        assert!(matches!(Offer::decode(&bad_key), Err(PairError::Protocol)));
        let bad_name = raw_offer("lap\ntop", "https://ok.example", &[], &good);
        assert!(matches!(Offer::decode(&bad_name), Err(PairError::Protocol)));
    }

    #[test]
    fn the_key_derivation_is_slow_enough_to_matter_in_release() {
        // The real cost is measured in release builds only (see KdfParams).
        let started = std::time::Instant::now();
        derive_keys(&code("12345678"), &code("87654321"), KdfParams::FAST).unwrap();
        assert!(started.elapsed() < Duration::from_secs(5));
    }
}
