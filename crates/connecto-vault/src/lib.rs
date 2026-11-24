//! The vault: the person's own secrets, for terminals to use without reading.
//!
//! Values live in Connecto's secret store ([`connecto_secrets::Secrets`]), the
//! same document as the device keys and the Jev key, under
//! `user.global.<name>` or `user.project.<project id>.<name>`, encrypted with
//! a master key kept apart (see [`crypt`]). What is not secret (names,
//! descriptions, bindings, times) is kept beside it, in
//! `<data>/secrets/vault.json`, so listing never needs a value.
//!
//! Only the app links this crate. Terminals reach it through the bridge
//! ([`connecto_core::VaultBridge`]), which gives names and never values. A
//! value leaves the vault only into the app's own code that carries out a
//! binding: the web proxy ([`proxy`]), a program the app runs ([`run`]), or
//! the ssh-agent ([`agent`]). Every such use is written to
//! `<data>/secrets/uses.jsonl`, by name.
//!
//! Terminals can also ask the person for a secret (`connecto secret
//! request`): the request waits in [`VaultView::asks`] until the person
//! answers in the window, and the terminal is then told the outcome.
//!
//! Nothing here logs, prints or puts a value in an error.

use std::collections::HashMap;
use std::io::Write as _;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, PoisonError};
use std::time::{SystemTime, UNIX_EPOCH};

use connecto_core::vault::{self, MAX_DESCRIPTION, MAX_REASON};
use connecto_core::{
    AskAnswer, AskReply, AskRequest, Binding, ChangeError, GeneratedKey, ProviderScope, RunOutput,
    SecretAsk, SecretCaller, SecretEntry, SshAccess, VaultBridge, VaultChange, VaultControl,
    VaultView,
};
use connecto_secrets::Secrets;
use serde_json::{Value, json};

#[cfg(unix)]
pub mod agent;
#[cfg(not(unix))]
mod agent_none;
#[cfg(not(unix))]
use agent_none as agent;
mod binding;
pub mod crypt;
mod meta;
pub mod proxy;
pub mod run;

pub use binding::check_binding;
pub use crypt::{FileKey, FixedKey, KEY_ACCOUNT, MasterKey, StoreKey};

/// The file of names and bindings, inside the secrets folder.
pub const META_FILE: &str = "vault.json";
/// The log of uses, inside the secrets folder.
pub const USES_FILE: &str = "uses.jsonl";
/// The master key file where there is no system store.
pub const KEY_FILE: &str = "vault.key";

/// Requests one terminal may have waiting.
const MAX_ASKS_PER_TERMINAL: usize = 3;
/// Requests waiting in all.
const MAX_ASKS: usize = 20;

/// Where the metadata goes for a data folder.
#[must_use]
pub fn meta_file(data_dir: &Path) -> PathBuf {
    data_dir.join(connecto_secrets::FOLDER).join(META_FILE)
}

/// Where the log of uses goes for a data folder.
#[must_use]
pub fn uses_file(data_dir: &Path) -> PathBuf {
    data_dir.join(connecto_secrets::FOLDER).join(USES_FILE)
}

/// Where the master key file goes for a data folder.
#[must_use]
pub fn key_file(data_dir: &Path) -> PathBuf {
    data_dir.join(connecto_secrets::FOLDER).join(KEY_FILE)
}

/// The key of a value in the secret store.
#[must_use]
pub fn store_key(scope: &ProviderScope, name: &str) -> String {
    match scope {
        ProviderScope::Global => format!("user.global.{name}"),
        ProviderScope::Project(id) => format!("user.project.{id}.{name}"),
    }
}

struct PendingAsk {
    ask: SecretAsk,
    reply: Option<AskReply>,
}

struct Inner {
    entries: Vec<SecretEntry>,
    /// The values, opened, by store key. Only what entries name.
    values: HashMap<String, String>,
    /// Why the vault cannot be changed: its file could not be read, so it is
    /// never written (it may be sound).
    frozen: Option<String>,
    asks: Vec<PendingAsk>,
    /// What paired machines changed, newest last, numbered from 1.
    remote_changes: Vec<(u64, String)>,
    /// The number of the last change noted: numbers never repeat in a run.
    last_change: u64,
    view: Arc<VaultView>,
    known: Arc<[String]>,
}

/// What the brokers need once they run.
pub(crate) struct Brokers {
    pub(crate) proxy_port: u16,
    /// The private folder of ssh-agent sockets.
    pub(crate) agent_dir: PathBuf,
}

