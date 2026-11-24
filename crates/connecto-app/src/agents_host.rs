//! The Agents section of Settings: every supported agent with its state, and
//! Activate / Deactivate. The work (finding the program, asking it for its
//! version, asking whether the user is signed in, installing) runs on threads of
//! its own; the window reads the rows each frame.

use std::fs;
use std::path::Path;
#[cfg(test)]
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use connecto_core::{
    AgentBusy, AgentRow, AgentsControl, AgentsView, AuthState, ProviderScope, ToolKind,
};
use connecto_session::Wake;
use connecto_session::launch::{Layout, system_signin};
use connecto_store::{Codesign, Failure, Signature, Store, Tool};

/// A shared signature check, usable where a sized one is asked for.
struct Shared(Arc<dyn Signature + Send + Sync>);

impl Signature for Shared {
    fn verify(&self, file: &Path, team: Option<&str>) -> Result<(), String> {
        self.0.verify(file, team)
    }

    fn check(&self, file: &Path, team: Option<&str>) -> Result<(), Failure> {
        self.0.check(file, team)
    }

    /// Passed on, so a checker that does a batch in one go (Windows: one
    /// PowerShell for all of Codex's files) still does.
    fn check_all(&self, files: &[(&Path, Option<&str>)]) -> Result<(), Failure> {
        self.0.check_all(files)
    }

    fn checks_together(&self) -> bool {
        self.0.checks_together()
    }
}

use crate::agent_install::{AgentInstaller, source_of, version_of_install};
use crate::agentic_works::agents::{Availability, Profile};
use crate::model_providers::ModelProvidersHost;
use crate::pi_providers::PiProvidersHost;
use crate::platform::THIS_COMPUTER;
use crate::setup::{SETTINGS_AGENTS as AGENTS, runnable_entry};

/// Agents switched off in Settings, comma separated, in the data folder.
pub const INACTIVE_FILE: &str = ".agents-off";

/// The agents switched off in Settings.
#[must_use]
pub fn inactive_agents(data_root: &Path) -> Vec<Tool> {
    let Ok(text) = fs::read_to_string(data_root.join(INACTIVE_FILE)) else {
        return Vec::new();
    };
    AGENTS
        .into_iter()
        .filter(|tool| text.split(',').any(|word| word.trim() == tool.name()))
        .collect()
}

fn write_inactive(data_root: &Path, list: &[Tool]) {
    let names: Vec<&str> = list.iter().map(|t| t.name()).collect();
    let _ = fs::create_dir_all(data_root);
    let _ = fs::write(data_root.join(INACTIVE_FILE), names.join(","));
}

/// Pi has nothing to talk to with these providers: not offered.
fn pi_unavailable(pi: &connecto_core::PiProvidersView) -> bool {
    !pi.usable() && !pi.undecided()
}

fn kind_of(tool: Tool) -> ToolKind {
    match tool {
        Tool::Claude => ToolKind::Claude,
        Tool::Codex => ToolKind::Codex,
        Tool::Pi => ToolKind::Pi,
        Tool::Antigravity => ToolKind::Antigravity,
        Tool::OpenCode => ToolKind::OpenCode,
        Tool::Shell => ToolKind::Shell,
    }
}

fn tool_of(kind: ToolKind) -> Option<Tool> {
    match kind {
        ToolKind::Claude => Some(Tool::Claude),
        ToolKind::Codex => Some(Tool::Codex),
        ToolKind::Pi => Some(Tool::Pi),
        ToolKind::Antigravity => Some(Tool::Antigravity),
        ToolKind::OpenCode => Some(Tool::OpenCode),
        ToolKind::Shell => None,
    }
}

/// Said on Pi's row when it is installed and has no provider that works.
const NO_PI_PROVIDER: &str = "Pi has no model provider that works yet, so it is not offered for new terminals. Set one up: a key for OpenRouter or Together AI, or a local LM Studio server that is running with a model.";

/// Whether an agent is offered for new terminals: not switched off, and for
/// Pi, Antigravity and OpenCode (which the first launch only installs when
/// asked to) also on this computer. So one nobody installed is not in the
/// chooser, and its row in Settings is the way to add it.
fn offered(tool: Tool, installed: bool, off: &[Tool]) -> bool {
    !off.contains(&tool)
        && (!matches!(tool, Tool::Pi | Tool::Antigravity | Tool::OpenCode) || installed)
}

/// The name an agent is shown by, whether or not Connecto can install it
/// here (it was taken from the installer, so an agent without one was
/// shown as "Shell").
const fn name_of(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "Claude Code",
        Tool::Codex => "Codex",
        Tool::Pi => "Pi",
        Tool::Antigravity => "Antigravity CLI",
        Tool::OpenCode => "OpenCode",
        Tool::Shell => "Shell",
    }
}

pub struct AgentsHost {
    layout: Layout,
    installer: Arc<dyn AgentInstaller>,
    signature: Arc<dyn Signature + Send + Sync>,
    rows: Mutex<Vec<AgentRow>>,
    /// The model providers Pi terminals are given.
    pi: Arc<PiProvidersHost>,
    /// Claude's providers, and those of projects with their own.
    providers: Arc<ModelProvidersHost>,
    wake: Wake,
}

impl AgentsHost {
    /// Rows at once (the program is looked for, nothing is run); the version
    /// and sign-in follow from a thread.
    pub fn start(
        data_root: &Path,
        home: &Path,
        installer: Arc<dyn AgentInstaller>,
        providers: Arc<ModelProvidersHost>,
        wake: Wake,
    ) -> Arc<Self> {
        Self::start_with(
            data_root,
            home,
            installer,
            Arc::new(Codesign),
            providers,
            wake,
        )
    }

    pub fn start_with(
        data_root: &Path,
        home: &Path,
        installer: Arc<dyn AgentInstaller>,
        signature: Arc<dyn Signature + Send + Sync>,
        providers: Arc<ModelProvidersHost>,
        wake: Wake,
    ) -> Arc<Self> {
        let layout = Layout {
            data_root: data_root.to_owned(),
            home: home.to_owned(),
        };
        let off = inactive_agents(data_root);
        let rows = AGENTS
            .into_iter()
            .map(|tool| (tool, Store::find_installed(tool, home).is_some()))
            .map(|(tool, installed)| AgentRow {
                tool: kind_of(tool),
                name: name_of(tool).to_owned(),
                source: source_of(tool).map_or_else(String::new, |s| {
                    format!("{}'s installer at {}", s.vendor, s.host)
                }),
                installed,
                version: None,
                signed_in: None,
                active: offered(tool, installed, &off),
                busy: Some(AgentBusy::Checking),
                error: None,
                signin_requested: false,
                node_prompt: None,
            })
            .collect();
        // Read here, not in the background: a Pi terminal restored at this
        // launch waits for it (`connecto_session::launch::pi_providers`).
        #[cfg(not(test))]
        let (keys, probe): (
            Arc<dyn crate::pi_providers::PiKeys>,
            Arc<dyn crate::pi_providers::LocalProbe>,
        ) = (
            Arc::new(crate::pi_providers::StoredKeys),
            Arc::new(crate::pi_providers::SystemProbe),
        );
        #[cfg(test)]
        let (keys, probe): (
            Arc<dyn crate::pi_providers::PiKeys>,
            Arc<dyn crate::pi_providers::LocalProbe>,
        ) = (
            Arc::new(crate::pi_providers::MemoryKeys::default()),
            Arc::new(crate::pi_providers::FakeProbe::default()),
        );
        let pi = PiProvidersHost::start(data_root, home, keys, probe, Arc::clone(&wake));
        let host = Arc::new(Self {
            layout,
            installer,
            signature,
            rows: Mutex::new(rows),
            pi,
            providers,
            wake,
        });
        host.refresh_in_background();
        host
    }

    /// The host of `scope`'s Pi providers.
    fn pi_host(&self, scope: &ProviderScope) -> Arc<PiProvidersHost> {
        match scope {
            ProviderScope::Global => Arc::clone(&self.pi),
            ProviderScope::Project(id) => self.providers.project_pi_host(id),
        }
    }

