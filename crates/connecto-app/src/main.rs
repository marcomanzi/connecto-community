//! The connecto desktop terminal.
//!
//! Usage:
//!
//! - `connecto-app provision` copies the installed CLIs into the private store.
//! - `connecto-app audit verify <file>` checks an audit log's hash chain.
//! - `connecto-app` restores the workspace: every project that was open comes
//!   back with its terminals, and each terminal resumes its conversation. The
//!   window opens at once; preflight shows in each terminal.
//! - `connecto-app --project <dir>` also opens `<dir>` as a project, or shows
//!   it if it is already open.
//!
//! The `frame-capture` feature adds what measuring needs, and is never in the
//! default build. See the README.

// A release build is a GUI program on Windows: no console window opens beside
// it. A fatal error then reaches the user in a message box (`platform::fatal`).
#![cfg_attr(all(windows, not(debug_assertions)), windows_subsystem = "windows")]

use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::sync::Arc;
use std::time::{Duration, Instant};

mod agent_install;
mod agentic_works;
mod agents_host;
mod auto_update;
mod automation_host;
mod functional_memory;
mod memory_git;
mod memory_host;
mod memory_private;
mod model_providers;
mod node_setup;
mod ops_host;
mod packages_host;
mod pi_providers;
mod platform;
mod push_gateway;
mod roster_host;
mod setup;
#[cfg(test)]
mod test_support;
mod token_host;
mod turn_judge;

use connecto_audit::{Audit, verify};
use connecto_automation::Automations;
use connecto_core::{TerminalSession, WorkspaceControl};
use connecto_packages::Packages;
use connecto_session::launch::{LaunchExtras, Layout};
use connecto_session::{LiveSession, SIGKILL_GRACE, Wake, shell_spec, wait_for_sweeps};
use connecto_store::{Codesign, Store};
use connecto_ui::{Diagnostics, PaneConfig, WakeSlot, run_with_memory};
#[cfg(feature = "frame-capture")]
use connecto_ui::{MetricsSink, ScreenshotSink};
use connecto_workspace::{Workspace, WorkspaceConfig};

const PANE_COUNT: usize = 2;
const PANE_LABELS: [&str; PANE_COUNT] = ["Terminal 1", "Terminal 2"];

/// Extra time for the kernel to reap children after SIGKILL.
const REAP_MARGIN: Duration = Duration::from_secs(1);
const AUDIT_FLUSH: Duration = Duration::from_secs(2);
/// How long a signal waits for the window to close before shutting down anyway.
const WINDOW_CLOSE_WAIT: Duration = Duration::from_secs(3);
/// How long a project's registry has to save the turns an update cuts.
const UPDATE_MARK_WAIT: Duration = Duration::from_secs(2);

fn layout() -> Result<Layout, String> {
    let home = connecto_store::home_dir().ok_or(if cfg!(windows) {
        "USERPROFILE is not set"
    } else {
        "HOME is not set"
    })?;
    Ok(Layout {
        data_root: connecto_store::data_root(connecto_store::data_dir_override(), &home),
        home,
    })
}

/// Copy the installed CLIs into the store. The app does this by itself at its
/// first launch (see `setup`); this command is for scripts. It runs the same code.
fn provision() -> ExitCode {
    let layout = match layout() {
        Ok(layout) => layout,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::FAILURE;
        }
    };
    let store = Store::new(&layout.data_root);
    let mut failed = false;
    let helper = std::env::current_exe().map(|exe| exe.with_file_name(hook_file_name()));
    match helper
        .map_err(|err| err.to_string())
        .and_then(|path| store.install_hook(&path).map_err(|err| err.to_string()))
    {
        Ok(path) => println!("connecto-hook -> {}", path.display()),
        Err(err) => {
            eprintln!("connecto-hook: {err}");
            failed = true;
        }
    }
    let done = setup::provision_all(&layout.data_root, &layout.home, &Codesign, &mut |line| {
        println!("{line}");
    });
    if failed || done.is_err() {
        ExitCode::FAILURE
    } else {
        ExitCode::SUCCESS
    }
}

#[cfg(not(feature = "frame-capture"))]
fn parse_panes(mut args: impl Iterator<Item = String>) -> Result<Vec<String>, String> {
    args.next().map_or(Ok(Vec::new()), |arg| {
        Err(format!("unknown argument: {arg}"))
    })
}

#[cfg(feature = "frame-capture")]
fn parse_panes(args: impl Iterator<Item = String>) -> Result<Vec<String>, String> {
    let mut commands = Vec::new();
    let mut args = args;
    while let Some(arg) = args.next() {
        if arg != "--pane" {
            return Err(format!("unknown argument: {arg}"));
        }
        let command = args.next().ok_or("--pane needs a shell command")?;
        commands.push(command);
    }
    if commands.len() > PANE_COUNT {
        return Err(format!("at most {PANE_COUNT} --pane arguments are allowed"));
    }
    Ok(commands)
}

/// The hook helper's file name beside the app (`connecto-hook.exe` on Windows).
fn hook_file_name() -> String {
    format!("connecto-hook{}", std::env::consts::EXE_SUFFIX)
}

fn working_directory() -> PathBuf {
    std::env::current_dir()
        .ok()
        .or_else(connecto_store::home_dir)
        .unwrap_or_else(|| PathBuf::from("/"))
}

#[cfg(feature = "frame-capture")]
fn metrics_sink() -> Option<MetricsSink> {
    let path = std::env::var_os("CONNECTO_METRICS")?;
    match MetricsSink::to_file(path.as_ref()) {
        Ok(sink) => Some(sink),
        Err(err) => {
            eprintln!("metrics disabled: {err}");
            None
        }
    }
}

