//! The model providers that depend on the secret store or on the project,
//! for Settings and for each launch (`connecto_session::launch::ModelProviders`).
//!
//! - Claude Code with Together AI: a key, app-wide or for one project, kept in
//!   the secret store (`claude.together-api-key`,
//!   `project.<id>.claude.together-api-key`). With one, a Claude terminal
//!   starts through TogetherLink (`connecto_adapter::TogetherLink`), and is
//!   refused, saying why, when TogetherLink is not installed.
//! - Pi's providers of a project of its own (`crate::pi_providers`, scoped).
//!
//! TogetherLink is Together's own launcher. When a key is saved and it is
//! missing, the person is offered to install it with Together's own installer
//! (`https://link.together.ai/install`), downloaded over HTTPS and run as the
//! agents' installers are (`crate::agent_install`): from a private folder,
//! with a minimal environment, no terminal and no input, under a time limit.
//! That installer also installs Bun when it is missing, and adds
//! `~/.local/bin` to the shell's startup file; the offer says so.
//!
//! A key is never written anywhere else, and never shown.

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::time::Duration;

use connecto_adapter::{ClaudeProviders, PiProviders, TogetherLink};
use connecto_core::pi::clean_api_key;
use connecto_core::providers::CLAUDE_PROVIDERS_SUPPORTED;
use connecto_core::{ClaudeProvidersView, ProviderScope, TogetherLinkState, ToolKind};
use connecto_session::Wake;
use connecto_session::project_settings::{load_providers, update_providers};

use crate::pi_providers::{LocalProbe, PiKeys, PiProvidersHost};

/// The secret's name for the Together AI key Claude terminals are started
/// with: the app-wide one, or one of the project's own.
#[must_use]
pub fn claude_secret(scope: &ProviderScope) -> String {
    match scope {
        ProviderScope::Global => "claude.together-api-key".to_owned(),
        ProviderScope::Project(id) => format!("project.{id}.claude.together-api-key"),
    }
}

/// Where TogetherLink's installer puts its command: a link in `~/.local/bin`,
/// and the launcher itself under `~/.togetherlink/bin`.
#[must_use]
pub fn find_togetherlink(home: &Path) -> Option<PathBuf> {
    if !CLAUDE_PROVIDERS_SUPPORTED {
        return None;
    }
    [
        home.join(".local/bin/togetherlink"),
        home.join(".togetherlink/bin/togetherlink"),
    ]
    .into_iter()
    .find(|path| runnable(path))
}

#[cfg(unix)]
fn runnable(path: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(path)
        .is_ok_and(|meta| meta.is_file() && meta.permissions().mode() & 0o111 != 0)
}

#[cfg(not(unix))]
fn runnable(_path: &Path) -> bool {
    false
}

/// Whether TogetherLink has a key of its own saved (`togetherlink configure`,
/// in `~/.togetherlink/config.json`), which it uses before the one in its
/// environment. Only whether there is one is read; the key is not kept.
#[must_use]
pub fn togetherlink_own_key(home: &Path) -> bool {
    let Ok(text) = std::fs::read_to_string(home.join(".togetherlink/config.json")) else {
        return false;
    };
    serde_json::from_str::<serde_json::Value>(&text)
        .ok()
        .and_then(|config| {
            config
                .get("apiKey")
                .and_then(serde_json::Value::as_str)
                .map(|key| !key.trim().is_empty() && key != "{env:TOGETHER_API_KEY}")
        })
        .unwrap_or(false)
}

/// Installs TogetherLink. A fake stands in for tests.
pub trait TogetherLinkInstaller: Send + Sync {
    /// Install it for the user whose home is `home`, telling `say` which
    /// step it is on.
    ///
    /// # Errors
    /// Why it did not finish, for a person.
    fn install(&self, home: &Path, say: &dyn Fn(&str)) -> Result<(), String>;
}

/// Together's own installer.
#[cfg_attr(test, allow(dead_code))]
pub struct OfficialTogetherLink {
    /// Where the script is kept while it runs (a private folder).
    pub work: PathBuf,
}

/// How long the download of the script, and the install, may take.
#[cfg_attr(test, allow(dead_code))]
const DOWNLOAD_LIMIT_SECS: &str = "120";
#[cfg_attr(test, allow(dead_code))]
const INSTALL_LIMIT: Duration = Duration::from_secs(10 * 60);

impl TogetherLinkInstaller for OfficialTogetherLink {
    #[cfg(unix)]
    fn install(&self, home: &Path, say: &dyn Fn(&str)) -> Result<(), String> {
        let outcome = self.run(home, say);
        let _ = std::fs::remove_dir_all(&self.work);
        outcome
    }