    fn lock(&self) -> MutexGuard<'_, Vec<AgentRow>> {
        self.rows.lock().unwrap_or_else(PoisonError::into_inner)
    }

    fn change(&self, tool: Tool, edit: impl FnOnce(&mut AgentRow)) {
        if let Some(row) = self.lock().iter_mut().find(|r| r.tool == kind_of(tool)) {
            edit(row);
        }
        (self.wake)();
    }

    fn refresh_in_background(self: &Arc<Self>) {
        let me = Arc::clone(self);
        if let Err(err) = std::thread::Builder::new()
            .name("agents-check".to_owned())
            .spawn(move || {
                for tool in AGENTS {
                    me.look(tool);
                }
            })
        {
            eprintln!("the agents check did not start: {err}");
        }
    }

    /// Find the program, its version, and whether the user is signed in. The
    /// verified stored copy is asked, as setup asks it (each check verifies it
    /// in full, as each launch does); the user's own install only when there is
    /// no usable stored copy (a Node launcher cannot run from here).
    fn look(&self, tool: Tool) {
        let (found, problem) = runnable_entry(&self.layout.data_root, &self.layout.home, tool);
        let version = found
            .as_deref()
            .and_then(|path| version_of_install(tool, path, &self.layout.home));
        let signed_in = found
            .as_deref()
            // The program itself (an npm Codex's `codex.cmd` is not run).
            .and_then(|path| crate::platform::runnable(tool, path))
            .and_then(|path| match system_signin(&self.layout, tool, &path) {
                // Antigravity's and OpenCode's answers only say that it runs.
                _ if matches!(tool, Tool::Antigravity | Tool::OpenCode) => None,
                AuthState::LoggedIn { .. } => Some(true),
                AuthState::LoggedOut => Some(false),
                AuthState::Unknown => None,
            });
        let off = inactive_agents(&self.layout.data_root);
        // Asked of the program as it is now (OpenCode upgrades itself in
        // place, and OpenCode 2 cannot run Connecto's plugin): not of the
        // store's record, and not by the file being there.
        let unsupported = found
            .as_deref()
            .and_then(|path| crate::setup::live_unsupported(tool, path, &self.layout.home));
        self.change(tool, |row| {
            row.installed = found.is_some();
            row.version = version;
            row.signed_in = signed_in;
            // Said, not silently replaced by the user's own install.
            row.error = unsupported.clone().or(problem);
            row.active = unsupported.is_none() && offered(tool, row.installed, &off);
            if matches!(row.busy, Some(AgentBusy::Checking)) {
                row.busy = None;
            }
        });
    }

    fn install_and_set_up(&self, tool: Tool) {
        let name = name_of(tool);
        let mut installed_now = false;
        let unusable = Store::unusable_installs(tool, &self.layout.home);
        if Store::find_installed(tool, &self.layout.home).is_none() && !unusable.is_empty() {
            let found = unusable
                .iter()
                .map(|(path, why)| format!("{}: {why}", path.display()))
                .collect::<Vec<_>>()
                .join("; ");
            self.change(tool, |row| {
                row.busy = None;
                row.error = Some(format!(
                    "{name} is on {THIS_COMPUTER} but Connecto cannot use this copy ({found}). Install it from its official page, or fix that install."
                ));
            });
            return;
        }
        if tool == Tool::Pi
            && Store::find_installed(tool, &self.layout.home).is_none()
            && let Some(prompt) =
                crate::node_setup::prompt(&self.installer.node_for_pi(&self.layout.home))
        {
            // Pi's installer would stop: the person is asked first.
            self.change(tool, |row| {
                row.busy = None;
                row.node_prompt = Some(prompt);
            });
            return;
        }
        if Store::find_installed(tool, &self.layout.home).is_none() {
            let say = |line: &str| {
                let line = line.to_owned();
                self.change(tool, |row| row.busy = Some(AgentBusy::Installing(line)));
            };
            match self.installer.install(tool, &self.layout.home, &say) {
                Ok(_) => installed_now = true,
                Err(failure) => {
                    self.change(tool, |row| {
                        row.busy = None;
                        row.error = Some(format!("{} {}", failure.what, failure.next));
                    });
                    return;
                }
            }
        }
        // Made ready for the isolated sessions: a verified copy in Connecto's store.
        if Store::find_installed(tool, &self.layout.home).is_some() {
            let store = Store::new(&self.layout.data_root);
            if let Err(err) = store.provision_found(
                tool,
                &self.layout.home,
                &Shared(Arc::clone(&self.signature)),
            ) && store
                .newest(tool)
                .ok()
                .and_then(|v| store.verify(tool, &v).ok())
                .is_none()
            {
                self.change(tool, |row| {
                    row.busy = None;
                    row.error = Some(format!(
                        "{name} could not be made ready ({err}). Try again, or reinstall it from its official page."
                    ));
                });
                return;
            }
        }
        crate::setup::forget_skipped(&self.layout.data_root, tool);
        let mut off = inactive_agents(&self.layout.data_root);
        off.retain(|t| *t != tool);
        write_inactive(&self.layout.data_root, &off);
        self.look(tool);
        self.change(tool, |row| {
            row.busy = None;
            row.error = None;
            // After an install, or when not signed in, the person goes on to sign in.
            // Pi has no account to sign in to: its providers are in Settings.
            row.signin_requested =
                tool != Tool::Pi && (installed_now || row.signed_in != Some(true));
        });
    }
}

/// What Agentic Works decides a job's agent from (ADR 0041), as a terminal
/// launch in the project would: the settings (the rows the background checks
/// keep, the agents switched off, the project's saved Tools), read at every
/// claim; then the look: for Pi its providers (a project's own host started
/// when it is not running yet), and the agent's sign-in in the profile the
/// project uses (the launch's own check, Claude's providers included).
pub struct JobAgents(pub Arc<AgentsHost>);

impl JobAgents {
    /// Pi's providers in `scope` (the one resolved for the look).
    fn pi(&self, scope: &ProviderScope) -> Availability {
        let host = &self.0;
        // A project's own host is started here when it is not running yet,
        // its keys read, in the look's counted blocking task.
        let running = match scope {
            ProviderScope::Global => Arc::clone(&host.pi),
            ProviderScope::Project(id) => host.providers.project_pi_host(id),
        };
        let view = running.view();
        if view.usable() {
            Availability::Usable
        } else if view.undecided() {
            Availability::Pending("Connecto is still asking Pi's local model server".into())
        } else {
            Availability::Unusable("no model provider that works for the project".into())
        }
    }

    /// The agent's sign-in as a launch in `root` checks it, in `profile`
    /// (resolved for the look, not read again): Claude set up for Together
    /// AI needs none; else its status command, in that profile.
    fn signed_in(&self, root: &Path, tool: Tool, profile: &Profile) -> Availability {
        use connecto_session::launch::ModelProviders;
        let layout = &self.0.layout;
        // Claude's providers, asked as its launch asks them (the providers
        // host is the launch's `model_providers()`): through TogetherLink no
        // login is needed; a configuration that does not work (TogetherLink
        // missing, the secret store unreadable) stops the launch, so the
        // job looks further.
        if tool == Tool::Claude
            && let Some(scope) = &profile.scope
        {
            match self.0.providers.claude(scope) {
                Ok(providers) if providers.together.is_some() => return Availability::Usable,
                Ok(_) => {}
                Err(why) => return Availability::Unusable(why),
            }
        }
        let Some(entry) = runnable_entry(&layout.data_root, &layout.home, tool)
            .0
            .and_then(|found| crate::platform::runnable(tool, &found))
        else {
            return Availability::Unusable("not installed".into());
        };
        let installed = connecto_store::Installed {
            version: String::new(),
            entry,
        };
        let system_config = profile.system_config.unwrap_or(true);
        match connecto_session::launch::check_auth_as(layout, root, tool, &installed, system_config)
        {
            Ok(AuthState::LoggedIn { .. }) => Availability::Usable,
            Ok(AuthState::LoggedOut) => {
                Availability::Unusable("not signed in for this project".into())
            }
            Ok(_) => Availability::Unusable("its sign-in for this project cannot be told".into()),
            Err(err) => Availability::Unusable(err.to_string()),
        }
    }
}

/// Whether `project`'s Tools, as saved, allow `tool`. As saved: the packages
/// manager saves a project's settings before its view has them, and has not
/// looked at a project just opened.
fn project_allows(data_root: &Path, project: &str, tool: ToolKind) -> bool {
    connecto_session::project_settings::load(data_root, project)
        .tools
        .allows(tool)
}

/// The profile `tool` is run in for `project` (the system's or the
/// project's own, as its settings say) and whose model providers it gets, as
/// a launch would resolve them: what a look depends on.
fn auth_profile(data_root: &Path, project: &str, tool: ToolKind) -> Profile {
    use connecto_core::ConfigMode;
    let settings = connecto_session::project_settings::load(data_root, project);
    let mode = match tool {
        ToolKind::Claude => settings.claude_config,
        ToolKind::Codex => settings.codex_config,
        ToolKind::Pi => settings.pi_config,
        ToolKind::OpenCode => settings.opencode_config,
        // Antigravity always runs in the user's own profile.
        ToolKind::Antigravity | ToolKind::Shell => ConfigMode::System,
    };
    let store_tool = tool_of(tool);
    Profile {
        system_config: store_tool.map(|_| mode == ConfigMode::System),
        scope: store_tool
            .filter(|t| matches!(t, Tool::Claude | Tool::Pi))
            .map(|t| connecto_session::launch::provider_scope(data_root, project, t)),
    }
}

