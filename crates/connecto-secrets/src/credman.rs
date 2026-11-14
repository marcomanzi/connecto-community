//! The Windows Credential Manager, used only when the person chose to keep
//! Connecto's secrets there (Settings); by default they are in a file.
//!
//! Each item is one *generic* credential whose target name is
//! `<service>/<account>` (the secrets item is `connecto/secrets`). It is kept
//! with `CRED_PERSIST_LOCAL_MACHINE`: it stays on this computer and does not
//! roam with the user's profile, since the device keys belong to this machine.
//!
//! One credential holds at most `MAX_ITEM_BYTES`; a larger value is split
//! across several (see `parts`), up to `MAX_VALUE_BYTES`.

mod parts;

pub use parts::{MAX_PARTS, MAX_VALUE_BYTES};

use std::collections::HashMap;
use std::sync::Arc;

use keyring_core::api::CredentialStoreApi;
use keyring_core::{Entry, Error};

use crate::Items;

/// `CRED_MAX_CREDENTIAL_BLOB_SIZE` (5*512): the most bytes one credential can
/// hold (`CREDENTIALW.CredentialBlobSize`, Microsoft's `wincred.h`). A larger
/// value is split across several credentials; one larger than
/// `MAX_VALUE_BYTES` is refused before anything is written, never cut short.
pub const MAX_ITEM_BYTES: usize = 5 * 512;

/// The Windows Credential Manager (generic credentials of the signed-in user).
/// A value larger than one credential is split across several (`parts`).
pub struct CredentialManagerItems;

/// Exactly one credential per item, no splitting.
struct OneCredential;

/// The target name of an item.
fn target(service: &str, account: &str) -> String {
    format!("{service}/{account}")
}

/// What went wrong, without the secret: keyring-core's messages hold names and
/// Windows error codes, never the stored bytes.
fn failed(err: &Error) -> String {
    format!("Credential Manager: {err}")
}

fn entry(service: &str, account: &str) -> Result<Entry, String> {
    let store = windows_native_keyring_store::Store::new().map_err(|err| failed(&err))?;
    let target = target(service, account);
    let modifiers = HashMap::from([("target", target.as_str()), ("persistence", "Local")]);
    store
        .build(service, account, Some(&modifiers))
        .map_err(|err| failed(&err))
}

impl Items for CredentialManagerItems {
    fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        parts::read(&OneCredential, service, account)
    }

    fn write(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        parts::write(&OneCredential, service, account, value)
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        parts::delete(&OneCredential, service, account)
    }
}

impl Items for OneCredential {
    fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        match entry(service, account)?.get_secret() {
            Ok(bytes) => Ok(Some(bytes)),
            Err(Error::NoEntry) => Ok(None),
            Err(err) => Err(failed(&err)),
        }
    }

    fn write(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        if value.len() > MAX_ITEM_BYTES {
            return Err(format!(
                "Credential Manager: the item {} would be {} bytes, more than the {MAX_ITEM_BYTES} Windows keeps in one credential; nothing was written",
                target(service, account),
                value.len()
            ));
        }
        entry(service, account)?
            .set_secret(value)
            .map_err(|err| failed(&err))
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        match entry(service, account)?.delete_credential() {
            Ok(()) | Err(Error::NoEntry) => Ok(()),
            Err(err) => Err(failed(&err)),
        }
    }
}

/// Windows Credential Manager, for [`crate::Secrets::system`] when the person
/// chose the system store.
pub(crate) fn items() -> Arc<dyn Items> {
    Arc::new(CredentialManagerItems)
}