    #[cfg(not(unix))]
    fn install(&self, _home: &Path, _say: &dyn Fn(&str)) -> Result<(), String> {
        Err("TogetherLink runs on macOS and Linux only.".to_owned())
    }
}

#[cfg(unix)]
impl OfficialTogetherLink {
    fn run(&self, home: &Path, say: &dyn Fn(&str)) -> Result<(), String> {
        use std::process::Stdio;
        say("Downloading the installer from link.together.ai…");
        std::fs::create_dir_all(&self.work)
            .map_err(|err| format!("Connecto could not prepare a folder ({err}). Try again."))?;
        crate::platform::make_private(&self.work);
        let script = self.work.join("install-togetherlink.sh");
        let fetched = crate::platform::curl()
            .args([
                "--proto",
                "=https",
                "--tlsv1.2",
                "--fail",
                "--silent",
                "--show-error",
                "--location",
                "--connect-timeout",
                "20",
                "--max-time",
                DOWNLOAD_LIMIT_SECS,
                "--output",
            ])
            .arg(&script)
            .arg(connecto_core::providers::TOGETHERLINK_INSTALLER_URL)
            .stdin(Stdio::null())
            .output()
            .map_err(|err| format!("Connecto could not start the download ({err}). Try again."))?;
        if !fetched.status.success() {
            return Err(format!(
                "Connecto could not download TogetherLink's installer from link.together.ai. Is {} online? Try again.",
                crate::platform::THIS_COMPUTER
            ));
        }
        say("Installing TogetherLink and, if needed, Bun (this can take a minute)…");
        let log = self.work.join("log.txt");
        let log_file = std::fs::File::create(&log)
            .map_err(|err| format!("Connecto could not prepare a folder ({err}). Try again."))?;
        let mut command = std::process::Command::new("/bin/bash");
        command
            .arg(&script)
            .env_clear()
            .env("HOME", home)
            .env(
                "PATH",
                format!(
                    "/usr/bin:/bin:/usr/sbin:/sbin:{}",
                    home.join(".local/bin").display()
                ),
            )
            .env("USER", std::env::var_os("USER").unwrap_or_default())
            .env("LOGNAME", std::env::var_os("LOGNAME").unwrap_or_default())
            // Its PATH line goes in the startup file of the person's shell,
            // as when they run it themselves.
            .env("SHELL", std::env::var_os("SHELL").unwrap_or_default())
            .env("LANG", "en_US.UTF-8")
            .env("TERM", "dumb")
            .env("NO_COLOR", "1")
            // It would offer to start TogetherLink at the end.
            .env("TOGETHERLINK_NO_LAUNCH", "1")
            .current_dir(home)
            .stdin(Stdio::null())
            .stdout(log_file.try_clone().map_err(|err| {
                format!("Connecto could not prepare a folder ({err}). Try again.")
            })?)
            .stderr(log_file);
        let mut child = command
            .spawn()
            .map_err(|err| format!("Connecto could not start TogetherLink's installer ({err})."))?;
        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(status)) => break status,
                Ok(None) if started.elapsed() > INSTALL_LIMIT => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return Err("TogetherLink's installer took too long. Try again.".to_owned());
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(200)),
                Err(err) => return Err(format!("Connecto lost track of the installer ({err}).")),
            }
        };
        if !status.success() {
            return Err(format!(
                "TogetherLink's installer did not finish: {}",
                last_line(&log).unwrap_or_else(|| "it said nothing.".to_owned())
            ));
        }
        say("Checking that it runs…");
        let program = find_togetherlink(home).ok_or_else(|| {
            "TogetherLink was installed, but Connecto cannot find it in ~/.local/bin or ~/.togetherlink/bin.".to_owned()
        })?;
        let mut version = std::process::Command::new(&program);
        version
            .arg("--version")
            .env_clear()
            .env("HOME", home)
            .env("PATH", "/usr/bin:/bin:/usr/sbin:/sbin")
            .current_dir(home);
        let printed = crate::pi_providers::output_within(version, Duration::from_secs(30))
            .map(|out| String::from_utf8_lossy(&out).trim().to_owned());
        match printed {
            Some(line) if line.starts_with("togetherlink v") => Ok(()),
            _ => Err("TogetherLink is installed but did not start. Try again, or run its installer in a terminal.".to_owned()),
        }
    }
}

/// The installer's last line, which says why it stopped. Never a key: the
/// installer is given none.
#[cfg(unix)]
fn last_line(log: &Path) -> Option<String> {
    let text = std::fs::read_to_string(log).ok()?;
    let line = text.lines().rev().find(|line| !line.trim().is_empty())?;
    Some(line.trim().chars().take(300).collect())
}

