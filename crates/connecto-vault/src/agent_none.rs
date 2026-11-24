//! No ssh-agent where there are no Unix sockets (Windows): SSH keys can be
//! kept and generated, and terminals are told they cannot sign with them here.

use std::path::Path;

use connecto_core::{GeneratedKey, SecretCaller, SshAccess};
use ssh_key::{Algorithm, LineEnding, PrivateKey};

use crate::Vault;

const NOT_HERE: &str = "Signing with an SSH key through Connecto works on macOS and Linux only.";

pub(crate) fn make_private_dir(_dir: &Path) -> Result<(), String> {
    Ok(())
}

pub(crate) fn parse_private(text: &str) -> Result<PrivateKey, String> {
    let key = PrivateKey::from_openssh(text.trim().as_bytes())
        .map_err(|_| "This is not an OpenSSH private key.".to_owned())?;
    if key.is_encrypted() {
        return Err(
            "This key has a passphrase. Give one without, or generate a new one.".to_owned(),
        );
    }
    Ok(key)
}

pub(crate) fn generate(comment: &str) -> Result<GeneratedKey, String> {
    let mut key = PrivateKey::random(&mut ssh_key::rand_core::OsRng, Algorithm::Ed25519)
        .map_err(|err| format!("a key could not be made: {err}"))?;
    key.set_comment(comment.trim());
    Ok(GeneratedKey {
        private_openssh: key
            .to_openssh(LineEnding::LF)
            .map_err(|err| format!("the key could not be written out: {err}"))?
            .to_string(),
        public_openssh: key
            .public_key()
            .to_openssh()
            .map_err(|err| format!("the public key could not be written out: {err}"))?,
    })
}

pub(crate) fn access(
    _vault: &Vault,
    _caller: &SecretCaller,
    _name: &str,
) -> Result<SshAccess, String> {
    Err(NOT_HERE.to_owned())
}

pub(crate) fn close_for(_vault: &Vault, _scope: &connecto_core::ProviderScope, _name: &str) {}
