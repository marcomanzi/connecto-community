//! The Jev API key: the macOS Keychain (one item for all secrets; the
//! Credential Manager on Windows), nowhere else. The key never goes to
//! disk, a log or a view.

use std::sync::Mutex;
use std::sync::PoisonError;

pub trait KeyStore: Send + Sync {
    /// `None` when no key is stored.
    fn get(&self) -> Result<Option<String>, String>;
    fn set(&self, key: &str) -> Result<(), String>;
    /// Clearing a key that is not there is not an error.
    fn clear(&self) -> Result<(), String>;
}

/// The key as one entry of Connecto's secrets document (`connecto_secrets`:
/// the secrets file, or the system store when the person chose it), read once
/// per launch together with the other secrets.
pub struct StoredKeys;

impl KeyStore for StoredKeys {
    fn get(&self) -> Result<Option<String>, String> {
        connecto_secrets::Secrets::system().get(connecto_secrets::JEV_API_KEY)
    }

    fn set(&self, key: &str) -> Result<(), String> {
        connecto_secrets::Secrets::system().set(connecto_secrets::JEV_API_KEY, key)
    }

    fn clear(&self) -> Result<(), String> {
        connecto_secrets::Secrets::system().delete(connecto_secrets::JEV_API_KEY)
    }
}

/// The key in a file (mode 0600), for a build run with a data folder of its own
/// (`CONNECTO_DATA_DIR`): it never touches the Keychain, so it cannot prompt
/// for or read the installed app's entry. Nothing is logged or shown.
pub struct FileKeys(pub std::path::PathBuf);

impl KeyStore for FileKeys {
    fn get(&self) -> Result<Option<String>, String> {
        match std::fs::read_to_string(&self.0) {
            Ok(text) => Ok(Some(text.trim().to_owned()).filter(|key| !key.is_empty())),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err.to_string()),
        }
    }

    fn set(&self, key: &str) -> Result<(), String> {
        use std::io::Write as _;
        if let Some(parent) = self.0.parent() {
            std::fs::create_dir_all(parent).map_err(|err| err.to_string())?;
        }
        let mut options = std::fs::OpenOptions::new();
        options.write(true).create(true).truncate(true);
        private_file(&mut options);
        let mut file = options.open(&self.0).map_err(|err| err.to_string())?;
        file.write_all(key.as_bytes())
            .map_err(|err| err.to_string())
    }

    fn clear(&self) -> Result<(), String> {
        match std::fs::remove_file(&self.0) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(err.to_string()),
        }
    }
}

/// A new key file is readable by its owner only: mode 0600.
#[cfg(unix)]
fn private_file(options: &mut std::fs::OpenOptions) {
    use std::os::unix::fs::OpenOptionsExt as _;
    options.mode(0o600);
}

/// Windows has no modes: a file under the user's profile (the data folder,
/// `%APPDATA%\connecto`) is private to the user by the folder's inherited ACL,
/// so nothing is set here.
#[cfg(windows)]
fn private_file(_options: &mut std::fs::OpenOptions) {}

/// A key held in memory, for tests.
#[derive(Default)]
pub struct MemoryKeys(Mutex<Option<String>>);

impl MemoryKeys {
    #[must_use]
    pub fn with(key: &str) -> Self {
        Self(Mutex::new(Some(key.to_owned())))
    }
}

impl KeyStore for MemoryKeys {
    fn get(&self) -> Result<Option<String>, String> {
        Ok(self
            .0
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone())
    }
    fn set(&self, key: &str) -> Result<(), String> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = Some(key.to_owned());
        Ok(())
    }
    fn clear(&self) -> Result<(), String> {
        *self.0.lock().unwrap_or_else(PoisonError::into_inner) = None;
        Ok(())
    }
}

/// A key as the user pasted it: trimmed, one line, no spaces.
pub fn clean_key(key: &str) -> Result<String, String> {
    let key = key.trim();
    if key.is_empty() {
        return Err("the key is empty".to_owned());
    }
    if key.len() > 512 || key.chars().any(|c| c.is_whitespace() || c.is_control()) {
        return Err("that does not look like an API key".to_owned());
    }
    Ok(key.to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_trimmed_and_odd_ones_refused() {
        assert_eq!(clean_key("  abc123\n").as_deref(), Ok("abc123"));
        assert!(clean_key("").is_err());
        assert!(clean_key("a b").is_err());
    }

    #[test]
    fn the_memory_store_round_trips() {
        let store = MemoryKeys::default();
        assert_eq!(store.get(), Ok(None));
        store.set("k").unwrap_or_default();
        assert_eq!(store.get(), Ok(Some("k".to_owned())));
        store.clear().unwrap_or_default();
        assert_eq!(store.get(), Ok(None));
    }

    #[test]
    fn the_file_store_round_trips() {
        let dir = std::env::temp_dir().join(format!("connecto-keys-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = FileKeys(dir.join("sub").join("jev.key"));
        assert_eq!(store.get(), Ok(None));
        assert_eq!(store.set("k1"), Ok(()));
        assert_eq!(store.set("k2"), Ok(()));
        assert_eq!(store.get(), Ok(Some("k2".to_owned())));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt as _;
            let mode = std::fs::metadata(&store.0).map(|m| m.permissions().mode() & 0o777);
            assert_eq!(mode.ok(), Some(0o600));
        }
        assert_eq!(store.clear(), Ok(()));
        assert_eq!(store.clear(), Ok(()));
        assert_eq!(store.get(), Ok(None));
        let _ = std::fs::remove_dir_all(&dir);
    }
}