/// What is known about TogetherLink on this computer.
#[derive(Default)]
struct TogetherLinkInfo {
    state: TogetherLinkState,
    own_key: bool,
    offered: bool,
    error: Option<String>,
}

/// How long a launch waits for the secret store to be set up.
const READY_WAIT: Duration = Duration::from_secs(10);

pub struct ModelProvidersHost {
    data_root: PathBuf,
    home: PathBuf,
    keys: Arc<dyn PiKeys>,
    probe: Arc<dyn LocalProbe>,
    installer: Arc<dyn TogetherLinkInstaller>,
    /// The secret store is set up (where secrets are kept is known).
    ready: Mutex<bool>,
    ready_changed: Condvar,
    togetherlink: Mutex<TogetherLinkInfo>,
    /// The Pi providers of the projects whose settings are open.
    project_pi: Mutex<BTreeMap<String, Arc<PiProvidersHost>>>,
    /// The projects whose Pi host is being started (its keys read from the
    /// secret store), outside the lock of `project_pi`; `pi_started` wakes
    /// those waiting for one.
    pi_starting: Mutex<BTreeSet<String>>,
    pi_started: Condvar,
    wake: Wake,
}

impl ModelProvidersHost {
    /// Nothing is read from the secret store until [`Self::mark_ready`].
    pub fn new(
        data_root: &Path,
        home: &Path,
        keys: Arc<dyn PiKeys>,
        probe: Arc<dyn LocalProbe>,
        installer: Arc<dyn TogetherLinkInstaller>,
        wake: Wake,
    ) -> Arc<Self> {
        let host = Arc::new(Self {
            data_root: data_root.to_owned(),
            home: home.to_owned(),
            keys,
            probe,
            installer,
            ready: Mutex::new(false),
            ready_changed: Condvar::new(),
            togetherlink: Mutex::new(TogetherLinkInfo::default()),
            project_pi: Mutex::new(BTreeMap::new()),
            pi_starting: Mutex::new(BTreeSet::new()),
            pi_started: Condvar::new(),
            wake,
        });
        host.check_togetherlink();
        host
    }

    /// The secret store is set up: launches may read it from now on.
    pub fn mark_ready(&self) {
        *self.ready.lock().unwrap_or_else(PoisonError::into_inner) = true;
        self.ready_changed.notify_all();
    }

    fn wait_ready(&self) -> bool {
        let ready = self.ready.lock().unwrap_or_else(PoisonError::into_inner);
        let (ready, _) = self
            .ready_changed
            .wait_timeout_while(ready, READY_WAIT, |ready| !*ready)
            .unwrap_or_else(PoisonError::into_inner);
        *ready
    }