/// The vault. One per app; shared as an `Arc`.
pub struct Vault {
    secrets: Arc<Secrets>,
    master: Arc<dyn MasterKey>,
    /// `None`: nothing is kept on disk (a test).
    meta: Option<PathBuf>,
    uses: Option<PathBuf>,
    inner: Mutex<Inner>,
    /// Per-terminal tokens of the web proxy, with who they stand for.
    tokens: Mutex<HashMap<String, SecretCaller>>,
    brokers: OnceLock<Brokers>,
    /// Open ssh-agent sockets, by token and name.
    pub(crate) agents: Mutex<HashMap<(String, String), PathBuf>>,
    /// A save to fail, counted from 1 (tests).
    #[cfg(test)]
    fail_save: std::sync::atomic::AtomicU32,
    /// Itself, weakly, for the brokers' threads (set by `start_brokers`).
    me: OnceLock<std::sync::Weak<Vault>>,
    /// Called when requests come or go, so the window looks again.
    on_change: OnceLock<Arc<dyn Fn() + Send + Sync>>,
}

impl Vault {
    /// Read the metadata and the values it names. A missing file is an empty
    /// vault. A file that cannot be read leaves the vault empty and unchanged
    /// on disk, with the reason in [`VaultView::problem`]. `data_dir`: where
    /// the metadata and the log of uses go (`None`: nowhere, a test).
    #[must_use]
    pub fn open(
        secrets: Arc<Secrets>,
        master: Arc<dyn MasterKey>,
        data_dir: Option<&Path>,
    ) -> Self {
        let meta = data_dir.map(meta_file);
        let (entries, frozen) = match meta.as_deref().map(meta::read) {
            None | Some(Ok(None)) => (Vec::new(), None),
            Some(Ok(Some(entries))) => (entries, None),
            Some(Err(why)) => (Vec::new(), Some(why)),
        };
        let vault = Self {
            secrets,
            master,
            meta,
            uses: data_dir.map(uses_file),
            inner: Mutex::new(Inner {
                entries,
                values: HashMap::new(),
                frozen,
                asks: Vec::new(),
                remote_changes: Vec::new(),
                last_change: 0,
                view: Arc::default(),
                known: Arc::from(Vec::new()),
            }),
            tokens: Mutex::new(HashMap::new()),
            brokers: OnceLock::new(),
            agents: Mutex::new(HashMap::new()),
            #[cfg(test)]
            fail_save: std::sync::atomic::AtomicU32::new(0),
            me: OnceLock::new(),
            on_change: OnceLock::new(),
        };
        {
            let mut inner = vault.lock();
            vault.load_values(&mut inner);
            vault.refresh(&mut inner);
        }
        vault
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Open every value an entry names. One that cannot be opened is left
    /// out and said in the view's problem.
    fn load_values(&self, inner: &mut Inner) {
        inner.values.clear();
        for entry in &inner.entries {
            let key = store_key(&entry.scope, &entry.name);
            if let Ok(Some(stored)) = self.secrets.get(&key)
                && let Ok(value) = crypt::open(&*self.master, &key, &stored)
            {
                inner.values.insert(key, value);
            }
        }
    }

    /// Rebuild the view and the forms to hide from the entries and values.
    fn refresh(&self, inner: &mut Inner) {
        let mut problem = inner.frozen.clone();
        let missing: Vec<&str> = inner
            .entries
            .iter()
            .filter(|e| !inner.values.contains_key(&store_key(&e.scope, &e.name)))
            .map(|e| e.name.as_str())
            .collect();
        if problem.is_none() && !missing.is_empty() {
            problem = Some(format!(
                "These secrets could not be opened, so terminals cannot use them: {}. Set their values again.",
                missing.join(", ")
            ));
        }
        let mut entries = inner.entries.clone();
        entries.sort_by(|a, b| a.name.cmp(&b.name).then_with(|| a.scope.cmp(&b.scope)));
        inner.view = Arc::new(VaultView {
            entries,
            problem,
            asks: inner.asks.iter().map(|p| p.ask.clone()).collect(),
            protection: self.master.describe(),
            remote_changes: inner.remote_changes.clone(),
            pages: Vec::new(),
        });
        inner.known = Arc::from(connecto_redact::known_set(
            inner.values.values().map(String::as_str),
        ));
    }

    /// Write the metadata as it is in memory.
    fn save(&self, inner: &Inner) -> Result<(), String> {
        let Some(path) = &self.meta else {
            return Ok(());
        };
        #[cfg(test)]
        if self.failing_save() {
            return Err(format!("{} could not be written: a test", path.display()));
        }
        meta::write(path, &inner.entries)
    }

    /// Fail the `n`th save of the list from now (1 is the next).
    #[cfg(test)]
    pub(crate) fn fail_save(&self, n: u32) {
        self.fail_save.store(n, std::sync::atomic::Ordering::SeqCst);
    }

    #[cfg(test)]
    fn failing_save(&self) -> bool {
        use std::sync::atomic::Ordering;
        match self.fail_save.load(Ordering::SeqCst) {
            0 => false,
            1 => {
                self.fail_save.store(0, Ordering::SeqCst);
                true
            }
            n => {
                self.fail_save.store(n - 1, Ordering::SeqCst);
                false
            }
        }
    }

    fn writable(inner: &Inner) -> Result<(), String> {
        match &inner.frozen {
            Some(why) => Err(format!("The vault cannot be changed now: {why}")),
            None => Ok(()),
        }
    }

    /// The secret `name` as a terminal of `project` sees it (its project's
    /// own first, then the app-wide one), with its value, for the app's code
    /// that carries out a binding, and for nothing else: never send the value
    /// to a terminal. Marks it used.
    ///
    /// # Errors
    /// When there is no such secret, or its value cannot be opened.
    pub fn value_for_use(
        &self,
        project: Option<&str>,
        name: &str,
    ) -> Result<(SecretEntry, String), String> {
        let mut inner = self.lock();
        let entry = inner
            .view
            .visible_to(project)
            .into_iter()
            .find(|entry| entry.name == name)
            .cloned()
            .ok_or_else(|| format!("There is no secret named {name} for this terminal."))?;
        let value = inner
            .values
            .get(&store_key(&entry.scope, &entry.name))
            .cloned()
            .ok_or_else(|| {
                format!("The secret {name} cannot be opened; the person can set it again in Settings > Secrets.")
            })?;
        if let Some(stored) = inner
            .entries
            .iter_mut()
            .find(|e| e.scope == entry.scope && e.name == entry.name)
        {
            stored.last_used_unix = Some(now());
        }
        // Best effort: a time not saved loses nothing that matters.
        if inner.frozen.is_none() {
            let _ = self.save(&inner);
        }
        self.refresh(&mut inner);
        Ok((entry, value))
    }

    /// As [`Vault::value_for_use`] for `caller`, with the binding `pick`
    /// finds, and the use written to the log.
    pub(crate) fn use_secret<T>(
        &self,
        caller: &SecretCaller,
        name: &str,
        how: &str,
        pick: impl Fn(&Binding) -> Option<T>,
    ) -> Result<(SecretEntry, T, String), String> {
        let (entry, value) = self.value_for_use(caller.project.as_deref(), name)?;
        let binding = entry.bindings.iter().find_map(&pick).ok_or_else(|| {
            let allowed = entry
                .bindings
                .iter()
                .map(Binding::kind)
                .collect::<Vec<_>>()
                .join(", ");
            if allowed.is_empty() {
                format!("The person has not allowed any use of {name} yet.")
            } else {
                format!("{name} is not for {how}; it is for: {allowed}.")
            }
        })?;
        Ok((entry, binding, value))
    }

    /// Write one use to the log, by name. Best effort.
    pub(crate) fn log_use(&self, caller: &SecretCaller, entry: &SecretEntry, how: &str, to: &str) {
        let Some(path) = &self.uses else {
            return;
        };
        let line = json!({
            "time": now(),
            "name": entry.name,
            "scope": scope_json(&entry.scope),
            "terminal": caller.terminal,
            "how": how,
            "to": to,
        });
        let mut options = std::fs::OpenOptions::new();
        options.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        if let Ok(mut file) = options.open(path) {
            let _ = writeln!(file, "{line}");
        }
    }

    /// The entries as stored, for tests and the app.
    #[must_use]
    pub fn entries(&self) -> Vec<SecretEntry> {
        self.lock().entries.clone()
    }

    /// The token `caller` reaches the brokers with: the same one for the same
    /// terminal of the same project, for this run of the app.
    pub(crate) fn token_for(&self, caller: &SecretCaller) -> Result<String, String> {
        let mut tokens = self.tokens.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some((token, _)) = tokens
            .iter()
            .find(|(_, c)| c.terminal == caller.terminal && c.project == caller.project)
        {
            return Ok(token.clone());
        }
        let mut raw = [0_u8; 16];
        getrandom::fill(&mut raw).map_err(|err| format!("no random bytes: {err}"))?;
        let token: String = raw.iter().map(|b| format!("{b:02x}")).collect();
        tokens.insert(token.clone(), caller.clone());
        Ok(token)
    }

    /// Who a token stands for.
    pub(crate) fn caller_of(&self, token: &str) -> Option<SecretCaller> {
        self.tokens
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(token)
            .cloned()
    }

    /// Say whom to wake when a terminal's request comes or is answered (the
    /// window, which then shows or closes the dialog). Once.
    pub fn set_on_change(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        let _ = self.on_change.set(wake);
    }

    fn changed(&self) {
        if let Some(wake) = self.on_change.get() {
            wake();
        }
    }

    pub(crate) fn brokers(&self) -> Result<&Brokers, String> {
        self.brokers
            .get()
            .ok_or_else(|| "Connecto's secret brokers are not running.".to_owned())
    }

    /// Start the web proxy and make the private folder of ssh-agent sockets
    /// (`agent_dir`, made 0700). Once per app.
    ///
    /// # Errors
    /// When the proxy cannot listen or the folder cannot be made.
    pub fn start_brokers(self: &Arc<Self>, agent_dir: PathBuf) -> Result<(), String> {
        if self.brokers.get().is_some() {
            return Ok(());
        }
        let _ = self.me.set(Arc::downgrade(self));
        agent::make_private_dir(&agent_dir)?;
        let proxy_port = proxy::start(Arc::downgrade(self))?;
        let _ = self.brokers.set(Brokers {
            proxy_port,
            agent_dir,
        });
        Ok(())
    }

    /// Store `value` under `scope`/`name`: the value first (encrypted), then
    /// the entry, so an entry never names a value that is not there. A new
    /// entry gets `bindings` in the same save (none when None).
    fn put_locked(
        &self,
        inner: &mut Inner,
        scope: &ProviderScope,
        name: &str,
        description: &str,
        value: &str,
        bindings: Option<Vec<Binding>>,
    ) -> Result<(), ChangeError> {
        let invalid = ChangeError::invalid;
        vault::check_name(name).map_err(invalid)?;
        let value = vault::clean_value(value).map_err(invalid)?;
        let description = description.trim();
        if description.chars().count() > MAX_DESCRIPTION {
            return Err(invalid(format!(
                "A description is at most {MAX_DESCRIPTION} characters."
            )));
        }
        if let ProviderScope::Project(id) = scope {
            check_project_id(id).map_err(invalid)?;
        }
        Self::writable(inner).map_err(|why| ChangeError::storage(why, false))?;
        let key = store_key(scope, name);
        let sealed = crypt::seal(&*self.master, &key, &value)
            .map_err(|why| ChangeError::storage(why, false))?;
        // The list as it is on disk: whatever fails below, the list in
        // memory goes back to it, so no use the disk does not have (a new
        // entry's, or a new use) is ever in force.
        let mut persisted = inner.entries.clone();
        let index = inner
            .entries
            .iter()
            .position(|e| &e.scope == scope && e.name == name);
        // A new use with the new value: the old use is taken off on disk
        // first, so no moment on disk (a failed write, a crash) pairs the new
        // value with the old use. Between the writes the secret is usable by
        // no terminal, which Settings says.
        let use_changes = index.is_some_and(|i| {
            bindings.as_ref().is_some_and(|new| {
                !inner.entries[i].bindings.is_empty() && *new != inner.entries[i].bindings
            })
        });
        let mut partial = false;
        if let (true, Some(i)) = (use_changes, index) {
            inner.entries[i].bindings.clear();
            if let Err(why) = self.save(inner) {
                inner.entries = persisted;
                return Err(ChangeError::storage(why, false));
            }
            persisted = inner.entries.clone();
            partial = true;
        }
        if let Err(why) = self.secrets.set(&key, &sealed) {
            inner.entries = persisted;
            return Err(ChangeError::storage(why, partial));
        }
        // The value is written: whatever follows, it is no longer the old.
        inner.values.insert(key, value);
        let now = now();
        match index {
            Some(i) => {
                let entry = &mut inner.entries[i];
                entry.description = description.to_owned();
                entry.updated_unix = now;
                if let Some(bindings) = bindings {
                    entry.bindings = bindings;
                }
            }
            None => inner.entries.push(SecretEntry {
                name: name.to_owned(),
                scope: scope.clone(),
                description: description.to_owned(),
                bindings: bindings.unwrap_or_default(),
                updated_unix: now,
                last_used_unix: None,
                public_key: None,
            }),
        }
        if let Err(why) = self.save(inner) {
            inner.entries = persisted;
            return Err(ChangeError::storage(why, true));
        }
        Ok(())
    }

    /// Keep `value` as `name` used as `bindings`, both at once: the use is
    /// checked against the new value first, and nothing is written when
    /// either is refused, so a secret never has its new value under its old
    /// use, nor a refused value at all.
    fn put_with_bindings_locked(
        &self,
        inner: &mut Inner,
        scope: &ProviderScope,
        name: &str,
        description: &str,
        value: &str,
        bindings: Vec<Binding>,
    ) -> Result<(), ChangeError> {
        vault::check_name(name).map_err(ChangeError::invalid)?;
        let clean = vault::clean_value(value).map_err(ChangeError::invalid)?;
        check_bindings(scope, &bindings, Some(&clean)).map_err(ChangeError::invalid)?;
        self.put_locked(inner, scope, name, description, &clean, Some(bindings))
    }

    /// Resolve a host-only binding by exact project and name, without global fallback.
    ///
    /// # Errors
    /// When the project has no such secret with that use.
    pub fn works_credential(&self, project: &str, name: &str) -> Result<String, String> {
        self.host_secret(project, name, &Binding::AgenticWorks)
            .map_err(|()| {
                "Choose a project secret with the agentic_works binding in Settings > Secrets."
                    .to_owned()
            })?
    }

    /// The service account key JSON kept for `project` under `name`.
    ///
    /// # Errors
    /// When the project has no such key.
    pub fn google_key(&self, project: &str, name: &str) -> Result<String, String> {
        self.host_secret(project, name, &Binding::GoogleServiceAccount)
            .map_err(|()| {
                format!("The service account key {name} is not in this project's secrets: import it again.")
            })?
    }

    /// Names of `project`'s own secrets used as `binding`, sorted.
    #[must_use]
    pub fn host_secrets(&self, project: &str, binding: &Binding) -> Vec<String> {
        let scope = ProviderScope::Project(project.to_owned());
        let mut names: Vec<String> = self
            .lock()
            .entries
            .iter()
            .filter(|e| e.scope == scope && e.bindings.len() == 1 && &e.bindings[0] == binding)
            .map(|e| e.name.clone())
            .collect();
        names.sort();
        names
    }

    /// Store `value` as `project`'s secret `name`, used only as `binding`
    /// (a host-only use), in one step.
    ///
    /// # Errors
    /// Why it was not kept; never the value.
    pub fn put_host_secret(
        &self,
        project: &str,
        name: &str,
        description: &str,
        value: &str,
        binding: Binding,
    ) -> Result<(), String> {
        if !binding.host_only() {
            return Err("Only a host-only use can be set this way.".to_owned());
        }
        if binding == Binding::GoogleServiceAccount {
            check_service_account_key(value)?;
        }
        let scope = ProviderScope::Project(project.to_owned());
        let mut inner = self.lock();
        // Never put a host-only value behind another use: a secret of that
        // name must be new, or already this use.
        if inner
            .entries
            .iter()
            .any(|e| e.scope == scope && e.name == name && e.bindings != [binding.clone()])
        {
            return Err(format!(
                "This project already has a secret named {name} with another use: rename or remove it in Settings → Secrets first."
            ));
        }
        // One save: a new entry is never on disk without its use.
        let result = self.put_locked(
            &mut inner,
            &scope,
            name,
            description,
            value,
            Some(vec![binding]),
        );
        self.refresh(&mut inner);
        result.map_err(String::from)
    }

    fn host_secret(
        &self,
        project: &str,
        name: &str,
        binding: &Binding,
    ) -> Result<Result<String, String>, ()> {
        let scope = ProviderScope::Project(project.to_owned());
        let inner = self.lock();
        let entry = inner
            .entries
            .iter()
            .find(|e| {
                e.scope == scope && e.name == name && e.bindings.as_slice() == [binding.clone()]
            })
            .ok_or(())?;
        Ok(inner
            .values
            .get(&store_key(&scope, &entry.name))
            .cloned()
            .ok_or_else(|| "The project credential is unavailable.".into()))
    }

    fn set_bindings_locked(
        &self,
        inner: &mut Inner,
        scope: &ProviderScope,
        name: &str,
        bindings: Vec<Binding>,
    ) -> Result<(), ChangeError> {
        let missing = || ChangeError::invalid(format!("There is no secret named {name} here."));
        Self::writable(inner).map_err(|why| ChangeError::storage(why, false))?;
        if !inner
            .entries
            .iter()
            .any(|e| &e.scope == scope && e.name == name)
        {
            return Err(missing());
        }
        let value = inner
            .values
            .get(&store_key(scope, name))
            .map(String::as_str);
        check_bindings(scope, &bindings, value).map_err(ChangeError::invalid)?;
        let Some(entry) = inner
            .entries
            .iter_mut()
            .find(|e| &e.scope == scope && e.name == name)
        else {
            return Err(missing());
        };
        let before = std::mem::replace(&mut entry.bindings, bindings);
        if let Err(why) = self.save(inner) {
            if let Some(entry) = inner
                .entries
                .iter_mut()
                .find(|e| &e.scope == scope && e.name == name)
            {
                entry.bindings = before;
            }
            return Err(ChangeError::storage(why, false));
        }
        Ok(())
    }
}

/// A request's number: random, above 0, and within what every JSON reader
/// keeps exactly (2^53).
fn random_ask_id() -> Result<u64, String> {
    let mut raw = [0_u8; 8];
    getrandom::fill(&mut raw).map_err(|err| format!("no random bytes: {err}"))?;
    Ok((u64::from_le_bytes(raw) & ((1 << 53) - 1)).max(1))
}

/// Whether `bindings` may be the use of a secret of `scope` whose value is
/// `value` (`None`: it cannot be opened). Checked before anything is written.
fn check_bindings(
    scope: &ProviderScope,
    bindings: &[Binding],
    value: Option<&str>,
) -> Result<(), String> {
    if bindings.iter().any(Binding::host_only) && (bindings.len() != 1 || scope.project().is_none())
    {
        return Err("Agentic Works requires an exclusive project-scoped binding.".into());
    }
    for binding in bindings {
        check_binding(binding)?;
    }
    if bindings.contains(&Binding::GoogleServiceAccount) {
        check_service_account_key(value.ok_or("The key cannot be opened.")?)?;
    }
    if bindings.contains(&Binding::Ssh) {
        agent::parse_private(value.ok_or("The key cannot be opened.")?)?;
    }
    Ok(())
}

/// Whether a service account key's `token_uri` is Google's token endpoint
/// (today's, or the one older keys name). A key is signed into an assertion
/// sent there, so no other address is accepted.
#[must_use]
pub fn google_token_uri_ok(url: &str) -> bool {
    matches!(
        url,
        "https://oauth2.googleapis.com/token" | "https://accounts.google.com/o/oauth2/token"
    )
}

/// Check a Google service account key (its JSON, as downloaded) and return
/// the account's email. The key is never part of the message.
///
/// # Errors
/// What is wrong with it, for the person.
pub fn check_service_account_key(json: &str) -> Result<String, String> {
    let value: serde_json::Value = serde_json::from_str(json)
        .map_err(|_| "This is not a service account key file (it is not JSON).".to_owned())?;
    let text = |field: &str| {
        value
            .get(field)
            .and_then(serde_json::Value::as_str)
            .filter(|s| !s.trim().is_empty())
    };
    if text("type") != Some("service_account") {
        return Err(
            "This is not a service account key file (its type is not service_account).".to_owned(),
        );
    }
    let email = text("client_email")
        .filter(|e| e.contains('@'))
        .ok_or("The key file has no client_email.")?;
    if !text("private_key").is_some_and(|k| k.contains("PRIVATE KEY")) {
        return Err("The key file has no private_key.".to_owned());
    }
    if !text("token_uri").is_some_and(google_token_uri_ok) {
        return Err("The key file's token_uri is not Google's token endpoint.".to_owned());
    }
    Ok(email.to_owned())
}

fn now() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |since| since.as_secs())
}