#[cfg(feature = "frame-capture")]
fn screenshot_sink() -> Option<ScreenshotSink> {
    let dir = std::env::var_os("CONNECTO_SHOT_DIR")?;
    match ScreenshotSink::to_dir(PathBuf::from(dir)) {
        Ok(sink) => Some(sink),
        Err(err) => {
            eprintln!("screenshots disabled: {err}");
            None
        }
    }
}

/// Everything that must end before the app exits.
#[derive(Default)]
struct Running {
    shells: Vec<Arc<LiveSession>>,
    /// Every open project, each with its relay and terminals.
    workspace: Option<Arc<Workspace>>,
    /// The automations engine. The window gets it as an
    /// `Arc<dyn connecto_core::AutomationsControl>` (see [`automations_handle`]).
    works: Option<Arc<agentic_works::Works>>,
    automations: Option<Arc<Automations>>,
    /// The packages manager. The window gets it as an
    /// `Arc<dyn connecto_core::PackagesControl>` (see [`packages_handle`]).
    packages: Option<Arc<Packages>>,
    /// Remote hosts and hosting (ADR 0017). Off until the user turns it on.
    remote: Option<Arc<connecto_net::manager::RemoteManager>>,
    /// Settings > Agents: what is installed and signed in, Activate and Deactivate.
    agents: Option<Arc<agents_host::AgentsHost>>,
    /// Connecto updating itself.
    updates: Option<Arc<connecto_update::Updater>>,
    /// Project memory. The window gets it as an
    /// `Arc<dyn connecto_core::MemoryControl>` (see [`memory_handle`]).
    memory: Option<connecto_memory::Engine>,
    /// Where images pasted on a terminal are kept: beside the project's chat
    /// uploads, so they share its limits and cleanup, and Antigravity's read
    /// access.
    pasted: Option<Arc<dyn connecto_core::PastedFiles>>,
    /// The person's secrets, for terminals to use without reading them.
    vault: Option<Arc<connecto_vault::Vault>>,
    /// Project operations (ADR 0031).
    ops: Option<Arc<ops_host::OpsHost>>,
    /// Agent Token Optimization, behind Project Settings > Tokens.
    tokens: Option<token_host::TokenHandle>,
    /// Functional memory (ADR 0033): `.functionalMemory/` kept up to date.
    functional_memory: Option<Arc<functional_memory::FmHost>>,
}

/// The window's and peers' handle on functional memory.
fn functional_memory_handle(
    running: &Running,
) -> Arc<dyn connecto_core::functional_memory::FunctionalMemoryControl> {
    running.functional_memory.clone().map_or_else(
        || {
            Arc::new(connecto_core::functional_memory::NoFunctionalMemory)
                as Arc<dyn connecto_core::functional_memory::FunctionalMemoryControl>
        },
        |host| host as Arc<dyn connecto_core::functional_memory::FunctionalMemoryControl>,
    )
}

/// Saves pasted images in the project's attachments folder.
/// The window's handle on project operations: `NoOps` when they did not start.
fn ops_handle(running: &Running) -> Arc<dyn connecto_core::ops::OpsControl> {
    running.ops.clone().map_or_else(
        || Arc::new(connecto_core::ops::NoOps) as Arc<dyn connecto_core::ops::OpsControl>,
        |host| Arc::new(ops_host::OpsUi(host)) as Arc<dyn connecto_core::ops::OpsControl>,
    )
}

struct PastedStore(PathBuf);

impl connecto_core::PastedFiles for PastedStore {
    fn save(&self, project: &str, name: &str, bytes: &[u8]) -> Result<PathBuf, String> {
        connecto_net::store_pasted(&self.0, project, name, bytes).map(PathBuf::from)
    }
}

/// One path for window close and signals: Pause, Stop all, drain, flush audit,
/// over every open project.
fn shut_down(running: &Running) {
    // Once: the window, a stop signal and an automatic update restart can all
    // ask. A second caller waits here until the first has finished, so none
    // exits while the first is still cleaning up.
    static DONE: std::sync::Mutex<bool> = std::sync::Mutex::new(false);
    let mut done = DONE
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if *done {
        return;
    }
    *done = true;
    // No automatic hand-over starts once the quit has begun, so whether it is
    // for an update is settled here.
    if let Some(updates) = &running.updates {
        updates.stop_auto();
    }
    // Before anything is stopped, so the exits that follow are not saved as
    // "stopped" in any project's terminal list.
    // First, so no key is typed while the terminals stop.
    if let Some(remote) = &running.remote {
        remote.shutdown();
    }
    if let Some(works) = &running.works {
        works.stop();
    }
    if let Some(automations) = &running.automations {
        automations.stop();
    }
    if let Some(packages) = &running.packages {
        packages.stop();
    }
    // Memory runs are cancelled and its threads end before the terminals
    // stop; what is on disk stays consistent (every write is a transaction).
    if let Some(memory) = &running.memory {
        memory.shutdown();
    }
    // A quit for an update: the turns it cuts are saved as such, as late as
    // possible while the registries still follow the panes, so the new
    // version carries on exactly those (ADR 0030).
    if let (Some(updates), Some(workspace)) = (&running.updates, &running.workspace)
        && updates.handed_over()
        && !workspace.mark_cut_by_update(UPDATE_MARK_WAIT)
    {
        eprintln!("the terminals' state at the update could not all be saved");
    }
    let panes = running.workspace.as_ref().map(|workspace| {
        workspace.begin_shutdown();
        workspace.stop_everything();
        workspace.panes()
    });
    let panes = panes.unwrap_or_default();
    for pane in &panes {
        pane.close();
    }
    for session in &running.shells {
        session.terminate();
    }
    let deadline = Instant::now() + SIGKILL_GRACE + REAP_MARGIN;
    let remaining = || deadline.saturating_duration_since(Instant::now());
    let unfinished = panes
        .iter()
        .map(|pane| pane.wait_exit(remaining()))
        .chain(running.shells.iter().map(|s| s.wait_exit(remaining())))
        .filter(|ended| !ended)
        .count();
    if unfinished > 0 {
        eprintln!("a session did not end before the shutdown deadline");
    }
    // A descendant that ignored SIGTERM dies at the grace deadline, not before.
    if !wait_for_sweeps(remaining()) {
        eprintln!("a descendant sweep did not finish before the shutdown deadline");
    }
    if let Some(workspace) = &running.workspace
        && !workspace.shut_down(AUDIT_FLUSH)
    {
        eprintln!("the workspace did not shut down cleanly: the audit log may be incomplete");
    }
    for pane in &panes {
        pane.release();
    }
}

