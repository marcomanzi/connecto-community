//! Pi's model providers: the keys in Connecto's secret store, the address of
//! the local LM Studio server in a small file, and both handed to the launcher
//! (`connecto_session::launch::set_pi_providers`) at start and at every change.
//!
//! A project whose Pi config is Isolated can have providers of its own
//! ([`ProviderScope::Project`]): its keys are in the secret store under names
//! of the project's own, and its address and default model in the project's
//! settings file (`connecto_session::project_settings::update_providers`). Its
//! terminals get them at launch through `crate::model_providers`.
//!
//! A key is never written anywhere else: not into Pi's own `auth.json`, not
//! into a settings file, a log or a view. A Pi terminal gets it in its
//! environment, the way Pi's own documentation has a key passed in.

use std::fs;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use connecto_core::ProviderScope;
use connecto_core::pi::{
    DEFAULT_LMSTUDIO_URL, PiLocalState, PiProvider, PiProvidersView, clean_api_key,
    clean_lmstudio_url,
};
use connecto_session::launch::set_pi_providers;
use connecto_session::{PiProviders, Wake};

/// In the data folder: the LM Studio address. Absent means the default one;
/// empty means the local server is not used.
pub const LMSTUDIO_FILE: &str = "pi-lmstudio-url";

/// Where the keys are kept.
pub trait PiKeys: Send + Sync {
    fn get(&self, name: &str) -> Result<Option<String>, String>;
    fn set(&self, name: &str, key: &str) -> Result<(), String>;
    fn delete(&self, name: &str) -> Result<(), String>;
}

/// Connecto's secret store (the secrets file, or the system store when the
/// person chose it).
#[cfg_attr(test, allow(dead_code))]
pub struct StoredKeys;

impl PiKeys for StoredKeys {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        connecto_secrets::Secrets::system().get(name)
    }

    fn set(&self, name: &str, key: &str) -> Result<(), String> {
        connecto_secrets::Secrets::system().set(name, key)
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        connecto_secrets::Secrets::system().delete(name)
    }
}

/// The secret's name for a provider's key: the app-wide one, or one of the
/// project's own (`project.<id>.pi.<provider>-api-key`).
#[must_use]
pub fn pi_secret(scope: &ProviderScope, provider: PiProvider) -> String {
    match scope {
        ProviderScope::Global => match provider {
            PiProvider::OpenRouter => connecto_secrets::PI_OPENROUTER_API_KEY,
            PiProvider::Together => connecto_secrets::PI_TOGETHER_API_KEY,
        }
        .to_owned(),
        ProviderScope::Project(id) => format!("project.{id}.pi.{}-api-key", provider.id()),
    }
}

/// Asks a local server what it has. A fake stands in for tests.
pub trait LocalProbe: Send + Sync {
    /// How many chat models the server at `url` offers, or `None` when
    /// nothing answers there as LM Studio's server does.
    fn chat_models(&self, url: &str) -> Option<usize>;
    /// The port LM Studio says its server is on, when it runs.
    fn lms_port(&self, home: &Path) -> Option<u16>;
    /// Every model Pi offers with these providers, as `provider/model`, asked
    /// of Pi itself. `None` when Pi is not installed or did not answer.
    fn list_models(&self, request: &ListRequest) -> Option<Vec<String>>;
}

/// What asking Pi for its models needs.
#[cfg_attr(test, allow(dead_code))]
pub struct ListRequest {
    pub home: PathBuf,
    pub data_root: PathBuf,
    /// The providers' keys, as Pi reads them from its environment.
    pub env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
    pub lmstudio_url: Option<String>,
}

/// In the data folder: the model a new Pi terminal starts on. Absent leaves
/// the choice to Pi.
pub const DEFAULT_MODEL_FILE: &str = "pi-default-model";

/// How long Pi may take to list its models.
#[cfg_attr(test, allow(dead_code))]
const LIST_LIMIT: Duration = Duration::from_secs(45);

/// The models in what `pi --list-models` prints: a table whose first two
/// columns are the provider and the model, under a header line.
fn parse_model_list(printed: &str) -> Vec<String> {
    printed
        .lines()
        .skip_while(|line| !line.trim_start().starts_with("provider"))
        .skip(1)
        .filter_map(|line| {
            let mut words = line.split_whitespace();
            let (provider, model) = (words.next()?, words.next()?);
            let plain = |word: &str| {
                word.bytes().all(|b| {
                    b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'/' | b':')
                })
            };
            (plain(provider) && plain(model)).then(|| format!("{provider}/{model}"))
        })
        .collect()
}

