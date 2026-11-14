//! Where the device key and the relay token live.
//!
//! The Keychain (one shared item), as the Jev key does (`connecto-automation`): neither is ever a
//! plain file in the data folder, a log line or a view. A file store (0600) is
//! here for tests and for a machine without a login Keychain.

use std::path::PathBuf;
use std::sync::Mutex;

use crate::trust::write_private;

/// The 32-byte Ed25519 secret of this machine's iroh endpoint.
pub const DEVICE_KEY: &str = "device-key";
/// The shared token the self-hosted relay asks for.
pub const RELAY_TOKEN: &str = "relay-token";

/// Named secrets.
pub trait SecretStore: Send + Sync {
    /// # Errors
    /// When the store cannot be read.
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String>;
    /// # Errors
    /// When the store cannot be written.
    fn set(&self, name: &str, value: &[u8]) -> Result<(), String>;
    /// Removing what is not there is not an error.
    ///
    /// # Errors
    /// When the store cannot be written.
    fn clear(&self, name: &str) -> Result<(), String>;
}

/// Entries of Connecto's single secrets item (`connecto_secrets`), under the
/// `remote.` names.
pub struct SharedSecrets(pub std::sync::Arc<connecto_secrets::Secrets>);

/// The process's secret store, through its shared secrets document: the
/// secrets file in the data folder, or the system store (the macOS Keychain,
/// Windows Credential Manager, whose item has a size limit) when the person
/// chose it.
pub struct StoredSecrets;

/// The key of a secret in the shared store, and whether it is binary.
fn shared_key(name: &str) -> (String, bool) {
    (format!("remote.{name}"), name == DEVICE_KEY)
}

impl SecretStore for SharedSecrets {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let (key, binary) = shared_key(name);
        if binary {
            self.0.get_bytes(&key)
        } else {
            Ok(self.0.get(&key)?.map(String::into_bytes))
        }
    }

    fn set(&self, name: &str, value: &[u8]) -> Result<(), String> {
        let (key, binary) = shared_key(name);
        if binary {
            self.0.set_bytes(&key, value)
        } else {
            let text =
                std::str::from_utf8(value).map_err(|_| format!("the secret {name} is not text"))?;
            self.0.set(&key, text)
        }
    }

    fn clear(&self, name: &str) -> Result<(), String> {
        self.0.delete(&shared_key(name).0)
    }
}

impl SecretStore for StoredSecrets {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        SharedSecrets(connecto_secrets::Secrets::system()).get(name)
    }

    fn set(&self, name: &str, value: &[u8]) -> Result<(), String> {
        SharedSecrets(connecto_secrets::Secrets::system()).set(name, value)
    }

    fn clear(&self, name: &str) -> Result<(), String> {
        SharedSecrets(connecto_secrets::Secrets::system()).clear(name)
    }
}

/// Files in one folder, each 0600.
pub struct FileSecrets(pub PathBuf);

impl SecretStore for FileSecrets {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        match std::fs::read(self.0.join(name)) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }

    fn set(&self, name: &str, value: &[u8]) -> Result<(), String> {
        write_private(&self.0.join(name), value).map_err(|err| err.to_string())
    }

    fn clear(&self, name: &str) -> Result<(), String> {
        match std::fs::remove_file(self.0.join(name)) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }
}

/// Secrets in memory, for tests.
#[derive(Default)]
pub struct MemorySecrets(Mutex<Vec<(String, Vec<u8>)>>);

impl SecretStore for MemorySecrets {
    fn get(&self, name: &str) -> Result<Option<Vec<u8>>, String> {
        let all = self.0.lock().map_err(|_| "poisoned".to_owned())?;
        Ok(all.iter().find(|(n, _)| n == name).map(|(_, v)| v.clone()))
    }

    fn set(&self, name: &str, value: &[u8]) -> Result<(), String> {
        let mut all = self.0.lock().map_err(|_| "poisoned".to_owned())?;
        all.retain(|(n, _)| n != name);
        all.push((name.to_owned(), value.to_vec()));
        Ok(())
    }

    fn clear(&self, name: &str) -> Result<(), String> {
        let mut all = self.0.lock().map_err(|_| "poisoned".to_owned())?;
        all.retain(|(n, _)| n != name);
        Ok(())
    }
}

/// This machine's endpoint secret, made on first use. A stored value that is
/// not 32 bytes is an error, never silently replaced: replacing it would
/// change the machine's identity and break every pin that names it.
///
/// # Errors
/// When the store fails or holds something that is not a key.
pub fn device_key(store: &dyn SecretStore) -> Result<[u8; 32], String> {
    if let Some(bytes) = store.get(DEVICE_KEY)? {
        return <[u8; 32]>::try_from(bytes.as_slice())
            .map_err(|_| "the stored device key is not 32 bytes".to_owned());
    }
    let mut key = [0_u8; 32];
    getrandom::fill(&mut key).map_err(|err| err.to_string())?;
    store.set(DEVICE_KEY, &key)?;
    Ok(key)
}

/// The SSH key this machine pairs with. It is made on first use, in the secret
/// store, so there is nothing to copy and no ssh-agent needed.
pub const DEVICE_SSH_KEY: &str = "device-ssh-key";