fn audit_path(layout: &Layout) -> PathBuf {
    let stamp = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |elapsed| elapsed.as_millis());
    layout
        .data_root
        .join("audit")
        .join(connecto_audit::log_name(stamp, std::process::id()))
}

/// Start this launch's audit log, then delete the oldest ones: every launch
/// adds a file and nothing else ever removes one.
fn open_audit(path: &Path) -> std::io::Result<Audit> {
    let audit = Audit::open(path)?;
    if let Some(dir) = path.parent()
        && let Err(err) = connecto_audit::prune_logs(dir, path, connecto_audit::KEPT_LOGS)
    {
        eprintln!("old audit logs could not be removed: {err}");
    }
    Ok(audit)
}

/// Start the workspace: it restores the open projects and their terminals on
/// its own thread, so this returns at once. `project` is opened on top.
fn managed_workspace(
    project: Option<&str>,
    wake: &Wake,
    extras: &LaunchExtras,
    after_update: bool,
    running: &mut Running,
) -> Result<(), String> {
    let layout = layout()?;
    // A newer build brings its helper without touching the stored tools, so the
    // bridge stays on the verified versions (R20).
    if let Ok(exe) = std::env::current_exe() {
        let helper = exe.with_file_name(hook_file_name());
        if let Err(err) = Store::new(&layout.data_root).refresh_hook(&helper) {
            eprintln!("the hook helper could not be refreshed: {err}");
        }
    }
    // Before any project is read: System is the default for new projects, and the
    // ones that already exist keep the isolated profile they have.
    connecto_session::project_settings::pin_existing_projects(&layout.data_root);
    // Profiles that were copied from Magneto earlier: each Claude profile keeps its sign-in
    // (the Keychain entry is named after the folder's path), and a profile that a
    // login left empty gets its conversations back, so saved sessions resume.
    connecto_session::launch::repair_secure_storage(&layout.data_root);
    connecto_session::launch::repair_empty_generations(&layout.data_root);
    let audit = open_audit(&audit_path(&layout)).map_err(|err| err.to_string())?;
    let remote_audit = audit.clone();
    let works_audit = audit.clone();
    let tokens_audit = audit.clone();
    let tokens_layout = layout.clone();
    let data_root = layout.data_root.clone();
    // Before the workspace starts any terminal: each terminal's output is
    // numbered from its first byte, so a remote peer can resume (ADR 0017).
    let rings = Arc::new(connecto_net::hosting::RingBook::default());
    connecto_session::launch::set_tap_factory(
        Arc::clone(&rings) as Arc<dyn connecto_core::TapFactory>
    );
    let layout_data_root = layout.data_root.clone();
    let layout_home = layout.home.clone();
    // Before the workspace sets any project up: a terminal waits for its
    // project's packages. The packages manager opens the gate again.
    connecto_session::install_gate::enable(&layout_data_root);
    // Before the workspace starts any terminal: their capture and their
    // `connecto memory` commands go to the engine. A failure here costs
    // memory, not the app.
    let memory = match memory_host::start(&layout, wake) {
        Ok(memory) => Some(memory),
        Err(err) => {
            eprintln!("project memory did not start: {err}");
            None
        }
    };
    // The Jev key: automations and the operations' prompt hook both read it.
    // A build run with a data folder of its own keeps the key in a file of its
    // own there (as builds before the secrets file did).
    let keys: Arc<dyn connecto_automation::KeyStore> =
        if connecto_store::data_dir_override().is_some() {
            Arc::new(connecto_automation::FileKeys(
                layout.data_root.join("automations").join("jev-api-key"),
            ))
        } else {
            Arc::new(connecto_automation::StoredKeys)
        };
    // Before the workspace starts any terminal: their `connecto op` commands
    // and prompt hooks go to it.
    let ops = ops_host::start(&layout, wake, Arc::clone(&keys));
    // Before the workspace starts any terminal: a Claude terminal set up for
    // Together AI, or a Pi terminal of a project with providers of its own,
    // gets them from here. Nothing is read from the secret store before it is
    // set up below; a launch waits for that.
    let providers = model_providers::ModelProvidersHost::new(
        &layout.data_root,
        &layout.home,
        Arc::new(pi_providers::StoredKeys),
        Arc::new(pi_providers::SystemProbe),
        Arc::new(model_providers::OfficialTogetherLink {
            work: layout
                .data_root
                .join("run")
                .join(format!("togetherlink-install-{}", std::process::id())),
        }),
        Arc::clone(wake),
    );
    connecto_session::launch::set_model_providers(
        Arc::clone(&providers) as Arc<dyn connecto_session::launch::ModelProviders>
    );
    let workspace = Workspace::start(WorkspaceConfig {
        layout,
        extras: extras.clone(),
        audit,
        wake: Arc::clone(wake),
        // A new project starts with no terminal: its settings open on Tools,
        // where the user picks the agents it uses and starts them.
        default_tools: Vec::new(),
        after_update,
    })
    .map_err(|err| err.to_string())?;
    // Before the first secret is asked for: where secrets are kept this run,
    // the secrets file in the data folder unless the person chose the system
    // store. A first launch after a change moves them across once.
    connecto_secrets::configure_system(
        data_root.clone(),
        connecto_secrets::system_store_available() && workspace.secrets_in_system_store(),
    );
    running.vault = Some(start_vault(&data_root, wake));
    providers.mark_ready();
    if let Some(project) = project
        && let Err(why) = workspace.open_project(Path::new(project))
    {
        eprintln!("{project} was not opened: {why}");
    }
    let workspace = Arc::new(workspace);
    // Before Agentic Works: its first job's agent is chosen from these rows
    // (ADR 0041).
    let agents = agents_host::AgentsHost::start(
        &layout_data_root,
        &layout_home,
        Arc::new(agent_install::Official::new(&layout_data_root)),
        providers,
        Arc::clone(wake),
    );
    running.agents = Some(Arc::clone(&agents));
    // `connecto agents`: which CLIs a project can start now (ADR 0045).
    connecto_session::set_roster_bridge(Arc::new(roster_host::Roster::new(Arc::clone(&agents))));
    if let Some(vault) = &running.vault {
        match agentic_works::Works::start(
            &data_root,
            workspace.clone(),
            vault.clone(),
            works_audit,
            Arc::new(agents_host::JobAgents(agents)),
        ) {
            Ok(works) => {
                // A job's question to the user shows at once.
                works.set_wake(Arc::clone(wake));
                // Before an agent asks: `connecto works` goes to it.
                connecto_session::set_works_bridge(
                    Arc::clone(&works) as Arc<dyn connecto_core::agentic_works::WorksBridge>
                );
                running.works = Some(works);
            }
            Err(e) => eprintln!("Agentic Works could not start: {e}"),
        }
    }
    running.tokens = Some(token_host::start(
        &tokens_layout,
        &workspace,
        tokens_audit,
        wake,
    ));
    running.functional_memory = Some(functional_memory::start(&tokens_layout, &workspace, wake));
    if let Some(memory) = memory {
        memory_host::attach(&memory, &workspace);
        running.memory = Some(memory.engine);
    }
    ops.attach(&workspace);
    {
        let ops = Arc::clone(&ops);
        workspace.on_stop_everything(Arc::new(move || ops.cancel_runs()));
    }
    running.ops = Some(ops);
    // A failure here costs the automations, not the app.
    match Automations::start_with_keys(
        data_root,
        Arc::new(automation_host::WorkspaceHost {
            workspace: Arc::clone(&workspace),
        }),
        Arc::clone(wake),
        Arc::clone(&keys),
    ) {
        Ok(automations) => running.automations = Some(Arc::new(automations)),
        Err(err) => eprintln!("automations did not start: {err}"),
    }
    // Likewise: a failure here costs packages, not the app.
    let data_root = layout_data_root.clone();
    match Packages::start(
        data_root,
        Arc::new(packages_host::PackagesHost {
            workspace: Arc::clone(&workspace),
            automations: running.automations.clone(),
        }),
        Arc::clone(wake),
    ) {
        Ok(packages) => running.packages = Some(Arc::new(packages)),
        Err(err) => {
            connecto_session::install_gate::disable(&layout_data_root);
            eprintln!("packages did not start: {err}");
        }
    }
    // Remote hosting stays off until it is turned on in Settings. A failure
    // here costs remote sessions, not the app.
    let workspace_control = Arc::clone(&workspace) as Arc<dyn WorkspaceControl>;
    let guard = Arc::new(connecto_session::RemoteGate::new(
        workspace.pause_switch(),
        remote_audit,
    ));
    let secrets = remote_secrets(
        connecto_store::data_dir_override().is_some(),
        &layout_data_root,
    );
    // Voice requests from the phone (Siri), taken in charge here: kept in a
    // table so a retry is never typed twice, and notified through the push
    // gateway when one is set up.
    let siri = Arc::new(connecto_net::siri::SiriDesk::new(
        Some(
            layout_data_root
                .join("remote")
                .join(connecto_net::siri::STORE_FILE),
        ),
        Arc::new(connecto_net::hosting::WorkspaceSessions(Arc::clone(
            &workspace_control,
        ))),
        // Signed with this machine's device key, which the gateway enrolls:
        // made now if a gateway is set up and there is none yet.
        Some(layout_data_root.join("remote").join("push-gateway.json"))
            .filter(|config| config.exists())
            .and_then(|config| {
                let key = connecto_net::secrets::device_ssh_key(&*secrets).ok()?;
                push_gateway::HttpGateway::load(
                    &config,
                    Arc::new(connecto_net::auth::KeySigner(key)),
                )
            })
            .map(|g| Arc::new(g) as Arc<dyn connecto_net::siri::PushSender>),
    ));
    siri.start();
    running.remote = Some(connecto_net::manager::RemoteManager::start(
        connecto_net::manager::ManagerConfig {
            dir: layout_data_root.join("remote"),
            secrets: Arc::clone(&secrets),
            guard,
            terminals: Arc::new(
                connecto_net::hosting::WorkspaceTerminals::new(workspace_control, rings)
                    .with_attachment_root(layout_data_root.clone())
                    .with_siri(siri)
                    // A conversation model for peers with none (the phone's
                    // voice calls), when one is set.
                    .with_assistant(layout_data_root.join("remote").join("assistant.json"))
                    // Pi's models in the app's chooser, as the window offers them.
                    .with_agents(agents_handle(running))
                    // A peer's History and Project Settings of these projects,
                    // through the same controls the window uses.
                    .with_packages(packages_handle(running))
                    .with_tokens(tokens_handle(running))
                    // A peer's Operations: viewed, approved and run as the
                    // person here would (ADR 0031).
                    .with_ops(ops_handle(running))
                    // A peer's voice call reads a project's functional
                    // memory, and can ask for an update (ADR 0033).
                    .with_functional_memory(functional_memory_handle(running))
                    // A peer's project secrets and its terminals' requests
                    // for one: that project's own, never a value back.
                    .with_vault(vault_handle(running))
                    // Whether a voice call's turn is over, from Jev with the
                    // automations' key (off without one).
                    .with_turn_judge(Arc::new(turn_judge::JevTurns::new(Arc::clone(&keys)))),
            ),
            // The key made by pairing first, then whatever ssh-agent holds
            // (the manual way).
            signers: Arc::new(connecto_net::auth::CombinedSigners(vec![
                Arc::new(connecto_net::auth::DeviceKeySigners(secrets)),
                Arc::new(connecto_net::auth::AgentSigners),
            ])),
            wake: Arc::clone(wake),
            supervisor: connecto_net::reconnect::SupervisorConfig::DEFAULT,
            bind: None,
            share_loopback: false,
            device_name: device_name(),
            kdf: connecto_net::pairing::KdfParams::PRODUCTION,
            pairing_lifetime: connecto_net::pairing::CODE_LIFETIME,
            default_relay: Some(connecto_net::manager::DEFAULT_RELAY_URL.to_owned()),
        },
    ));
    running.pasted = Some(Arc::new(PastedStore(layout_data_root)));
    running.workspace = Some(workspace);
    Ok(())
}