/// How long one question to the local server, or to `lms`, may take.
#[cfg_attr(test, allow(dead_code))]
const PROBE_LIMIT: Duration = Duration::from_secs(4);

/// The real thing: `curl` for the server, LM Studio's `lms` for the port.
#[cfg_attr(test, allow(dead_code))]
pub struct SystemProbe;

/// Run `command` and give what it printed, if it ended well within the limit.
#[cfg_attr(test, allow(dead_code))]
pub(crate) fn output_within(
    mut command: std::process::Command,
    limit: Duration,
) -> Option<Vec<u8>> {
    use std::io::Read;
    let mut child = command
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()
        .ok()?;
    let mut pipe = child.stdout.take()?;
    let reader = std::thread::spawn(move || {
        let mut out = Vec::new();
        let _ = (&mut pipe).take(4 * 1024 * 1024).read_to_end(&mut out);
        out
    });
    let deadline = Instant::now() + limit;
    let ended_well = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Ok(None) if Instant::now() < deadline => {
                std::thread::sleep(Duration::from_millis(20));
            }
            _ => {
                let _ = child.kill();
                let _ = child.wait();
                break false;
            }
        }
    };
    let out = reader.join().ok()?;
    ended_well.then_some(out)
}

/// The chat models in an OpenAI-style model list: every entry with an id
/// that is not an embedding model.
fn count_chat_models(body: &[u8]) -> Option<usize> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let models = value.get("data")?.as_array()?;
    Some(
        models
            .iter()
            .filter_map(|model| model.get("id")?.as_str())
            .filter(|id| !id.to_lowercase().contains("embed"))
            .count(),
    )
}

/// The smallest context a model must take to hold Pi's own instructions and
/// tools. As in the extension, which lists the models to Pi.
const MIN_CONTEXT: u64 = 8192;

/// The models Pi can use in LM Studio's own listing, which says what each
/// one is: not an embedding model, not one of LM Studio's helper models
/// (speech and the like, listed as chat models), not one too small for Pi.
fn count_usable_models(body: &[u8]) -> Option<usize> {
    let value: serde_json::Value = serde_json::from_slice(body).ok()?;
    let models = value.get("data")?.as_array()?;
    Some(
        models
            .iter()
            .filter(|model| model.get("id").is_some_and(serde_json::Value::is_string))
            .filter(|model| model["type"] != "embeddings")
            .filter(|model| model["publisher"] != "model-extension-pack")
            .filter(|model| {
                model["max_context_length"]
                    .as_u64()
                    .or_else(|| model["loaded_context_length"].as_u64())
                    .is_none_or(|context| context >= MIN_CONTEXT)
            })
            .count(),
    )
}

impl LocalProbe for SystemProbe {
    fn chat_models(&self, url: &str) -> Option<usize> {
        let ask = |path: &str| {
            let mut curl = crate::platform::curl();
            curl.args(["--silent", "--fail", "--max-time", "3"])
                .arg(format!("{url}{path}"));
            output_within(curl, PROBE_LIMIT)
        };
        // LM Studio's own listing first; any OpenAI-compatible server has
        // the plain one.
        ask("/api/v0/models")
            .and_then(|body| count_usable_models(&body))
            .or_else(|| count_chat_models(&ask("/v1/models")?))
    }

    fn lms_port(&self, home: &Path) -> Option<u16> {
        let lms = home
            .join(".lmstudio/bin")
            .join(format!("lms{}", std::env::consts::EXE_SUFFIX));
        let mut command = std::process::Command::new(lms);
        command.args(["server", "status", "--json"]);
        let out = output_within(command, PROBE_LIMIT)?;
        let line = String::from_utf8_lossy(&out);
        let status: serde_json::Value = serde_json::from_str(line.lines().last()?).ok()?;
        (status.get("running")?.as_bool()?)
            .then(|| status.get("port")?.as_u64())
            .flatten()
            .and_then(|port| u16::try_from(port).ok())
    }

