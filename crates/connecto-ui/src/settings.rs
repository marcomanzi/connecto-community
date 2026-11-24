//! The Settings sheet (Connecto menu): Connecto Home, the agents, how coding
//! CLIs are launched, and remote access. One section shows at a time, under a
//! row of tabs.

use connecto_core::{
    AgentBusy, AgentMode, AgentsView, HomeMigration, HostLink, HostingStatus, LaunchModes,
    MachineRow, PAIRING_CODE_DIGITS, PackagesView, ProviderScope, RemoteView, ToolKind,
};
use egui::{Id, Key, Rect, RichText, TextEdit, Ui};

use crate::forms::{self, Out, heading, muted, problem, warning};
use crate::providers::{ProvidersAction, ProvidersDialog};
use crate::vault_ui::{ProjectName, VaultAction, VaultForm};
use crate::{platform, theme};

/// The sections of Settings, in the order of the tab row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SettingsTab {
    #[default]
    Home,
    Agents,
    Launch,
    Secrets,
    Remote,
    Cli,
}

impl SettingsTab {
    pub(crate) const ALL: [Self; 6] = [
        Self::Home,
        Self::Agents,
        Self::Launch,
        Self::Secrets,
        Self::Remote,
        Self::Cli,
    ];

    /// The tab's name, which is also the section's wording.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Home => "Connecto Home",
            Self::Agents => "Agents",
            Self::Launch => "How Coding CLIs are launched",
            Self::Secrets => "Secrets",
            Self::Remote => "Remote Access",
            Self::Cli => "CLI improvements",
        }
    }

    /// The key the tab is marked with, for tests.
    fn key(self) -> &'static str {
        match self {
            Self::Home => "tab-home",
            Self::Agents => "tab-agents",
            Self::Launch => "tab-launch",
            Self::Secrets => "tab-secrets",
            Self::Remote => "tab-remote",
            Self::Cli => "tab-cli",
        }
    }
}

#[derive(Default)]
pub(crate) struct SettingsSheet {
    /// Why the folder was refused.
    pub(crate) error: Option<String>,
    /// The relay address being typed. `None` until the sheet first shows the
    /// saved one.
    pub(crate) relay_url: Option<String>,
    /// The relay token being typed. Never filled from the saved one.
    pub(crate) relay_token: String,
    /// Why the relay could not be saved, shown under its buttons.
    pub(crate) relay_error: Option<String>,
    /// Why an agent's sign-in could not be opened.
    pub(crate) agents_error: Option<String>,
    /// The dialog that sets Claude's or Pi's providers up, when open.
    pub(crate) providers: Option<ProvidersDialog>,
    /// Settings > Secrets.
    pub(crate) vault: VaultForm,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SettingsAction {
    Close,
    /// Show another section.
    SelectTab(SettingsTab),
    /// Open the Utility Sessions sheet, where the utility model is set.
    OpenUtilitySessions,
    /// Pick Connecto Home with the system panel, then set it.
    ChooseHome,
    /// How one tool is launched from now on.
    SetLaunchMode(ToolKind, AgentMode),
    SetHosting(bool),
    /// Open the pairing dialog.
    StartPairing,
    /// Ask before removing a machine's pairing (the confirm dialog removes it).
    AskRemoveMachine {
        id: String,
        name: String,
    },
    /// Keep secrets in the system store from the next launch on.
    SetSecretsInSystemStore(bool),
    /// Check for updates at launch and every minute, or not.
    SetAutoUpdate(bool),
    /// Install ready updates by themselves, or wait for the person.
    SetAutoInstall(bool),
    /// Check for updates now.
    CheckForUpdates,
    SetRelay {
        url: Option<String>,
        /// `Some("")` clears the token.
        token: Option<String>,
    },
    /// Install the agent if it is missing, offer it, and go on to sign in.
    ActivateAgent(ToolKind),
    /// Stop offering the agent in new terminals; nothing is uninstalled.
    DeactivateAgent(ToolKind),
    /// The person agreed to install Node.js for the agent, then the agent.
    InstallNode(ToolKind),
    /// The person declined Node.js: the agent is not installed.
    DeclineNode(ToolKind),
    /// Open the agent's sign-in.
    SignInAgent(ToolKind),
    /// Look again at what is installed and signed in.
    RefreshAgents,
    /// A change in the model providers dialog.
    Providers(ProvidersAction),
    /// A change to the person's secrets.
    Vault(VaultAction),
    /// Start a Claude terminal on Opus that runs `/insights` and applies
    /// its suggestions (CLI improvements).
    ApplyClaudeInsights,
}

impl From<ProvidersAction> for SettingsAction {
    fn from(action: ProvidersAction) -> Self {
        Self::Providers(action)
    }
}

impl From<VaultAction> for SettingsAction {
    fn from(action: VaultAction) -> Self {
        Self::Vault(action)
    }
}

/// What the sheet shows, read from the controls once per frame.
#[derive(Clone, Copy)]
pub(crate) struct Inputs<'a> {
    pub(crate) view: &'a PackagesView,
    pub(crate) remote: &'a RemoteView,
    pub(crate) agents: &'a AgentsView,
    /// The section on show.
    pub(crate) tab: SettingsTab,
    /// The saved choice: secrets in the system store (from the next launch).
    pub(crate) secrets_in_system_store: bool,
    /// Connecto updating itself.
    pub(crate) updates: &'a connecto_core::UpdateView,
    /// The person's secrets, without values.
    pub(crate) vault: &'a connecto_core::VaultView,
    /// The open projects, for a project's own secrets.
    pub(crate) projects: &'a [ProjectName],
    /// CLI improvements: the project a terminal they start opens in (the
    /// active one of this machine), and what the last start came to.
    pub(crate) cli: CliInputs<'a>,
}