/// Where the remote device key and relay token are kept. A build run with its
/// own data folder (`CONNECTO_DATA_DIR`, for trying a build beside the installed
/// app) keeps them in files of their own in that folder, as it did before the
/// secrets file. Otherwise: the secret store (`connecto_secrets`).
fn remote_secrets(
    own_data_dir: bool,
    data_root: &Path,
) -> Arc<dyn connecto_net::secrets::SecretStore> {
    if own_data_dir {
        Arc::new(connecto_net::secrets::FileSecrets(
            data_root.join("remote").join("secrets"),
        ))
    } else {
        Arc::new(connecto_net::secrets::StoredSecrets)
    }
}

/// What this machine calls itself to the machine it pairs with.
fn device_name() -> String {
    let name = platform::computer_name().unwrap_or_else(|| platform::UNNAMED_COMPUTER.to_owned());
    name.chars().filter(|c| !c.is_control()).take(64).collect()
}

/// What the window is given for remote hosts and hosting.
/// What the window is given for updates.
fn updates_handle(running: &Running) -> Arc<dyn connecto_core::UpdateControl> {
    running.updates.clone().map_or_else(
        || Arc::new(connecto_core::NoUpdates) as Arc<dyn connecto_core::UpdateControl>,
        |updates| updates as Arc<dyn connecto_core::UpdateControl>,
    )
}