/// What the asking terminal is told once its secret is set: how to use it.
fn how_to_use(name: &str, bindings: &[Binding]) -> String {
    let mut ways: Vec<String> = Vec::new();
    for binding in bindings {
        ways.push(match binding {
            Binding::Http { .. } => format!(
                "web requests: `connecto secret url {name}` gives the address to send them to"
            ),
            Binding::Command { .. } => {
                format!("a program: `connecto secret run {name} -- <program> [args]`")
            }
            Binding::AgenticWorks => "host-only PostgreSQL queue".into(),
            Binding::GoogleServiceAccount => "host-only Google Cloud SQL access".into(),
            Binding::Ssh => {
                format!("SSH: `connecto secret ssh {name}` gives SSH_AUTH_SOCK and the public key")
            }
        });
    }
    if ways.is_empty() {
        "The person has not allowed any use of it yet.".to_owned()
    } else {
        format!("Use it through {}.", ways.join("; "))
    }
}

impl Vault {
    fn put_c(
        &self,
        scope: &ProviderScope,
        name: &str,
        description: &str,
        value: &str,
    ) -> Result<(), ChangeError> {
        let mut inner = self.lock();
        let result = self.put_locked(&mut inner, scope, name, description, value, None);
        self.refresh(&mut inner);
        result
    }