/// What Settings > CLI improvements shows.
#[derive(Clone, Copy, Default)]
pub(crate) struct CliInputs<'a> {
    /// The name of the project a terminal opens in.
    pub(crate) project: Option<&'a str>,
    /// Why no terminal can be started now (no project of this machine on
    /// show, Claude deactivated or turned off in the project).
    pub(crate) unavailable: Option<&'a str>,
    /// Why the last start failed.
    pub(crate) error: Option<&'a str>,
    /// A start is on its way.
    pub(crate) starting: bool,
}

pub(crate) fn sheet(
    ctx: &egui::Context,
    area: Rect,
    state: &mut SettingsSheet,
    inputs: Inputs<'_>,
    modes: LaunchModes,
    out: &mut Out<SettingsAction>,
) {
    let closed = crate::sheet::show(ctx, area, "settings-panel", "Settings", |ui, content| {
        body(ui, content, state, inputs, modes, out);
    });
    if closed {
        out.actions.push(SettingsAction::Close);
    }
    if let Some(dialog) = &mut state.providers {
        let mut closed = false;
        crate::dialog::card(ctx, "providers-dialog", |ui| {
            crate::providers::dialog(
                ui,
                dialog,
                &inputs.agents.claude,
                &inputs.agents.pi,
                out,
                &mut closed,
            );
        });
        if closed {
            state.providers = None;
        }
    }
}

fn body(
    ui: &mut Ui,
    rect: Rect,
    state: &mut SettingsSheet,
    inputs: Inputs<'_>,
    modes: LaunchModes,
    out: &mut Out<SettingsAction>,
) {
    let Inputs {
        view,
        remote,
        agents,
        tab,
        secrets_in_system_store,
        updates,
        vault,
        projects,
        cli,
    } = inputs;
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        // Escape closes what is on top: the setup dialog, then the sheet.
        if state.providers.take().is_none() {
            out.actions.push(SettingsAction::Close);
        }
    }
    forms::column(ui, rect, |ui| {
        tab_row(ui, tab, out);
        // A section is longer than a window can be.
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| match tab {
                SettingsTab::Home => {
                    home_section(ui, state, view, out);
                    updates_section(ui, updates, out);
                }
                SettingsTab::Agents => agents_section(ui, state, agents, out),
                SettingsTab::Launch => launch_section(ui, modes, out),
                SettingsTab::Secrets => {
                    crate::vault_ui::section(ui, &mut state.vault, vault, projects, out);
                    secrets_section(ui, secrets_in_system_store, out);
                }
                SettingsTab::Remote => remote_section(ui, state, remote, out),
                SettingsTab::Cli => cli_section(ui, agents, cli, out),
            });
    });
}

