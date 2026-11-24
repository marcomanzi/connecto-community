//! An ssh-agent for one terminal and one key: the terminal signs with the key
//! and gets its public half, never the key.
//!
//! `connecto secret ssh NAME` gives the terminal a socket in a private folder
//! (0700). A connection is served only when its process belongs to the
//! terminal the socket was made for (the peer pid, walked up to the
//! terminal's root). It speaks the agent protocol's two requests that `ssh`
//! needs: list identities (11) and sign (13). Anything else is refused, so
//! nothing can add, remove or export keys through it. Every signature is
//! written to the vault's log of uses. The key is read from the vault for
//! each request, so a replaced or removed key takes effect at once.

use std::collections::HashMap;
use std::io::{self, Read, Write};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::Path;
use std::sync::{PoisonError, Weak};
use std::thread;
use std::time::Duration;

use connecto_core::{Binding, GeneratedKey, ProviderScope, SecretCaller, SshAccess};
use ssh_encoding::Encode as _;
use ssh_key::{Algorithm, LineEnding, PrivateKey};

use crate::Vault;

const REQUEST_IDENTITIES: u8 = 11;
const IDENTITIES_ANSWER: u8 = 12;
const SIGN_REQUEST: u8 = 13;
const SIGN_RESPONSE: u8 = 14;
const FAILURE: u8 = 5;
/// Longest message: a sign request carries a session hash and a little more.
const MAX_MESSAGE: usize = 256 * 1024;
/// A connection idle this long is closed.
const IDLE: Duration = Duration::from_secs(120);

/// Make the private folder of sockets, or check the one that is there.
///
/// # Errors
/// When it cannot be made, or is not a folder only this user can enter.
pub(crate) fn make_private_dir(dir: &Path) -> Result<(), String> {
    use std::os::unix::fs::{MetadataExt, PermissionsExt};
    std::fs::create_dir_all(dir)
        .map_err(|err| format!("{} could not be made: {err}", dir.display()))?;
    std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))
        .map_err(|err| format!("{} could not be made private: {err}", dir.display()))?;
    let meta = std::fs::symlink_metadata(dir)
        .map_err(|err| format!("{} could not be checked: {err}", dir.display()))?;
    if !meta.is_dir() || meta.uid() != nix::unistd::getuid().as_raw() {
        return Err(format!("{} is not a private folder", dir.display()));
    }
    Ok(())
}

/// A pasted or stored key, checked.
///
/// # Errors
/// When it is not an OpenSSH private key this agent can use.
pub(crate) fn parse_private(text: &str) -> Result<PrivateKey, String> {
    let key = PrivateKey::from_openssh(text.trim().as_bytes()).map_err(|_| {
        concat!(
            "This is not an OpenSSH private key (it begins with -----BEGIN OPENSSH ",
            "PRIVATE KEY-----)."
        )
        .to_owned()
    })?;
    if key.is_encrypted() {
        return Err(
            "This key has a passphrase. Give one without (ssh-keygen -p), or generate a new one."
                .to_owned(),
        );
    }
    Ok(key)
}

/// A new Ed25519 key pair.
///
/// # Errors
/// When there are no random bytes or it cannot be written out.
pub(crate) fn generate(comment: &str) -> Result<GeneratedKey, String> {
    let mut key = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)
        .map_err(|err| format!("a key could not be made: {err}"))?;
    let comment: String = comment
        .chars()
        .filter(|ch| ch.is_ascii_graphic() || *ch == ' ')
        .take(80)
        .collect();
    key.set_comment(comment.trim());
    let private_openssh = key
        .to_openssh(LineEnding::LF)
        .map_err(|err| format!("the key could not be written out: {err}"))?
        .to_string();
    let public_openssh = key
        .public_key()
        .to_openssh()
        .map_err(|err| format!("the public key could not be written out: {err}"))?;
    Ok(GeneratedKey {
        private_openssh,
        public_openssh,
    })
}

fn is_ssh(binding: &Binding) -> Option<()> {
    matches!(binding, Binding::Ssh).then_some(())
}