    fn info(&self) -> MutexGuard<'_, TogetherLinkInfo> {
        self.togetherlink
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
    }

    /// Look again for TogetherLink (not while it is being installed).
    pub fn check_togetherlink(&self) {
        let found = find_togetherlink(&self.home).is_some();
        let own_key = togetherlink_own_key(&self.home);
        {
            let mut info = self.info();
            if matches!(info.state, TogetherLinkState::Installing(_)) {
                return;
            }
            info.state = if found {
                info.offered = false;
                TogetherLinkState::Installed
            } else {
                TogetherLinkState::Missing
            };
            info.own_key = own_key;
        }
        (self.wake)();
    }

    /// What Settings shows about `scope`'s Claude providers.
    pub fn claude_view(&self, scope: &ProviderScope) -> ClaudeProvidersView {
        let (together_key, keys_unreadable) = match self.keys.get(&claude_secret(scope)) {
            Ok(Some(key)) => (!key.is_empty(), false),
            Ok(None) => (false, false),
            Err(_) => (false, true),
        };
        let own = match scope {
            ProviderScope::Global => true,
            ProviderScope::Project(id) => load_providers(&self.data_root, id).claude_own,
        };
        let info = self.info();
        ClaudeProvidersView {
            together_key,
            keys_unreadable,
            own,
            togetherlink: info.state.clone(),
            togetherlink_own_key: info.own_key,
            install_offered: info.offered,
            install_error: info.error.clone(),
        }
    }

    /// Save (`Some`) or remove (`None`) `scope`'s Together AI key. A key saved
    /// while TogetherLink is missing brings the offer to install it.
    pub fn set_claude_together_key(
        &self,
        scope: &ProviderScope,
        key: Option<&str>,
    ) -> Result<(), String> {
        if !CLAUDE_PROVIDERS_SUPPORTED {
            return Err("TogetherLink runs on macOS and Linux only.".to_owned());
        }
        let name = claude_secret(scope);
        match key {
            Some(typed) => self.keys.set(&name, &clean_api_key(typed)?),
            None => self.keys.delete(&name),
        }
        .map_err(|why| format!("The key could not be saved: {why}"))?;
        if key.is_some() {
            self.check_togetherlink();
            let mut info = self.info();
            if info.state == TogetherLinkState::Missing {
                info.offered = true;
                info.error = None;
            }
        }
        (self.wake)();
        Ok(())
    }

    /// The person declined the offer to install TogetherLink.
    pub fn decline_togetherlink(&self) {
        self.info().offered = false;
        (self.wake)();
    }

    /// Install TogetherLink in the background; the view says how it goes.
    pub fn install_togetherlink(self: &Arc<Self>) {
        if !CLAUDE_PROVIDERS_SUPPORTED {
            return;
        }
        {
            let mut info = self.info();
            if matches!(info.state, TogetherLinkState::Installing(_)) {
                return;
            }
            info.state = TogetherLinkState::Installing("Starting…".to_owned());
            info.offered = false;
            info.error = None;
        }
        (self.wake)();
        let me = Arc::clone(self);
        let run = move || {
            let say = |line: &str| {
                me.info().state = TogetherLinkState::Installing(line.to_owned());
                (me.wake)();
            };
            let outcome = me.installer.install(&me.home, &say);
            me.info().state = TogetherLinkState::Missing;
            me.check_togetherlink();
            let mut info = me.info();
            match outcome {
                Ok(()) if info.state == TogetherLinkState::Installed => {}
                Ok(()) => {
                    info.error = Some(
                        "The installer finished, but TogetherLink is not where it puts it."
                            .to_owned(),
                    );
                }
                Err(why) => info.error = Some(why),
            }
            drop(info);
            (me.wake)();
        };
        if let Err(err) = std::thread::Builder::new()
            .name("togetherlink-install".to_owned())
            .spawn(run)
        {
            let mut info = self.info();
            info.state = TogetherLinkState::Missing;
            info.error = Some(format!("Could not start: {err}"));
        }
    }

    /// Whether `project`'s terminals of `tool` use the project's own
    /// providers (when that tool's config is Isolated).
    pub fn set_own(&self, project: &str, tool: ToolKind, own: bool) -> Result<(), String> {
        if !matches!(tool, ToolKind::Claude | ToolKind::Pi) {
            return Err("This tool has no model providers to set.".to_owned());
        }
        // Only this field, read and written under the settings' lock.
        update_providers(&self.data_root, project, |saved| match tool {
            ToolKind::Pi => saved.pi_own = own,
            _ => saved.claude_own = own,
        })
        .map_err(|err| format!("The choice could not be saved: {err}"))?;
        if let Some(host) = self.live_pi(project) {
            host.reload();
        }
        (self.wake)();
        Ok(())
    }

    /// The project's Pi providers host when one is running already: nothing
    /// is started, nothing read from the secret store.
    pub fn live_pi(&self, project: &str) -> Option<Arc<PiProvidersHost>> {
        self.project_pi
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(project)
            .cloned()
    }

    /// The host of `project`'s own Pi providers, started (its local server
    /// checked, Pi asked for its models) the first time it is asked for.
    /// It is started, its keys read, with no lock held that another project
    /// or [`Self::live_pi`] needs: one caller starts it, the others of that
    /// project wait for it.
    pub fn project_pi_host(&self, project: &str) -> Arc<PiProvidersHost> {
        loop {
            if let Some(host) = self.live_pi(project) {
                return host;
            }
            let mut starting = self
                .pi_starting
                .lock()
                .unwrap_or_else(PoisonError::into_inner);
            // Looked at again under this lock: a start that ended has put
            // its host in the map before it let go of its mark.
            if self.live_pi(project).is_some() {
                continue;
            }
            if starting.insert(project.to_owned()) {
                break;
            }
            drop(
                self.pi_started
                    .wait(starting)
                    .unwrap_or_else(PoisonError::into_inner),
            );
        }
        let started = PiProvidersHost::start_for(
            ProviderScope::Project(project.to_owned()),
            &self.data_root,
            &self.home,
            Arc::clone(&self.keys),
            Arc::clone(&self.probe),
            Arc::clone(&self.wake),
        );
        let host = Arc::clone(
            self.project_pi
                .lock()
                .unwrap_or_else(PoisonError::into_inner)
                .entry(project.to_owned())
                .or_insert(started),
        );
        self.pi_starting
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .remove(project);
        self.pi_started.notify_all();
        host
    }

    /// Why a terminal set up for Together AI cannot start: TogetherLink is
    /// missing. Points to the offer to install it, which is made again.
    fn missing_togetherlink(&self, scope: &ProviderScope) -> String {
        {
            let mut info = self.info();
            if info.state != TogetherLinkState::Installed {
                info.offered = true;
            }
        }
        (self.wake)();
        let place = match scope {
            ProviderScope::Global => "Settings > Agents > Claude Code",
            ProviderScope::Project(_) => "Project settings > Tools > Claude",
        };
        format!(
            "This Claude terminal is set to use Together AI through TogetherLink, which is not installed on {}. Install it with Set up model providers… in {place}, or remove the Together AI key there, then start the terminal again.",
            crate::platform::THIS_COMPUTER
        )
    }
}