/// CLI improvements: tasks a coding CLI does on itself. Each shows only
/// when its CLI is installed; with none, the section says what it needs.
fn cli_section(ui: &mut Ui, agents: &AgentsView, cli: CliInputs, out: &mut Out<SettingsAction>) {
    out.say(ui, heading("CLI IMPROVEMENTS"));
    out.say(
        ui,
        muted("A coding CLI improves how it is set up for you, from what it learned of your work. It asks you whenever it is in doubt and before anything risky."),
    );
    let claude = agents
        .rows
        .iter()
        .any(|row| row.tool == ToolKind::Claude && row.installed);
    if !claude {
        out.say(
            ui,
            muted("Nothing to do here yet: these work through an installed coding CLI. Install Claude Code in Settings > Agents."),
        );
        return;
    }
    forms::separator(ui);
    out.say(
        ui,
        RichText::new("Claude auto apply insights").color(theme::TEXT),
    );
    out.say(
        ui,
        muted("Opens a Claude Code terminal on Opus that runs /insights, reads the report and applies its suggestions: settings, hooks, CLAUDE.md, skills. What is about you goes in the Claude config folder the terminal runs on (~/.claude, or the project's isolated one), what is about the project in the project."),
    );
    match (cli.project, cli.unavailable) {
        (Some(name), _) => out.say(ui, muted(format!("The terminal opens in {name}."))),
        (None, Some(why)) => out.say(ui, warning(why)),
        (None, None) => out.say(
            ui,
            warning("Open a project first: the terminal opens in it."),
        ),
    }
    if let Some(why) = cli.error {
        out.say(ui, problem(why));
    }
    if cli.starting {
        out.say(ui, muted("Starting the terminal…"));
    }
    if out.button(
        ui,
        "claude-insights",
        "Claude auto apply insights",
        cli.project.is_some() && !cli.starting,
    ) {
        out.actions.push(SettingsAction::ApplyClaudeInsights);
    }
}

/// The row of section tabs. It wraps onto a second line when the sheet is
/// narrow, and the selected one is highlighted.
fn tab_row(ui: &mut Ui, current: SettingsTab, out: &mut Out<SettingsAction>) {
    let tabs = SettingsTab::ALL.map(|tab| (tab, tab.key(), tab.label()));
    if let Some(tab) = forms::tab_row(ui, out, current, tabs) {
        out.actions.push(SettingsAction::SelectTab(tab));
    }
}

fn home_section(
    ui: &mut Ui,
    state: &SettingsSheet,
    view: &PackagesView,
    out: &mut Out<SettingsAction>,
) {
    out.say(ui, heading("CONNECTO HOME"));
    if let Some(error) = &state.error {
        out.say(ui, problem(error));
    }
    if let Some(error) = &view.last_error {
        out.say(ui, problem(format!("Last error: {error}")));
    }
    match &view.home {
        Some(path) => out.say(
            ui,
            RichText::new(format!("Folder: {}", path.display())).color(theme::TEXT),
        ),
        None => out.say(ui, warning("Folder: not set")),
    }
    out.say(
        ui,
        muted("Holds your automations (automations/) and your packages of skills and plugins (packages/)."),
    );
    let label = if view.home.is_some() {
        "Change…"
    } else {
        "Choose…"
    };
    if out.button(ui, "choose-home", label, true) {
        out.actions.push(SettingsAction::ChooseHome);
    }
    match &view.migration {
        HomeMigration::None => {}
        HomeMigration::Working => out.say(ui, muted("Moving automations…")),
        HomeMigration::Done { moved } => {
            let text = match moved {
                0 => "Nothing needed moving.".to_owned(),
                1 => "Moved 1 automation into the new folder.".to_owned(),
                n => format!("Moved {n} automations into the new folder."),
            };
            out.say(ui, RichText::new(text).size(11.5).color(theme::GREEN));
        }
        HomeMigration::Error(error) => {
            out.say(ui, problem(format!("Moving failed: {error}")));
        }
    }
}