    fn list_models(&self, request: &ListRequest) -> Option<Vec<String>> {
        let pi = connecto_store::Store::find_installed(connecto_store::Tool::Pi, &request.home)?;
        // The extension offers the local server's models, as in a terminal.
        // Each listing has a folder of its own: the app-wide host and the
        // projects' can list at once, each with its own server.
        static LISTING: AtomicU64 = AtomicU64::new(0);
        let work = listing_dir(&request.data_root, LISTING.fetch_add(1, Ordering::Relaxed));
        fs::create_dir_all(&work).ok()?;
        crate::platform::make_private(&work);
        let extension = work.join("connecto-pi.mjs");
        let listed = fs::write(
            &extension,
            connecto_adapter::pi_extension_for_listing(request.lmstudio_url.as_deref()),
        )
        .ok()
        .and_then(|()| {
            let mut command = std::process::Command::new(&pi);
            command
                .arg("--extension")
                .arg(&extension)
                .arg("--list-models")
                .env_clear()
                .env("HOME", &request.home)
                .env("PATH", connecto_store::run_path(&pi, &request.home))
                .envs(connecto_store::launcher_env(&request.home))
                .env("TERM", "dumb")
                .env("PI_SKIP_VERSION_CHECK", "1")
                .envs(request.env.iter().cloned())
                .current_dir(&work);
            output_within(command, LIST_LIMIT)
        });
        let _ = fs::remove_dir_all(&work);
        Some(parse_model_list(&String::from_utf8_lossy(&listed?)))
    }
}

/// The private folder of one listing of Pi's models.
fn listing_dir(data_root: &Path, listing: u64) -> PathBuf {
    data_root
        .join("run")
        .join(format!("pi-models-{}-{listing}", std::process::id()))
}

/// A model as Pi names one: `provider/model`, plain characters only (it goes
/// on a command line), at most 64, as the terminal list takes it.
fn valid_model_name(model: &str) -> bool {
    (1..=64).contains(&model.len())
        && model.as_bytes()[0].is_ascii_alphanumeric()
        && model.bytes().all(|b| {
            b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'[' | b']' | b'/' | b':')
        })
}

/// `http://<host>` when `url` names this computer, to try another port on.
fn local_host(url: &str) -> Option<&str> {
    let rest = url.strip_prefix("http://")?;
    let host = rest.split(['/', ':']).next()?;
    matches!(host, "localhost" | "127.0.0.1").then(|| &url[..("http://".len() + host.len())])
}

pub struct PiProvidersHost {
    /// Whose providers: the app's, or one project's.
    scope: ProviderScope,
    data_root: PathBuf,
    home: PathBuf,
    keys: Arc<dyn PiKeys>,
    probe: Arc<dyn LocalProbe>,
    view: Mutex<PiProvidersView>,
    /// Counts the checks of the local server, so a slow one that ends after a
    /// newer one began changes nothing.
    checks: AtomicU64,
    /// The same for the listings of Pi's models.
    listings: AtomicU64,
    wake: Wake,
}

impl PiProvidersHost {
    /// Read what is saved, give it to the launcher, and start asking the
    /// local server whether it is there. Reads the secret store, so call it
    /// once the store is set up, off the UI thread.
    pub fn start(
        data_root: &Path,
        home: &Path,
        keys: Arc<dyn PiKeys>,
        probe: Arc<dyn LocalProbe>,
        wake: Wake,
    ) -> Arc<Self> {
        Self::start_for(ProviderScope::Global, data_root, home, keys, probe, wake)
    }

    /// As [`PiProvidersHost::start`], for `scope`'s providers. Only the
    /// app-wide ones are handed to the launcher here; a project's are asked
    /// for at each launch ([`PiProvidersHost::launch_providers`]).
    pub fn start_for(
        scope: ProviderScope,
        data_root: &Path,
        home: &Path,
        keys: Arc<dyn PiKeys>,
        probe: Arc<dyn LocalProbe>,
        wake: Wake,
    ) -> Arc<Self> {
        let host = Self::idle(scope, data_root, home, keys, probe, wake);
        host.apply();
        host.check_local();
        host.list_models();
        host
    }

    /// The host without its checks: nothing is asked of the local server or
    /// of Pi, and nothing is handed to the launcher. For reading what a
    /// project's terminals are given at launch.
    pub fn idle(
        scope: ProviderScope,
        data_root: &Path,
        home: &Path,
        keys: Arc<dyn PiKeys>,
        probe: Arc<dyn LocalProbe>,
        wake: Wake,
    ) -> Arc<Self> {
        Arc::new(Self {
            scope,
            data_root: data_root.to_owned(),
            home: home.to_owned(),
            keys,
            probe,
            view: Mutex::new(PiProvidersView::default()),
            checks: AtomicU64::new(0),
            listings: AtomicU64::new(0),
            wake,
        })
    }