/// The version of this build.
fn this_version() -> Option<connecto_update::Version> {
    env!("CARGO_PKG_VERSION").parse().ok()
}

fn remote_handle(running: &Running) -> Arc<dyn connecto_core::RemoteControl> {
    running.remote.clone().map_or_else(
        || Arc::new(connecto_core::NoRemote) as Arc<dyn connecto_core::RemoteControl>,
        |remote| remote as Arc<dyn connecto_core::RemoteControl>,
    )
}

/// Open the vault and hand it to everything that must hide its values (every
/// redaction in the process) or list its names (the bridge).
fn start_vault(data_root: &Path, wake: &Wake) -> Arc<connecto_vault::Vault> {
    let vault = Arc::new(connecto_vault::Vault::open(
        connecto_secrets::Secrets::system(),
        vault_master_key(data_root),
        Some(data_root),
    ));
    // A terminal's request shows its dialog at once, with no polling.
    vault.set_on_change(Arc::clone(wake));
    let known = Arc::clone(&vault);
    connecto_redact::set_known_source(Arc::new(move || connecto_core::VaultBridge::known(&*known)));
    connecto_session::set_vault_bridge(Arc::clone(&vault) as Arc<dyn connecto_core::VaultBridge>);
    // The web proxy and the ssh-agent sockets. Their folder is short, as a
    // socket path must be, and private to this user.
    let agents = std::env::temp_dir().join(format!("connecto-vault-{}", platform_user_id()));
    if let Err(why) = vault.start_brokers(agents) {
        eprintln!("the secret brokers did not start: {why}");
    }
    vault
}

/// Where the vault's master key is: the system store (the macOS Keychain,
/// Windows Credential Manager), or a key file where there is none, and for a
/// build with a data folder of its own (it would otherwise share, or prompt
/// for, the installed app's Keychain item).
fn vault_master_key(data_root: &Path) -> Arc<dyn connecto_vault::MasterKey> {
    if connecto_store::data_dir_override().is_none()
        && let Some(items) = connecto_secrets::system_items()
    {
        let words = if cfg!(windows) {
            "Values are encrypted; the key is in Windows Credential Manager. That protects them from other users and from copies of the file, not from programs running as you."
        } else {
            "Values are encrypted; the key is in the macOS Keychain, which asks you before any program other than Connecto reads it."
        };
        return Arc::new(connecto_vault::StoreKey::new(
            items,
            connecto_vault::KEY_ACCOUNT,
            words,
        ));
    }
    Arc::new(connecto_vault::FileKey::new(connecto_vault::key_file(
        data_root,
    )))
}

/// This user's id, to name the private folder of sockets.
fn platform_user_id() -> u32 {
    #[cfg(unix)]
    {
        nix::unistd::getuid().as_raw()
    }
    #[cfg(not(unix))]
    {
        0
    }
}

/// What the window is given for Settings > Secrets.
fn vault_handle(running: &Running) -> Arc<dyn connecto_core::VaultControl> {
    running.vault.clone().map_or_else(
        || Arc::new(connecto_core::NoVault) as Arc<dyn connecto_core::VaultControl>,
        |vault| vault as Arc<dyn connecto_core::VaultControl>,
    )
}

/// What the window is given for Settings > Agents.
fn agents_handle(running: &Running) -> Arc<dyn connecto_core::AgentsControl> {
    running.agents.clone().map_or_else(
        || Arc::new(connecto_core::NoAgents) as Arc<dyn connecto_core::AgentsControl>,
        |agents| Arc::new(agents_host::Handle(agents)) as Arc<dyn connecto_core::AgentsControl>,
    )
}