/// The version running, how updates are checked, and a check now.
fn updates_section(ui: &mut Ui, view: &connecto_core::UpdateView, out: &mut Out<SettingsAction>) {
    use connecto_core::UpdateStatus;
    forms::separator(ui);
    out.say(ui, heading("UPDATES"));
    let current = if view.current.is_empty() {
        "This is Connecto.".to_owned()
    } else {
        format!("This is Connecto {}.", view.current)
    };
    out.say(ui, RichText::new(current).color(theme::TEXT));
    let status = match &view.status {
        UpdateStatus::Idle => None,
        UpdateStatus::Checking => Some("Checking…".to_owned()),
        UpdateStatus::UpToDate => Some("It is the newest version.".to_owned()),
        UpdateStatus::Downloading { version } => Some(format!("Downloading {version}…")),
        UpdateStatus::Ready { version } if view.auto_install => Some(match &view.held {
            Some(held) => {
                format!("{version} is ready and installs itself when nothing holds it back. {held}")
            }
            None => format!("{version} is ready and installs itself in a moment."),
        }),
        UpdateStatus::Ready { version } => Some(format!(
            "{version} is ready: restart from the strip at the top to install it."
        )),
        UpdateStatus::Manual { version, why } => Some(format!("{version} is available. {why}")),
        UpdateStatus::Failed(why) => Some(format!("The last check failed: {why}")),
        UpdateStatus::Unavailable(why) => Some(why.clone()),
    };
    if let Some(status) = status {
        out.say(ui, muted(status));
    }
    let mut on = view.auto_check;
    let toggle = ui.checkbox(&mut on, "Check for updates automatically");
    out.mark("auto-update", toggle.rect);
    if toggle.changed() {
        out.actions.push(SettingsAction::SetAutoUpdate(on));
    }
    out.say(
        ui,
        muted("At launch and then every minute. A new version is downloaded in the background and checked before it is installed."),
    );
    let mut auto = view.auto_install;
    let toggle = ui.checkbox(&mut auto, "Install updates automatically");
    out.mark("auto-install", toggle.rect);
    if toggle.changed() {
        out.actions.push(SettingsAction::SetAutoInstall(auto));
    }
    out.say(
        ui,
        muted("Connecto restarts by itself once nothing holds it back (a pairing, a file transfer, typing in a terminal), and agents that were in a turn are told \"continua\" to carry on. Off: it waits for Update and restart."),
    );
    if out.button(ui, "check-updates", "Check now", true) {
        out.actions.push(SettingsAction::CheckForUpdates);
    }
}

