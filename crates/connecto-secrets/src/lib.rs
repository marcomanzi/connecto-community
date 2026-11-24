//! All of Connecto's secrets in ONE document.
//!
//! By default the document is a file, `<data folder>/secrets/secrets.json`
//! (`~/Library/Application Support/connecto` on macOS, `%APPDATA%\connecto`
//! on Windows, or `CONNECTO_DATA_DIR`), as `gh`, `ssh` and `aws` keep theirs:
//! the file is 0600 in a 0700 folder on macOS and Linux (on Windows the
//! per-user `%APPDATA%` keeps it private), and it is replaced atomically. So
//! there are no Keychain prompts, ever. A person who wants the system store
//! can turn on "Store secrets in the system keychain" in Settings: the
//! document is then the macOS Keychain item (service `connecto`, account
//! `secrets`) or the Windows Credential Manager credential `connecto/secrets`.
//! The choice takes effect at the next launch.
//!
//! The document:
//!
//! ```json
//! {"version": 1, "secrets": {"jev.api-key": "...", "remote.device-key": "<base64>"}}
//! ```
//!
//! Keys are namespaced (`jev.`, `remote.`, ...). Text is stored as it is,
//! binary as base64. Fields this version does not know (keys of secrets,
//! top-level fields of a later version) are kept when it is written back.
//!
//! - The document is read once per process, when the first secret is asked
//!   for, and kept in memory.
//! - A change applies only that change and writes the whole document back,
//!   under an advisory file lock, so a second Connecto process cannot drop a
//!   key of the first. The document is read again first only when the lock
//!   file's stamp shows that someone else wrote it since; a change that
//!   changes nothing writes nothing.
//! - A document this version cannot read is never overwritten: every call
//!   fails with the reason, and the document is left as it is.
//! - Moving between the two places: when the chosen place has no document
//!   yet, the other place is read once. A document found there is written to
//!   the chosen place and then removed from the other one. So the first
//!   launch of a version with the file store reads the Keychain item once
//!   (the last prompt) and deletes it. If that read fails (the person said
//!   no, say), an empty document is written with a note, and the move is not
//!   tried again: the secrets that were in the Keychain are not used.
//!
//! Nothing here logs, prints or puts a secret in an error.