/// What the window is given for the Automations screen.
fn automations_handle(running: &Running) -> Option<Arc<dyn connecto_core::AutomationsControl>> {
    running
        .automations
        .clone()
        .map(|automations| automations as Arc<dyn connecto_core::AutomationsControl>)
}

/// What the window is given for project memory, or `NoMemory` when it is off.
/// What the window is given for Project Settings > Tokens.
fn tokens_handle(running: &Running) -> Arc<dyn connecto_core::TokenControl> {
    running.tokens.clone().map_or_else(
        || Arc::new(connecto_core::NoTokens) as Arc<dyn connecto_core::TokenControl>,
        |tokens| Arc::new(tokens) as Arc<dyn connecto_core::TokenControl>,
    )
}

fn memory_handle(running: &Running) -> Arc<dyn connecto_core::MemoryControl> {
    running.memory.clone().map_or_else(
        || Arc::new(connecto_core::NoMemory) as Arc<dyn connecto_core::MemoryControl>,
        |memory| Arc::new(memory) as Arc<dyn connecto_core::MemoryControl>,
    )
}

/// What the window is given for Skills & Plugins and the project settings.
fn packages_handle(running: &Running) -> Arc<dyn connecto_core::PackagesControl> {
    running.packages.clone().map_or_else(
        || Arc::new(connecto_core::NoPackages) as Arc<dyn connecto_core::PackagesControl>,
        |packages| packages as Arc<dyn connecto_core::PackagesControl>,
    )
}

/// SIGTERM, SIGINT and SIGHUP (on Windows: Ctrl+C and SIGTERM) close the
/// window, which then shuts down as usual. A window that cannot answer does not
/// keep the sessions alive.
fn handle_signals(slot: Arc<WakeSlot>, running: Arc<Running>) -> std::io::Result<()> {
    platform::on_stop_signal(move || {
        slot.request_close();
        std::thread::sleep(WINDOW_CLOSE_WAIT);
        shut_down(&running);
        std::process::exit(0);
    })
}

fn audit_verify(path: &str) -> ExitCode {
    match verify(path.as_ref()) {
        Ok(count) => {
            println!("ok: {count} records");
            ExitCode::SUCCESS
        }
        Err(err) => {
            eprintln!("{err}");
            ExitCode::FAILURE
        }
    }
}

/// What the command line asked for.
struct Options {
    project: Option<String>,
    commands: Vec<String>,
    extras: LaunchExtras,
    /// Given by the setup window's restart of a launch that followed an
    /// update ([`AFTER_UPDATE_ARG`]).
    after_update: bool,
}

/// Passed by the setup window to the app it starts again when the launch it
/// interrupted followed an update: by then the updater may have forgotten the
/// update, and the restore must still bring back what was running (ADR 0030).
const AFTER_UPDATE_ARG: &str = "--after-update";

fn parse_options(mut args: Vec<String>) -> Result<Options, String> {
    // First only, where the setup window puts it: never an option's value.
    let after_update = args.first().is_some_and(|arg| arg == AFTER_UPDATE_ARG);
    if after_update {
        args.remove(0);
    }
    let extras = take_extras(&mut args)?;
    if args.len() == 2 && args[0] == "--project" {
        return Ok(Options {
            project: args.pop(),
            commands: Vec::new(),
            extras,
            after_update,
        });
    }
    Ok(Options {
        project: None,
        commands: parse_panes(args.into_iter())?,
        extras,
        after_update,
    })
}

#[cfg(feature = "test-endpoints")]
fn take_extras(args: &mut Vec<String>) -> Result<LaunchExtras, String> {
    let Some(index) = args.iter().position(|arg| arg == "--fake-model") else {
        return Ok(LaunchExtras::default());
    };
    if index + 1 >= args.len() {
        return Err("--fake-model needs a URL".to_owned());
    }
    let url = args.remove(index + 1);
    args.remove(index);
    if !url.starts_with("http://127.0.0.1:") {
        return Err("--fake-model must be a local address".to_owned());
    }
    Ok(LaunchExtras {
        fake_model: Some(url),
        ..LaunchExtras::default()
    })
}

#[cfg(not(feature = "test-endpoints"))]
#[allow(clippy::unnecessary_wraps)]
fn take_extras(_args: &mut Vec<String>) -> Result<LaunchExtras, String> {
    Ok(LaunchExtras::default())
}