/// Every supported agent with where it stands, and what can be done about it,
/// then the utility model.
fn agents_section(
    ui: &mut Ui,
    state: &mut SettingsSheet,
    agents: &AgentsView,
    out: &mut Out<SettingsAction>,
) {
    out.say(ui, heading("AGENTS"));
    out.say(
        ui,
        muted("The coding agents Connecto works with. Activate one to install it if needed and sign in; Deactivate hides it from new terminals and removes nothing."),
    );
    if let Some(error) = &state.agents_error {
        out.say(ui, problem(error.clone()));
    }
    for row in &agents.rows {
        let key = row.tool.label().to_lowercase();
        forms::card(ui, |ui| {
            out.say(ui, RichText::new(&row.name).color(theme::TEXT));
            let installed = if row.installed {
                match &row.version {
                    Some(version) => format!("Installed: {version}"),
                    None => "Installed".to_owned(),
                }
            } else {
                "Not installed".to_owned()
            };
            out.say(ui, muted(installed));
            let signed_in_here = format!("Signed in on {}", platform::THIS_COMPUTER);
            let providers = crate::providers::pi_providers_line(&agents.pi);
            let (sign, color) = match (row.installed, row.signed_in) {
                // Pi has no account: what it can reach is its providers.
                _ if row.tool == ToolKind::Pi => (
                    providers.as_str(),
                    if agents.pi.usable() || agents.pi.undecided() {
                        theme::TEXT_MUTED
                    } else {
                        theme::AMBER
                    },
                ),
                (false, _) => ("Sign-in: not yet", theme::TEXT_MUTED),
                // Its sign-in is in the system keychain and is not asked:
                // its terminal shows the sign-in screen when it needs one.
                (true, None) if row.tool == ToolKind::Antigravity => (
                    "Signs in with Google in its own terminal",
                    theme::TEXT_MUTED,
                ),
                // It signs in to each model provider on its own, and has
                // free models without any.
                (true, _) if row.tool == ToolKind::OpenCode => (
                    "Signs in to its providers with /connect in its own terminal",
                    theme::TEXT_MUTED,
                ),
                (true, Some(true)) => (signed_in_here.as_str(), theme::GREEN),
                (true, Some(false)) => ("Not signed in", theme::AMBER),
                (true, None) => ("Sign-in: could not be checked", theme::TEXT_MUTED),
            };
            out.say(ui, RichText::new(sign).size(11.5).color(color));
            if row.tool == ToolKind::Claude
                && crate::providers::CLAUDE_SUPPORTED
                && let Some((line, color)) = crate::providers::claude_providers_line(&agents.claude)
            {
                out.say(ui, RichText::new(line).size(11.5).color(color));
            }
            if !row.active {
                out.say(ui, muted("Hidden from new terminals."));
            }
            match &row.busy {
                Some(AgentBusy::Installing(line)) => out.say(ui, muted(line.clone())),
                Some(AgentBusy::Checking) => out.say(ui, muted("Checking…")),
                None => {}
            }
            if let Some(error) = &row.error {
                out.say(ui, problem(error.clone()));
            }
            if let Some(prompt) = &row.node_prompt {
                out.say(ui, warning(prompt.clone()));
            }
            if !row.installed && row.busy.is_none() {
                out.say(ui, muted(format!("Installs from {}.", row.source)));
            }
            let idle = row.busy.is_none();
            // Pi that is installed and has no provider that works is not a
            // thing to activate: it is a thing to set up.
            let needs_setup = row.tool == ToolKind::Pi
                && row.installed
                && !agents.pi.usable()
                && !agents.pi.undecided();
            ui.horizontal_wrapped(|ui| {
                if row.tool == ToolKind::Pi
                    && row.installed
                    && out.button(ui, "pi-setup-open", "Set up model providers…", true)
                {
                    state.providers =
                        Some(ProvidersDialog::new(ProviderScope::Global, ToolKind::Pi));
                }
                if row.tool == ToolKind::Claude
                    && crate::providers::claude_setup_offered(
                        crate::providers::CLAUDE_SUPPORTED,
                        row.installed,
                    )
                    && out.button(ui, "claude-setup-open", "Set up model providers…", true)
                {
                    state.providers = Some(ProvidersDialog::new(
                        ProviderScope::Global,
                        ToolKind::Claude,
                    ));
                }
                if row.node_prompt.is_some() {
                    // The install waits for an answer.
                    if out.button(
                        ui,
                        &format!("agent-node-install:{key}"),
                        "Install Node.js and continue",
                        idle,
                    ) {
                        out.actions.push(SettingsAction::InstallNode(row.tool));
                    }
                    if out.button(ui, &format!("agent-node-decline:{key}"), "Not now", idle) {
                        out.actions.push(SettingsAction::DeclineNode(row.tool));
                    }
                } else if needs_setup {
                } else if !row.installed || !row.active {
                    let label = if row.installed {
                        "Activate"
                    } else {
                        "Install and activate"
                    };
                    if out.button(ui, &format!("agent-activate:{key}"), label, idle) {
                        out.actions.push(SettingsAction::ActivateAgent(row.tool));
                    }
                } else if out.button(ui, &format!("agent-deactivate:{key}"), "Deactivate", idle) {
                    out.actions.push(SettingsAction::DeactivateAgent(row.tool));
                }
                if row.installed
                    && row.tool != ToolKind::Pi
                    && row.signed_in != Some(true)
                    && out.button(ui, &format!("agent-signin:{key}"), "Sign in", idle)
                {
                    state.agents_error = None;
                    out.actions.push(SettingsAction::SignInAgent(row.tool));
                }
            });
        });
    }
    if !agents.rows.is_empty() && out.button(ui, "agents-refresh", "Check again", true) {
        out.actions.push(SettingsAction::RefreshAgents);
    }
    utility_model(ui, out);
}