    /// The project's own provider settings (a project scope only).
    fn project_settings(
        &self,
    ) -> Option<(&str, connecto_session::project_settings::ProjectProviders)> {
        let id = self.scope.project()?;
        Some((
            id,
            connecto_session::project_settings::load_providers(&self.data_root, id),
        ))
    }

    fn saved_default_model(&self) -> Option<String> {
        let text = match self.project_settings() {
            Some((_, saved)) => saved.pi_default_model?,
            None => fs::read_to_string(self.data_root.join(DEFAULT_MODEL_FILE)).ok()?,
        };
        let model = text.trim();
        valid_model_name(model).then(|| model.to_owned())
    }

    /// Ask Pi, in the background, for every model it offers with the
    /// providers as they are now.
    pub fn list_models(self: &Arc<Self>) {
        let listing = self.listings.fetch_add(1, Ordering::SeqCst) + 1;
        let request = {
            let mut view = self.lock();
            view.models_loading = true;
            ListRequest {
                home: self.home.clone(),
                data_root: self.data_root.clone(),
                env: self.key_env().0,
                lmstudio_url: (!view.lmstudio_url.is_empty()).then(|| view.lmstudio_url.clone()),
            }
        };
        (self.wake)();
        let me = Arc::clone(self);
        let run = move || {
            let models = me.probe.list_models(&request);
            if me.listings.load(Ordering::SeqCst) != listing {
                return;
            }
            {
                let mut view = me.lock();
                view.models_loading = false;
                if let Some(models) = models {
                    view.models = models;
                }
            }
            // The default goes to the launcher only while Pi offers it.
            me.apply();
            (me.wake)();
        };
        if std::thread::Builder::new()
            .name("pi-models".to_owned())
            .spawn(run)
            .is_err()
        {
            self.lock().models_loading = false;
        }
    }

    /// The keys as Pi's environment, which providers have one, and whether
    /// the store could not be read.
    #[allow(clippy::type_complexity)]
    fn key_env(
        &self,
    ) -> (
        Vec<(std::ffi::OsString, std::ffi::OsString)>,
        Vec<PiProvider>,
        bool,
    ) {
        let mut env = Vec::new();
        let mut keys_set = Vec::new();
        let mut unreadable = false;
        for provider in PiProvider::ALL {
            match self.keys.get(&pi_secret(&self.scope, provider)) {
                Ok(Some(key)) if !key.is_empty() => {
                    env.push((provider.env_var().into(), key.into()));
                    keys_set.push(provider);
                }
                Ok(_) => {}
                Err(_) => unreadable = true,
            }
        }
        (env, keys_set, unreadable)
    }