    fn set_bindings_c(
        &self,
        scope: &ProviderScope,
        name: &str,
        bindings: Vec<Binding>,
    ) -> Result<(), ChangeError> {
        let mut inner = self.lock();
        let result = self.set_bindings_locked(&mut inner, scope, name, bindings);
        self.refresh(&mut inner);
        result
    }

    fn delete_c(&self, scope: &ProviderScope, name: &str) -> Result<(), ChangeError> {
        let mut inner = self.lock();
        Self::writable(&inner).map_err(|why| ChangeError::storage(why, false))?;
        let Some(index) = inner
            .entries
            .iter()
            .position(|e| &e.scope == scope && e.name == name)
        else {
            return Ok(());
        };
        let removed = inner.entries.remove(index);
        // The entry first: a value no entry names is never used, and the
        // next put of the name replaces it.
        if let Err(why) = self.save(&inner) {
            inner.entries.insert(index, removed);
            return Err(ChangeError::storage(why, false));
        }
        let key = store_key(scope, name);
        inner.values.remove(&key);
        let deleted = self.secrets.delete(&key);
        self.refresh(&mut inner);
        drop(inner);
        agent::close_for(self, scope, name);
        // The entry is gone either way.
        deleted.map_err(|why| ChangeError::storage(why, true))
    }