use std::fs::File;
use std::io::{Read as _, Seek as _, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use std::collections::{HashMap, HashSet};

use serde_json::{Map, Value, json};

/// Keychain service of the item (when the system store is chosen).
pub const SERVICE: &str = "connecto";
/// Keychain account of the item (when the system store is chosen).
pub const ACCOUNT: &str = "secrets";

/// The Jev API key (text).
pub const JEV_API_KEY: &str = "jev.api-key";
/// The OpenRouter API key Pi terminals are given (text).
pub const PI_OPENROUTER_API_KEY: &str = "pi.openrouter-api-key";
/// The Together AI API key Pi terminals are given (text).
pub const PI_TOGETHER_API_KEY: &str = "pi.together-api-key";
/// The shared token of the self-hosted relay (text).
pub const REMOTE_RELAY_TOKEN: &str = "remote.relay-token";
/// The 32-byte secret of this machine's iroh endpoint (binary).
pub const REMOTE_DEVICE_KEY: &str = "remote.device-key";
/// This machine's SSH key, OpenSSH text.
pub const REMOTE_DEVICE_SSH_KEY: &str = "remote.device-ssh-key";

/// The document version this build writes (and the newest it will change).
const VERSION: u64 = 1;

/// How long to wait for another process that is changing the document.
const LOCK_WAIT: Duration = Duration::from_secs(5);

/// The folder of the secrets file, inside the data folder.
pub const FOLDER: &str = "secrets";
/// The secrets file, inside [`FOLDER`].
pub const FILE: &str = "secrets.json";

/// Where secrets live, as the app's messages name it.
pub const STORE_NAME: &str = "Connecto's secret store";

/// Keychain items by service and account. A read that fails (denied by the
/// user, locked, unavailable) is an `Err`; only an item that is not there is
/// `Ok(None)`.
pub trait Items: Send + Sync {
    /// # Errors
    /// When the item cannot be read, which is not the same as being absent.
    fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String>;
    /// # Errors
    /// When the item cannot be written.
    fn write(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String>;
    /// Removing what is not there is not an error.
    ///
    /// # Errors
    /// When the item cannot be removed.
    fn delete(&self, service: &str, account: &str) -> Result<(), String>;
}

#[cfg(target_os = "macos")]
mod keychain;
#[cfg(target_os = "macos")]
use keychain as platform;
#[cfg(target_os = "macos")]
pub use keychain::KeychainItems;

#[cfg(windows)]
mod credman;
#[cfg(windows)]
use credman as platform;
#[cfg(windows)]
pub use credman::{CredentialManagerItems, MAX_ITEM_BYTES, MAX_PARTS, MAX_VALUE_BYTES};

/// No system store on other platforms: the file is the only place.
#[cfg(not(any(target_os = "macos", windows)))]
mod platform {
    pub(crate) const AVAILABLE: bool = false;
    pub(crate) fn items() -> std::sync::Arc<dyn crate::Items> {
        std::sync::Arc::new(crate::FileItems::new(std::path::PathBuf::new()))
    }
}

/// Whether this platform has a system store to offer (macOS, Windows).
#[must_use]
pub const fn system_store_available() -> bool {
    cfg!(any(target_os = "macos", windows))
}

/// The system store itself (the macOS Keychain, Windows Credential Manager),
/// for an item kept apart from the document, such as the vault's master key.
/// `None` where there is none.
#[must_use]
pub fn system_items() -> Option<Arc<dyn Items>> {
    #[cfg(any(target_os = "macos", windows))]
    {
        Some(platform::items())
    }
    #[cfg(not(any(target_os = "macos", windows)))]
    {
        None
    }
}

// ---------- the file ----------

/// The document in one file. Only [`SERVICE`]/[`ACCOUNT`] lives here: any
/// other item reads as absent.
pub struct FileItems {
    path: PathBuf,
}

impl FileItems {
    #[must_use]
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    fn is_doc(service: &str, account: &str) -> bool {
        service == SERVICE && account == ACCOUNT
    }

    fn failed(&self, what: &str, err: &std::io::Error) -> String {
        format!(
            "the secrets file {} could not be {what}: {err}",
            self.path.display()
        )
    }
}

impl Items for FileItems {
    fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        if !Self::is_doc(service, account) {
            return Ok(None);
        }
        match std::fs::read(&self.path) {
            Ok(bytes) => Ok(Some(bytes)),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(self.failed("read", &err)),
        }
    }

    fn write(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        if !Self::is_doc(service, account) {
            return Err("only the secrets document is kept in the secrets file".to_owned());
        }
        write_private(&self.path, value).map_err(|err| self.failed("written", &err))
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        if !Self::is_doc(service, account) {
            return Ok(());
        }
        match std::fs::remove_file(&self.path) {
            Ok(()) => Ok(()),
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => Ok(()),
            Err(err) => Err(self.failed("removed", &err)),
        }
    }
}

/// Write `bytes` to `path` so that a reader sees the old file or the new one,
/// never half of one: a new file beside it (0600), flushed to disk, then
/// renamed over it. The folder is made 0700.
///
/// # Errors
/// When the folder or the file cannot be made, written or renamed.
pub fn write_private(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    let dir = path
        .parent()
        .ok_or_else(|| std::io::Error::other("the secrets file has no folder"))?;
    std::fs::create_dir_all(dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(dir, std::fs::Permissions::from_mode(0o700))?;
    }
    let name = path
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_default();
    let temp = dir.join(format!(".{name}.{}.{}", std::process::id(), new_stamp()));
    let mut options = std::fs::OpenOptions::new();
    options.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    let result = (|| {
        let mut file = options.open(&temp)?;
        file.write_all(bytes)?;
        file.sync_all()?;
        drop(file);
        std::fs::rename(&temp, path)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temp);
    }
    result
}

/// The data folder, when the app has not said: `CONNECTO_DATA_DIR`, else the
/// platform's (the same rule as `connecto-store`'s data root).
fn default_data_dir() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CONNECTO_DATA_DIR") {
        return Some(PathBuf::from(dir));
    }
    #[cfg(windows)]
    {
        std::env::var_os("APPDATA").map(|dir| PathBuf::from(dir).join("connecto"))
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME")
            .map(|home| PathBuf::from(home).join("Library/Application Support/connecto"))
    }
}

