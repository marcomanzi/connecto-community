//! Vault values at rest: in the same secrets document as Connecto's other
//! secrets, but encrypted, the way IntelliJ keeps a KeePass file with its
//! master key in the system store.
//!
//! Each value is AES-256-GCM under a 32-byte master key, with a fresh nonce,
//! and the value's store key as associated data (a value moved under another
//! name does not open). Stored as `vault:v1:<base64 of nonce and ciphertext>`.
//!
//! The master key lives apart from the document:
//! - macOS: a Keychain item (service `connecto`, account `vault-key`). The
//!   Keychain gives it to Connecto silently and asks the person before any
//!   other program (an agent running `security`) reads it, so an agent that
//!   copies the secrets file gets ciphertext.
//! - Windows: a Credential Manager credential. Any program of the same user
//!   can read those, so this is protection from other users and from copies
//!   of the file, not from an agent.
//! - Linux, and builds with a data folder of their own: a key file beside the
//!   document (0600). It keeps the values out of a plain read of the
//!   document, nothing more.

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};

use aes_gcm::aead::{Aead, KeyInit, Payload};
use aes_gcm::{Aes256Gcm, Nonce};
use connecto_secrets::Items;

const PREFIX: &str = "vault:v1:";
const NONCE: usize = 12;

/// Where the master key is.
pub trait MasterKey: Send + Sync {
    /// The key, made and kept on first use.
    ///
    /// # Errors
    /// When it cannot be read or kept (the person denied the Keychain, say).
    fn key(&self) -> Result<[u8; 32], String>;

    /// Where it is, and what that protects against, in words for Settings.
    fn describe(&self) -> String;
}

/// The key in a system store item.
pub struct StoreKey {
    items: Arc<dyn Items>,
    account: String,
    words: String,
    cached: Mutex<Option<[u8; 32]>>,
}

impl StoreKey {
    /// The item `connecto`/`account` of `items`; `words` says what it is.
    #[must_use]
    pub fn new(items: Arc<dyn Items>, account: &str, words: &str) -> Self {
        Self {
            items,
            account: account.to_owned(),
            words: words.to_owned(),
            cached: Mutex::new(None),
        }
    }
}

/// The account of the master key's item.
pub const KEY_ACCOUNT: &str = "vault-key";

impl MasterKey for StoreKey {
    fn key(&self) -> Result<[u8; 32], String> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = *cached {
            return Ok(key);
        }
        let key = match self.items.read(connecto_secrets::SERVICE, &self.account)? {
            Some(bytes) => parse_key(&bytes)?,
            None => {
                let key = new_key()?;
                self.items.write(
                    connecto_secrets::SERVICE,
                    &self.account,
                    connecto_secrets::b64_encode(&key).as_bytes(),
                )?;
                key
            }
        };
        *cached = Some(key);
        Ok(key)
    }

    fn describe(&self) -> String {
        self.words.clone()
    }
}

/// The key in a file of its own (0600).
pub struct FileKey {
    path: PathBuf,
    cached: Mutex<Option<[u8; 32]>>,
}

impl FileKey {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self {
            path,
            cached: Mutex::new(None),
        }
    }
}

impl MasterKey for FileKey {
    fn key(&self) -> Result<[u8; 32], String> {
        let mut cached = self.cached.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(key) = *cached {
            return Ok(key);
        }
        let key = match std::fs::read(&self.path) {
            Ok(bytes) => parse_key(&bytes)?,
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {
                let key = new_key()?;
                connecto_secrets::write_private(
                    &self.path,
                    connecto_secrets::b64_encode(&key).as_bytes(),
                )
                .map_err(|err| format!("the vault key could not be written: {err}"))?;
                key
            }
            Err(err) => return Err(format!("the vault key could not be read: {err}")),
        };
        *cached = Some(key);
        Ok(key)
    }

    fn describe(&self) -> String {
        "Values are encrypted; the key is in a file beside them, so a program running as you can still read them.".to_owned()
    }
}

/// A key given outright, for tests.
pub struct FixedKey(pub [u8; 32]);

impl MasterKey for FixedKey {
    fn key(&self) -> Result<[u8; 32], String> {
        Ok(self.0)
    }

    fn describe(&self) -> String {
        "a fixed key (tests)".to_owned()
    }
}