impl connecto_session::launch::ModelProviders for ModelProvidersHost {
    fn claude(&self, scope: &ProviderScope) -> Result<ClaudeProviders, String> {
        if !CLAUDE_PROVIDERS_SUPPORTED {
            return Ok(ClaudeProviders::default());
        }
        if !self.wait_ready() {
            return Err(
                "Connecto's secret store is not ready yet: start the terminal again in a moment."
                    .to_owned(),
            );
        }
        // As usual only when the store said there is no key: one that could
        // not be read may be a Together AI key, and the terminal must not
        // start on the Anthropic account in its place.
        let key = match self.keys.get(&claude_secret(scope)) {
            Ok(Some(key)) if !key.trim().is_empty() => key,
            Ok(_) => return Ok(ClaudeProviders::default()),
            Err(_) => {
                return Err(
                    "Connecto could not read its secret store, so it cannot tell whether this Claude terminal is set to use Together AI, and does not start it on your Anthropic account instead. Start it again once the store can be read (Set up model providers… says whether it can)."
                        .to_owned(),
                );
            }
        };
        match find_togetherlink(&self.home) {
            Some(program) => Ok(ClaudeProviders {
                together: Some(TogetherLink {
                    program,
                    api_key: key,
                }),
            }),
            None => Err(self.missing_togetherlink(scope)),
        }
    }

    fn project_pi(&self, project_id: &str) -> PiProviders {
        if !self.wait_ready() {
            return PiProviders::default();
        }
        match self.live_pi(project_id) {
            Some(host) => host.launch_providers(),
            None => PiProvidersHost::idle(
                ProviderScope::Project(project_id.to_owned()),
                &self.data_root,
                &self.home,
                Arc::clone(&self.keys),
                Arc::clone(&self.probe),
                Arc::clone(&self.wake),
            )
            .launch_providers(),
        }
    }
}

/// A secret store that cannot be read, for tests.
#[cfg(test)]
pub struct BrokenKeys;

#[cfg(test)]
impl PiKeys for BrokenKeys {
    fn get(&self, _name: &str) -> Result<Option<String>, String> {
        Err("the stored secrets cannot be read".to_owned())
    }
    fn set(&self, _name: &str, _key: &str) -> Result<(), String> {
        Err("the stored secrets cannot be read".to_owned())
    }
    fn delete(&self, _name: &str) -> Result<(), String> {
        Err("the stored secrets cannot be read".to_owned())
    }
}

/// An installer for tests: puts a TogetherLink launcher where the real one
/// does, or fails.
#[cfg(test)]
pub struct FakeTogetherLink {
    pub fail: bool,
    pub runs: Mutex<u32>,
}

#[cfg(test)]
impl TogetherLinkInstaller for FakeTogetherLink {
    fn install(&self, home: &Path, say: &dyn Fn(&str)) -> Result<(), String> {
        *self.runs.lock().unwrap_or_else(PoisonError::into_inner) += 1;
        say("Downloading the installer from link.together.ai…");
        if self.fail {
            return Err("Need unzip to install Bun.".to_owned());
        }
        put_togetherlink(home);
        Ok(())
    }
}