/// The model and effort that titles use. They are set in the Utility Sessions
/// sheet, which this opens.
fn utility_model(ui: &mut Ui, out: &mut Out<SettingsAction>) {
    forms::separator(ui);
    out.say(ui, heading("UTILITY MODEL"));
    out.say(
        ui,
        muted("The model and effort for titles are set with the gear in View > Utility Sessions."),
    );
    if out.button(ui, "open-utility", "Open Utility Sessions…", true) {
        out.actions.push(SettingsAction::OpenUtilitySessions);
    }
}

/// How each coding CLI is launched with respect to its own permission prompts.
fn launch_section(ui: &mut Ui, modes: LaunchModes, out: &mut Out<SettingsAction>) {
    out.say(ui, heading("HOW CODING CLIS ARE LAUNCHED"));
    out.say(
        ui,
        muted("Applies to every terminal started or restarted from now on, whoever asks for it (you, an agent, an automation or a remote peer). A terminal that is already running keeps how it was launched: restart it to change that."),
    );
    for (tool, name, labels) in [
        (
            ToolKind::Claude,
            "Claude",
            [
                "Skip permission checks  (--dangerously-skip-permissions)",
                "Auto mode: a model reviews each action  (--permission-mode auto)",
                "Ask before acting: Claude's normal permission prompts  (no flag)",
            ],
        ),
        (
            ToolKind::Codex,
            "Codex",
            [
                "Skip approvals and the sandbox  (--dangerously-bypass-approvals-and-sandbox, --yolo)",
                "Automatic review, sandboxed  (--approve-for-me)",
                "Ask before acting: Codex's normal approvals  (no flag)",
            ],
        ),
        (
            ToolKind::Pi,
            "Pi",
            // Pi never asks before a tool call. Its one question is whether a
            // project's own `.pi` resources may load, so it has two choices.
            [
                "Load a project's own .pi resources without asking  (--approve)",
                "Ask before loading a project's own .pi resources  (no flag)",
                "",
            ],
        ),
        (
            ToolKind::Antigravity,
            "Antigravity",
            [
                "Skip permission prompts  (--dangerously-skip-permissions)",
                "Auto mode, where your account offers it  (--mode auto)",
                "Ask before acting: Antigravity's normal permission prompts  (no flag)",
            ],
        ),
        (
            ToolKind::OpenCode,
            "OpenCode",
            // OpenCode has no reviewing mode: approve, or ask.
            [
                "Approve every permission your config does not deny  (--auto)",
                "",
                "Ask before acting: OpenCode's permission prompts  (no flag)",
            ],
        ),
    ] {
        out.say(ui, RichText::new(name).color(theme::TEXT));
        for (mode, label) in AgentMode::ALL.into_iter().zip(labels) {
            if label.is_empty() {
                continue;
            }
            let chosen = modes.for_tool(tool);
            // Pi's two choices: the third mode launches as the second does.
            let on = chosen == mode
                || (tool == ToolKind::Pi && mode == AgentMode::Auto && chosen == AgentMode::Prompt);
            let key = format!("mode:{}:{}", name.to_lowercase(), mode.cli_value());
            if out.choice(ui, &key, on, label) && !on {
                out.actions.push(SettingsAction::SetLaunchMode(tool, mode));
            }
        }
    }
    if modes.claude != AgentMode::Bypass
        || modes.codex != AgentMode::Bypass
        || modes.antigravity != AgentMode::Bypass
    {
        out.say(
            ui,
            warning("A coding CLI that is not skipping its checks can stop and wait at a permission prompt. The terminal then shows it (Claude also marks it as needing you); answer it in the terminal, here or from a remote machine. Messages sent to that terminal wait behind it."),
        );
    }
}