impl crate::agentic_works::agents::AgentCheck for JobAgents {
    fn profile(&self, project: &str, tool: ToolKind) -> Profile {
        auth_profile(&self.0.layout.data_root, project, tool)
    }

    fn allows(&self, project: &str, tool: ToolKind) -> bool {
        project_allows(&self.0.layout.data_root, project, tool)
    }

    /// Pi's models from the providers its terminals in the project get: the
    /// app's, or the project's own when their host runs already. A host not
    /// running is not started from a view (nor its keys read): then only
    /// Pi's default model is offered.
    fn models(&self, project: &str, tool: ToolKind) -> Option<Vec<String>> {
        (tool == ToolKind::Pi).then(|| {
            match connecto_session::launch::provider_scope(
                &self.0.layout.data_root,
                project,
                Tool::Pi,
            ) {
                ProviderScope::Global => self.0.pi.view().models,
                ProviderScope::Project(id) => self
                    .0
                    .providers
                    .live_pi(&id)
                    .map(|host| host.view().models)
                    .unwrap_or_default(),
            }
        })
    }

    fn settings(&self, project: &str, tool: ToolKind) -> Availability {
        let host = &self.0;
        let data_root = &host.layout.data_root;
        let row = host.lock().iter().find(|r| r.tool == tool).cloned();
        let off: Vec<ToolKind> = inactive_agents(data_root)
            .into_iter()
            .map(kind_of)
            .collect();
        crate::agentic_works::agents::from_settings(
            row.as_ref(),
            project_allows(data_root, project, tool),
            &off,
        )
    }

    fn look(&self, _: &str, root: &Path, tool: ToolKind, profile: &Profile) -> Availability {
        let Some(store_tool) = tool_of(tool) else {
            return Availability::Unusable("not an agent".into());
        };
        if tool == ToolKind::Pi {
            let pi = profile
                .scope
                .as_ref()
                .map_or(Availability::Usable, |scope| self.pi(scope));
            if pi != Availability::Usable {
                return pi;
            }
        }
        self.signed_in(root, store_tool, profile)
    }
}

/// What `connecto agents` reads (ADR 0045), read only: it never starts a
/// project's Pi providers host (whose first check may save a moved LM Studio
/// address), and never builds the Settings view (which reads the secret
/// store).
impl JobAgents {
    /// The version the background check found, from the rows it keeps.
    pub fn version(&self, tool: ToolKind) -> Option<String> {
        self.0
            .lock()
            .iter()
            .find(|row| row.tool == tool)
            .and_then(|row| row.version.clone())
    }

    /// Pi's providers in `scope` as they are known now: the app's, or a
    /// project's own when their host runs already; `None` when it does not
    /// run yet (it is started by the project's first Pi terminal or job).
    pub fn pi_known(&self, scope: &ProviderScope) -> Option<Availability> {
        let view = match scope {
            ProviderScope::Global => self.0.pi.view(),
            ProviderScope::Project(id) => self.0.providers.live_pi(id)?.view(),
        };
        Some(if view.usable() {
            Availability::Usable
        } else if view.undecided() {
            Availability::Pending("Connecto is still asking Pi's local model server".into())
        } else {
            Availability::Unusable("no model provider that works for the project".into())
        })
    }

    /// Whether Claude reaches its model through TogetherLink in `profile`
    /// (which then refuses `--model` and runs Together's own model).
    ///
    /// # Errors
    /// The providers cannot be read (the secret store, TogetherLink missing).
    pub fn claude_through_together(&self, profile: &Profile) -> Result<bool, String> {
        use connecto_session::launch::ModelProviders;
        let Some(scope) = &profile.scope else {
            return Ok(false);
        };
        self.0
            .providers
            .claude(scope)
            .map(|providers| providers.together.is_some())
    }

    /// The sign-in check alone, as a launch in `root` makes it.
    pub fn sign_in(&self, root: &Path, tool: ToolKind, profile: &Profile) -> Availability {
        match tool_of(tool) {
            Some(store_tool) => self.signed_in(root, store_tool, profile),
            None => Availability::Unusable("not an agent".into()),
        }
    }
}

/// What the window holds.
pub struct Handle(pub Arc<AgentsHost>);

impl AgentsControl for Handle {
    fn view(&self) -> Arc<AgentsView> {
        let pi = self.0.pi.view();
        let mut rows = self.0.lock().clone();
        // Pi with nothing to talk to is not offered: a terminal could only
        // say that it has no model. Said on its row, where it is set up.
        if pi_unavailable(&pi)
            && let Some(row) = rows
                .iter_mut()
                .find(|row| row.tool == ToolKind::Pi && row.installed)
        {
            row.active = false;
            if row.error.is_none() {
                row.error = Some(NO_PI_PROVIDER.to_owned());
            }
        }
        let claude = self
            .0
            .providers
            .claude_view(&connecto_core::ProviderScope::Global);
        Arc::new(AgentsView { rows, pi, claude })
    }

    fn activate(&self, kind: ToolKind) {
        let Some(tool) = tool_of(kind) else {
            return;
        };
        {
            let mut rows = self.0.lock();
            let Some(row) = rows.iter_mut().find(|r| r.tool == kind) else {
                return;
            };
            if row.busy.is_some() {
                return;
            }
            row.busy = Some(AgentBusy::Installing("Starting…".to_owned()));
            row.error = None;
            row.node_prompt = None;
        }
        (self.0.wake)();
        let host = Arc::clone(&self.0);
        if let Err(err) = std::thread::Builder::new()
            .name("agents-activate".to_owned())
            .spawn(move || host.install_and_set_up(tool))
        {
            self.0.change(tool, |row| {
                row.busy = None;
                row.error = Some(format!("Could not start: {err}"));
            });
        }
    }

    fn install_node(&self, kind: ToolKind) {
        let Some(tool) = tool_of(kind) else {
            return;
        };
        {
            let mut rows = self.0.lock();
            let Some(row) = rows.iter_mut().find(|r| r.tool == kind) else {
                return;
            };
            if row.busy.is_some() || row.node_prompt.is_none() {
                return;
            }
            row.node_prompt = None;
            row.busy = Some(AgentBusy::Installing("Starting…".to_owned()));
            row.error = None;
        }
        (self.0.wake)();
        let host = Arc::clone(&self.0);
        let run = move || {
            let say = |line: &str| {
                let line = line.to_owned();
                host.change(tool, |row| row.busy = Some(AgentBusy::Installing(line)));
            };
            match host.installer.install_node(&host.layout.home, &say) {
                Ok(()) => host.install_and_set_up(tool),
                Err(failure) => host.change(tool, |row| {
                    row.busy = None;
                    row.error = Some(format!("{} {}", failure.what, failure.next));
                }),
            }
        };
        if let Err(err) = std::thread::Builder::new()
            .name("agents-node".to_owned())
            .spawn(run)
        {
            self.0.change(tool, |row| {
                row.busy = None;
                row.error = Some(format!("Could not start: {err}"));
            });
        }
    }

    fn decline_node(&self, kind: ToolKind) {
        if let Some(tool) = tool_of(kind) {
            self.0.change(tool, |row| {
                if row.node_prompt.take().is_some() {
                    row.error = Some(crate::node_setup::declined());
                }
            });
        }
    }

    fn deactivate(&self, kind: ToolKind) {
        let Some(tool) = tool_of(kind) else {
            return;
        };
        let mut off = inactive_agents(&self.0.layout.data_root);
        if !off.contains(&tool) {
            off.push(tool);
        }
        write_inactive(&self.0.layout.data_root, &off);
        self.0.change(tool, |row| row.active = false);
    }

    fn refresh(&self) {
        self.0.refresh_in_background();
        self.0.pi.check_local();
    }

    fn signin_started(&self, kind: ToolKind) {
        if let Some(tool) = tool_of(kind) {
            self.0.change(tool, |row| row.signin_requested = false);
        }
    }

    fn set_pi_key(
        &self,
        scope: &ProviderScope,
        provider: connecto_core::PiProvider,
        key: Option<String>,
    ) -> Result<(), String> {
        let done = self.0.pi_host(scope).set_key(provider, key.as_deref());
        (self.0.wake)();
        done
    }

    fn set_pi_default_model(
        &self,
        scope: &ProviderScope,
        model: Option<String>,
    ) -> Result<(), String> {
        let done = self.0.pi_host(scope).set_default_model(model.as_deref());
        (self.0.wake)();
        done
    }

    fn set_pi_lmstudio_url(&self, scope: &ProviderScope, url: String) -> Result<(), String> {
        let done = self.0.pi_host(scope).set_lmstudio_url(&url);
        (self.0.wake)();
        done
    }

    fn pi_providers(&self, scope: &ProviderScope) -> connecto_core::PiProvidersView {
        self.0.pi_host(scope).view()
    }

