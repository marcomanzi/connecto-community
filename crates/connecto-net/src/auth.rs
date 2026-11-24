//! The SSH-key handshake, as a state machine that knows nothing of the network.
//!
//! The host sends a random nonce. The client signs, with an SSH key (held in
//! ssh-agent, so a passphrase or a hardware key works), a payload that binds
//! the nonce to this one session: both endpoint ids and a value both ends
//! derive from the connection's TLS secrets. The signature is an SSHSIG in the
//! [`AUTH_NAMESPACE`], so it cannot be mistaken for a signature made for git or
//! `ssh-keygen -Y sign` with the same key. The host accepts it only from a key
//! in its own `authorized_keys` file for Connecto.
//!
//! Replaying a recorded response fails three ways: the nonce is fresh per
//! connection, the payload names this connection's binding, and the nonce is
//! spent by the first response whatever it holds.

use std::path::{Path, PathBuf};
use std::sync::Arc;

use ssh_key::{HashAlg, LineEnding, PrivateKey, PublicKey, SshSig};

use crate::transport::EndpointId;
use crate::wire::{DenyReason, Message, PROTOCOL_VERSION};

/// The SSHSIG namespace for Connecto's remote handshake.
pub const AUTH_NAMESPACE: &str = "connecto-remote-auth@v1";

const PAYLOAD_TAG: &[u8] = b"connecto-remote-auth-v1\0";

/// What the client signs: the nonce, who is connecting to whom, and the
/// session's binding value, each with a length so fields cannot shift.
#[must_use]
pub fn auth_payload(
    nonce: &[u8; 32],
    client: &EndpointId,
    host: &EndpointId,
    binding: &[u8; 32],
) -> Vec<u8> {
    let mut out = PAYLOAD_TAG.to_vec();
    for field in [nonce.as_slice(), client, host, binding] {
        out.extend_from_slice(&u32::try_from(field.len()).unwrap_or(0).to_be_bytes());
        out.extend_from_slice(field);
    }
    out
}

/// Why signing or key loading failed on the client.
#[derive(Debug, thiserror::Error)]
pub enum SignError {
    #[error("ssh-agent: {0}")]
    Agent(String),
    #[error("signing: {0}")]
    Sign(String),
    #[error("no ssh-agent is running (SSH_AUTH_SOCK is not set)")]
    NoAgent,
}

/// The named pipe the Windows OpenSSH agent (service `ssh-agent`) listens on.
#[cfg(windows)]
pub const WINDOWS_AGENT_PIPE: &str = r"\\.\pipe\openssh-ssh-agent";

/// Where the agent is when `SSH_AUTH_SOCK` does not say: nowhere on Unix.
#[cfg(not(windows))]
fn default_agent_path() -> Option<PathBuf> {
    None
}

/// Where the agent is when `SSH_AUTH_SOCK` does not say: the Windows OpenSSH
/// agent's pipe.
#[cfg(windows)]
fn default_agent_path() -> Option<PathBuf> {
    Some(PathBuf::from(WINDOWS_AGENT_PIPE))
}

/// A connection to the agent at `path` (a Unix socket, or on Windows a named
/// pipe).
fn connect_agent(path: &Path) -> Result<ssh_agent_client_rs::Client, SignError> {
    ssh_agent_client_rs::Client::connect(path)
        .map_err(|err| SignError::Agent(format!("{err}{}", agent_hint(path))))
}

#[cfg(not(windows))]
fn agent_hint(_path: &Path) -> &'static str {
    ""
}

/// The usual reason the Windows agent's pipe is not there.
#[cfg(windows)]
fn agent_hint(path: &Path) -> &'static str {
    if path == Path::new(WINDOWS_AGENT_PIPE) {
        " (is the \"OpenSSH Authentication Agent\" service running?)"
    } else {
        ""
    }
}

/// Something that can sign an SSHSIG with one SSH key.
pub trait Signer: Send + Sync {
    fn public_key(&self) -> PublicKey;

    /// # Errors
    /// When the key or the agent cannot sign.
    fn sign(&self, namespace: &str, hash: HashAlg, msg: &[u8]) -> Result<SshSig, SignError>;
}

