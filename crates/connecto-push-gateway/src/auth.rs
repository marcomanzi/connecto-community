//! How a host proves a push request is its own: a bearer token, and an
//! SSHSIG by its device key (Ed25519) over the method, the path, a time, a
//! nonce of its own and the body's SHA-256. The gateway accepts a time only within [`WINDOW_SECS`]
//! of its own, and each signature once ([`crate::server`]), so a request
//! seen on the way can be neither altered nor replayed.

use base64::Engine as _;
use sha2::Digest as _;
use ssh_key::{HashAlg, PublicKey, SshSig};

/// The SSHSIG namespace: a signature made for anything else never passes.
pub const NAMESPACE: &str = "connecto-push-gateway";
/// How far a request's time may be from the gateway's.
pub const WINDOW_SECS: u64 = 300;

/// The header naming the host (its entry in the gateway's config).
pub const HOST_HEADER: &str = "x-connecto-host";
/// The header with the request's time, seconds since 1970.
pub const TIME_HEADER: &str = "x-connecto-time";
/// The header with the signature: base64 of the armored SSHSIG.
pub const SIGNATURE_HEADER: &str = "x-connecto-signature";
/// The header with the request's nonce: new for each request, so the same
/// push sent again (a retry) is a new request, never taken for a replay.
pub const NONCE_HEADER: &str = "x-connecto-nonce";
/// The longest nonce.
pub const MAX_NONCE: usize = 64;

/// What is signed for one request.
#[must_use]
pub fn signed_message(method: &str, path: &str, time: u64, nonce: &str, body: &[u8]) -> Vec<u8> {
    let digest: String = sha2::Sha256::digest(body)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect();
    format!("{NAMESPACE}/v1\n{method}\n{path}\n{time}\n{nonce}\n{digest}").into_bytes()
}

/// Whether `nonce` is one a host sends: 16 to [`MAX_NONCE`] letters, digits
/// or dashes.
#[must_use]
pub fn valid_nonce(nonce: &str) -> bool {
    (16..=MAX_NONCE).contains(&nonce.len())
        && nonce.chars().all(|c| c.is_ascii_alphanumeric() || c == '-')
}

/// An armored SSHSIG as the signature header carries it.
#[must_use]
pub fn encode_signature(sig: &SshSig) -> Option<String> {
    let pem = sig.to_pem(ssh_key::LineEnding::LF).ok()?;
    Some(base64::engine::general_purpose::STANDARD.encode(pem))
}

/// One request as it is signed.
#[derive(Debug, Clone, Copy)]
pub struct Signed<'a> {
    pub method: &'a str,
    pub path: &'a str,
    /// Seconds since 1970.
    pub time: u64,
    pub nonce: &'a str,
    pub body: &'a [u8],
}

impl Signed<'_> {
    #[must_use]
    pub fn message(&self) -> Vec<u8> {
        signed_message(self.method, self.path, self.time, self.nonce, self.body)
    }
}

/// Whether `signature` (the header's value) is `key`'s over `request`.
#[must_use]
pub fn verify(key: &PublicKey, request: &Signed<'_>, signature: &str) -> bool {
    let Ok(pem) = base64::engine::general_purpose::STANDARD.decode(signature) else {
        return false;
    };
    let Ok(sig) = SshSig::from_pem(pem) else {
        return false;
    };
    // An Ed25519 device key only, signing with SHA-512 as ssh-keygen does.
    valid_nonce(request.nonce)
        && key.algorithm() == ssh_key::Algorithm::Ed25519
        && sig.hash_alg() == HashAlg::Sha512
        && key.verify(NAMESPACE, &request.message(), &sig).is_ok()
}

/// The SHA-256 of a bearer token, hex: what the gateway's config keeps
/// instead of the token.
#[must_use]
pub fn token_digest(token: &str) -> String {
    sha2::Sha256::digest(token.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use ssh_key::rand_core::OsRng;
    use ssh_key::{Algorithm, PrivateKey};

    const NONCE: &str = "0123456789abcdef";

    fn request(path: &str, time: u64, nonce: &'static str, body: &'static [u8]) -> Signed<'static> {
        Signed {
            method: "POST",
            path: if path == "/v1/push" {
                "/v1/push"
            } else {
                "/v1/other"
            },
            time,
            nonce,
            body,
        }
    }

    #[test]
    fn a_signature_covers_the_method_path_time_nonce_and_body() {
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        let good = request("/v1/push", 7, NONCE, b"{}");
        let header = encode_signature(
            &key.sign(NAMESPACE, HashAlg::Sha512, &good.message())
                .unwrap(),
        )
        .unwrap();
        let public = key.public_key();
        assert!(verify(public, &good, &header));
        assert!(
            !verify(public, &request("/v1/push", 8, NONCE, b"{}"), &header),
            "time"
        );
        assert!(
            !verify(public, &request("/v1/push", 7, NONCE, b"{ }"), &header),
            "body"
        );
        assert!(
            !verify(public, &request("/v1/other", 7, NONCE, b"{}"), &header),
            "path"
        );
        assert!(
            !verify(
                public,
                &request("/v1/push", 7, "0123456789abcdeX", b"{}"),
                &header
            ),
            "nonce"
        );
        let other = PrivateKey::random(&mut OsRng, Algorithm::Ed25519).unwrap();
        assert!(!verify(other.public_key(), &good, &header));
        // Another namespace's signature (the remote handshake's) never passes.
        let elsewhere = key
            .sign("connecto-remote", HashAlg::Sha512, &good.message())
            .unwrap();
        assert!(!verify(
            public,
            &good,
            &encode_signature(&elsewhere).unwrap()
        ));
        // A nonce too short to tell retries apart is refused.
        let short = request("/v1/push", 7, "abc", b"{}");
        let signed = encode_signature(
            &key.sign(NAMESPACE, HashAlg::Sha512, &short.message())
                .unwrap(),
        )
        .unwrap();
        assert!(!verify(public, &short, &signed));
    }
}