/// The secrets file under a data folder.
#[must_use]
pub fn secrets_file(data_dir: &Path) -> PathBuf {
    data_dir.join(FOLDER).join(FILE)
}

/// How [`Secrets::system`] is set up.
struct Config {
    data_dir: Option<PathBuf>,
    system_store: bool,
}

static CONFIG: OnceLock<Config> = OnceLock::new();

/// Say where the data folder is and whether the person chose the system store
/// (the macOS Keychain, Windows Credential Manager). Call it once, before the
/// first secret is asked for; a later call changes nothing and returns false.
/// Without it, the file in the default data folder is used.
pub fn configure_system(data_dir: PathBuf, system_store: bool) -> bool {
    CONFIG
        .set(Config {
            data_dir: Some(data_dir),
            system_store,
        })
        .is_ok()
}

/// Items in memory, for tests; a read or a delete can be made to fail.
#[derive(Default)]
pub struct MemoryItems {
    items: Mutex<HashMap<(String, String), Vec<u8>>>,
    denied: Mutex<HashSet<(String, String)>>,
    stuck: Mutex<HashSet<(String, String)>>,
    reads: Mutex<Vec<(String, String)>>,
    writes: Mutex<Vec<(String, String)>>,
    on_read: Mutex<Option<ReadHook>>,
    fail_writes: Mutex<bool>,
    lose_writes: Mutex<bool>,
}

/// Called with the service and account of each read, before it is answered.
type ReadHook = Arc<dyn Fn(&str, &str) + Send + Sync>;

impl MemoryItems {
    fn pair(service: &str, account: &str) -> (String, String) {
        (service.to_owned(), account.to_owned())
    }

    /// Put an item there, as an earlier version or another process would.
    pub fn put(&self, service: &str, account: &str, value: &[u8]) {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(Self::pair(service, account), value.to_vec());
    }

    /// The item as it is now.
    #[must_use]
    pub fn peek(&self, service: &str, account: &str) -> Option<Vec<u8>> {
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(&Self::pair(service, account))
            .cloned()
    }

    /// Reads of this item fail (the user said no) until it is allowed again.
    pub fn deny(&self, service: &str, account: &str, on: bool) {
        let mut denied = self.denied.lock().unwrap_or_else(PoisonError::into_inner);
        if on {
            denied.insert(Self::pair(service, account));
        } else {
            denied.remove(&Self::pair(service, account));
        }
    }

    /// Deleting this item fails until it is allowed again.
    pub fn stick(&self, service: &str, account: &str, on: bool) {
        let mut stuck = self.stuck.lock().unwrap_or_else(PoisonError::into_inner);
        if on {
            stuck.insert(Self::pair(service, account));
        } else {
            stuck.remove(&Self::pair(service, account));
        }
    }

    /// Run `hook` at the start of every read (a test making something happen
    /// while a read is going on).
    pub fn on_read(&self, hook: impl Fn(&str, &str) + Send + Sync + 'static) {
        *self.on_read.lock().unwrap_or_else(PoisonError::into_inner) = Some(Arc::new(hook));
    }

    /// Writes fail with an error.
    pub fn fail_writes(&self, on: bool) {
        *self
            .fail_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = on;
    }

    /// Writes are accepted and silently not kept.
    pub fn lose_writes(&self, on: bool) {
        *self
            .lose_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = on;
    }

    /// How many times the item was read.
    #[must_use]
    pub fn reads_of(&self, service: &str, account: &str) -> usize {
        let wanted = Self::pair(service, account);
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|read| **read == wanted)
            .count()
    }

    /// How many times the item was read, all items together.
    #[must_use]
    pub fn reads(&self) -> usize {
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .len()
    }

    /// How many times a write of the item was asked for (failed or lost ones
    /// too).
    #[must_use]
    pub fn writes_of(&self, service: &str, account: &str) -> usize {
        let wanted = Self::pair(service, account);
        self.writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .iter()
            .filter(|write| **write == wanted)
            .count()
    }
}

