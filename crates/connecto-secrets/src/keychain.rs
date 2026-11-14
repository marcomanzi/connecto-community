//! The macOS Keychain, used only when the person chose to keep Connecto's
//! secrets there (Settings); by default they are in a file (see the crate
//! docs).

use std::sync::Arc;

use crate::Items;

/// `errSecItemNotFound`.
const NOT_FOUND: i32 = -25300;

/// The macOS Keychain.
pub struct KeychainItems;

impl Items for KeychainItems {
    fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        match security_framework::passwords::get_generic_password(service, account) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.code() == NOT_FOUND => Ok(None),
            Err(err) => Err(format!("Keychain: {err}")),
        }
    }

    fn write(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        security_framework::passwords::set_generic_password(service, account, value)
            .map_err(|err| format!("Keychain: {err}"))
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        match security_framework::passwords::delete_generic_password(service, account) {
            Ok(()) => Ok(()),
            Err(err) if err.code() == NOT_FOUND => Ok(()),
            Err(err) => Err(format!("Keychain: {err}")),
        }
    }
}

/// The Keychain, for [`crate::Secrets::system`] when the person chose the
/// system store.
pub(crate) fn items() -> Arc<dyn Items> {
    Arc::new(KeychainItems)
}