    fn check_pi_local(&self, scope: &ProviderScope) {
        self.0.pi_host(scope).check_local();
    }

    fn claude_providers(&self, scope: &ProviderScope) -> connecto_core::ClaudeProvidersView {
        self.0.providers.claude_view(scope)
    }

    fn set_claude_together_key(
        &self,
        scope: &ProviderScope,
        key: Option<String>,
    ) -> Result<(), String> {
        self.0
            .providers
            .set_claude_together_key(scope, key.as_deref())
    }

    fn check_togetherlink(&self) {
        self.0.providers.check_togetherlink();
    }

    fn install_togetherlink(&self) {
        self.0.providers.install_togetherlink();
    }

    fn decline_togetherlink(&self) {
        self.0.providers.decline_togetherlink();
    }

    fn set_own_providers(&self, project: &str, tool: ToolKind, own: bool) -> Result<(), String> {
        self.0.providers.set_own(project, tool, own)
    }

    fn provider_scope(&self, project: &str, tool: ToolKind) -> ProviderScope {
        tool_of(tool).map_or(ProviderScope::Global, |tool| {
            connecto_session::launch::provider_scope(&self.0.layout.data_root, project, tool)
        })
    }

    /// Switched off in Settings or not installed, as the rows say before
    /// any provider is looked at; then Pi, with the providers that project's
    /// Pi terminals would get.
    fn inactive_for(&self, project: &str) -> Vec<ToolKind> {
        let rows = self.0.lock().clone();
        let mut kinds: Vec<ToolKind> = rows
            .iter()
            .filter(|row| !row.active)
            .map(|row| row.tool)
            .chain(
                inactive_agents(&self.0.layout.data_root)
                    .into_iter()
                    .map(kind_of),
            )
            .fold(Vec::new(), |mut kinds, kind| {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
                kinds
            });
        let pi_installed = rows
            .iter()
            .any(|row| row.tool == ToolKind::Pi && row.installed);
        if pi_installed && !kinds.contains(&ToolKind::Pi) {
            let scope = self.provider_scope(project, ToolKind::Pi);
            if pi_unavailable(&self.0.pi_host(&scope).view()) {
                kinds.push(ToolKind::Pi);
            }
        }
        kinds
    }