impl Items for MemoryItems {
    fn read(&self, service: &str, account: &str) -> Result<Option<Vec<u8>>, String> {
        let pair = Self::pair(service, account);
        self.reads
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(pair.clone());
        let hook = self
            .on_read
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        if let Some(hook) = hook {
            hook(service, account);
        }
        if self
            .denied
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&pair)
        {
            return Err("Keychain: user canceled the operation".to_owned());
        }
        Ok(self.peek(service, account))
    }

    fn write(&self, service: &str, account: &str, value: &[u8]) -> Result<(), String> {
        self.writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push(Self::pair(service, account));
        if *self
            .fail_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            return Err("Keychain: cannot write".to_owned());
        }
        if !*self
            .lose_writes
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
        {
            self.put(service, account, value);
        }
        Ok(())
    }

    fn delete(&self, service: &str, account: &str) -> Result<(), String> {
        let pair = Self::pair(service, account);
        if self
            .stuck
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .contains(&pair)
        {
            return Err("Keychain: cannot delete".to_owned());
        }
        self.items
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(&pair);
        Ok(())
    }
}

// ---------- the document ----------

/// The JSON document. The whole object is kept, so what this version does not
/// know goes back as it came.
#[derive(Debug, Clone, PartialEq)]
struct Doc(Map<String, Value>);

impl Doc {
    fn empty() -> Self {
        let mut root = Map::new();
        root.insert("version".to_owned(), json!(1));
        root.insert("secrets".to_owned(), json!({}));
        Self(root)
    }

    /// With a short note of what happened to the move (see `move_in`).
    fn noted(mut self, note: &str) -> Self {
        self.0.insert("moved".to_owned(), json!(note));
        self
    }

    fn parse(bytes: &[u8]) -> Result<Self, String> {
        let Value::Object(root) = serde_json::from_slice::<Value>(bytes)
            .map_err(|_| "it is not valid JSON".to_owned())?
        else {
            return Err("it is not a JSON object".to_owned());
        };
        // The version says what may be done with the document. A missing or
        // odd one is not guessed at.
        match root.get("version").and_then(Value::as_u64) {
            Some(version) if version >= 1 => {}
            _ => return Err("its `version` is not a number from 1".to_owned()),
        }
        if !root.get("secrets").is_some_and(Value::is_object) {
            return Err("it has no `secrets` object".to_owned());
        }
        Ok(Self(root))
    }

    /// Written by a later Connecto, whose rules this one does not know: it can
    /// be read for the keys it knows, and is never written.
    fn is_newer(&self) -> bool {
        self.0
            .get("version")
            .and_then(Value::as_u64)
            .is_some_and(|version| version > VERSION)
    }

    /// `Err` when the value is there and is not text: it is not this secret as
    /// far as this code can tell, and not absent either.
    fn get(&self, key: &str) -> Result<Option<&str>, String> {
        match self.0.get("secrets").and_then(|map| map.get(key)) {
            None => Ok(None),
            Some(Value::String(text)) => Ok(Some(text)),
            Some(_) => Err(format!(
                "the stored secret {key} is not text; it has been left as it is"
            )),
        }
    }

    // `parse` and `empty` make sure `secrets` is an object.
    fn set(&mut self, key: &str, value: &str) {
        if let Some(Value::Object(map)) = self.0.get_mut("secrets") {
            map.insert(key.to_owned(), json!(value));
        }
    }

    fn remove(&mut self, key: &str) {
        if let Some(Value::Object(map)) = self.0.get_mut("secrets") {
            map.remove(key);
        }
    }

    fn to_bytes(&self) -> Vec<u8> {
        let mut root = self.0.clone();
        // Never lower the version of a later document.
        if !root.get("version").is_some_and(Value::is_number) {
            root.insert("version".to_owned(), json!(1));
        }
        serde_json::to_vec(&Value::Object(root)).unwrap_or_default()
    }
}

// ---------- base64 ----------

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

/// Standard base64 with padding, as the document stores binary.
#[must_use]
pub fn b64_encode(bytes: &[u8]) -> String {
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b = [
            chunk[0],
            *chunk.get(1).unwrap_or(&0),
            *chunk.get(2).unwrap_or(&0),
        ];
        let n = (u32::from(b[0]) << 16) | (u32::from(b[1]) << 8) | u32::from(b[2]);
        for (i, shift) in [18, 12, 6, 0].into_iter().enumerate() {
            if i <= chunk.len() {
                out.push(char::from(B64[((n >> shift) & 63) as usize]));
            } else {
                out.push('=');
            }
        }
    }
    out
}