/// The socket and public key `caller` uses `name` with.
///
/// # Errors
/// When there is no such key for this terminal, or no socket can be made.
pub(crate) fn access(
    vault: &Vault,
    caller: &SecretCaller,
    name: &str,
) -> Result<SshAccess, String> {
    let (entry, (), value) = vault.use_secret(caller, name, "SSH", is_ssh)?;
    let key = parse_private(&value)?;
    let public_openssh = key
        .public_key()
        .to_openssh()
        .map_err(|err| format!("the public key could not be written out: {err}"))?;
    let token = vault.token_for(caller)?;
    let pair = (token, name.to_owned());
    let mut agents = vault.agents.lock().unwrap_or_else(PoisonError::into_inner);
    if let Some(socket) = agents.get(&pair)
        && socket.exists()
    {
        vault.log_use(caller, &entry, "ssh", "socket");
        return Ok(SshAccess {
            socket: socket.clone(),
            public_openssh,
        });
    }
    let dir = vault.brokers()?.agent_dir.clone();
    let mut raw = [0_u8; 8];
    getrandom::fill(&mut raw).map_err(|err| format!("no random bytes: {err}"))?;
    let file: String = raw.iter().map(|b| format!("{b:02x}")).collect();
    let socket = dir.join(format!("{file}.sock"));
    let listener = UnixListener::bind(&socket)
        .map_err(|err| format!("the ssh-agent socket could not be made: {err}"))?;
    let me = vault.me();
    let serve_caller = caller.clone();
    let serve_pair = pair.clone();
    thread::Builder::new()
        .name("vault-ssh-agent".into())
        .spawn(move || listen(&listener, &me, &serve_caller, &serve_pair))
        .map_err(|err| format!("the ssh-agent could not start: {err}"))?;
    agents.insert(pair, socket.clone());
    vault.log_use(caller, &entry, "ssh", "socket");
    Ok(SshAccess {
        socket,
        public_openssh,
    })
}

/// Close every socket of `name` (it was removed or replaced).
/// Only the sockets of terminals that used that secret: a project's, or
/// the app-wide one for terminals whose project has none of that name. Call
/// it after the secret is gone from the list.
pub(crate) fn close_for(vault: &Vault, scope: &ProviderScope, name: &str) {
    let entries = connecto_core::VaultControl::view(vault).entries.clone();
    let projects: HashMap<String, Option<String>> = vault
        .tokens
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .iter()
        .map(|(token, caller)| (token.clone(), caller.project.clone()))
        .collect();
    let used_it = |token: &str| match (projects.get(token), scope) {
        // A terminal this vault no longer knows: closed, to be sure.
        (None, _) => true,
        (Some(project), ProviderScope::Project(id)) => project.as_deref() == Some(id.as_str()),
        (Some(project), ProviderScope::Global) => project.as_ref().is_none_or(|p| {
            !entries
                .iter()
                .any(|e| e.name == name && e.scope.project() == Some(p.as_str()))
        }),
    };
    let mut agents = vault.agents.lock().unwrap_or_else(PoisonError::into_inner);
    let gone: Vec<(String, String)> = agents
        .keys()
        .filter(|(token, n)| n == name && used_it(token))
        .cloned()
        .collect();
    for pair in gone {
        if let Some(socket) = agents.remove(&pair) {
            let _ = std::fs::remove_file(&socket);
            // Wakes its accept, which then sees it is no longer wanted.
            let _ = UnixStream::connect(&socket);
        }
    }
}

fn still_wanted(vault: &Vault, pair: &(String, String)) -> bool {
    vault
        .agents
        .lock()
        .unwrap_or_else(PoisonError::into_inner)
        .contains_key(pair)
}

fn listen(
    listener: &UnixListener,
    vault: &Weak<Vault>,
    caller: &SecretCaller,
    pair: &(String, String),
) {
    for stream in listener.incoming() {
        let Some(strong) = vault.upgrade() else {
            return;
        };
        if !still_wanted(&strong, pair) {
            return;
        }
        let Ok(stream) = stream else { continue };
        let Ok(pid) = peer_pid(&stream) else { continue };
        if !(caller.owns_pid)(pid) {
            // Not the terminal this socket is for: closed without a word.
            continue;
        }
        let vault = Weak::clone(vault);
        let caller = caller.clone();
        let name = pair.1.clone();
        let _ = thread::Builder::new()
            .name("vault-ssh-conn".into())
            .spawn(move || {
                let _ = serve(stream, &vault, &caller, &name);
            });
    }
}

fn peer_pid(stream: &UnixStream) -> io::Result<i32> {
    use nix::sys::socket::{getsockopt, sockopt};
    #[cfg(target_os = "linux")]
    let pid = getsockopt(stream, sockopt::PeerCredentials).map(|peer| peer.pid());
    #[cfg(not(target_os = "linux"))]
    let pid = getsockopt(stream, sockopt::LocalPeerPid);
    pid.map_err(io::Error::from)
}