/// A key held in memory. For tests and for keys the user has chosen to give
/// Connecto directly; the normal path is [`AgentSigner`].
pub struct KeySigner(pub PrivateKey);

impl Signer for KeySigner {
    fn public_key(&self) -> PublicKey {
        self.0.public_key().clone()
    }

    fn sign(&self, namespace: &str, hash: HashAlg, msg: &[u8]) -> Result<SshSig, SignError> {
        self.0
            .sign(namespace, hash, msg)
            .map_err(|err| SignError::Sign(err.to_string()))
    }
}

/// A key held by ssh-agent, which signs without ever giving the key up.
pub struct AgentSigner {
    socket: PathBuf,
    key: PublicKey,
}

impl AgentSigner {
    /// The agent named by `SSH_AUTH_SOCK`, or `socket`. On Windows, when
    /// neither is given, the named pipe of the Windows OpenSSH agent
    /// (`\\.\pipe\openssh-ssh-agent`); whether it is running shows on connecting.
    ///
    /// # Errors
    /// `NoAgent` when neither is available (never on Windows).
    pub fn socket_path(socket: Option<&Path>) -> Result<PathBuf, SignError> {
        socket
            .map(Path::to_owned)
            .or_else(|| std::env::var_os("SSH_AUTH_SOCK").map(PathBuf::from))
            .or_else(default_agent_path)
            .ok_or(SignError::NoAgent)
    }

    /// The keys the agent holds.
    ///
    /// # Errors
    /// When the agent cannot be reached.
    pub fn list(socket: Option<&Path>) -> Result<Vec<PublicKey>, SignError> {
        let path = Self::socket_path(socket)?;
        let mut client = connect_agent(&path)?;
        let all = client
            .list_all_identities()
            .map_err(|err| SignError::Agent(err.to_string()))?;
        Ok(all
            .into_iter()
            .filter_map(|identity| match identity {
                ssh_agent_client_rs::Identity::PublicKey(key) => Some(key.into_owned()),
                ssh_agent_client_rs::Identity::Certificate(_) => None,
            })
            .collect())
    }

    #[must_use]
    pub fn new(socket: PathBuf, key: PublicKey) -> Self {
        Self { socket, key }
    }
}

impl Signer for AgentSigner {
    fn public_key(&self) -> PublicKey {
        self.key.clone()
    }

    fn sign(&self, namespace: &str, hash: HashAlg, msg: &[u8]) -> Result<SshSig, SignError> {
        let data = SshSig::signed_data(namespace, hash, msg)
            .map_err(|err| SignError::Sign(err.to_string()))?;
        let mut client = connect_agent(&self.socket)?;
        let signature = client
            .sign(&self.key, &data)
            .map_err(|err| SignError::Agent(err.to_string()))?;
        SshSig::new(self.key.key_data().clone(), namespace, hash, signature)
            .map_err(|err| SignError::Sign(err.to_string()))
    }
}

/// Every key the local ssh-agent holds, each as a signer, asked afresh each
/// time so a key added later is found. An agent that cannot be reached
/// yields none.
pub struct AgentSigners;

impl crate::reconnect::SignerSource for AgentSigners {
    fn signers(&self) -> Vec<Arc<dyn Signer>> {
        let Ok(socket) = AgentSigner::socket_path(None) else {
            return Vec::new();
        };
        AgentSigner::list(Some(&socket))
            .unwrap_or_default()
            .into_iter()
            .take(8)
            .map(|key| Arc::new(AgentSigner::new(socket.clone(), key)) as Arc<dyn Signer>)
            .collect()
    }
}

/// The key a paired machine signs with: this machine's dedicated key in the
/// secret store. Empty until the machine has paired, because reading never
/// makes the key.
pub struct DeviceKeySigners(pub Arc<dyn crate::secrets::SecretStore>);

impl crate::reconnect::SignerSource for DeviceKeySigners {
    fn signers(&self) -> Vec<Arc<dyn Signer>> {
        match crate::secrets::existing_device_ssh_key(&*self.0) {
            Ok(Some(key)) => vec![Arc::new(KeySigner(key)) as Arc<dyn Signer>],
            _ => Vec::new(),
        }
    }
}