    /// Set (`Some`) or clear (`None`) the model a new Pi terminal starts on.
    pub fn set_default_model(&self, model: Option<&str>) -> Result<(), String> {
        if let Some(id) = self.scope.project() {
            let model = model.map(str::trim);
            if model.is_some_and(|model| !valid_model_name(model)) {
                return Err("That is not a model name.".to_owned());
            }
            // Only this field, read and written under the settings' lock.
            connecto_session::project_settings::update_providers(&self.data_root, id, |saved| {
                saved.pi_default_model = model.map(str::to_owned);
            })
            .map_err(|err| format!("The model could not be saved: {err}"))?;
            self.apply();
            return Ok(());
        }
        let file = self.data_root.join(DEFAULT_MODEL_FILE);
        let saved = match model.map(str::trim) {
            Some(model) => {
                if !valid_model_name(model) {
                    return Err("That is not a model name.".to_owned());
                }
                fs::create_dir_all(&self.data_root).and_then(|()| fs::write(&file, model))
            }
            None => match fs::remove_file(&file) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            },
        };
        saved.map_err(|err| format!("The model could not be saved: {err}"))?;
        self.apply();
        Ok(())
    }

    fn lock(&self) -> MutexGuard<'_, PiProvidersView> {
        self.view.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Read again what is saved (the project chose, or stopped using, its
    /// own providers).
    pub fn reload(&self) {
        self.apply();
    }

    pub fn view(&self) -> PiProvidersView {
        self.lock().clone()
    }

    fn saved_url(&self) -> String {
        if let Some((_, saved)) = self.project_settings() {
            return saved.pi_lmstudio_url.map_or_else(
                || DEFAULT_LMSTUDIO_URL.to_owned(),
                |text| {
                    clean_lmstudio_url(&text).unwrap_or_else(|_| DEFAULT_LMSTUDIO_URL.to_owned())
                },
            );
        }
        match fs::read_to_string(self.data_root.join(LMSTUDIO_FILE)) {
            // A file someone edited into nonsense is as good as the default.
            Ok(text) => {
                clean_lmstudio_url(&text).unwrap_or_else(|_| DEFAULT_LMSTUDIO_URL.to_owned())
            }
            Err(_) => DEFAULT_LMSTUDIO_URL.to_owned(),
        }
    }

    fn save_url(&self, url: &str) -> std::io::Result<()> {
        if let Some(id) = self.scope.project() {
            // Only this field, read and written under the settings' lock: a
            // server found in the background never undoes another choice.
            return connecto_session::project_settings::update_providers(
                &self.data_root,
                id,
                |saved| {
                    // The default needs no entry.
                    saved.pi_lmstudio_url = (url != DEFAULT_LMSTUDIO_URL).then(|| url.to_owned());
                },
            );
        }
        let file = self.data_root.join(LMSTUDIO_FILE);
        if url == DEFAULT_LMSTUDIO_URL {
            // The default needs no file.
            match fs::remove_file(&file) {
                Err(err) if err.kind() != std::io::ErrorKind::NotFound => Err(err),
                _ => Ok(()),
            }
        } else {
            fs::create_dir_all(&self.data_root).and_then(|()| fs::write(&file, url))
        }
    }

    /// What a Pi terminal with these providers is given at launch, as they
    /// are saved now.
    pub fn launch_providers(&self) -> PiProviders {
        let lmstudio_url = self.saved_url();
        let (env, _, _) = self.key_env();
        let default_model = self.saved_default_model();
        let view = self.lock();
        Self::for_launch(&view, env, &lmstudio_url, default_model)
    }

    /// A default Pi no longer offers (its provider's key was removed, the
    /// local model deleted) would stop every new terminal at once: it is
    /// kept, shown, and not used until Pi offers it again.
    fn for_launch(
        view: &PiProvidersView,
        env: Vec<(std::ffi::OsString, std::ffi::OsString)>,
        lmstudio_url: &str,
        default_model: Option<String>,
    ) -> PiProviders {
        let offered = view.models.is_empty()
            || default_model
                .as_ref()
                .is_some_and(|model| view.models.contains(model));
        PiProviders {
            env,
            lmstudio_url: (!lmstudio_url.is_empty()).then(|| lmstudio_url.to_owned()),
            default_model: default_model.filter(|_| offered),
        }
    }

    /// Read the keys and the address, publish the view, tell the launcher
    /// (the app-wide providers). What is known about the local server stays
    /// while its address does.
    fn apply(&self) {
        let lmstudio_url = self.saved_url();
        let (env, keys_set, keys_unreadable) = self.key_env();
        let default_model = self.saved_default_model();
        let own = self
            .project_settings()
            .is_none_or(|(_, saved)| saved.pi_own);
        let mut view = self.lock();
        let launch = Self::for_launch(&view, env, &lmstudio_url, default_model.clone());
        if self.scope == ProviderScope::Global {
            set_pi_providers(launch);
        }
        let lmstudio = if lmstudio_url.is_empty() {
            PiLocalState::Off
        } else if view.lmstudio_url == lmstudio_url {
            view.lmstudio.clone()
        } else {
            PiLocalState::Checking
        };
        *view = PiProvidersView {
            keys_set,
            lmstudio_url,
            lmstudio,
            keys_unreadable,
            models: std::mem::take(&mut view.models),
            models_loading: view.models_loading,
            default_model,
            own,
        };
    }

    /// Ask the local server, in the background, whether it answers with chat
    /// models. When nothing answers at a local address, LM Studio is asked
    /// which port it is really on (it moves off 1234 when that is taken); a
    /// server found there replaces the saved address.
    pub fn check_local(self: &Arc<Self>) {
        let check = self.checks.fetch_add(1, Ordering::SeqCst) + 1;
        let url = {
            let mut view = self.lock();
            if view.lmstudio_url.is_empty() {
                view.lmstudio = PiLocalState::Off;
                return;
            }
            view.lmstudio = PiLocalState::Checking;
            view.lmstudio_url.clone()
        };
        (self.wake)();
        let me = Arc::clone(self);
        let run = move || {
            let mut found = me
                .probe
                .chat_models(&url)
                .map(|models| (url.clone(), models));
            if found.is_none()
                && let Some(host) = local_host(&url)
                && let Some(port) = me.probe.lms_port(&me.home)
            {
                let other = format!("{host}:{port}");
                if other != url {
                    found = me.probe.chat_models(&other).map(|models| (other, models));
                }
            }
            if me.checks.load(Ordering::SeqCst) != check {
                return;
            }
            match found {
                Some((at, models)) => {
                    if at != url && me.save_url(&at).is_ok() {
                        me.apply();
                    }
                    me.lock().lmstudio = PiLocalState::Working { models };
                }
                None => me.lock().lmstudio = PiLocalState::NotAnswering,
            }
            (me.wake)();
            // What the local server has may have changed with it.
            me.list_models();
        };
        if std::thread::Builder::new()
            .name("pi-local-check".to_owned())
            .spawn(run)
            .is_err()
        {
            self.lock().lmstudio = PiLocalState::NotAnswering;
        }
    }

    /// Save (`Some`) or remove (`None`) a provider's key. Terminals started
    /// from now on get it; a running one keeps what it was launched with.
    pub fn set_key(
        self: &Arc<Self>,
        provider: PiProvider,
        key: Option<&str>,
    ) -> Result<(), String> {
        let name = pi_secret(&self.scope, provider);
        match key {
            Some(typed) => self.keys.set(&name, &clean_api_key(typed)?),
            None => self.keys.delete(&name),
        }
        .map_err(|why| format!("The key could not be saved: {why}"))?;
        self.apply();
        self.list_models();
        Ok(())
    }

    /// Set the local server's address; empty turns the local server off. The
    /// address is saved and the server asked at once: the view says whether
    /// it answered.
    pub fn set_lmstudio_url(self: &Arc<Self>, typed: &str) -> Result<(), String> {
        let url = clean_lmstudio_url(typed)?;
        self.save_url(&url)
            .map_err(|err| format!("The address could not be saved: {err}"))?;
        self.apply();
        self.check_local();
        Ok(())
    }
}