/// The bytes of standard base64 text, `None` when it is not.
#[must_use]
pub fn b64_decode(text: &str) -> Option<Vec<u8>> {
    let bytes = text.as_bytes();
    if !bytes.len().is_multiple_of(4) {
        return None;
    }
    let mut out = Vec::with_capacity(bytes.len() / 4 * 3);
    for quad in bytes.chunks(4) {
        let pad = quad.iter().rev().take_while(|b| **b == b'=').count();
        if pad > 2 {
            return None;
        }
        let mut n = 0_u32;
        for (i, b) in quad.iter().enumerate() {
            let v = if *b == b'=' && i >= 4 - pad {
                0
            } else {
                u32::try_from(B64.iter().position(|c| c == b)?).ok()?
            };
            n = (n << 6) | v;
        }
        let triple = [(n >> 16) as u8, (n >> 8) as u8, n as u8];
        out.extend_from_slice(&triple[..3 - pad]);
    }
    Some(out)
}

// ---------- the store ----------

/// A value no other write has used: the time and this process.
fn new_stamp() -> String {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_nanos());
    format!("{nanos}-{}", std::process::id())
}

enum Loaded {
    NotYet,
    Ready(Doc),
    /// The document could not be read or understood; it is left as it is.
    Failed(String),
}

/// The secrets, behind one document.
///
/// All callers in a process share [`Secrets::system`]; the state lock is held
/// across the first read, so a second caller waits for it instead of reading
/// again.
pub struct Secrets {
    items: Arc<dyn Items>,
    /// The other place the document may still be in, moved in once (see the
    /// crate docs). `None`: nothing to move from.
    previous: Option<Arc<dyn Items>>,
    lock_file: Option<PathBuf>,
    state: Mutex<Loaded>,
    /// The stamp in the lock file when this process last read or wrote the
    /// document. Every write puts a new one there, so an unchanged stamp means
    /// the copy in memory is what is stored, and a change needs no read first.
    stamp: Mutex<Option<String>>,
}

impl Secrets {
    /// `lock_file`: where processes take turns changing the document (`None`:
    /// no other process, as in a test).
    #[must_use]
    pub fn new(items: Arc<dyn Items>, lock_file: Option<PathBuf>) -> Self {
        Self::moving_from(items, None, lock_file)
    }

    /// As [`Secrets::new`], with `previous` read once when `items` has no
    /// document yet (see the crate docs).
    #[must_use]
    pub fn moving_from(
        items: Arc<dyn Items>,
        previous: Option<Arc<dyn Items>>,
        lock_file: Option<PathBuf>,
    ) -> Self {
        Self {
            items,
            previous,
            lock_file,
            state: Mutex::new(Loaded::NotYet),
            stamp: Mutex::new(None),
        }
    }

    /// The process's store, shared by the whole process so that it is read
    /// once: the secrets file, or the system store when the person chose it
    /// (see [`configure_system`]).
    #[must_use]
    pub fn system() -> Arc<Self> {
        static SYSTEM: OnceLock<Arc<Secrets>> = OnceLock::new();
        Arc::clone(SYSTEM.get_or_init(|| {
            let config = CONFIG.get_or_init(|| Config {
                data_dir: default_data_dir(),
                system_store: false,
            });
            let Some(dir) = config.data_dir.clone() else {
                // No data folder at all (no HOME): nothing can be kept.
                return Arc::new(Self::new(Arc::new(FileItems::new(PathBuf::new())), None));
            };
            let file: Arc<dyn Items> = Arc::new(FileItems::new(secrets_file(&dir)));
            let lock = Some(dir.join(".secrets.lock"));
            #[cfg(any(target_os = "macos", windows))]
            {
                let system = platform::items();
                if config.system_store {
                    return Arc::new(Self::moving_from(system, Some(file), lock));
                }
                Arc::new(Self::moving_from(file, Some(system), lock))
            }
            #[cfg(not(any(target_os = "macos", windows)))]
            {
                let _ = platform::AVAILABLE;
                let _ = platform::items;
                Arc::new(Self::new(file, lock))
            }
        }))
    }