/// The key this machine uses to dial one paired machine: the key pairing made
/// for it. A machine paired by the older flow has none, and uses the device key.
pub struct PeerKeySigners {
    pub store: Arc<dyn crate::secrets::SecretStore>,
    pub id: EndpointId,
}

impl crate::reconnect::SignerSource for PeerKeySigners {
    fn signers(&self) -> Vec<Arc<dyn Signer>> {
        match crate::secrets::existing_pair_key(&*self.store, &self.id) {
            Ok(Some(key)) => vec![Arc::new(KeySigner(key)) as Arc<dyn Signer>],
            Ok(None) => DeviceKeySigners(Arc::clone(&self.store)).signers(),
            Err(_) => Vec::new(),
        }
    }
}

/// Several sources, tried in order: the paired key first, then the agent's.
pub struct CombinedSigners(pub Vec<Arc<dyn crate::reconnect::SignerSource>>);

impl crate::reconnect::SignerSource for CombinedSigners {
    fn signers(&self) -> Vec<Arc<dyn Signer>> {
        self.0.iter().flat_map(|s| s.signers()).collect()
    }
}

/// One line of an `authorized_keys` file as a person reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct KeyLine {
    pub fingerprint: String,
    pub algorithm: String,
    pub comment: String,
}

/// The usable keys in `text`, one row each.
#[must_use]
pub fn key_lines(text: &str) -> Vec<KeyLine> {
    text.lines()
        .filter_map(|raw| {
            let line = raw.split_once('#').map_or(raw, |(l, _)| l).trim();
            let key = PublicKey::from_openssh(line).ok()?;
            Some(KeyLine {
                fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
                algorithm: key.algorithm().to_string(),
                comment: key.comment().to_owned(),
            })
        })
        .collect()
}

/// `text` with the lines of the key `fingerprint` removed. Other lines, comments
/// and lines with options stay exactly as they were.
#[must_use]
pub fn without_key(text: &str, fingerprint: &str) -> String {
    let mut out = String::new();
    for raw in text.lines() {
        let line = raw.split_once('#').map_or(raw, |(l, _)| l).trim();
        let hit = PublicKey::from_openssh(line)
            .is_ok_and(|k| k.fingerprint(HashAlg::Sha256).to_string() == fingerprint);
        if !hit {
            out.push_str(raw);
            out.push('\n');
        }
    }
    out
}

/// A line that may be added: one key, no options.
///
/// # Errors
/// With why, for the person who pasted it.
pub fn checked_key_line(line: &str) -> Result<String, String> {
    let line = line.trim();
    if line.contains('\n') || line.is_empty() {
        return Err("paste one public key, on one line".to_owned());
    }
    let key = PublicKey::from_openssh(line).map_err(|_| {
        "that is not a public key (it starts with ssh-ed25519, ecdsa-sha2- or ssh-rsa)".to_owned()
    })?;
    if matches!(
        key.algorithm(),
        ssh_key::Algorithm::SkEd25519 | ssh_key::Algorithm::SkEcdsaSha2NistP256
    ) {
        return Err("hardware (sk-) keys are not supported yet".to_owned());
    }
    Ok(line.to_owned())
}

/// The client's answer to a host's nonce.
///
/// # Errors
/// When the signer fails or the signature cannot be armored.
pub fn client_response(
    signer: &dyn Signer,
    nonce: &[u8; 32],
    client: &EndpointId,
    host: &EndpointId,
    binding: &[u8; 32],
) -> Result<Message, SignError> {
    let payload = auth_payload(nonce, client, host, binding);
    let sig = signer.sign(AUTH_NAMESPACE, HashAlg::Sha512, &payload)?;
    let pem = sig
        .to_pem(LineEnding::LF)
        .map_err(|err| SignError::Sign(err.to_string()))?;
    let public_key = signer
        .public_key()
        .to_bytes()
        .map_err(|err| SignError::Sign(err.to_string()))?;
    Ok(Message::AuthResponse {
        public_key,
        signature: pem.into_bytes(),
    })
}