    /// One write: the key and its use change together, so the new key is
    /// never under the secret's old use (a web address or a program). Open
    /// sockets read the key at each request: they sign with the new one.
    fn generate_c(
        &self,
        scope: &ProviderScope,
        name: &str,
        description: &str,
    ) -> Result<(), ChangeError> {
        vault::check_name(name).map_err(ChangeError::invalid)?;
        let made = agent::generate(&format!("connecto {name}"))
            .map_err(|why| ChangeError::storage(why, false))?;
        let mut inner = self.lock();
        let result = self.put_with_bindings_locked(
            &mut inner,
            scope,
            name,
            description,
            &made.private_openssh,
            vec![Binding::Ssh],
        );
        self.refresh(&mut inner);
        result
    }

    fn answer_c(&self, ask: u64, answer: AskAnswer) -> Result<(), ChangeError> {
        let mut inner = self.lock();
        let index = inner
            .asks
            .iter()
            .position(|p| p.ask.id == ask)
            .ok_or_else(|| ChangeError::invalid("That request is no longer waiting.".to_owned()))?;
        let asked = inner.asks[index].ask.clone();
        // A key made here: its private half never leaves this vault.
        let given = match answer {
            AskAnswer::Decline => None,
            AskAnswer::GenerateSshKey { scope } => Some((
                scope,
                agent::generate(&format!("connecto {}", asked.name))
                    .map_err(|why| ChangeError::storage(why, false))?
                    .private_openssh,
                vec![Binding::Ssh],
            )),
            AskAnswer::Provide {
                scope,
                value,
                bindings,
            } => Some((scope, value, bindings)),
        };
        let told = match given {
            None => format!("The person declined to give the secret {}.", asked.name),
            Some((scope, value, bindings)) => {
                // Only where the asking terminal can see it.
                let fits = match (&scope, &asked.project) {
                    (ProviderScope::Global, _) => true,
                    (ProviderScope::Project(id), Some(project)) => id == project,
                    (ProviderScope::Project(_), None) => false,
                };
                if !fits {
                    return Err(ChangeError::invalid(
                        "A secret for this request goes app-wide or in the asking terminal's project."
                            .to_owned(),
                    ));
                }
                let reason: String = asked.reason.chars().take(MAX_DESCRIPTION).collect();
                let put = self.put_with_bindings_locked(
                    &mut inner,
                    &scope,
                    &asked.name,
                    &reason,
                    &value,
                    bindings.clone(),
                );
                if let Err(why) = put {
                    self.refresh(&mut inner);
                    return Err(why);
                }
                format!(
                    "The person set the secret {}. {}",
                    asked.name,
                    how_to_use(&asked.name, &bindings)
                )
            }
        };
        let pending = inner.asks.remove(index);
        self.refresh(&mut inner);
        drop(inner);
        self.changed();
        if let Some(reply) = pending.reply {
            reply(told);
        }
        Ok(())
    }
}