fn parse_key(bytes: &[u8]) -> Result<[u8; 32], String> {
    let text = String::from_utf8_lossy(bytes);
    connecto_secrets::b64_decode(text.trim())
        .and_then(|raw| <[u8; 32]>::try_from(raw.as_slice()).ok())
        .ok_or_else(|| "the vault key is damaged; it was left as it is".to_owned())
}

fn new_key() -> Result<[u8; 32], String> {
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key).map_err(|err| format!("no random bytes: {err}"))?;
    Ok(key)
}

/// `value`, encrypted for the store key `aad`.
///
/// # Errors
/// When the key cannot be had or there are no random bytes.
pub fn seal(master: &dyn MasterKey, aad: &str, value: &str) -> Result<String, String> {
    let key = master.key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "a bad vault key".to_owned())?;
    let mut nonce = [0_u8; NONCE];
    getrandom::fill(&mut nonce).map_err(|err| format!("no random bytes: {err}"))?;
    let sealed = cipher
        .encrypt(
            Nonce::from_slice(&nonce),
            Payload {
                msg: value.as_bytes(),
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| "the value could not be encrypted".to_owned())?;
    let mut out = nonce.to_vec();
    out.extend_from_slice(&sealed);
    Ok(format!("{PREFIX}{}", connecto_secrets::b64_encode(&out)))
}

/// The value `seal` made for `aad`.
///
/// # Errors
/// When it is not sealed, the key is wrong, or it was made for another name.
pub fn open(master: &dyn MasterKey, aad: &str, stored: &str) -> Result<String, String> {
    let body = stored
        .strip_prefix(PREFIX)
        .ok_or("the stored value is not encrypted")?;
    let raw = connecto_secrets::b64_decode(body).ok_or("the stored value is damaged")?;
    if raw.len() <= NONCE {
        return Err("the stored value is damaged".to_owned());
    }
    let (nonce, sealed) = raw.split_at(NONCE);
    let key = master.key()?;
    let cipher = Aes256Gcm::new_from_slice(&key).map_err(|_| "a bad vault key".to_owned())?;
    let plain = cipher
        .decrypt(
            Nonce::from_slice(nonce),
            Payload {
                msg: sealed,
                aad: aad.as_bytes(),
            },
        )
        .map_err(|_| "the stored value does not open with this vault key".to_owned())?;
    String::from_utf8(plain).map_err(|_| "the stored value is not text".to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_sealed_value_opens_only_with_its_key_and_name() {
        let key = FixedKey([7; 32]);
        let sealed = seal(&key, "user.global.A", "the-value-123").unwrap();
        assert!(!sealed.contains("the-value"));
        assert_eq!(
            open(&key, "user.global.A", &sealed).unwrap(),
            "the-value-123"
        );
        assert!(open(&key, "user.global.B", &sealed).is_err());
        assert!(open(&FixedKey([8; 32]), "user.global.A", &sealed).is_err());
        assert!(open(&key, "user.global.A", "plain").is_err());
    }

    #[test]
    fn each_seal_uses_a_new_nonce() {
        let key = FixedKey([7; 32]);
        assert_ne!(
            seal(&key, "a", "same-value").unwrap(),
            seal(&key, "a", "same-value").unwrap()
        );
    }

    #[test]
    fn a_file_key_is_made_once_and_kept_private() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("secrets").join("vault.key");
        let first = FileKey::new(path.clone()).key().unwrap();
        let again = FileKey::new(path.clone()).key().unwrap();
        assert_eq!(first, again);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(&path).unwrap().permissions().mode();
            assert_eq!(mode & 0o777, 0o600);
        }
    }

    #[test]
    fn a_store_key_is_made_once_in_the_store() {
        let items = Arc::new(connecto_secrets::MemoryItems::default());
        let first = StoreKey::new(Arc::clone(&items) as _, KEY_ACCOUNT, "test")
            .key()
            .unwrap();
        let again = StoreKey::new(Arc::clone(&items) as _, KEY_ACCOUNT, "test")
            .key()
            .unwrap();
        assert_eq!(first, again);
        assert_eq!(items.writes_of(connecto_secrets::SERVICE, KEY_ACCOUNT), 1);
    }
}