    fn inactive(&self) -> Vec<ToolKind> {
        let off = inactive_agents(&self.0.layout.data_root);
        self.view()
            .rows
            .iter()
            .filter(|row| !row.active)
            .map(|row| row.tool)
            .chain(off.into_iter().map(kind_of))
            .fold(Vec::new(), |mut kinds, kind| {
                if !kinds.contains(&kind) {
                    kinds.push(kind);
                }
                kinds
            })
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use crate::agent_install::{Failure, Installed};
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::time::{Duration, Instant};

    struct AnySignature;
    impl Signature for AnySignature {
        fn verify(&self, _file: &Path, _team: Option<&str>) -> Result<(), String> {
            Ok(())
        }
    }

    /// Installs a fake Claude into the home it is given, or fails when told to.
    struct Fake {
        offline: AtomicBool,
        calls: Mutex<Vec<Tool>>,
        /// What the Node.js check finds; installing Node.js makes it `Ready`.
        node: Mutex<crate::node_setup::NodeCheck>,
        node_installs: Mutex<u32>,
    }

    impl AgentInstaller for Fake {
        fn node_for_pi(&self, _home: &Path) -> crate::node_setup::NodeCheck {
            self.node.lock().unwrap().clone()
        }

        fn install_node(&self, _home: &Path, say: &dyn Fn(&str)) -> Result<(), Failure> {
            say("Downloading Node.js from nodejs.org…");
            *self.node_installs.lock().unwrap() += 1;
            *self.node.lock().unwrap() = crate::node_setup::NodeCheck::Ready;
            Ok(())
        }

        fn install(
            &self,
            tool: Tool,
            home: &Path,
            say: &dyn Fn(&str),
        ) -> Result<Installed, Failure> {
            self.calls.lock().unwrap().push(tool);
            say("Downloading the installer…");
            if self.offline.load(Ordering::SeqCst) {
                return Err(Failure {
                    what: "Connecto could not reach claude.ai. Is this Mac online?".to_owned(),
                    next: "Check your internet connection, then try again.".to_owned(),
                    details: String::new(),
                });
            }
            // `claude.exe` on Windows, where discovery looks for that name.
            let path = home.join(".local/bin").join(format!(
                "{}{}",
                tool.name(),
                std::env::consts::EXE_SUFFIX
            ));
            match tool {
                // Claude is taken only as a native program, so its fake is one.
                Tool::Claude => crate::test_support::link_native("4.5.6 (Fake)", &path),
                _ => crate::test_support::link_fake("#!/bin/sh\necho '4.5.6 (Fake)'\n", &path),
            }
            Ok(Installed {
                path,
                version: "4.5.6 (Fake)".to_owned(),
            })
        }
    }

    /// Claude's and the projects' providers, with keys in memory.
    fn test_providers(root: &Path) -> Arc<ModelProvidersHost> {
        let host = ModelProvidersHost::new(
            &root.join("data"),
            &root.join("home"),
            Arc::new(crate::pi_providers::MemoryKeys::default()),
            Arc::new(crate::pi_providers::FakeProbe::default()),
            Arc::new(crate::model_providers::FakeTogetherLink {
                fail: false,
                runs: Mutex::default(),
            }),
            Arc::new(|| {}),
        );
        host.mark_ready();
        host
    }

    fn setup(name: &str, offline: bool) -> (PathBuf, Arc<Fake>, Handle) {
        static N: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = N.fetch_add(1, Ordering::Relaxed);
        let root = fs::canonicalize({
            let dir = std::env::temp_dir()
                .join(format!("connecto-agents-{name}-{}-{n}", std::process::id()));
            fs::create_dir_all(dir.join("home")).unwrap();
            fs::create_dir_all(dir.join("data")).unwrap();
            dir
        })
        .unwrap();
        let fake = Arc::new(Fake {
            offline: AtomicBool::new(offline),
            calls: Mutex::default(),
            node: Mutex::new(crate::node_setup::NodeCheck::Ready),
            node_installs: Mutex::default(),
        });
        let host = AgentsHost::start_with(
            &root.join("data"),
            &root.join("home"),
            Arc::clone(&fake) as Arc<dyn AgentInstaller>,
            Arc::new(AnySignature),
            test_providers(&root),
            Arc::new(|| {}),
        );
        (root, fake, Handle(host))
    }

    /// An OpenCode recorded as 1.x that upgraded itself in place to 2: its
    /// row says it is installed and why it is not supported, and it is not
    /// offered for new terminals, though the file and the record are there.
    #[cfg(unix)]
    #[test]
    fn an_opencode_upgraded_in_place_to_2_is_shown_and_not_offered() {
        upgraded_to_2_is_not_offered("oc2", "#!/bin/sh\necho '2.3.2'\n");
    }

    /// The same when the 2 prints its version on stderr alone.
    #[cfg(unix)]
    #[test]
    fn an_opencode_upgraded_to_a_2_that_prints_on_stderr_is_not_offered() {
        upgraded_to_2_is_not_offered("oc2-err", "#!/bin/sh\necho '2.3.2' >&2\n");
    }

    #[cfg(unix)]
    fn upgraded_to_2_is_not_offered(name: &str, two: &str) {
        let root = fs::canonicalize({
            let dir =
                std::env::temp_dir().join(format!("connecto-agents-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("home")).unwrap();
            fs::create_dir_all(dir.join("data")).unwrap();
            dir
        })
        .unwrap();
        let entry = root.join("home/.opencode/bin/opencode");
        crate::test_support::link_fake("#!/bin/sh\necho '1.19.6'\n", &entry);
        Store::new(&root.join("data"))
            .provision(Tool::OpenCode, &entry, &AnySignature)
            .unwrap();
        crate::test_support::link_fake(two, &entry);
        let host = AgentsHost::start_with(
            &root.join("data"),
            &root.join("home"),
            Arc::new(Fake {
                offline: AtomicBool::new(false),
                calls: Mutex::default(),
                node: Mutex::new(crate::node_setup::NodeCheck::Ready),
                node_installs: Mutex::default(),
            }),
            Arc::new(AnySignature),
            test_providers(&root),
            Arc::new(|| {}),
        );
        let handle = Handle(host);
        let view = wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        let opencode = row(&view, ToolKind::OpenCode);
        assert!(opencode.installed, "{opencode:?}");
        assert!(!opencode.active, "{opencode:?}");
        assert!(
            opencode
                .error
                .as_deref()
                .is_some_and(|e| e.contains("OpenCode 2.3.2 is not supported yet")),
            "{opencode:?}"
        );
        assert!(handle.inactive().contains(&ToolKind::OpenCode));
        let _ = fs::remove_dir_all(&root);
    }

    /// Waits for the rows to reach `done`. Returns as soon as they do; the limit
    /// is generous, because a loaded machine (a parallel test run) is slow.
    fn wait(handle: &Handle, done: impl Fn(&AgentsView) -> bool) -> Arc<AgentsView> {
        let deadline = Instant::now() + crate::test_support::scaled(Duration::from_secs(60));
        loop {
            let view = handle.view();
            if done(&view) {
                return view;
            }
            assert!(Instant::now() < deadline, "waiting: {:?}", handle.view());
            std::thread::sleep(Duration::from_millis(25));
        }
    }

    fn row(view: &AgentsView, tool: ToolKind) -> AgentRow {
        view.rows.iter().find(|r| r.tool == tool).unwrap().clone()
    }

    /// A project whose Pi is Isolated with a provider of its own that works
    /// offers Pi even when no app-wide provider works; other projects do not,
    /// and switching Pi off in Settings still wins.
    #[test]
    fn pi_is_offered_by_the_providers_of_the_project_its_terminal_would_get() {
        use connecto_core::{ConfigMode, ProjectSettings};
        let (root, _fake, handle) = setup("pi-scope", false);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        // Pi is installed (as the rows say); nothing app-wide works.
        if let Some(row) = handle.0.lock().iter_mut().find(|r| r.tool == ToolKind::Pi) {
            row.installed = true;
            row.active = true;
        }
        wait(&handle, |v| {
            v.pi.lmstudio != connecto_core::PiLocalState::Checking
        });
        assert!(handle.inactive().contains(&ToolKind::Pi));
        let data = root.join("data");
        for id in ["own", "other"] {
            connecto_session::project_settings::save(
                &data,
                id,
                &ProjectSettings {
                    pi_config: ConfigMode::Isolated,
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
        }
        let own = ProviderScope::Project("own".into());
        handle.set_own_providers("own", ToolKind::Pi, true).unwrap();
        handle
            .set_pi_key(
                &own,
                connecto_core::PiProvider::Together,
                Some("tgp_v1_own".into()),
            )
            .unwrap();
        assert_eq!(handle.provider_scope("own", ToolKind::Pi), own);
        assert_eq!(
            handle.provider_scope("other", ToolKind::Pi),
            ProviderScope::Global
        );
        assert!(!handle.inactive_for("own").contains(&ToolKind::Pi));
        assert!(handle.inactive_for("other").contains(&ToolKind::Pi));
        // The chooser lists that project's models, not the app-wide ones.
        assert!(
            handle
                .pi_providers(&own)
                .has_key(connecto_core::PiProvider::Together)
        );
        assert!(handle.view().pi.keys_set.is_empty());
        // Switched off in Settings: off everywhere.
        handle.deactivate(ToolKind::Pi);
        assert!(handle.inactive_for("own").contains(&ToolKind::Pi));
    }

    /// Review (Agent for jobs), finding 4: a project with Pi providers of its
    /// own is offered its Pi models once its host runs; one whose host is
    /// not running is offered none, and no host is started for it.
    #[test]
    fn a_projects_own_pi_models_are_offered_for_its_jobs_once_loaded() {
        use crate::agentic_works::agents::AgentCheck;
        use connecto_core::{ConfigMode, ProjectSettings};
        let (root, _fake, _) = setup("pi-job-models", false);
        let probe = Arc::new(crate::pi_providers::FakeProbe::default());
        *probe.models.lock().unwrap() = Some(vec!["together/own-model".into()]);
        let providers = ModelProvidersHost::new(
            &root.join("data"),
            &root.join("home"),
            Arc::new(crate::pi_providers::MemoryKeys::default()),
            Arc::clone(&probe) as Arc<dyn crate::pi_providers::LocalProbe>,
            Arc::new(crate::model_providers::FakeTogetherLink {
                fail: false,
                runs: Mutex::default(),
            }),
            Arc::new(|| {}),
        );
        providers.mark_ready();
        let handle = Handle(AgentsHost::start_with(
            &root.join("data"),
            &root.join("home"),
            Arc::new(Fake {
                offline: AtomicBool::new(false),
                calls: Mutex::default(),
                node: Mutex::new(crate::node_setup::NodeCheck::Ready),
                node_installs: Mutex::default(),
            }) as Arc<dyn AgentInstaller>,
            Arc::new(AnySignature),
            providers,
            Arc::new(|| {}),
        ));
        let data = root.join("data");
        for id in ["own", "cold"] {
            connecto_session::project_settings::save(
                &data,
                id,
                &ProjectSettings {
                    pi_config: ConfigMode::Isolated,
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
            handle.set_own_providers(id, ToolKind::Pi, true).unwrap();
        }
        let jobs = JobAgents(Arc::clone(&handle.0));
        assert_eq!(jobs.models("cold", ToolKind::Pi), Some(Vec::new()));
        assert!(
            handle.0.providers.live_pi("cold").is_none(),
            "no host started from a view"
        );
        handle
            .set_pi_key(
                &ProviderScope::Project("own".into()),
                connecto_core::PiProvider::Together,
                Some("tgp_v1_own".into()),
            )
            .unwrap();
        let deadline = Instant::now() + Duration::from_secs(10);
        while jobs.models("own", ToolKind::Pi) != Some(vec!["together/own-model".into()]) {
            assert!(
                Instant::now() < deadline,
                "offered {:?}",
                jobs.models("own", ToolKind::Pi)
            );
            std::thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(jobs.models("own", ToolKind::Codex), None);
        let _ = fs::remove_dir_all(&root);
    }

    /// The settings, then the look when they let the agent through: what a
    /// job is told of it.
    fn both(jobs: &JobAgents, project: &str, folder: &Path, tool: ToolKind) -> Availability {
        use crate::agentic_works::agents::AgentCheck;
        match jobs.settings(project, tool) {
            Availability::Usable => jobs.look(project, folder, tool, &jobs.profile(project, tool)),
            other => other,
        }
    }

    /// Review findings (ADR 0041): a job's agent is chosen from the
    /// project's Tools as saved, even before the packages manager has looked
    /// at the project; an agent still being checked is pending, never
    /// guessed; a project's Pi host not started yet is started off the
    /// caller's thread and Pi is pending meanwhile.
    #[test]
    fn a_jobs_agents_come_from_the_saved_tools_and_finished_checks() {
        use crate::agentic_works::agents::AgentCheck;
        use connecto_core::{ConfigMode, ProjectSettings, ProjectTools};
        let (root, _fake, handle) = setup("job-agents", false);
        let data = root.join("data");
        let folder = root.join("home").join("project");
        connecto_session::project_settings::save(
            &data,
            "claude-only",
            &ProjectSettings {
                tools: ProjectTools::Claude,
                ..ProjectSettings::default()
            },
        )
        .unwrap();
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        for row in handle.0.lock().iter_mut() {
            row.installed = true;
            row.active = true;
        }
        let jobs = JobAgents(Arc::clone(&handle.0));
        assert_eq!(
            both(&jobs, "claude-only", &folder, ToolKind::Codex),
            Availability::Unusable("turned off in the project's settings".into())
        );
        // A project with every tool: Codex still being checked is pending.
        if let Some(row) = handle
            .0
            .lock()
            .iter_mut()
            .find(|r| r.tool == ToolKind::Codex)
        {
            row.busy = Some(AgentBusy::Checking);
        }
        assert!(matches!(
            both(&jobs, "any", &folder, ToolKind::Codex),
            Availability::Pending(_)
        ));
        // Pi, Isolated with no host yet: pending, and the host is started.
        let pi_project = "pi-own";
        connecto_session::project_settings::save(
            &data,
            pi_project,
            &ProjectSettings {
                pi_config: ConfigMode::Isolated,
                ..ProjectSettings::default()
            },
        )
        .unwrap();
        handle
            .set_own_providers(pi_project, ToolKind::Pi, true)
            .unwrap();
        assert!(handle.0.providers.live_pi(pi_project).is_none());
        // Fourth review, finding 3: started within the look itself (its
        // counted blocking task), not on a thread left running after it.
        let pi = jobs.look(
            pi_project,
            &folder,
            ToolKind::Pi,
            &jobs.profile(pi_project, ToolKind::Pi),
        );
        assert!(
            handle.0.providers.live_pi(pi_project).is_some(),
            "the look ended before the project's Pi host started ({pi:?})"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// `connecto agents` only reads, since it runs under Pause too (ADR
    /// 0045): for a project's own Pi providers whose host is not running it
    /// says Pi is unverified and leaves the host unstarted (a started one may
    /// save a moved LM Studio address); Antigravity, whose check sees only
    /// that `agy` answers, is never more than unverified; versions come from
    /// the rows.
    #[test]
    fn the_roster_never_starts_a_projects_pi_host_and_calls_antigravity_unverified() {
        use crate::roster_host::{HostProbe, Probe};
        use connecto_core::roster::AgentState;
        use connecto_core::{ConfigMode, ProjectSettings};
        let (root, _fake, handle) = setup("roster-reads", false);
        let data = root.join("data");
        let folder = root.join("home").join("project");
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        for row in handle.0.lock().iter_mut() {
            row.installed = true;
            row.active = true;
            row.version = Some(format!("{}-1.0.0", row.tool.name()));
        }
        let pi_project = "roster-pi";
        connecto_session::project_settings::save(
            &data,
            pi_project,
            &ProjectSettings {
                pi_config: ConfigMode::Isolated,
                ..ProjectSettings::default()
            },
        )
        .unwrap();
        handle
            .set_own_providers(pi_project, ToolKind::Pi, true)
            .unwrap();
        let probe = HostProbe(Arc::clone(&handle.0));
        let profile = probe.profile(pi_project, ToolKind::Pi);
        let pi = probe.look(pi_project, &folder, ToolKind::Pi, &profile);
        assert!(matches!(pi.state, AgentState::Unverified(_)), "{pi:?}");
        assert!(
            handle.0.providers.live_pi(pi_project).is_none(),
            "the roster started the project's Pi host"
        );
        let profile = probe.profile(pi_project, ToolKind::Antigravity);
        let agy = probe.look(pi_project, &folder, ToolKind::Antigravity, &profile);
        assert!(
            matches!(
                agy.state,
                AgentState::Unverified(_) | AgentState::Unusable(_)
            ),
            "{agy:?}"
        );
        assert_eq!(
            probe.version(ToolKind::Codex).as_deref(),
            Some("codex-1.0.0")
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Third review, finding 2: Claude set up for Together AI with no
    /// TogetherLink cannot start; its launch says why. A job's look keeps
    /// that reason (not "usable", not "not installed"), so the choice goes
    /// on to Pi and OpenCode.
    #[test]
    fn a_jobs_claude_with_a_broken_provider_is_unusable_with_its_reason() {
        use connecto_session::launch::ModelProviders;
        let (root, _fake, handle) = setup("job-claude-provider", false);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        if let Some(row) = handle
            .0
            .lock()
            .iter_mut()
            .find(|r| r.tool == ToolKind::Claude)
        {
            row.installed = true;
            row.active = true;
        }
        handle
            .0
            .providers
            .set_claude_together_key(&ProviderScope::Global, Some("tgp_v1_key"))
            .unwrap();
        let why = handle
            .0
            .providers
            .claude(&ProviderScope::Global)
            .unwrap_err();
        let folder = root.join("home").join("project");
        fs::create_dir_all(&folder).unwrap();
        assert_eq!(
            both(
                &JobAgents(Arc::clone(&handle.0)),
                "p",
                &folder,
                ToolKind::Claude
            ),
            Availability::Unusable(why)
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// A Codex release, stored as Connecto's copy and shown installed, whose
    /// sign-in depends on the profile it is run in: its own CODEX_HOME
    /// (Isolated: signed in when `<root>/isolated-in` exists) or none (the
    /// system's: `<root>/system-in`).
    #[cfg(unix)]
    fn fake_codex(root: &Path, handle: &Handle) {
        let data = root.join("data");
        let release = root.join("codex-release");
        fs::create_dir_all(release.join("bin")).unwrap();
        fs::write(release.join("codex-package.json"), "{}").unwrap();
        for (dir, file) in [("codex-path", "rg"), ("codex-resources", "notes")] {
            fs::create_dir_all(release.join(dir)).unwrap();
            fs::write(release.join(dir).join(file), "").unwrap();
        }
        let entry = release.join("bin").join("codex");
        fs::write(
            &entry,
            format!(
                "#!/bin/sh\n\
                 if [ \"$1\" = --version ]; then echo 'codex-cli 0.167.0'; exit 0; fi\n\
                 if [ \"$1 $2\" = 'login status' ]; then\n\
                   if [ -n \"$CODEX_HOME\" ]; then f='{0}/isolated-in'; else f='{0}/system-in'; fi\n\
                   if [ -e \"$f\" ]; then echo 'Logged in using ChatGPT'; exit 0; fi\n\
                   echo 'Not logged in'; exit 1\n\
                 fi\n",
                root.display()
            ),
        )
        .unwrap();
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        Store::new(&data)
            .provision(Tool::Codex, &entry, &AnySignature)
            .unwrap();
        wait(handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        if let Some(row) = handle
            .0
            .lock()
            .iter_mut()
            .find(|r| r.tool == ToolKind::Codex)
        {
            row.installed = true;
            row.active = true;
        }
    }

    /// Fourth review, finding 1: with looks kept as in production, a job
    /// claimed after the project's Tools turned Codex off is told so, not
    /// given the look kept from the job before; the look itself is kept.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_change_of_tools_holds_for_the_next_job_with_looks_kept() {
        use crate::agentic_works::agents::{KEEP_LOOKS, MAX_LOOKS, Probes};
        use connecto_core::{ProjectSettings, ProjectTools};
        let (root, _fake, handle) = setup("job-tools", false);
        fake_codex(&root, &handle);
        fs::write(root.join("system-in"), "").unwrap();
        let data = root.join("data");
        let folder = root.join("home").join("project");
        fs::create_dir_all(&folder).unwrap();
        let tools = |tools| {
            connecto_session::project_settings::save(
                &data,
                "p",
                &ProjectSettings {
                    tools,
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
        };
        let probes = Probes::new(
            Arc::new(JobAgents(Arc::clone(&handle.0))),
            MAX_LOOKS,
            KEEP_LOOKS,
        );
        let never = || None;
        tools(ProjectTools::All);
        assert_eq!(
            probes.ask("p", &folder, ToolKind::Codex, &never).await,
            Ok(Availability::Usable)
        );
        tools(ProjectTools::Claude);
        assert_eq!(
            probes.ask("p", &folder, ToolKind::Codex, &never).await,
            Ok(Availability::Unusable(
                "turned off in the project's settings".into()
            ))
        );
        // Back on, and signed out since: the look kept from the first job
        // still answers (it is kept 30 seconds).
        tools(ProjectTools::All);
        fs::remove_file(root.join("system-in")).unwrap();
        assert_eq!(
            probes.ask("p", &folder, ToolKind::Codex, &never).await,
            Ok(Availability::Usable)
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Fifth review: a look is kept per profile. With looks kept as in
    /// production, a project whose Codex switches from the system profile
    /// (signed in) to its own (signed out) is told it is signed out, not
    /// given the look kept from the system profile; and back.
    #[cfg(unix)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_switch_of_profile_is_not_answered_by_a_kept_look() {
        use crate::agentic_works::agents::{KEEP_LOOKS, MAX_LOOKS, Probes};
        use connecto_core::{ConfigMode, ProjectSettings};
        use connecto_session::launch::project_id;
        let (root, _fake, handle) = setup("job-profile", false);
        fake_codex(&root, &handle);
        fs::write(root.join("system-in"), "").unwrap();
        let data = root.join("data");
        let folder = root.join("home").join("project");
        fs::create_dir_all(&folder).unwrap();
        let folder = fs::canonicalize(folder).unwrap();
        let id = project_id(&folder);
        let codex_config = |mode| {
            connecto_session::project_settings::save(
                &data,
                &id,
                &ProjectSettings {
                    codex_config: mode,
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
        };
        let probes = Probes::new(
            Arc::new(JobAgents(Arc::clone(&handle.0))),
            MAX_LOOKS,
            KEEP_LOOKS,
        );
        let (never, codex) = (|| None, ToolKind::Codex);
        codex_config(ConfigMode::System);
        assert_eq!(
            probes.ask(&id, &folder, codex, &never).await,
            Ok(Availability::Usable)
        );
        codex_config(ConfigMode::Isolated);
        assert_eq!(
            probes.ask(&id, &folder, codex, &never).await,
            Ok(Availability::Unusable(
                "not signed in for this project".into()
            ))
        );
        // Back to the system profile: its own kept look answers.
        codex_config(ConfigMode::System);
        assert_eq!(
            probes.ask(&id, &folder, codex, &never).await,
            Ok(Availability::Usable)
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Sixth review: a look runs in the profile it is given (the one it is
    /// kept under), not the one the settings say by the time it runs.
    #[cfg(unix)]
    #[test]
    fn a_look_runs_in_the_profile_it_is_given() {
        use crate::agentic_works::agents::AgentCheck;
        use connecto_core::{ConfigMode, ProjectSettings};
        use connecto_session::launch::project_id;
        let (root, _fake, handle) = setup("job-given-profile", false);
        fake_codex(&root, &handle);
        fs::write(root.join("system-in"), "").unwrap();
        let data = root.join("data");
        let folder = root.join("home").join("project");
        fs::create_dir_all(&folder).unwrap();
        let folder = fs::canonicalize(folder).unwrap();
        let id = project_id(&folder);
        let codex_config = |mode| {
            connecto_session::project_settings::save(
                &data,
                &id,
                &ProjectSettings {
                    codex_config: mode,
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
        };
        let jobs = JobAgents(Arc::clone(&handle.0));
        codex_config(ConfigMode::System);
        let system = jobs.profile(&id, ToolKind::Codex);
        assert_eq!(system.system_config, Some(true));
        // Switched before the look runs: it still runs in the system's.
        codex_config(ConfigMode::Isolated);
        assert_eq!(
            jobs.look(&id, &folder, ToolKind::Codex, &system),
            Availability::Usable
        );
        let isolated = jobs.profile(&id, ToolKind::Codex);
        assert_eq!(
            jobs.look(&id, &folder, ToolKind::Codex, &isolated),
            Availability::Unusable("not signed in for this project".into())
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Second review, finding 2: Codex's sign-in is asked in the profile the
    /// project's terminals use, as a launch there asks it (its own when
    /// Isolated), not the system profile the Settings row shows. Both ways:
    /// signed in only on the system, and signed in only in the project.
    #[cfg(unix)]
    #[test]
    fn a_jobs_codex_is_signed_in_as_its_projects_terminals_would_be() {
        use connecto_core::{ConfigMode, ProjectSettings};
        use connecto_session::launch::project_id;
        let (root, _fake, handle) = setup("job-signin", false);
        let data = root.join("data");
        fake_codex(&root, &handle);
        let projects = |name: &str, mode| {
            let folder = root.join("home").join(name);
            fs::create_dir_all(&folder).unwrap();
            let folder = fs::canonicalize(folder).unwrap();
            let id = project_id(&folder);
            connecto_session::project_settings::save(
                &data,
                &id,
                &ProjectSettings {
                    codex_config: mode,
                    ..ProjectSettings::default()
                },
            )
            .unwrap();
            (id, folder)
        };
        let (iso, iso_folder) = projects("isolated", ConfigMode::Isolated);
        let (sys, sys_folder) = projects("system", ConfigMode::System);
        let jobs = JobAgents(Arc::clone(&handle.0));
        let codex = |id: &str, folder: &Path| both(&jobs, id, folder, ToolKind::Codex);
        let out = Availability::Unusable("not signed in for this project".into());
        // Signed in on the system only: the Isolated project's Codex is not.
        fs::write(root.join("system-in"), "").unwrap();
        assert_eq!(codex(&sys, &sys_folder), Availability::Usable);
        assert_eq!(codex(&iso, &iso_folder), out);
        // Signed in in the project only: the system row would say no.
        fs::remove_file(root.join("system-in")).unwrap();
        fs::write(root.join("isolated-in"), "").unwrap();
        assert_eq!(codex(&iso, &iso_folder), Availability::Usable);
        assert_eq!(codex(&sys, &sys_folder), out);
        let _ = fs::remove_dir_all(&root);
    }

    /// Pi with an old Node.js: asked first; declined, nothing is installed and
    /// the row says why; agreed, Node.js and then Pi are installed.
    #[test]
    fn pi_asks_before_installing_nodejs_and_installs_nothing_when_declined() {
        let (_root, fake, handle) = setup("pi-node", false);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        *fake.node.lock().unwrap() = crate::node_setup::NodeCheck::TooOld("v20.17.0".into());

        handle.activate(ToolKind::Pi);
        let view = wait(&handle, |v| row(v, ToolKind::Pi).node_prompt.is_some());
        let pi = row(&view, ToolKind::Pi);
        assert!(pi.busy.is_none() && !pi.installed);
        assert!(pi.node_prompt.unwrap().contains("v20.17.0"));
        assert!(
            fake.calls.lock().unwrap().is_empty(),
            "nothing installed yet"
        );

        handle.decline_node(ToolKind::Pi);
        let pi = row(&handle.view(), ToolKind::Pi);
        assert!(pi.node_prompt.is_none());
        assert!(pi.error.unwrap().contains("Pi was not installed"));
        assert!(fake.calls.lock().unwrap().is_empty());
        assert_eq!(*fake.node_installs.lock().unwrap(), 0);

        // Asked again; agreed: Node.js first, then Pi's installer.
        handle.activate(ToolKind::Pi);
        wait(&handle, |v| row(v, ToolKind::Pi).node_prompt.is_some());
        handle.install_node(ToolKind::Pi);
        wait(&handle, |v| {
            let pi = row(v, ToolKind::Pi);
            pi.busy.is_none() && pi.node_prompt.is_none()
        });
        assert_eq!(*fake.node_installs.lock().unwrap(), 1);
        assert_eq!(*fake.calls.lock().unwrap(), [Tool::Pi]);
    }

    #[test]
    fn every_supported_agent_is_listed_with_its_state_even_when_none_is_installed() {
        let (_root, fake, handle) = setup("list", false);
        let view = wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        assert_eq!(view.rows.len(), 5);
        // So is an Antigravity nobody installed, from Google's installer.
        let agy = row(&view, ToolKind::Antigravity);
        assert_eq!(agy.name, "Antigravity CLI");
        assert!(!agy.installed && !agy.active && agy.source.contains("antigravity.google"));
        // And an OpenCode, from opencode.ai's installer where there is one.
        let opencode = row(&view, ToolKind::OpenCode);
        assert_eq!(opencode.name, "OpenCode");
        assert!(!opencode.installed && !opencode.active);
        #[cfg(unix)]
        assert!(
            opencode.source.contains("opencode.ai"),
            "{}",
            opencode.source
        );
        // A Pi nobody installed is listed, and not offered for new terminals.
        let pi = row(&view, ToolKind::Pi);
        // On every platform (on Windows it was once "Shell", from nowhere).
        assert_eq!(pi.name, "Pi");
        assert!(!pi.installed && !pi.active && pi.source.contains("pi.dev"));
        let claude = row(&view, ToolKind::Claude);
        assert_eq!(claude.name, "Claude Code");
        assert!(!claude.installed && claude.version.is_none() && claude.active);
        assert!(claude.source.contains("claude.ai"));
        assert!(
            fake.calls.lock().unwrap().is_empty(),
            "looking installs nothing"
        );
    }

    // Installs a shell-script agent: Unix only.
    #[cfg(unix)]
    #[test]
    fn activating_a_missing_agent_installs_it_shows_its_version_and_asks_for_sign_in_once() {
        let (root, fake, handle) = setup("activate", false);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        handle.activate(ToolKind::Claude);
        let view = wait(&handle, |v| {
            let r = row(v, ToolKind::Claude);
            r.installed && r.busy.is_none()
        });
        let claude = row(&view, ToolKind::Claude);
        assert_eq!(claude.version.as_deref(), Some("4.5.6 (Fake)"));
        assert!(claude.active && claude.error.is_none());
        assert!(claude.signin_requested, "the person goes on to sign in");
        assert_eq!(*fake.calls.lock().unwrap(), [Tool::Claude]);
        assert!(root.join("home/.local/bin/claude").is_file());
        // Acknowledged, it is not asked again.
        handle.signin_started(ToolKind::Claude);
        assert!(!row(&handle.view(), ToolKind::Claude).signin_requested);
        // Activating what is already installed installs nothing.
        handle.activate(ToolKind::Claude);
        wait(&handle, |v| row(v, ToolKind::Claude).busy.is_none());
        assert_eq!(fake.calls.lock().unwrap().len(), 1);
    }

    // Installs a shell-script agent: Unix only.
    #[cfg(unix)]
    #[test]
    fn an_install_that_fails_says_why_in_plain_words_and_can_be_tried_again() {
        let (_root, fake, handle) = setup("fail", true);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        handle.activate(ToolKind::Claude);
        let view = wait(&handle, |v| row(v, ToolKind::Claude).error.is_some());
        let claude = row(&view, ToolKind::Claude);
        assert!(claude.error.unwrap().contains("Is this Mac online?"));
        assert!(!claude.installed && claude.busy.is_none() && !claude.signin_requested);
        fake.offline.store(false, Ordering::SeqCst);
        handle.activate(ToolKind::Claude);
        let view = wait(&handle, |v| row(v, ToolKind::Claude).installed);
        assert!(row(&view, ToolKind::Claude).error.is_none());
    }

    // Installs a shell-script agent: Unix only.
    #[cfg(unix)]
    #[test]
    fn deactivating_hides_the_agent_and_removes_nothing_and_activating_brings_it_back() {
        let (root, _fake, handle) = setup("hide", false);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        handle.activate(ToolKind::Claude);
        wait(&handle, |v| {
            row(v, ToolKind::Claude).installed && row(v, ToolKind::Claude).busy.is_none()
        });
        handle.deactivate(ToolKind::Claude);
        assert!(!row(&handle.view(), ToolKind::Claude).active);
        // With Pi, Antigravity and OpenCode, which are not on this computer.
        assert_eq!(
            handle.inactive(),
            [
                ToolKind::Claude,
                ToolKind::Pi,
                ToolKind::Antigravity,
                ToolKind::OpenCode
            ]
        );
        assert!(
            root.join("home/.local/bin/claude").is_file(),
            "nothing is uninstalled"
        );
        assert_eq!(inactive_agents(&root.join("data")), [Tool::Claude]);
        // The choice outlasts a restart.
        let again = AgentsHost::start_with(
            &root.join("data"),
            &root.join("home"),
            Arc::new(Fake {
                offline: AtomicBool::new(false),
                calls: Mutex::default(),
                node: Mutex::new(crate::node_setup::NodeCheck::Ready),
                node_installs: Mutex::default(),
            }),
            Arc::new(AnySignature),
            test_providers(&root),
            Arc::new(|| {}),
        );
        assert!(
            !Handle(again)
                .view()
                .rows
                .iter()
                .find(|r| r.tool == ToolKind::Claude)
                .unwrap()
                .active
        );
        handle.activate(ToolKind::Claude);
        wait(&handle, |v| {
            row(v, ToolKind::Claude).active && row(v, ToolKind::Claude).busy.is_none()
        });
        assert_eq!(
            handle.inactive(),
            [ToolKind::Pi, ToolKind::Antigravity, ToolKind::OpenCode]
        );
    }

    /// A stand-in Codex: signed in, version 2.0.0.
    #[cfg(unix)]
    const CODEX_SCRIPT: &str = r#"#!/bin/sh
case "$*" in
  "login status") echo "Logged in using ChatGPT" ;;
  *) echo "codex-cli 2.0.0" ;;
esac
"#;

    #[cfg(unix)]
    fn write_script(path: &Path, text: &str) {
        crate::test_support::link_fake(text, path);
    }

    /// Claude Code as a native program, signed out, reporting version 9.9.9.
    fn write_native_claude(path: &Path) {
        crate::test_support::link_native_source(
            r#"fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.join(" ") == "auth status" {
        println!("{{\"loggedIn\":false,\"authMethod\":\"none\"}}");
    } else {
        println!("9.9.9 (Claude Code)");
    }
}
"#,
            path,
        );
    }

    /// Codex as `npm i -g @openai/codex` lays it out: `~/.npm-global/bin/codex`
    /// is a `#!/usr/bin/env node` launcher, and the native release is in the
    /// platform package beside it. The launcher's interpreter is not on the
    /// test's `PATH`, as for a Finder-started app. Gives the native entry.
    #[cfg(unix)]
    fn npm_codex(home: &Path) -> PathBuf {
        let (triple, platform) = if cfg!(target_arch = "x86_64") {
            ("x86_64-apple-darwin", "codex-darwin-x64")
        } else {
            ("aarch64-apple-darwin", "codex-darwin-arm64")
        };
        let package = home.join(".npm-global/lib/node_modules/@openai/codex");
        let launcher = package.join("bin/codex.js");
        write_script(&launcher, "#!/usr/bin/env connecto-no-such-node\n");
        fs::write(package.join("package.json"), r#"{"name":"@openai/codex"}"#).unwrap();
        let release = package
            .join("node_modules/@openai")
            .join(platform)
            .join("vendor")
            .join(triple);
        for tree in ["codex-path", "codex-resources"] {
            fs::create_dir_all(release.join(tree)).unwrap();
            fs::write(release.join(tree).join("data"), "x").unwrap();
        }
        fs::write(release.join("codex-package.json"), "{}").unwrap();
        let entry = release.join("bin/codex");
        write_script(&entry, CODEX_SCRIPT);
        let link = home.join(".npm-global/bin/codex");
        fs::create_dir_all(link.parent().unwrap()).unwrap();
        std::os::unix::fs::symlink(&launcher, link).unwrap();
        entry
    }

    fn fresh_root(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!(
            "connecto-agents-check-{name}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos())
        ));
        fs::create_dir_all(root.join("home")).unwrap();
        fs::create_dir_all(root.join("data")).unwrap();
        fs::canonicalize(root).unwrap()
    }

    /// Starts the host on `root` and waits until every row has been checked.
    fn checked_host(root: &Path) -> Arc<AgentsView> {
        let fake = Arc::new(Fake {
            offline: AtomicBool::new(true),
            calls: Mutex::default(),
            node: Mutex::new(crate::node_setup::NodeCheck::Ready),
            node_installs: Mutex::default(),
        });
        let host = AgentsHost::start_with(
            &root.join("data"),
            &root.join("home"),
            fake as Arc<dyn AgentInstaller>,
            Arc::new(AnySignature),
            test_providers(root),
            Arc::new(|| {}),
        );
        wait(&Handle(host), |v| v.rows.iter().all(|r| r.busy.is_none()))
    }

    // A shell-script Codex in a macOS npm layout: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_verified_stored_copy_is_checked_and_the_discovered_launcher_is_not() {
        let root = fresh_root("stored");
        let native = npm_codex(&root.join("home"));
        // What setup keeps: a verified copy of the same release.
        Store::new(&root.join("data"))
            .provision(Tool::Codex, &native, &AnySignature)
            .unwrap();
        let view = checked_host(&root);
        let codex = row(&view, ToolKind::Codex);
        assert!(codex.installed);
        assert_eq!(codex.version.as_deref(), Some("codex-cli 2.0.0"));
        assert_eq!(codex.signed_in, Some(true));
    }

    #[test]
    fn without_a_stored_copy_the_users_own_install_is_checked() {
        let root = fresh_root("own-install");
        write_native_claude(&root.join(format!(
            "home/.local/bin/claude{}",
            std::env::consts::EXE_SUFFIX
        )));
        let view = checked_host(&root);
        let claude = row(&view, ToolKind::Claude);
        assert!(claude.installed);
        assert_eq!(claude.version.as_deref(), Some("9.9.9 (Claude Code)"));
        assert_eq!(claude.signed_in, Some(false));
    }

    // Shell scripts and Unix modes: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_tampered_stored_copy_is_not_run_is_marked_for_repair_and_is_said() {
        let root = fresh_root("tampered");
        write_script(
            &root.join("release/claude"),
            r#"#!/bin/sh
case "$*" in
  "auth status") echo '{"loggedIn":true,"authMethod":"claude.ai"}' ;;
  *) echo "2.0.0 (Claude Code)" ;;
esac
"#,
        );
        let store = Store::new(&root.join("data"));
        store
            .provision(Tool::Claude, &root.join("release/claude"), &AnySignature)
            .unwrap();
        let version = store.newest(Tool::Claude).unwrap();
        let entry = store.verify(Tool::Claude, &version).unwrap().entry;
        // Changed after it was checked in.
        fs::set_permissions(&entry, fs::Permissions::from_mode(0o755)).unwrap();
        fs::write(&entry, "#!/bin/sh\necho 'changed'\n").unwrap();
        write_native_claude(&root.join("home/.local/bin/claude"));
        let view = checked_host(&root);
        let claude = row(&view, ToolKind::Claude);
        assert_eq!(
            claude.version.as_deref(),
            Some("9.9.9 (Claude Code)"),
            "the changed copy is not run; the user's own install is"
        );
        assert!(
            claude
                .error
                .as_deref()
                .is_some_and(|e| e.contains("did not pass its check")),
            "said to the person: {:?}",
            claude.error
        );
        assert!(
            Store::new(&root.join("data")).take_repair(),
            "marked for repair at the next start"
        );
    }

    // A macOS npm layout with a `#!` launcher: Unix only.
    #[cfg(unix)]
    #[test]
    fn an_npm_launcher_that_cannot_run_is_installed_but_claims_no_sign_in_state() {
        let root = fresh_root("launcher-only");
        npm_codex(&root.join("home"));
        let view = checked_host(&root);
        let codex = row(&view, ToolKind::Codex);
        assert!(codex.installed, "the launcher is the user's install");
        assert_eq!(codex.version, None);
        assert_eq!(codex.signed_in, None, "unknown, not signed out");
    }

    // Windows (M3): activating a missing agent runs its installer, as on
    // macOS; a failure is said in plain words.
    #[cfg(windows)]
    #[test]
    fn on_windows_activating_a_missing_agent_runs_its_installer() {
        let (_root, fake, handle) = setup("windows", true);
        wait(&handle, |v| v.rows.iter().all(|r| r.busy.is_none()));
        handle.activate(ToolKind::Codex);
        let view = wait(&handle, |v| row(v, ToolKind::Codex).error.is_some());
        let codex = row(&view, ToolKind::Codex);
        assert!(codex.error.unwrap().contains("online?"));
        assert!(codex.busy.is_none() && !codex.signin_requested);
        assert_eq!(*fake.calls.lock().unwrap(), [Tool::Codex]);
        assert!(codex.source.contains("chatgpt.com"), "{}", codex.source);
    }
}