/// Keys in memory, for tests: nothing of the real store is read or written.
#[cfg(test)]
#[derive(Default)]
pub struct MemoryKeys(pub Mutex<std::collections::BTreeMap<String, String>>);

#[cfg(test)]
impl PiKeys for MemoryKeys {
    fn get(&self, name: &str) -> Result<Option<String>, String> {
        Ok(self.0.lock().map_err(|e| e.to_string())?.get(name).cloned())
    }

    fn set(&self, name: &str, key: &str) -> Result<(), String> {
        self.0
            .lock()
            .map_err(|e| e.to_string())?
            .insert(name.to_owned(), key.to_owned());
        Ok(())
    }

    fn delete(&self, name: &str) -> Result<(), String> {
        self.0.lock().map_err(|e| e.to_string())?.remove(name);
        Ok(())
    }
}

/// A probe for tests: the servers that answer, and the port `lms` reports.
#[cfg(test)]
#[derive(Default)]
pub struct FakeProbe {
    pub servers: Mutex<std::collections::BTreeMap<String, usize>>,
    pub port: Mutex<Option<u16>>,
    pub models: Mutex<Option<Vec<String>>>,
}

#[cfg(test)]
impl LocalProbe for FakeProbe {
    fn chat_models(&self, url: &str) -> Option<usize> {
        self.servers.lock().ok()?.get(url).copied()
    }

    fn lms_port(&self, _home: &Path) -> Option<u16> {
        *self.port.lock().ok()?
    }