impl VaultControl for Vault {
    fn view(&self) -> Arc<VaultView> {
        Arc::clone(&self.lock().view)
    }

    fn put(
        &self,
        scope: &ProviderScope,
        name: &str,
        description: &str,
        value: &str,
    ) -> Result<(), String> {
        self.put_c(scope, name, description, value)
            .map_err(String::from)
    }

    fn set_bindings(
        &self,
        scope: &ProviderScope,
        name: &str,
        bindings: Vec<Binding>,
    ) -> Result<(), String> {
        self.set_bindings_c(scope, name, bindings)
            .map_err(String::from)
    }

    fn delete(&self, scope: &ProviderScope, name: &str) -> Result<(), String> {
        self.delete_c(scope, name).map_err(String::from)
    }

    fn answer(&self, ask: u64, answer: AskAnswer) -> Result<(), String> {
        self.answer_c(ask, answer).map_err(String::from)
    }

    fn change(&self, change: VaultChange) -> Result<(), ChangeError> {
        match change {
            VaultChange::Put {
                scope,
                name,
                description,
                value,
            } => self.put_c(&scope, &name, &description, &value),
            VaultChange::SetBindings {
                scope,
                name,
                bindings,
            } => self.set_bindings_c(&scope, &name, bindings),
            VaultChange::Delete { scope, name } => self.delete_c(&scope, &name),
            VaultChange::GenerateKey {
                scope,
                name,
                description,
            } => self.generate_c(&scope, &name, &description),
            VaultChange::Answer { ask, answer } => self.answer_c(ask, answer),
        }
    }

