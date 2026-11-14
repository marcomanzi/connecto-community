//! The Settings sheet (Connecto menu): Connecto Home, the agents, how coding
//! CLIs are launched, and remote access. One section shows at a time, under a
//! row of tabs.

use connecto_core::{
    AgentBusy, AgentMode, AgentsView, HomeMigration, HostLink, HostingStatus, LaunchModes,
    MachineRow, PAIRING_CODE_DIGITS, PackagesView, RemoteView, ToolKind,
};
use egui::{Id, Key, Rect, RichText, TextEdit, Ui};

use crate::forms::{self, Out, heading, muted, problem, warning};
use crate::{platform, theme};

/// The sections of Settings, in the order of the tab row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum SettingsTab {
    #[default]
    Home,
    Agents,
    Launch,
    Remote,
}

impl SettingsTab {
    pub(crate) const ALL: [Self; 4] = [Self::Home, Self::Agents, Self::Launch, Self::Remote];

    /// The tab's name, which is also the section's wording.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Home => "Connecto Home",
            Self::Agents => "Agents",
            Self::Launch => "How Coding CLIs are launched",
            Self::Remote => "Remote Access",
        }
    }

    /// The key the tab is marked with, for tests.
    fn key(self) -> &'static str {
        match self {
            Self::Home => "tab-home",
            Self::Agents => "tab-agents",
            Self::Launch => "tab-launch",
            Self::Remote => "tab-remote",
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
    /// Check for updates at launch and daily, or not.
    SetAutoUpdate(bool),
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
    /// Open the agent's sign-in.
    SignInAgent(ToolKind),
    /// Look again at what is installed and signed in.
    RefreshAgents,
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
    } = inputs;
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        out.actions.push(SettingsAction::Close);
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
                SettingsTab::Remote => {
                    remote_section(ui, state, remote, out);
                    secrets_section(ui, secrets_in_system_store, out);
                }
            });
    });
}

/// The row of section tabs. It wraps onto a second line when the sheet is
/// narrow, and the selected one is highlighted.
fn tab_row(ui: &mut Ui, current: SettingsTab, out: &mut Out<SettingsAction>) {
    ui.horizontal_wrapped(|ui| {
        for tab in SettingsTab::ALL {
            let on = tab == current;
            if out.choice(ui, tab.key(), on, tab.label()) && !on {
                out.actions.push(SettingsAction::SelectTab(tab));
            }
        }
    });
    ui.add_space(6.0);
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
        muted("At launch and then daily. A new version is downloaded in the background, checked, and installed only when you choose Update and restart."),
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
            let (sign, color) = match (row.installed, row.signed_in) {
                (false, _) => ("Sign-in: not yet", theme::TEXT_MUTED),
                (true, Some(true)) => (signed_in_here.as_str(), theme::GREEN),
                (true, Some(false)) => ("Not signed in", theme::AMBER),
                (true, None) => ("Sign-in: could not be checked", theme::TEXT_MUTED),
            };
            out.say(ui, RichText::new(sign).size(11.5).color(color));
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
            if !row.installed && row.busy.is_none() {
                out.say(ui, muted(format!("Installs from {}.", row.source)));
            }
            let idle = row.busy.is_none();
            ui.horizontal_wrapped(|ui| {
                if !row.installed || !row.active {
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
    ] {
        out.say(ui, RichText::new(name).color(theme::TEXT));
        for (mode, label) in AgentMode::ALL.into_iter().zip(labels) {
            let on = modes.for_tool(tool) == mode;
            let key = format!("mode:{}:{}", name.to_lowercase(), mode.cli_value());
            if out.choice(ui, &key, on, label) && !on {
                out.actions.push(SettingsAction::SetLaunchMode(tool, mode));
            }
        }
    }
    if modes.claude != AgentMode::Bypass || modes.codex != AgentMode::Bypass {
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
    out.say(ui, heading("SECRETS"));
    out.say(
        ui,
        muted("Device keys, the relay token and the Jev key are kept in a file only you can read, in Connecto's data folder. No system prompts."),
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