/// The keys a host lets in, from its Connecto `authorized_keys` file.
#[derive(Debug, Clone, Default)]
pub struct AuthorizedKeySet {
    keys: Vec<PublicKey>,
    /// Lines that carry options. They are not honoured, so they do not let
    /// anyone in: a restriction that is not enforced must not become a grant.
    skipped_with_options: usize,
}

impl AuthorizedKeySet {
    /// Parse OpenSSH `authorized_keys` text.
    ///
    /// # Errors
    /// When a line is not a key.
    pub fn parse(text: &str) -> Result<Self, ssh_key::Error> {
        let mut set = Self::default();
        for raw in text.lines() {
            let line = raw.split_once('#').map_or(raw, |(l, _)| l).trim();
            if line.is_empty() {
                continue;
            }
            // ssh-key's own parser mistakes "options key" for a key when the
            // line has no comment, so a line is tried as a key first and then
            // with a prefix of options cut off at each space.
            match PublicKey::from_openssh(line) {
                Ok(key) => set.keys.push(key),
                Err(err) => {
                    let has_options = line.match_indices(' ').any(|(i, _)| {
                        line.get(i + 1..)
                            .is_some_and(|rest| PublicKey::from_openssh(rest).is_ok())
                    });
                    if !has_options {
                        return Err(err);
                    }
                    set.skipped_with_options += 1;
                }
            }
        }
        Ok(set)
    }

    /// Read the file. A missing file is an empty set: nobody gets in.
    ///
    /// # Errors
    /// When the file exists and cannot be read or parsed.
    pub fn load(path: &Path) -> Result<Self, String> {
        match std::fs::read_to_string(path) {
            Ok(text) => Self::parse(&text).map_err(|err| format!("{}: {err}", path.display())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(Self::default()),
            Err(err) => Err(format!("{}: {err}", path.display())),
        }
    }

    #[must_use]
    pub fn contains(&self, key: &PublicKey) -> bool {
        self.keys.iter().any(|k| k.key_data() == key.key_data())
    }

    /// Whether a key with this `SHA256:` fingerprint is in the set.
    #[must_use]
    pub fn has_fingerprint(&self, fingerprint: &str) -> bool {
        self.keys
            .iter()
            .any(|k| k.fingerprint(HashAlg::Sha256).to_string() == fingerprint)
    }

    #[must_use]
    pub fn len(&self) -> usize {
        self.keys.len()
    }

    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.keys.is_empty()
    }

    #[must_use]
    pub const fn skipped_with_options(&self) -> usize {
        self.skipped_with_options
    }
}

/// A peer that passed the handshake.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AuthedKey {
    /// `SHA256:...`, as `ssh-keygen -l` prints it.
    pub fingerprint: String,
}

#[derive(Debug)]
enum State {
    AwaitHello,
    AwaitResponse { nonce: [u8; 32] },
    Done,
    Failed,
}

/// The host's side of one handshake. One value serves one connection.
pub struct HostAuth {
    host: EndpointId,
    client: EndpointId,
    binding: [u8; 32],
    keys: Arc<AuthorizedKeySet>,
    state: State,
    forced_nonce: Option<[u8; 32]>,
}

impl HostAuth {
    #[must_use]
    pub const fn new(
        host: EndpointId,
        client: EndpointId,
        binding: [u8; 32],
        keys: Arc<AuthorizedKeySet>,
    ) -> Self {
        Self {
            host,
            client,
            binding,
            keys,
            state: State::AwaitHello,
            forced_nonce: None,
        }
    }

    /// Use this nonce instead of a random one. Tests only.
    #[must_use]
    pub const fn with_nonce(mut self, nonce: [u8; 32]) -> Self {
        self.forced_nonce = Some(nonce);
        self
    }

    /// Whether the handshake finished successfully.
    #[must_use]
    pub const fn is_done(&self) -> bool {
        matches!(self.state, State::Done)
    }