fn input(
    ui: &mut Ui,
    id: &'static str,
    text: &mut String,
    hint: &str,
    password: bool,
) -> egui::Response {
    ui.add(
        TextEdit::singleline(text)
            .id(Id::unique(id))
            .hint_text(hint)
            .password(password)
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    )
}

/// Remote access: whether this machine lets others in, the pairing with other
/// machines, the machines paired, and the relay. Errors show at the top.
fn remote_section(
    ui: &mut Ui,
    state: &mut SettingsSheet,
    view: &RemoteView,
    out: &mut Out<SettingsAction>,
) {
    out.say(ui, heading("REMOTE ACCESS"));
    if let Some(error) = &view.last_error {
        out.say(ui, problem(error));
    }
    out.say(
        ui,
        muted("Lets another Connecto connect to this machine and use its terminals as you can. It is off until you turn it on, and only machines you pair with get in."),
    );
    let mut on = view.hosting_enabled;
    let toggle = ui.checkbox(&mut on, "Let other machines connect to this one");
    out.mark("hosting-toggle", toggle.rect);
    if toggle.changed() {
        out.actions.push(SettingsAction::SetHosting(on));
    }
    match &view.hosting {
        HostingStatus::Off => out.say(ui, muted("Off.")),
        HostingStatus::Starting => out.say(ui, muted("Starting…")),
        HostingStatus::Listening => out.say(
            ui,
            RichText::new("Listening.").size(11.5).color(theme::GREEN),
        ),
        HostingStatus::Error(why) => out.say(ui, problem(format!("Could not start: {why}"))),
    }
    if let Some(peer) = &view.peer {
        out.say(ui, warning(format!("Connected now: {peer}")));
    }
    forms::separator(ui);
    out.say(ui, heading("PAIR A DEVICE"));
    out.say(
        ui,
        muted(format!("Both machines click Pair a device. Each shows its own {PAIRING_CODE_DIGITS} digits and types the other's, then each names the other. The dialog shows each step.")),
    );
    if out.button(ui, "pair-start", "Pair a device", true) {
        out.actions.push(SettingsAction::StartPairing);
    }
    forms::separator(ui);
    machines_list(ui, &view.machines, out);
    if !view.my_fingerprint.is_empty() {
        forms::separator(ui);
        out.say(
            ui,
            muted("This machine's fingerprint, to compare when it is added elsewhere:"),
        );
        out.say(
            ui,
            RichText::new(&view.my_fingerprint)
                .monospace()
                .color(theme::CYAN),
        );
        out.say(ui, muted("Its address, to give to the other machine:"));
        ui.horizontal(|ui| {
            ui.add(
                egui::Label::new(RichText::new(&view.my_address).monospace().size(11.0))
                    .selectable(true)
                    .truncate(),
            );
        });
        if out.button(
            ui,
            "copy-address",
            "Copy address",
            !view.my_address.is_empty(),
        ) {
            ui.ctx().copy_text(view.my_address.clone());
        }
    }

    forms::separator(ui);
    relay_section(ui, state, view, out);
}

/// Where secrets are kept: Connecto's own file by default, or the system
/// store for those who want it. Only where there is one (macOS, Windows).
fn secrets_section(ui: &mut Ui, in_system_store: bool, out: &mut Out<SettingsAction>) {
    let Some(label) = crate::platform::SYSTEM_STORE_CHOICE else {
        return;
    };
    forms::separator(ui);
    out.say(ui, heading("WHERE SECRETS ARE KEPT"));
    out.say(
        ui,
        muted("These secrets, device keys, the relay token and the Jev key are kept in a file only you can read, in Connecto's data folder. No system prompts."),
    );
    let mut on = in_system_store;
    let toggle = ui.checkbox(&mut on, label);
    out.mark("secrets-system-store", toggle.rect);
    if toggle.changed() {
        out.actions
            .push(SettingsAction::SetSecretsInSystemStore(on));
    }
    out.say(
        ui,
        muted("Takes effect when Connecto starts again; the secrets are moved across then."),
    );
}