    fn generate_ssh_key(&self, comment: &str) -> Result<GeneratedKey, String> {
        agent::generate(comment)
    }

    /// One write: the key and its use change together, so the new key is
    /// never under the secret's old use (a web address or a program).
    fn put_generated_key(
        &self,
        scope: &ProviderScope,
        name: &str,
        description: &str,
    ) -> Result<(), String> {
        self.generate_c(scope, name, description)
            .map_err(String::from)
    }

    fn public_key(&self, scope: &ProviderScope, name: &str) -> Option<String> {
        let inner = self.lock();
        let value = inner.values.get(&store_key(scope, name))?;
        agent::parse_private(value)
            .ok()?
            .public_key()
            .to_openssh()
            .ok()
    }

    /// Every change is kept until the window has taken it: none is
    /// dropped, however many come while another notice is open.
    fn note_remote_change(&self, what: String) {
        let mut inner = self.lock();
        inner.last_change += 1;
        let number = inner.last_change;
        inner.remote_changes.push((number, what));
        self.refresh(&mut inner);
        drop(inner);
        self.changed();
    }

    fn ack_remote_changes(&self, upto: u64) {
        let mut inner = self.lock();
        inner.remote_changes.retain(|(n, _)| *n > upto);
        self.refresh(&mut inner);
    }
}