/// A launcher where TogetherLink's installer puts it.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
pub fn put_togetherlink(home: &Path) {
    let bin = home.join(".local/bin");
    std::fs::create_dir_all(&bin).unwrap();
    let file = bin.join("togetherlink");
    std::fs::write(&file, "#!/bin/sh\necho 'togetherlink v0.9.98'\n").unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&file, std::fs::Permissions::from_mode(0o755)).unwrap();
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::pi_providers::{FakeProbe, MemoryKeys};
    use connecto_core::PiProvider;
    use connecto_session::launch::ModelProviders;
    use std::time::Instant;

    fn temp(name: &str) -> PathBuf {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = std::env::temp_dir().join(format!(
            "connecto-providers-{name}-{}-{n}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(dir.join("home")).unwrap();
        std::fs::create_dir_all(dir.join("data")).unwrap();
        dir
    }

    fn host(dir: &Path, keys: &Arc<MemoryKeys>, fail: bool) -> Arc<ModelProvidersHost> {
        let host = ModelProvidersHost::new(
            &dir.join("data"),
            &dir.join("home"),
            Arc::clone(keys) as Arc<dyn PiKeys>,
            Arc::new(FakeProbe::default()),
            Arc::new(FakeTogetherLink {
                fail,
                runs: Mutex::new(0),
            }),
            Arc::new(|| {}),
        );
        host.mark_ready();
        host
    }

    fn settled(host: &ModelProvidersHost) -> ClaudeProvidersView {
        let deadline = Instant::now() + Duration::from_secs(10);
        loop {
            let view = host.claude_view(&ProviderScope::Global);
            if !matches!(view.togetherlink, TogetherLinkState::Installing(_)) {
                return view;
            }
            assert!(Instant::now() < deadline, "the install never ended");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    const KEY: &str = "tgp_v1_secret-global";
    const PROJECT_KEY: &str = "tgp_v1_secret-project";

    #[cfg(unix)]
    #[test]
    fn a_saved_key_starts_claude_through_togetherlink_and_each_scope_has_its_own() {
        let dir = temp("claude");
        let keys = Arc::new(MemoryKeys::default());
        let host = host(&dir, &keys, false);
        let project = ProviderScope::Project("p1".into());
        // No key: Claude as usual.
        assert_eq!(
            host.claude(&ProviderScope::Global).unwrap(),
            ClaudeProviders::default()
        );
        put_togetherlink(&dir.join("home"));
        host.check_togetherlink();
        host.set_claude_together_key(&ProviderScope::Global, Some(&format!(" {KEY}\n")))
            .unwrap();
        let global = host
            .claude(&ProviderScope::Global)
            .unwrap()
            .together
            .unwrap();
        assert_eq!(global.api_key, KEY);
        assert_eq!(global.program, dir.join("home/.local/bin/togetherlink"));
        // The project's own key is a separate secret; without one, nothing.
        assert_eq!(host.claude(&project).unwrap(), ClaudeProviders::default());
        host.set_claude_together_key(&project, Some(PROJECT_KEY))
            .unwrap();
        assert_eq!(
            host.claude(&project).unwrap().together.unwrap().api_key,
            PROJECT_KEY
        );
        assert_eq!(
            host.claude(&ProviderScope::Global)
                .unwrap()
                .together
                .unwrap()
                .api_key,
            KEY
        );
        let names: Vec<String> = keys.0.lock().unwrap().keys().cloned().collect();
        assert_eq!(
            names,
            [
                "claude.together-api-key",
                "project.p1.claude.together-api-key"
            ]
        );
        // The view says a key is saved, never which.
        let view = host.claude_view(&ProviderScope::Global);
        assert!(view.together_key && view.own);
        assert_eq!(view.togetherlink, TogetherLinkState::Installed);
        assert!(!view.install_offered);
        assert!(!format!("{view:?}").contains("tgp_v1"));
        assert!(!format!("{:?}", host.claude(&project).unwrap()).contains("tgp_v1"));
        // A project chooses its own providers in its settings file, where no
        // key ever is.
        assert!(!host.claude_view(&project).own);
        host.set_own("p1", ToolKind::Claude, true).unwrap();
        assert!(host.claude_view(&project).own);
        let settings = std::fs::read_to_string(connecto_session::project_settings::path(
            &dir.join("data"),
            "p1",
        ))
        .unwrap();
        assert!(!settings.contains("tgp_v1"));
        // Removed: as usual again.
        host.set_claude_together_key(&project, None).unwrap();
        assert_eq!(host.claude(&project).unwrap(), ClaudeProviders::default());
        assert!(
            host.set_claude_together_key(&project, Some("two words"))
                .is_err()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_key_without_togetherlink_offers_the_install_and_a_launch_is_refused_saying_so() {
        let dir = temp("offer");
        let keys = Arc::new(MemoryKeys::default());
        let host = host(&dir, &keys, false);
        assert_eq!(
            host.claude_view(&ProviderScope::Global).togetherlink,
            TogetherLinkState::Missing
        );
        host.set_claude_together_key(&ProviderScope::Global, Some(KEY))
            .unwrap();
        let view = host.claude_view(&ProviderScope::Global);
        assert!(view.install_offered && view.blocked());
        // Declined: the terminal does not start without it, and says where
        // to install it; the offer is made again.
        host.decline_togetherlink();
        assert!(!host.claude_view(&ProviderScope::Global).install_offered);
        let refused = host.claude(&ProviderScope::Global).unwrap_err();
        assert!(refused.contains("TogetherLink, which is not installed"));
        assert!(refused.contains("Settings > Agents > Claude Code"));
        assert!(!refused.contains("tgp_v1"));
        assert!(host.claude_view(&ProviderScope::Global).install_offered);
        // Accepted: installed, and the launch goes through it.
        host.install_togetherlink();
        let view = settled(&host);
        assert_eq!(view.togetherlink, TogetherLinkState::Installed);
        assert_eq!(view.install_error, None);
        assert!(!view.install_offered && !view.blocked());
        assert!(
            host.claude(&ProviderScope::Global)
                .unwrap()
                .together
                .is_some()
        );
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(unix)]
    #[test]
    fn a_failed_install_is_said_and_nothing_changes() {
        let dir = temp("fail");
        let keys = Arc::new(MemoryKeys::default());
        let host = host(&dir, &keys, true);
        host.set_claude_together_key(&ProviderScope::Global, Some(KEY))
            .unwrap();
        host.install_togetherlink();
        let view = settled(&host);
        assert_eq!(view.togetherlink, TogetherLinkState::Missing);
        assert_eq!(
            view.install_error.as_deref(),
            Some("Need unzip to install Bun.")
        );
        assert!(host.claude(&ProviderScope::Global).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    /// A store that cannot be read refuses the launch: it may hold a
    /// Together AI key, so Claude must not start on Anthropic instead.
    #[cfg(unix)]
    #[test]
    fn an_unreadable_secret_store_refuses_the_launch_instead_of_falling_back() {
        let dir = temp("broken");
        let host = ModelProvidersHost::new(
            &dir.join("data"),
            &dir.join("home"),
            Arc::new(BrokenKeys),
            Arc::new(FakeProbe::default()),
            Arc::new(FakeTogetherLink {
                fail: false,
                runs: Mutex::new(0),
            }),
            Arc::new(|| {}),
        );
        host.mark_ready();
        let refused = host.claude(&ProviderScope::Global).unwrap_err();
        assert!(
            refused.contains("could not read its secret store"),
            "{refused}"
        );
        assert!(host.claude_view(&ProviderScope::Global).keys_unreadable);
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn togetherlinks_own_saved_key_is_noticed_and_never_read_out() {
        let dir = temp("own-key");
        let home = dir.join("home");
        assert!(!togetherlink_own_key(&home));
        std::fs::create_dir_all(home.join(".togetherlink")).unwrap();
        let config = home.join(".togetherlink/config.json");
        std::fs::write(&config, r#"{"apiKey":"{env:TOGETHER_API_KEY}"}"#).unwrap();
        assert!(!togetherlink_own_key(&home));
        std::fs::write(&config, r#"{"apiKey":"tgp_v1_theirs"}"#).unwrap();
        assert!(togetherlink_own_key(&home));
        let _ = std::fs::remove_dir_all(dir);
    }

    /// Keys that wait, while held, before they answer: a secret store slow to
    /// read.
    #[derive(Default)]
    struct HeldKeys {
        held: Mutex<bool>,
        released: Condvar,
        /// A read has begun.
        asked: std::sync::atomic::AtomicBool,
    }
    impl PiKeys for HeldKeys {
        fn get(&self, _: &str) -> Result<Option<String>, String> {
            self.asked.store(true, std::sync::atomic::Ordering::SeqCst);
            let mut held = self.held.lock().unwrap();
            while *held {
                held = self.released.wait(held).unwrap();
            }
            Ok(None)
        }
        fn set(&self, _: &str, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _: &str) -> Result<(), String> {
            Ok(())
        }
    }

    /// Review finding (ADR 0041): a project's Pi host whose keys are slow to
    /// read holds no lock another project, or a look at the running hosts,
    /// needs; a second caller for the same project waits for the one host.
    #[test]
    fn a_pi_host_being_started_holds_no_other_project_up() {
        let dir = temp("pi-held");
        let keys = Arc::new(HeldKeys::default());
        let host = ModelProvidersHost::new(
            &dir.join("data"),
            &dir.join("home"),
            Arc::clone(&keys) as Arc<dyn PiKeys>,
            Arc::new(FakeProbe::default()),
            Arc::new(FakeTogetherLink {
                fail: false,
                runs: Mutex::new(0),
            }),
            Arc::new(|| {}),
        );
        host.mark_ready();
        std::thread::sleep(Duration::from_millis(100));
        *keys.held.lock().unwrap() = true;
        keys.asked.store(false, std::sync::atomic::Ordering::SeqCst);
        let starting = {
            let host = Arc::clone(&host);
            std::thread::spawn(move || host.project_pi_host("slow"))
        };
        let deadline = Instant::now() + Duration::from_secs(10);
        while !keys.asked.load(std::sync::atomic::Ordering::SeqCst) {
            assert!(Instant::now() < deadline, "the start never began");
            std::thread::sleep(Duration::from_millis(5));
        }
        // While "slow" reads its keys: a look at the running hosts answers
        // at once, for it and for another project.
        let (tx, rx) = std::sync::mpsc::channel();
        {
            let host = Arc::clone(&host);
            std::thread::spawn(move || {
                let _ = tx.send((
                    host.live_pi("slow").is_none(),
                    host.live_pi("other").is_none(),
                ));
            });
        }
        assert_eq!(
            rx.recv_timeout(Duration::from_secs(2)),
            Ok((true, true)),
            "a look at the running hosts waited for a start"
        );
        // A second caller for "slow" waits for the same host.
        let second = {
            let host = Arc::clone(&host);
            std::thread::spawn(move || host.project_pi_host("slow"))
        };
        std::thread::sleep(Duration::from_millis(100));
        assert!(!second.is_finished());
        *keys.held.lock().unwrap() = false;
        keys.released.notify_all();
        let first = starting.join().unwrap();
        assert!(Arc::ptr_eq(&first, &second.join().unwrap()));
        assert!(host.pi_starting.lock().unwrap().is_empty());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn a_projects_pi_providers_are_its_own_and_the_app_wide_ones_stay() {
        let dir = temp("pi");
        let keys = Arc::new(MemoryKeys::default());
        let host = host(&dir, &keys, false);
        let global = PiProvidersHost::idle(
            ProviderScope::Global,
            &dir.join("data"),
            &dir.join("home"),
            Arc::clone(&keys) as Arc<dyn PiKeys>,
            Arc::new(FakeProbe::default()),
            Arc::new(|| {}),
        );
        global
            .set_key(PiProvider::Together, Some("tgp_v1_global"))
            .unwrap();
        // Nothing of the project's own yet.
        assert!(host.project_pi("p1").env.is_empty());
        let project = host.project_pi_host("p1");
        project
            .set_key(PiProvider::OpenRouter, Some("sk-or-project"))
            .unwrap();
        project.set_lmstudio_url("").unwrap();
        project.set_default_model(Some("openrouter/x/y")).unwrap();
        let launch = host.project_pi("p1");
        assert_eq!(
            launch.env,
            [("OPENROUTER_API_KEY".into(), "sk-or-project".into())]
        );
        assert_eq!(launch.lmstudio_url, None);
        assert_eq!(launch.default_model.as_deref(), Some("openrouter/x/y"));
        // The app-wide ones are as they were, under their own names.
        assert_eq!(
            global.launch_providers().env,
            [("TOGETHER_API_KEY".into(), "tgp_v1_global".into())]
        );
        assert_eq!(
            global.launch_providers().lmstudio_url.as_deref(),
            Some(connecto_core::pi::DEFAULT_LMSTUDIO_URL)
        );
        assert!(
            keys.0
                .lock()
                .unwrap()
                .contains_key("project.p1.pi.openrouter-api-key")
        );
        // A launch without the dialog open reads the same.
        host.project_pi.lock().unwrap().clear();
        assert_eq!(host.project_pi("p1"), launch);
        // Whether the project uses them is its choice, shown in the view.
        assert!(!host.project_pi_host("p1").view().own);
        host.set_own("p1", ToolKind::Pi, true).unwrap();
        assert!(host.project_pi_host("p1").view().own);
        assert!(host.set_own("p1", ToolKind::Codex, true).is_err());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[cfg(windows)]
    #[test]
    fn windows_never_starts_claude_through_togetherlink() {
        let dir = temp("windows");
        let keys = Arc::new(MemoryKeys::default());
        let host = host(&dir, &keys, false);
        assert!(
            host.set_claude_together_key(&ProviderScope::Global, Some(KEY))
                .is_err()
        );
        keys.set(&claude_secret(&ProviderScope::Global), KEY)
            .unwrap();
        assert_eq!(
            host.claude(&ProviderScope::Global).unwrap(),
            ClaudeProviders::default()
        );
        let _ = std::fs::remove_dir_all(dir);
    }
}