    /// A text secret.
    ///
    /// # Errors
    /// When the store cannot be read, holds a document this version cannot
    /// read, or holds this secret as something that is not text.
    pub fn get(&self, key: &str) -> Result<Option<String>, String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.ensure_loaded(&mut state)?;
        match &*state {
            Loaded::Ready(doc) => Ok(doc.get(key)?.map(str::to_owned)),
            Loaded::Failed(why) => Err(why.clone()),
            Loaded::NotYet => Err("secrets are not loaded".to_owned()),
        }
    }

    /// A binary secret (stored as base64).
    ///
    /// # Errors
    /// As [`Secrets::get`], and when the stored text is not base64.
    pub fn get_bytes(&self, key: &str) -> Result<Option<Vec<u8>>, String> {
        self.get(key)?
            .map(|text| {
                b64_decode(&text).ok_or_else(|| {
                    format!("the stored secret {key} is not base64; it has been left as it is")
                })
            })
            .transpose()
    }

    /// # Errors
    /// When the store cannot be read or written, holds a document this version
    /// cannot read or must not change, or holds this key as something that is
    /// not text.
    pub fn set(&self, key: &str, value: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.ensure_loaded(&mut state)?;
        self.change(&mut state, |doc| {
            doc.get(key)?;
            doc.set(key, value);
            Ok(())
        })
    }

    /// # Errors
    /// As [`Secrets::set`].
    pub fn set_bytes(&self, key: &str, value: &[u8]) -> Result<(), String> {
        self.set(key, &b64_encode(value))
    }

    /// Removing what is not there is not an error, and writes nothing.
    ///
    /// # Errors
    /// As [`Secrets::set`].
    pub fn delete(&self, key: &str) -> Result<(), String> {
        let mut state = self.state.lock().unwrap_or_else(PoisonError::into_inner);
        self.ensure_loaded(&mut state)?;
        self.change(&mut state, |doc| {
            doc.get(key)?;
            doc.remove(key);
            Ok(())
        })
    }

    fn parse(bytes: &[u8]) -> Result<Doc, String> {
        Doc::parse(bytes).map_err(|why| {
            format!("the stored secrets cannot be read ({why}); they have been left as they are")
        })
    }

    /// The stored document; `None` when there is none.
    fn read_doc(&self) -> Result<Option<Doc>, String> {
        self.items
            .read(SERVICE, ACCOUNT)?
            .map(|bytes| Self::parse(&bytes))
            .transpose()
    }

    fn ensure_loaded(&self, state: &mut Loaded) -> Result<(), String> {
        match state {
            Loaded::Ready(_) => return Ok(()),
            Loaded::Failed(why) => return Err(why.clone()),
            Loaded::NotYet => {}
        }
        // The stamp first: a write by another process after this point changes
        // it, and the next change then reads the document again.
        let stamp = self.read_stamp();
        let loaded = match self.read_doc() {
            Ok(Some(doc)) => Ok(doc),
            Ok(None) => match &self.previous {
                Some(previous) => self.move_in(previous.as_ref()),
                None => Ok(Doc::empty()),
            },
            Err(why) => Err(why),
        };
        match loaded {
            Ok(doc) => {
                if self.stamp_unset() {
                    *self.stamp.lock().unwrap_or_else(PoisonError::into_inner) = stamp;
                }
                *state = Loaded::Ready(doc);
                Ok(())
            }
            Err(why) => {
                // Not asked again in this run.
                *state = Loaded::Failed(why.clone());
                Err(why)
            }
        }
    }

    fn stamp_unset(&self) -> bool {
        self.stamp
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .is_none()
    }

    /// The chosen place has no document: take the one in `previous`, once.
    ///
    /// Under the lock, so two processes do not both move. Whatever `previous`
    /// says, a document is written to the chosen place, so the move is never
    /// tried again: the one found (then removed from `previous`), an empty
    /// one when there was none, or an empty one with a note when `previous`
    /// could not be read or understood (its secrets are then not used, and it
    /// is left as it is).
    fn move_in(&self, previous: &dyn Items) -> Result<Doc, String> {
        let held = self.take_lock()?;
        // Another process may have moved it meanwhile.
        if let Some(doc) = self.read_doc()? {
            return Ok(doc);
        }
        let (bytes, doc, remove_previous) = match previous.read(SERVICE, ACCOUNT) {
            Ok(Some(bytes)) => match Doc::parse(&bytes) {
                Ok(doc) => (bytes, doc, true),
                Err(why) => {
                    let doc = Doc::empty().noted(&format!("not moved: {why}"));
                    (doc.to_bytes(), doc, false)
                }
            },
            Ok(None) => {
                let doc = Doc::empty();
                (doc.to_bytes(), doc, false)
            }
            Err(why) => {
                let doc = Doc::empty().noted(&format!("not moved: {why}"));
                (doc.to_bytes(), doc, false)
            }
        };
        self.items.write(SERVICE, ACCOUNT, &bytes)?;
        match self.items.read(SERVICE, ACCOUNT) {
            Ok(Some(back)) if back == bytes => {}
            _ => return Err("the moved secrets were not kept".to_owned()),
        }
        if remove_previous {
            // A copy left behind is never read again: this place has a
            // document now.
            let _ = previous.delete(SERVICE, ACCOUNT);
        }
        if let Some(held) = &held {
            self.put_new_stamp(held);
        }
        Ok(doc)
    }

    /// Apply `edit` and write the whole document back, under the lock. A
    /// document of a later version is not written. An edit that changes
    /// nothing in the copy in memory writes nothing; the document is read
    /// again first only when another process may have written it since.
    fn change(
        &self,
        state: &mut Loaded,
        edit: impl Fn(&mut Doc) -> Result<(), String>,
    ) -> Result<(), String> {
        if let Loaded::Ready(cached) = &*state {
            let mut probe = cached.clone();
            edit(&mut probe)?;
            if probe == *cached {
                return Ok(());
            }
        }
        let held = self.take_lock()?;
        let known = self
            .stamp
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        let fresh = match (&*state, known) {
            (Loaded::Ready(cached), Some(known))
                if held.as_ref().and_then(stamp_in).as_deref() == Some(known.as_str()) =>
            {
                cached.clone()
            }
            _ => self.read_doc()?.unwrap_or_else(Doc::empty),
        };
        if fresh.is_newer() {
            return Err(
                "the secrets were written by a newer Connecto and are not changed by this one"
                    .to_owned(),
            );
        }
        let mut doc = fresh.clone();
        edit(&mut doc)?;
        if doc == fresh {
            *state = Loaded::Ready(doc);
            return Ok(());
        }
        // Whatever happens next, the document may have changed: forget the
        // stamp until a new one is written.
        *self.stamp.lock().unwrap_or_else(PoisonError::into_inner) = None;
        self.items.write(SERVICE, ACCOUNT, &doc.to_bytes())?;
        if let Some(held) = &held {
            self.put_new_stamp(held);
        }
        *state = Loaded::Ready(doc);
        Ok(())
    }

    /// Write a new stamp into the lock file, through `held`, the handle that
    /// holds the lock. On Windows the lock is mandatory: a second handle could
    /// neither write nor read the file while it is held.
    fn put_new_stamp(&self, held: &File) {
        let stamp = new_stamp();
        let mut file = held;
        let written = file.set_len(0).is_ok()
            && file.seek(SeekFrom::Start(0)).is_ok()
            && file.write_all(stamp.as_bytes()).is_ok();
        *self.stamp.lock().unwrap_or_else(PoisonError::into_inner) = written.then_some(stamp);
    }

    /// The stamp in the lock file, if there is one, read without the lock.
    fn read_stamp(&self) -> Option<String> {
        let text = std::fs::read_to_string(self.lock_file.as_ref()?).ok()?;
        let text = text.trim();
        (!text.is_empty()).then(|| text.to_owned())
    }

    fn take_lock(&self) -> Result<Option<File>, String> {
        let Some(path) = &self.lock_file else {
            return Ok(None);
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(path)
            .map_err(|err| format!("secrets lock: {err}"))?;
        let began = Instant::now();
        loop {
            match file.try_lock() {
                Ok(()) => return Ok(Some(file)),
                Err(std::fs::TryLockError::WouldBlock) if began.elapsed() < LOCK_WAIT => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(_) => return Err("another Connecto is changing the secrets".to_owned()),
            }
        }
    }
}

/// The stamp in the lock file, read through `held`, the handle that holds
/// the lock (see [`Secrets::put_new_stamp`]).
fn stamp_in(held: &File) -> Option<String> {
    let mut file = held;
    file.seek(SeekFrom::Start(0)).ok()?;
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    let text = text.trim();
    (!text.is_empty()).then(|| text.to_owned())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests;