impl VaultBridge for Vault {
    fn visible(&self, project: Option<&str>) -> Vec<SecretEntry> {
        self.lock()
            .view
            .visible_to(project)
            .into_iter()
            .cloned()
            .collect()
    }

    fn known(&self) -> Arc<[String]> {
        Arc::clone(&self.lock().known)
    }

    fn ask(
        &self,
        caller: &SecretCaller,
        request: AskRequest,
        reply: AskReply,
    ) -> Result<u64, String> {
        vault::check_name(&request.name)?;
        for binding in &request.proposed {
            check_binding(binding)?;
            // As it will reach the person's form: one path per line.
            vault::check_proposed(binding)?;
        }
        // One line the dialog can quote, checked as shown: an overlong one is
        // refused, so nothing checked is ever cut off.
        vault::check_reason(&request.reason, &request.name)?;
        let reason = vault::display_line(&request.reason, MAX_REASON);
        let mut inner = self.lock();
        if inner.asks.len() >= MAX_ASKS
            || inner
                .asks
                .iter()
                .filter(|p| p.ask.terminal == caller.terminal)
                .count()
                >= MAX_ASKS_PER_TERMINAL
        {
            return Err("Too many requests are waiting for the person already.".to_owned());
        }
        if inner
            .asks
            .iter()
            .any(|p| p.ask.terminal == caller.terminal && p.ask.name == request.name)
        {
            return Err(format!(
                "A request for {} from this terminal is already waiting.",
                request.name
            ));
        }
        // Random, never a count: an answer given before this app started
        // (a dialog or a queued answer of a paired machine) cannot meet a
        // new request.
        let id = loop {
            let id = random_ask_id()?;
            if !inner.asks.iter().any(|p| p.ask.id == id) {
                break id;
            }
        };
        inner.asks.push(PendingAsk {
            ask: SecretAsk {
                id,
                terminal: caller.terminal.clone(),
                project: caller.project.clone(),
                name: request.name,
                kind: request.kind,
                reason,
                proposed: request.proposed,
                proposed_omitted: false,
            },
            reply: Some(reply),
        });
        self.refresh(&mut inner);
        drop(inner);
        self.changed();
        Ok(id)
    }

    fn http_url(&self, caller: &SecretCaller, name: &str) -> Result<String, String> {
        let entry = self
            .visible(caller.project.as_deref())
            .into_iter()
            .find(|e| e.name == name)
            .ok_or_else(|| format!("There is no secret named {name} for this terminal."))?;
        if !entry
            .bindings
            .iter()
            .any(|b| matches!(b, Binding::Http { .. }))
        {
            return Err(format!("{name} is not for web requests."));
        }
        let port = self.brokers()?.proxy_port;
        let token = self.token_for(caller)?;
        Ok(format!("http://127.0.0.1:{port}/{token}/{name}/"))
    }

    fn run(
        &self,
        caller: &SecretCaller,
        name: &str,
        program: &str,
        args: &[String],
        cwd: Option<&Path>,
    ) -> Result<RunOutput, String> {
        run::run(self, caller, name, program, args, cwd)
    }

    fn ssh(&self, caller: &SecretCaller, name: &str) -> Result<SshAccess, String> {
        agent::access(self, caller, name)
    }
}

/// Project ids go into a store key, so they stay plain.
fn check_project_id(id: &str) -> Result<(), String> {
    if id.is_empty()
        || id.len() > 128
        || !id
            .chars()
            .all(|ch| ch.is_ascii_alphanumeric() || matches!(ch, '_' | '-'))
    {
        return Err("That project cannot keep secrets of its own.".to_owned());
    }
    Ok(())
}

/// A scope as the metadata file writes it.
fn scope_json(scope: &ProviderScope) -> Value {
    match scope {
        ProviderScope::Global => json!("global"),
        ProviderScope::Project(id) => json!({ "project": id }),
    }
}

#[cfg(all(test, unix))]
mod tests;