    /// The client's first message. Returns the challenge to send.
    ///
    /// # Errors
    /// `Version` for another protocol; `Protocol` out of order.
    pub fn on_hello(&mut self, version: u16) -> Result<Message, DenyReason> {
        if !matches!(self.state, State::AwaitHello) {
            self.state = State::Failed;
            return Err(DenyReason::Protocol);
        }
        if version != PROTOCOL_VERSION {
            self.state = State::Failed;
            return Err(DenyReason::Version);
        }
        let nonce = match self.forced_nonce {
            Some(nonce) => nonce,
            None => {
                let mut nonce = [0_u8; 32];
                if getrandom::fill(&mut nonce).is_err() {
                    self.state = State::Failed;
                    return Err(DenyReason::Protocol);
                }
                nonce
            }
        };
        self.state = State::AwaitResponse { nonce };
        Ok(Message::Challenge { nonce })
    }

    /// The client's signed answer. The nonce is spent whatever the outcome, so
    /// one challenge allows one try.
    ///
    /// # Errors
    /// `Protocol` out of order; `NotAuthorized` for anything else wrong.
    pub fn on_response(
        &mut self,
        public_key: &[u8],
        signature: &[u8],
    ) -> Result<AuthedKey, DenyReason> {
        let State::AwaitResponse { nonce } = std::mem::replace(&mut self.state, State::Failed)
        else {
            return Err(DenyReason::Protocol);
        };
        let key = PublicKey::from_bytes(public_key).map_err(|_| DenyReason::NotAuthorized)?;
        if !self.keys.contains(&key) {
            return Err(DenyReason::NotAuthorized);
        }
        let sig = SshSig::from_pem(signature).map_err(|_| DenyReason::NotAuthorized)?;
        let payload = auth_payload(&nonce, &self.client, &self.host, &self.binding);
        key.verify(AUTH_NAMESPACE, &payload, &sig)
            .map_err(|_| DenyReason::NotAuthorized)?;
        self.state = State::Done;
        Ok(AuthedKey {
            fingerprint: key.fingerprint(HashAlg::Sha256).to_string(),
        })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
pub(crate) mod tests {
    use ssh_key::Algorithm;
    use ssh_key::rand_core::OsRng;

    use super::*;

    pub fn new_key() -> PrivateKey {
        PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap()
    }

    #[test]
    fn a_given_agent_socket_wins() {
        let given = Path::new("given.sock");
        assert_eq!(AgentSigner::socket_path(Some(given)).unwrap(), given);
    }

    /// Without `SSH_AUTH_SOCK`, the Windows OpenSSH agent's pipe; a pipe that
    /// is not there is an error that names the service, not a panic.
    #[cfg(windows)]
    #[test]
    fn on_windows_the_openssh_agent_pipe_is_the_default() {
        if std::env::var_os("SSH_AUTH_SOCK").is_none() {
            assert_eq!(
                AgentSigner::socket_path(None).unwrap(),
                Path::new(WINDOWS_AGENT_PIPE)
            );
        }
        let missing = Path::new(r"\\.\pipe\connecto-no-such-agent");
        assert!(matches!(
            AgentSigner::list(Some(missing)),
            Err(SignError::Agent(_))
        ));
        assert!(agent_hint(Path::new(WINDOWS_AGENT_PIPE)).contains("service"));
    }

    pub fn allow(keys: &[&PrivateKey]) -> Arc<AuthorizedKeySet> {
        let text: String = keys
            .iter()
            .map(|k| format!("{}\n", k.public_key().to_openssh().unwrap()))
            .collect();
        Arc::new(AuthorizedKeySet::parse(&text).unwrap())
    }

    const CLIENT: EndpointId = [1; 32];
    const HOST: EndpointId = [2; 32];
    const BINDING: [u8; 32] = [3; 32];
    const NONCE: [u8; 32] = [4; 32];

    fn host(keys: Arc<AuthorizedKeySet>) -> HostAuth {
        HostAuth::new(HOST, CLIENT, BINDING, keys).with_nonce(NONCE)
    }

    fn response(signer: &dyn Signer, nonce: &[u8; 32], binding: &[u8; 32]) -> (Vec<u8>, Vec<u8>) {
        let Message::AuthResponse {
            public_key,
            signature,
        } = client_response(signer, nonce, &CLIENT, &HOST, binding).unwrap()
        else {
            panic!("not a response");
        };
        (public_key, signature)
    }

    fn challenged(auth: &mut HostAuth) -> [u8; 32] {
        let Message::Challenge { nonce } = auth.on_hello(PROTOCOL_VERSION).unwrap() else {
            panic!("no challenge");
        };
        nonce
    }

    #[test]
    fn an_allowed_key_with_a_good_signature_gets_in() {
        let key = new_key();
        let mut auth = host(allow(&[&key]));
        let nonce = challenged(&mut auth);
        let (pk, sig) = response(&KeySigner(key.clone()), &nonce, &BINDING);
        let authed = auth.on_response(&pk, &sig).unwrap();
        assert!(authed.fingerprint.starts_with("SHA256:"));
        assert!(auth.is_done());
    }

    #[test]
    fn an_unknown_key_is_refused_even_with_a_valid_signature() {
        let allowed = new_key();
        let stranger = new_key();
        let mut auth = host(allow(&[&allowed]));
        let nonce = challenged(&mut auth);
        let (pk, sig) = response(&KeySigner(stranger), &nonce, &BINDING);
        assert_eq!(auth.on_response(&pk, &sig), Err(DenyReason::NotAuthorized));
        assert!(!auth.is_done());
    }

    #[test]
    fn a_signature_in_another_namespace_is_refused() {
        let key = new_key();
        let mut auth = host(allow(&[&key]));
        let nonce = challenged(&mut auth);
        let payload = auth_payload(&nonce, &CLIENT, &HOST, &BINDING);
        // The same key signs the same bytes for git.
        let sig = key.sign("git", HashAlg::Sha512, &payload).unwrap();
        let pem = sig.to_pem(LineEnding::LF).unwrap();
        let pk = key.public_key().to_bytes().unwrap();
        assert_eq!(
            auth.on_response(&pk, pem.as_bytes()),
            Err(DenyReason::NotAuthorized)
        );
    }

    #[test]
    fn a_replayed_response_is_refused_on_a_fresh_connection() {
        let key = new_key();
        let keys = allow(&[&key]);
        let mut first = host(Arc::clone(&keys));
        let nonce = challenged(&mut first);
        let (pk, sig) = response(&KeySigner(key), &nonce, &BINDING);
        first.on_response(&pk, &sig).unwrap();
        // An eavesdropper replays it to a new connection: same ids, but the
        // host has chosen a new nonce.
        let mut second = HostAuth::new(HOST, CLIENT, BINDING, keys).with_nonce([5; 32]);
        challenged(&mut second);
        assert_eq!(
            second.on_response(&pk, &sig),
            Err(DenyReason::NotAuthorized)
        );
    }

    #[test]
    fn a_nonce_is_spent_by_the_first_response() {
        let key = new_key();
        let mut auth = host(allow(&[&key]));
        let nonce = challenged(&mut auth);
        let (pk, sig) = response(&KeySigner(key), &nonce, &BINDING);
        // A first try with garbage spends the challenge.
        assert!(auth.on_response(&pk, b"junk").is_err());
        assert_eq!(auth.on_response(&pk, &sig), Err(DenyReason::Protocol));
        assert!(!auth.is_done());
    }

    #[test]
    fn a_signature_made_for_another_session_is_refused() {
        let key = new_key();
        let signer = KeySigner(key.clone());
        let keys = allow(&[&key]);
        // Different binding (a different TLS session).
        let mut auth = host(Arc::clone(&keys));
        let nonce = challenged(&mut auth);
        let (pk, sig) = response(&signer, &nonce, &[9; 32]);
        assert_eq!(auth.on_response(&pk, &sig), Err(DenyReason::NotAuthorized));
        // Different host id: a signature meant for another machine.
        let mut auth = host(Arc::clone(&keys));
        let nonce = challenged(&mut auth);
        let Message::AuthResponse {
            public_key,
            signature,
        } = client_response(&signer, &nonce, &CLIENT, &[8; 32], &BINDING).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            auth.on_response(&public_key, &signature),
            Err(DenyReason::NotAuthorized)
        );
        // Different client id: a relayed attempt from another endpoint.
        let mut auth = host(keys);
        let nonce = challenged(&mut auth);
        let Message::AuthResponse {
            public_key,
            signature,
        } = client_response(&signer, &nonce, &[7; 32], &HOST, &BINDING).unwrap()
        else {
            panic!()
        };
        assert_eq!(
            auth.on_response(&public_key, &signature),
            Err(DenyReason::NotAuthorized)
        );
    }

    #[test]
    fn messages_out_of_order_and_wrong_versions_fail_the_handshake() {
        let key = new_key();
        let mut auth = host(allow(&[&key]));
        assert_eq!(auth.on_response(b"", b""), Err(DenyReason::Protocol));
        let mut auth = host(allow(&[&key]));
        assert_eq!(
            auth.on_hello(PROTOCOL_VERSION + 1),
            Err(DenyReason::Version)
        );
        assert_eq!(auth.on_hello(PROTOCOL_VERSION), Err(DenyReason::Protocol));
    }

    #[test]
    fn a_version_one_peer_is_refused_by_a_version_two_host() {
        let key = new_key();
        let mut auth = host(allow(&[&key]));
        assert_eq!(auth.on_hello(1), Err(DenyReason::Version));
        assert!(!auth.is_done());
    }

    #[test]
    fn garbage_keys_and_signatures_are_refused_not_panicked_on() {
        let key = new_key();
        let mut auth = host(allow(&[&key]));
        challenged(&mut auth);
        assert_eq!(
            auth.on_response(&[1, 2, 3], &[4, 5]),
            Err(DenyReason::NotAuthorized)
        );
    }

    #[test]
    fn authorized_keys_parse_skips_option_lines_and_ignores_comments() {
        let a = new_key();
        let b = new_key();
        let text = format!(
            "# mine\n{} laptop\n\nno-pty {}\n",
            a.public_key().to_openssh().unwrap(),
            b.public_key().to_openssh().unwrap()
        );
        let set = AuthorizedKeySet::parse(&text).unwrap();
        assert_eq!(set.len(), 1);
        assert_eq!(set.skipped_with_options(), 1);
        assert!(set.contains(a.public_key()));
        assert!(!set.contains(b.public_key()));
        assert!(AuthorizedKeySet::parse("not a key\n").is_err());
    }

    #[test]
    fn a_missing_authorized_keys_file_lets_nobody_in() {
        let set =
            AuthorizedKeySet::load(Path::new("/nonexistent/connecto/authorized_keys")).unwrap();
        assert!(set.is_empty());
    }

    #[test]
    fn key_lines_list_add_check_and_remove() {
        let a = new_key();
        let b = new_key();
        let text = format!(
            "# mine\n{} laptop\nno-pty {}\n",
            a.public_key().to_openssh().unwrap(),
            b.public_key().to_openssh().unwrap()
        );
        let rows = key_lines(&text);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0].comment, "laptop");
        assert_eq!(rows[0].algorithm, "ssh-ed25519");
        let fp = rows[0].fingerprint.clone();
        let left = without_key(&text, &fp);
        assert!(!left.contains("laptop"));
        assert!(left.contains("# mine") && left.contains("no-pty"));
        assert!(checked_key_line("junk").is_err());
        assert!(checked_key_line("a\nb").is_err());
        assert!(checked_key_line(&a.public_key().to_openssh().unwrap()).is_ok());
    }

    #[test]
    fn the_payload_changes_with_every_field() {
        let base = auth_payload(&[1; 32], &[2; 32], &[3; 32], &[4; 32]);
        assert_ne!(base, auth_payload(&[0; 32], &[2; 32], &[3; 32], &[4; 32]));
        assert_ne!(base, auth_payload(&[1; 32], &[0; 32], &[3; 32], &[4; 32]));
        assert_ne!(base, auth_payload(&[1; 32], &[2; 32], &[0; 32], &[4; 32]));
        assert_ne!(base, auth_payload(&[1; 32], &[2; 32], &[3; 32], &[0; 32]));
        // Swapping client and host is a different payload too.
        assert_ne!(base, auth_payload(&[1; 32], &[3; 32], &[2; 32], &[4; 32]));
    }
}