fn read_message(stream: &mut UnixStream) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0_u8; 4];
    match stream.read_exact(&mut len) {
        Ok(()) => {}
        Err(err) if err.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(err) => return Err(err),
    }
    let len = u32::from_be_bytes(len) as usize;
    if len == 0 || len > MAX_MESSAGE {
        return Err(io::Error::other("bad length"));
    }
    let mut body = vec![0_u8; len];
    stream.read_exact(&mut body)?;
    Ok(Some(body))
}

fn write_message(stream: &mut UnixStream, body: &[u8]) -> io::Result<()> {
    let len = u32::try_from(body.len()).map_err(io::Error::other)?;
    stream.write_all(&len.to_be_bytes())?;
    stream.write_all(body)
}

fn put_string(out: &mut Vec<u8>, bytes: &[u8]) {
    out.extend_from_slice(&u32::try_from(bytes.len()).unwrap_or(0).to_be_bytes());
    out.extend_from_slice(bytes);
}

/// A length-prefixed string at `at`, and where the next field begins.
fn take_string(body: &[u8], at: usize) -> Option<(&[u8], usize)> {
    let len = body.get(at..at + 4)?;
    let len = u32::from_be_bytes([len[0], len[1], len[2], len[3]]) as usize;
    let start = at + 4;
    let bytes = body.get(start..start.checked_add(len)?)?;
    Some((bytes, start + len))
}

fn serve(
    mut stream: UnixStream,
    vault: &Weak<Vault>,
    caller: &SecretCaller,
    name: &str,
) -> io::Result<()> {
    stream.set_read_timeout(Some(IDLE))?;
    while let Some(body) = read_message(&mut stream)? {
        let Some(strong) = vault.upgrade() else {
            return Ok(());
        };
        let answer = answer(&strong, caller, name, &body).unwrap_or_else(|| vec![FAILURE]);
        write_message(&mut stream, &answer)?;
    }
    Ok(())
}

/// The reply to one request, `None` for a failure.
fn answer(vault: &Vault, caller: &SecretCaller, name: &str, body: &[u8]) -> Option<Vec<u8>> {
    let kind = *body.first()?;
    if kind != REQUEST_IDENTITIES && kind != SIGN_REQUEST {
        return None;
    }
    let (entry, (), value) = vault.use_secret(caller, name, "SSH", is_ssh).ok()?;
    let key = parse_private(&value).ok()?;
    let blob = key.public_key().to_bytes().ok()?;
    match kind {
        REQUEST_IDENTITIES => {
            let mut out = vec![IDENTITIES_ANSWER];
            out.extend_from_slice(&1_u32.to_be_bytes());
            put_string(&mut out, &blob);
            put_string(&mut out, name.as_bytes());
            Some(out)
        }
        _ => {
            let (asked, next) = take_string(body, 1)?;
            let (data, _) = take_string(body, next)?;
            if asked != blob.as_slice() {
                return None;
            }
            let signature: ssh_key::Signature = signature::Signer::try_sign(&key, data).ok()?;
            let mut encoded = Vec::new();
            signature.encode(&mut encoded).ok()?;
            vault.log_use(caller, &entry, "ssh", "sign");
            let mut out = vec![SIGN_RESPONSE];
            put_string(&mut out, &encoded);
            Some(out)
        }
    }
}

impl Vault {
    /// A weak handle to this vault, for threads that must not keep it alive.
    pub(crate) fn me(&self) -> Weak<Self> {
        self.me.get().cloned().unwrap_or_default()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_generated_key_parses_and_its_public_half_matches() {
        let made = generate("connecto test").unwrap();
        assert!(
            made.private_openssh
                .starts_with("-----BEGIN OPENSSH PRIVATE KEY-----")
        );
        assert!(made.public_openssh.starts_with("ssh-ed25519 "));
        assert!(made.public_openssh.ends_with("connecto test"));
        let key = parse_private(&made.private_openssh).unwrap();
        assert_eq!(key.public_key().to_openssh().unwrap(), made.public_openssh);
    }

    #[test]
    fn text_that_is_not_a_key_is_refused_without_echoing_it() {
        let why = parse_private("hunter2-not-a-key").unwrap_err();
        assert!(!why.contains("hunter2"));
    }

    #[test]
    fn strings_are_read_with_their_bounds_checked() {
        let mut body = vec![SIGN_REQUEST];
        put_string(&mut body, b"abc");
        put_string(&mut body, b"data");
        let (first, next) = take_string(&body, 1).unwrap();
        assert_eq!(first, b"abc");
        assert_eq!(take_string(&body, next).unwrap().0, b"data");
        assert!(take_string(&body[..6], 1).is_none());
        assert!(take_string(&[0, 0xff, 0xff, 0xff, 0xff], 1).is_none());
    }
}