/// Show the first-launch window while the setup works, then start the app
/// again (so it begins on a finished setup, and nothing reads a half-made
/// folder). A window closed halfway ends the app; the next start picks up.
fn run_setup_then_restart(plan: &setup::Plan, args: &[String], after_update: bool) -> ExitCode {
    let runner = setup::Runner::new();
    let worker = {
        let (plan, runner) = (plan.clone(), runner.clone());
        std::thread::spawn(move || {
            setup::run(
                &plan,
                &runner,
                &Codesign,
                &agent_install::Official::new(&plan.data_root),
            )
        })
    };
    if let Err(err) = connecto_ui::run_setup(Arc::new(runner.clone())) {
        platform::fatal(&format!("the setup window failed: {err}"));
    }
    // The worker ends with the window: closed, or finished.
    for _ in 0..40 {
        if worker.is_finished() {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    let outcome = runner.outcome().unwrap_or_default();
    if outcome.quit || runner.outcome().is_none() {
        return ExitCode::SUCCESS;
    }
    let Ok(exe) = std::env::current_exe() else {
        return ExitCode::FAILURE;
    };
    let mut command = std::process::Command::new(exe);
    command.args(restart_args(args, outcome.open.as_deref(), after_update));
    if outcome.skip_setup_next_start {
        command.env("CONNECTO_SKIP_SETUP", "1");
    }
    if let Some(prompt) = outcome.first_prompt {
        command.env("CONNECTO_FIRST_PROMPT", prompt);
    }
    // macOS: the process is replaced and this returns only on failure.
    // Windows: a new process is started and this one ends.
    match platform::start_again(&mut command) {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            platform::fatal(&format!("could not start Connecto again: {err}"));
            ExitCode::FAILURE
        }
    }
}

/// The arguments the setup window starts the app again with: the folder it
/// chose, or the launch's own, after the update marker when the interrupted
/// launch followed an update.
fn restart_args(
    args: &[String],
    open: Option<&Path>,
    after_update: bool,
) -> Vec<std::ffi::OsString> {
    let mut out: Vec<std::ffi::OsString> = Vec::new();
    if after_update {
        out.push(AFTER_UPDATE_ARG.into());
    }
    let own = match args.first() {
        Some(first) if first == AFTER_UPDATE_ARG => &args[1..],
        _ => args,
    };
    match open {
        Some(folder) => out.extend(["--project".into(), folder.as_os_str().to_owned()]),
        None => out.extend(own.iter().map(Into::into)),
    }
    out
}

/// Whether this launch brings back every terminal that was running when the
/// app quit, idle ones included: only the new version an update's hand-over
/// started (ADR 0030). A rollback, a version installed by hand meanwhile, or a
/// plain launch restores as a quit does.
fn restores_running_terminals(launch: connecto_update::state::Launch) -> bool {
    matches!(launch, connecto_update::state::Launch::Updated { .. })
}

fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    // Where the user's shell would find the agents, asked in the background.
    connecto_store::prefetch_login_path();
    match args.iter().map(String::as_str).collect::<Vec<_>>()[..] {
        ["provision"] => return provision(),
        ["audit", "verify", path] => return audit_verify(path),
        _ => {}
    }
    let mut options = match parse_options(args.clone()) {
        Ok(options) => options,
        Err(message) => {
            eprintln!("{message}");
            return ExitCode::from(2);
        }
    };
    // Before anything else can fail: a version just installed by the updater
    // says it started (after a while), or a rollback is noted.
    let (update_notice, after_update) = match (layout(), this_version()) {
        (Ok(layout), Some(version)) => {
            let start = connecto_update::start_of_app(&layout.data_root, version);
            (start.notice, restores_running_terminals(start.launch))
        }
        _ => (None, false),
    };
    let after_update = after_update || options.after_update;

    // First launch, or an agent that needs setting up: the setup window comes
    // first, and the app starts again when it is done.
    if let Some(home) = connecto_store::home_dir()
        && let Some(plan) = setup::plan(&home)
    {
        return run_setup_then_restart(&plan, &args, after_update);
    }
    if options.project.is_some()
        && let Ok(prompt) = std::env::var("CONNECTO_FIRST_PROMPT")
    {
        options.extras.initial_prompt = Some(prompt);
    }

    let cwd = working_directory();
    let slot = Arc::new(WakeSlot::default());
    let wake_slot = Arc::clone(&slot);
    let wake: Wake = Arc::new(move || wake_slot.wake());

    let mut running = Running::default();
    // The window's terminals come from the workspace's view. Only a static
    // shell (the `frame-capture` seam) hands the window panes of its own.
    let mut panes = Vec::new();
    if options.commands.is_empty() {
        if let Err(message) = managed_workspace(
            options.project.as_deref(),
            &wake,
            &options.extras,
            after_update,
            &mut running,
        ) {
            platform::fatal(&message);
            return ExitCode::FAILURE;
        }
        if let (Ok(layout), Some(version), Ok(exe)) =
            (layout(), this_version(), std::env::current_exe())
        {
            let wake = Arc::clone(&wake);
            running.updates = Some(connecto_update::start_for_app(
                layout.data_root,
                version,
                exe,
                Arc::new(move || wake()),
                update_notice,
            ));
        }
    } else {
        for (index, label) in PANE_LABELS.iter().enumerate() {
            let command = options.commands.get(index).map(String::as_str);
            let session = match LiveSession::start(shell_spec(command, &cwd), Arc::clone(&wake)) {
                Ok(session) => {
                    let session = Arc::new(session);
                    running.shells.push(Arc::clone(&session));
                    let session: Arc<dyn TerminalSession> = session;
                    Ok(session)
                }
                Err(err) => Err(err.to_string()),
            };
            panes.push(PaneConfig {
                label: (*label).to_owned(),
                session,
                control: None,
                tool: None,
            });
        }
    }

    let workspace = running
        .workspace
        .clone()
        .map(|workspace| workspace as Arc<dyn WorkspaceControl>);
    let running = Arc::new(running);
    if let Err(err) = handle_signals(Arc::clone(&slot), Arc::clone(&running)) {
        eprintln!("signals are not handled: {err}");
    }
    if let Ok(layout) = layout() {
        auto_update::start(&running, &slot, layout.data_root);
    }
    #[cfg(feature = "frame-capture")]
    let diagnostics = Diagnostics {
        metrics: metrics_sink(),
        shots: screenshot_sink(),
        workspace,
        works: running
            .works
            .clone()
            .map(|w| w as Arc<dyn connecto_core::agentic_works::WorksControl>),
        ops: ops_handle(&running),
        automations: automations_handle(&running),
        packages: packages_handle(&running),
        remote: remote_handle(&running),
        agents: agents_handle(&running),
        vault: vault_handle(&running),
        updates: updates_handle(&running),
        tokens: tokens_handle(&running),
        functional_memory: functional_memory_handle(&running),
        pasted: running.pasted.clone(),
    };
    #[cfg(not(feature = "frame-capture"))]
    let diagnostics = Diagnostics {
        metrics: None,
        workspace,
        works: running
            .works
            .clone()
            .map(|w| w as Arc<dyn connecto_core::agentic_works::WorksControl>),
        ops: ops_handle(&running),
        automations: automations_handle(&running),
        packages: packages_handle(&running),
        remote: remote_handle(&running),
        agents: agents_handle(&running),
        vault: vault_handle(&running),
        updates: updates_handle(&running),
        tokens: tokens_handle(&running),
        functional_memory: functional_memory_handle(&running),
        pasted: running.pasted.clone(),
    };
    let memory = memory_handle(&running);
    let result = run_with_memory(panes, slot, diagnostics, memory);
    shut_down(&running);
    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(err) => {
            platform::fatal(&format!("the window failed: {err}"));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_build_with_its_own_data_folder_keeps_its_remote_secrets_there_not_in_the_keychain() {
        use connecto_net::secrets::DEVICE_KEY;
        let dir = std::env::temp_dir().join(format!("mg-app-secrets-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let store = remote_secrets(true, &dir);
        store.set(DEVICE_KEY, &[7; 32]).unwrap();
        assert_eq!(store.get(DEVICE_KEY).unwrap().unwrap(), vec![7; 32]);
        assert!(dir.join("remote/secrets").join(DEVICE_KEY).is_file());
        std::fs::remove_dir_all(&dir).unwrap();
    }

    #[test]
    fn a_launch_starts_its_audit_log_and_keeps_only_the_newest_ones() {
        let dir = std::env::temp_dir().join(format!("mg-app-audit-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        let layout = Layout {
            data_root: dir.clone(),
            home: dir.join("home"),
        };
        let logs = dir.join("audit");
        std::fs::create_dir_all(&logs).unwrap();
        let extra = 10;
        for stamp in 1..=(connecto_audit::KEPT_LOGS + extra) {
            let name = connecto_audit::log_name(stamp as u128, 1);
            std::fs::write(logs.join(name), "old").unwrap();
        }
        std::fs::write(logs.join("notes.txt"), "mine").unwrap();
        let path = audit_path(&layout);
        assert_eq!(path.parent(), Some(logs.as_path()));
        let audit = open_audit(&path).unwrap();
        drop(audit);
        let names: Vec<String> = std::fs::read_dir(&logs)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .collect();
        assert_eq!(names.len(), connecto_audit::KEPT_LOGS + 1, "and the notes");
        assert!(path.is_file(), "this launch's log stays");
        assert!(logs.join("notes.txt").is_file());
        // The oldest went, the newest stayed.
        assert!(!logs.join(connecto_audit::log_name(1, 1)).exists());
        let newest = (connecto_audit::KEPT_LOGS + extra) as u128;
        assert!(logs.join(connecto_audit::log_name(newest, 1)).is_file());
        let _ = std::fs::remove_dir_all(&dir);
    }

    fn parse(args: &[&str]) -> Result<Vec<String>, String> {
        parse_panes(args.iter().map(|arg| (*arg).to_owned()))
    }

    #[cfg(not(feature = "frame-capture"))]
    #[test]
    fn the_default_build_has_no_pane_override() {
        assert_eq!(parse(&[]), Ok(vec![]));
        assert!(parse(&["--pane", "top"]).is_err());
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn no_arguments_means_default_shells() {
        assert_eq!(parse(&[]), Ok(vec![]));
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn panes_are_collected_in_order() {
        assert_eq!(
            parse(&["--pane", "top", "--pane", "yes"]),
            Ok(vec!["top".to_owned(), "yes".to_owned()])
        );
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn a_pane_flag_needs_a_command() {
        assert!(parse(&["--pane"]).is_err());
    }

    #[test]
    fn only_the_version_an_update_started_restores_every_running_terminal() {
        use connecto_update::state::Launch;
        let v = |text: &str| text.parse::<connecto_update::Version>().unwrap();
        assert!(restores_running_terminals(Launch::Updated {
            from: v("0.2.19"),
            to: v("0.3.0"),
        }));
        assert!(!restores_running_terminals(Launch::Normal));
        assert!(!restores_running_terminals(Launch::Unrelated));
        assert!(!restores_running_terminals(Launch::RolledBack {
            failed: v("0.3.0"),
        }));
    }

    /// A launch after an update that the setup window restarts keeps the
    /// update's restore, though the updater has forgotten the update by then.
    #[test]
    fn the_setup_restart_carries_the_update_restore_over() {
        let options = |args: &[&str]| {
            parse_options(args.iter().map(|arg| (*arg).to_owned()).collect()).unwrap()
        };
        // What the setup window passes is what the next launch reads.
        let relaunch = |args: &[&str], open: Option<&str>, after: bool| {
            let args: Vec<String> = args.iter().map(|a| (*a).to_owned()).collect();
            let out = restart_args(&args, open.map(Path::new), after);
            parse_options(
                out.iter()
                    .map(|a| a.to_string_lossy().into_owned())
                    .collect(),
            )
            .unwrap()
        };
        let carried = relaunch(&[], Some("/tmp/p"), true);
        assert!(carried.after_update);
        assert_eq!(carried.project.as_deref(), Some("/tmp/p"));
        let again = relaunch(&[AFTER_UPDATE_ARG, "--project", "/tmp/q"], None, true);
        assert!(again.after_update);
        assert_eq!(again.project.as_deref(), Some("/tmp/q"));
        assert!(!relaunch(&["--project", "/tmp/p"], None, false).after_update);
        // Only first: as a folder's name it is a folder.
        let folder = options(&["--project", AFTER_UPDATE_ARG]);
        assert!(!folder.after_update);
        assert_eq!(folder.project.as_deref(), Some(AFTER_UPDATE_ARG));
    }

    #[test]
    fn unknown_arguments_are_rejected() {
        assert!(parse(&["--nope"]).is_err());
    }

    #[cfg(feature = "frame-capture")]
    #[test]
    fn too_many_panes_are_rejected() {
        assert!(parse(&["--pane", "a", "--pane", "b", "--pane", "c"]).is_err());
    }
}