/// The paired machines: who they are, when, and whether they are connected.
fn machines_list(ui: &mut Ui, machines: &[MachineRow], out: &mut Out<SettingsAction>) {
    out.say(ui, heading("MACHINES"));
    if machines.is_empty() {
        out.say(ui, muted("No machines paired yet."));
        return;
    }
    for row in machines {
        forms::card(ui, |ui| {
            out.say(ui, RichText::new(&row.name).color(theme::TEXT));
            let paired = if row.paired_at.is_empty() {
                "Paired".to_owned()
            } else {
                format!("Paired {}", row.paired_at)
            };
            out.say(ui, muted(paired));
            out.say(ui, muted(connection_words(row)));
            if out.button(
                ui,
                &format!("disconnect-machine:{}", row.id),
                "Disconnect…",
                true,
            ) {
                out.actions.push(SettingsAction::AskRemoveMachine {
                    id: row.id.clone(),
                    name: row.name.clone(),
                });
            }
        });
    }
}

/// One side of a machine's connection, in words, or `None` when it is down.
fn side_words(link: &HostLink, way: &str) -> Option<String> {
    match link {
        HostLink::Disconnected => None,
        HostLink::Connecting => Some(format!("connecting {way}")),
        HostLink::Direct => Some(format!("connected {way} (direct)")),
        HostLink::Relayed => Some(format!("connected {way} (through the relay)")),
        HostLink::Connected => Some(format!("connected {way}")),
        HostLink::Reconnecting { retry_in_secs } => {
            Some(format!("reconnecting {way} in {retry_in_secs} s"))
        }
        HostLink::Refused(why) => Some(format!("refused {way}: {why}")),
    }
}

/// How a machine is connected now, both ways.
fn connection_words(row: &MachineRow) -> String {
    let parts: Vec<String> = [
        side_words(&row.inbound, "from it"),
        side_words(&row.outbound, "to it"),
    ]
    .into_iter()
    .flatten()
    .collect();
    if parts.is_empty() {
        "Not connected".to_owned()
    } else {
        parts.join("; ")
    }
}

fn relay_section(
    ui: &mut Ui,
    state: &mut SettingsSheet,
    view: &RemoteView,
    out: &mut Out<SettingsAction>,
) {
    out.say(ui, heading("RELAY"));
    out.say(
        ui,
        muted("Connects two machines that cannot reach each other directly. Leave the address empty for direct connections only."),
    );
    let url = state
        .relay_url
        .get_or_insert_with(|| view.relay_url.clone().unwrap_or_default());
    let response = input(ui, "relay-url", url, "https://relay.example.com", false);
    out.mark("relay-url", response.rect);
    let unreadable = format!(
        "Access token could not be read from the {}",
        crate::platform::KEY_STORE
    );
    let (access, hint) = if view.relay_token_unreadable {
        (
            unreadable.as_str(),
            "access token, if the relay asks for one",
        )
    } else if view.relay_token_set {
        ("Access token set", "access token saved: type to replace")
    } else if view.relay_builtin_token {
        (
            "Using Connecto's relay access",
            "optional: your own access token",
        )
    } else {
        ("No access token", "access token, if the relay asks for one")
    };
    out.say(ui, muted(access));
    let response = input(ui, "relay-token", &mut state.relay_token, hint, true);
    out.mark("relay-token", response.rect);
    ui.horizontal(|ui| {
        if out.button(ui, "relay-save", "Save relay", true) {
            state.relay_error = None;
            let token = (!state.relay_token.is_empty()).then(|| state.relay_token.clone());
            out.actions.push(SettingsAction::SetRelay {
                url: state.relay_url.clone(),
                token,
            });
            state.relay_token.clear();
        }
        if view.relay_token_set && out.button(ui, "relay-clear-token", "Remove saved token", true) {
            state.relay_error = None;
            out.actions.push(SettingsAction::SetRelay {
                url: state.relay_url.clone(),
                token: Some(String::new()),
            });
        }
    });
    if let Some(error) = &state.relay_error {
        out.say(ui, problem(error));
    }
}