/// This machine's dedicated SSH key, made if it is not there.
///
/// # Errors
/// When the store fails or holds something that is not a key.
pub fn device_ssh_key(store: &dyn SecretStore) -> Result<ssh_key::PrivateKey, String> {
    if let Some(bytes) = existing_device_ssh_key(store)? {
        return Ok(bytes);
    }
    let key =
        ssh_key::PrivateKey::random(&mut ssh_key::rand_core::OsRng, ssh_key::Algorithm::Ed25519)
            .map_err(|e| e.to_string())?;
    let pem = key
        .to_openssh(ssh_key::LineEnding::LF)
        .map_err(|e| e.to_string())?;
    store.set(DEVICE_SSH_KEY, pem.as_bytes())?;
    Ok(key)
}

/// The key, only if it has been made. Reading never makes one.
///
/// # Errors
/// When the store fails or holds something that is not a key.
pub fn existing_device_ssh_key(
    store: &dyn SecretStore,
) -> Result<Option<ssh_key::PrivateKey>, String> {
    store
        .get(DEVICE_SSH_KEY)?
        .map(|bytes| {
            ssh_key::PrivateKey::from_openssh(&bytes)
                .map_err(|_| "the stored device SSH key is not a key".to_owned())
        })
        .transpose()
}

/// The name of this machine's key for one paired machine, in the secret store.
#[must_use]
pub fn pair_key_name(id: &[u8; 32]) -> String {
    format!("pair.{}", crate::trust::hex(id))
}

/// This machine's key for the machine `id`, if pairing made one. Reading never
/// makes a key.
///
/// # Errors
/// When the store fails or holds something that is not a key.
pub fn existing_pair_key(
    store: &dyn SecretStore,
    id: &[u8; 32],
) -> Result<Option<ssh_key::PrivateKey>, String> {
    store
        .get(&pair_key_name(id))?
        .map(|bytes| {
            ssh_key::PrivateKey::from_openssh(&bytes)
                .map_err(|_| "the stored key for this machine is not a key".to_owned())
        })
        .transpose()
}

/// The relay token, if one is set.
///
/// # Errors
/// When the store fails or the token is not text.
pub fn relay_token(store: &dyn SecretStore) -> Result<Option<String>, String> {
    store
        .get(RELAY_TOKEN)?
        .map(|b| String::from_utf8(b).map_err(|_| "the relay token is not text".to_owned()))
        .transpose()
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn the_device_key_is_made_once_and_then_kept() {
        let store = MemorySecrets::default();
        let first = device_key(&store).unwrap();
        assert_eq!(device_key(&store).unwrap(), first);
        assert_ne!(first, [0; 32]);
    }

    #[test]
    fn the_device_ssh_key_is_made_once_kept_and_never_made_by_a_read() {
        let store = MemorySecrets::default();
        assert!(existing_device_ssh_key(&store).unwrap().is_none());
        assert!(
            store.get(DEVICE_SSH_KEY).unwrap().is_none(),
            "a read made no key"
        );
        let first = device_ssh_key(&store).unwrap();
        let second = device_ssh_key(&store).unwrap();
        assert_eq!(first.public_key(), second.public_key());
        assert_eq!(
            existing_device_ssh_key(&store)
                .unwrap()
                .unwrap()
                .public_key(),
            first.public_key()
        );
        store.set(DEVICE_SSH_KEY, b"not a key").unwrap();
        assert!(
            device_ssh_key(&store).is_err(),
            "a damaged key is not replaced"
        );
    }

    #[test]
    fn a_device_key_in_a_store_that_cannot_be_read_is_never_replaced() {
        use connecto_secrets::{MemoryItems, Secrets};
        let items = std::sync::Arc::new(MemoryItems::default());
        let doc = br#"{"version":1,"secrets":{"remote.device-key":"BQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQUFBQU="}}"#;
        items.put("connecto", "secrets", doc);
        items.deny("connecto", "secrets", true);
        let store = SharedSecrets(std::sync::Arc::new(Secrets::new(items.clone(), None)));
        assert!(device_key(&store).is_err());
        assert!(device_ssh_key(&store).is_err());
        assert!(existing_device_ssh_key(&store).is_err());
        // Nothing was generated or stored, and the document is untouched.
        assert_eq!(items.peek("connecto", "secrets").unwrap(), doc);
        // Readable at the next launch, the same identity is there.
        items.deny("connecto", "secrets", false);
        let next = SharedSecrets(std::sync::Arc::new(Secrets::new(items, None)));
        assert_eq!(device_key(&next).unwrap(), [5; 32]);
    }

    #[test]
    fn a_damaged_device_key_is_an_error_not_a_new_identity() {
        let store = MemorySecrets::default();
        store.set(DEVICE_KEY, b"short").unwrap();
        assert!(device_key(&store).is_err());
        assert_eq!(store.get(DEVICE_KEY).unwrap().unwrap(), b"short");
    }

    #[test]
    fn file_secrets_are_owner_only_and_clearable() {
        let dir = std::env::temp_dir().join(format!("mg-net-secrets-{}", std::process::id()));
        let store = FileSecrets(dir.clone());
        store.set(RELAY_TOKEN, b"tok").unwrap();
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join(RELAY_TOKEN))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }
        assert_eq!(relay_token(&store).unwrap().as_deref(), Some("tok"));
        store.clear(RELAY_TOKEN).unwrap();
        assert_eq!(relay_token(&store).unwrap(), None);
        store.clear(RELAY_TOKEN).unwrap();
        std::fs::remove_dir_all(dir).unwrap();
    }
}