    fn list_models(&self, _request: &ListRequest) -> Option<Vec<String>> {
        self.models.lock().ok()?.clone()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use connecto_session::launch::pi_providers;

    /// The launcher's providers are one value for the process: these tests
    /// change it, so they take turns.
    static LAUNCHER: Mutex<()> = Mutex::new(());

    fn root(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("connecto-pi-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    fn host(dir: &Path, keys: &Arc<MemoryKeys>, probe: &Arc<FakeProbe>) -> Arc<PiProvidersHost> {
        PiProvidersHost::start(
            dir,
            dir,
            Arc::clone(keys) as Arc<dyn PiKeys>,
            Arc::clone(probe) as Arc<dyn LocalProbe>,
            Arc::new(|| {}),
        )
    }

    fn settled(host: &PiProvidersHost) -> PiProvidersView {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let view = host.view();
            // The listing that follows a check has begun and ended too.
            let listed = host.listings.load(Ordering::SeqCst) > 1;
            if view.lmstudio != PiLocalState::Checking && !view.models_loading && listed {
                return view;
            }
            assert!(Instant::now() < deadline, "the check never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn a_saved_key_reaches_the_launcher_and_the_view_only_says_it_is_set() {
        let _turn = LAUNCHER.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = root("key");
        let keys = Arc::new(MemoryKeys::default());
        let probe = Arc::new(FakeProbe::default());
        let host = host(&dir, &keys, &probe);
        assert!(host.view().keys_set.is_empty());
        assert_eq!(host.view().lmstudio_url, DEFAULT_LMSTUDIO_URL);
        // Nothing answers and there is no key: Pi has nothing to talk to.
        assert_eq!(settled(&host).lmstudio, PiLocalState::NotAnswering);
        assert!(!host.view().usable());

        host.set_key(PiProvider::OpenRouter, Some("  sk-or-abc \n"))
            .unwrap();
        assert_eq!(host.view().keys_set, [PiProvider::OpenRouter]);
        assert!(host.view().usable());
        assert_eq!(
            pi_providers().env,
            [("OPENROUTER_API_KEY".into(), "sk-or-abc".into())]
        );
        assert!(!format!("{:?}", host.view()).contains("sk-or-abc"));
        // A key that is not one word is refused and nothing changes.
        assert!(host.set_key(PiProvider::Together, Some("a b")).is_err());
        assert_eq!(host.view().keys_set, [PiProvider::OpenRouter]);

        // A new host (the next launch) reads the same store.
        let again = self::host(&dir, &keys, &probe);
        assert!(again.view().has_key(PiProvider::OpenRouter));
        again.set_key(PiProvider::OpenRouter, None).unwrap();
        assert!(again.view().keys_set.is_empty());
        assert!(pi_providers().env.is_empty());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_saved_address_is_checked_and_the_view_says_whether_the_server_answers() {
        let _turn = LAUNCHER.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = root("url");
        let keys = Arc::new(MemoryKeys::default());
        let probe = Arc::new(FakeProbe::default());
        probe
            .servers
            .lock()
            .unwrap()
            .insert("http://localhost:1235".to_owned(), 2);
        let host = host(&dir, &keys, &probe);
        settled(&host);

        host.set_lmstudio_url("http://localhost:1235/v1/").unwrap();
        let view = settled(&host);
        assert_eq!(view.lmstudio_url, "http://localhost:1235");
        assert_eq!(view.lmstudio, PiLocalState::Working { models: 2 });
        assert!(view.usable());
        assert_eq!(
            pi_providers().lmstudio_url.as_deref(),
            Some("http://localhost:1235")
        );
        // Not an address: refused, nothing changes.
        assert!(host.set_lmstudio_url("localhost").is_err());
        assert_eq!(host.view().lmstudio_url, "http://localhost:1235");

        // An address where nothing answers is saved and said to be silent.
        host.set_lmstudio_url("http://192.168.1.9:1234").unwrap();
        let view = settled(&host);
        assert_eq!(view.lmstudio, PiLocalState::NotAnswering);
        assert!(!view.usable());

        // Off: no local server, and the next launch remembers.
        host.set_lmstudio_url("").unwrap();
        assert_eq!(host.view().lmstudio, PiLocalState::Off);
        assert_eq!(pi_providers().lmstudio_url, None);
        assert_eq!(self::host(&dir, &keys, &probe).view().lmstudio_url, "");

        // Back to the default: the file goes.
        host.set_lmstudio_url(DEFAULT_LMSTUDIO_URL).unwrap();
        assert!(!dir.join(LMSTUDIO_FILE).exists());
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn a_local_server_on_another_port_is_found_and_its_address_saved() {
        let _turn = LAUNCHER.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = root("port");
        let keys = Arc::new(MemoryKeys::default());
        let probe = Arc::new(FakeProbe::default());
        probe
            .servers
            .lock()
            .unwrap()
            .insert("http://localhost:1235".to_owned(), 1);
        *probe.port.lock().unwrap() = Some(1235);
        // The default address is silent; LM Studio says 1235.
        let host = host(&dir, &keys, &probe);
        let view = settled(&host);
        assert_eq!(view.lmstudio, PiLocalState::Working { models: 1 });
        assert_eq!(view.lmstudio_url, "http://localhost:1235");
        assert_eq!(
            fs::read_to_string(dir.join(LMSTUDIO_FILE)).unwrap(),
            "http://localhost:1235"
        );
        assert_eq!(
            pi_providers().lmstudio_url.as_deref(),
            Some("http://localhost:1235")
        );
        // A server elsewhere is never swapped for the local one.
        host.set_lmstudio_url("http://10.0.0.5:1234").unwrap();
        assert_eq!(settled(&host).lmstudio, PiLocalState::NotAnswering);
        assert_eq!(host.view().lmstudio_url, "http://10.0.0.5:1234");
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn pis_models_are_listed_and_a_default_is_used_only_while_pi_offers_it() {
        let _turn = LAUNCHER.lock().unwrap_or_else(PoisonError::into_inner);
        let dir = root("models");
        let keys = Arc::new(MemoryKeys::default());
        let probe = Arc::new(FakeProbe::default());
        *probe.models.lock().unwrap() = Some(vec![
            "lmstudio/google/gemma-4-e2b".to_owned(),
            "openrouter/openai/gpt-5".to_owned(),
        ]);
        let host = host(&dir, &keys, &probe);
        assert_eq!(settled(&host).models.len(), 2);
        assert_eq!(host.view().default_model, None);

        host.set_default_model(Some("lmstudio/google/gemma-4-e2b"))
            .unwrap();
        assert_eq!(
            pi_providers().default_model.as_deref(),
            Some("lmstudio/google/gemma-4-e2b")
        );
        assert!(host.set_default_model(Some("--resume")).is_err());

        // The model goes (deleted in LM Studio): the choice is kept and
        // shown, and new terminals are left to Pi until it is back.
        *probe.models.lock().unwrap() = Some(vec!["openrouter/openai/gpt-5".to_owned()]);
        host.list_models();
        let view = settled(&host);
        assert_eq!(
            view.default_model.as_deref(),
            Some("lmstudio/google/gemma-4-e2b")
        );
        assert_eq!(pi_providers().default_model, None);

        // The next launch remembers; clearing leaves the choice to Pi.
        assert!(
            self::host(&dir, &keys, &probe)
                .view()
                .default_model
                .is_some()
        );
        host.set_default_model(None).unwrap();
        assert_eq!(host.view().default_model, None);
        let _ = fs::remove_dir_all(dir);
    }

    #[test]
    fn each_listing_of_pis_models_has_a_folder_of_its_own() {
        let root = Path::new("/data");
        assert_ne!(listing_dir(root, 1), listing_dir(root, 2));
        assert!(listing_dir(root, 1).starts_with("/data/run"));
    }

    #[test]
    fn pis_own_listing_is_read_as_provider_and_model() {
        let printed = "Warning: something\nprovider  model                    context  max-out  thinking  images\nlmstudio  google/gemma-4-e2b       131.1K   8.2K     no        yes\nopenrouter  openai/gpt-5:free  400K  128K  yes  yes\n\n";
        assert_eq!(
            parse_model_list(printed),
            [
                "lmstudio/google/gemma-4-e2b",
                "openrouter/openai/gpt-5:free"
            ]
        );
        assert!(parse_model_list("No models available.").is_empty());
    }

    #[test]
    fn a_model_list_counts_its_chat_models() {
        let list =
            br#"{"data":[{"id":"google/gemma-4-e2b"},{"id":"text-embedding-nomic"},{"id":"x"}]}"#;
        assert_eq!(count_chat_models(list), Some(2));
        assert_eq!(count_chat_models(b"<html>"), None);
        // LM Studio's own listing: its helper model and an embedding model
        // are not models for Pi, and neither is one with a tiny context.
        let native = br#"{"data":[
            {"id":"phonon-40m-multilingual","type":"llm","publisher":"model-extension-pack","loaded_context_length":2048},
            {"id":"google/gemma-4-e2b","type":"vlm","publisher":"google","max_context_length":131072},
            {"id":"tiny","type":"llm","publisher":"x","max_context_length":2048},
            {"id":"nomic","type":"embeddings","publisher":"nomic-ai"}]}"#;
        assert_eq!(count_usable_models(native), Some(1));
        assert_eq!(count_chat_models(br#"{"error":"no"}"#), None);
        assert_eq!(
            local_host("http://localhost:1234"),
            Some("http://localhost")
        );
        assert_eq!(local_host("http://127.0.0.1"), Some("http://127.0.0.1"));
        assert_eq!(local_host("http://10.0.0.5:1234"), None);
        assert_eq!(local_host("https://localhost:1234"), None);
    }
}
