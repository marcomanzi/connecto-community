//! The Project Settings sheet (gear in the sidebar): which packages a project
//! gets, which tools it uses and where each tool reads its configuration.

use connecto_core::{
    ConfigMode, InstallState, InstalledPackage, PackageItem, PackagesView, ProjectPackagesView,
    ProjectSettings, ProjectTools, RequirementState, RequirementView, ToolKind,
};
use egui::{Key, Rect, RichText, Ui};

use crate::forms::{self, Out, card, heading, muted, problem, warning};
use crate::providers::{ProvidersAction, ProvidersDialog};
use crate::theme;
use crate::utility::ago;

/// The sections of Project Settings, in the order of the tab row.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum ProjectTab {
    /// The default package and the added ones.
    #[default]
    Packages,
    Tools,
    /// Where the packages stand in this project, with Refresh.
    Status,
    /// The project's memory (see [`crate::memory`]).
    Memory,
    /// The project's operations (see [`crate::operations`]).
    Operations,
    /// Agent Token Optimization (see [`crate::token_opt`]).
    Tokens,
}

impl ProjectTab {
    pub(crate) const ALL: [Self; 6] = [
        Self::Packages,
        Self::Tools,
        Self::Status,
        Self::Memory,
        Self::Operations,
        Self::Tokens,
    ];

    /// The tab's name, which is also the section's wording.
    pub(crate) fn label(self) -> &'static str {
        match self {
            Self::Packages => "Packages",
            Self::Tools => "Tools",
            Self::Status => "Status",
            Self::Memory => "Project Memory",
            Self::Operations => "Operations",
            Self::Tokens => "Tokens",
        }
    }

    /// The key the tab is marked with, for tests.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Self::Packages => "ptab-packages",
            Self::Tools => "ptab-tools",
            Self::Status => "ptab-status",
            Self::Memory => "ptab-memory",
            Self::Operations => "ptab-operations",
            Self::Tokens => "ptab-tokens",
        }
    }
}

pub(crate) struct ProjectSettingsSheet {
    pub(crate) project: String,
    /// Why the last change was refused.
    pub(crate) error: Option<String>,
    pub(crate) memory: crate::memory::MemoryPanel,
    pub(crate) tokens: crate::token_opt::TokenPanel,
    /// The section drawn last frame, to see when the user leaves Project Memory.
    pub(crate) drawn_tab: ProjectTab,
    /// The dialog that sets this project's own model providers up, when open.
    pub(crate) providers: Option<ProvidersDialog>,
    /// The Operations tab: what its list remembers, and the view it shows.
    pub(crate) ops_list: crate::operations::ListState,
    pub(crate) ops_view: Option<(std::time::Instant, connecto_core::ops::OpsView)>,
    /// The tools a package needs, while the user is adding it (ADR 0037).
    pub(crate) requirements: Option<RequirementsDialog>,
}

/// A package the user is adding that needs tools: it is added only once
/// every one of them is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct RequirementsDialog {
    pub(crate) package: String,
    /// The settings to save once the tools are there (the package added).
    pub(crate) settings: ProjectSettings,
    /// The install command waiting for the user's yes: the package that
    /// names the tool (the one added, or one it extends), the tool, and the
    /// command as shown, which is the one approved.
    pub(crate) confirming: Option<(String, String, String)>,
}

impl ProjectSettingsSheet {
    pub(crate) fn new(project: String) -> Self {
        Self {
            project,
            error: None,
            memory: crate::memory::MemoryPanel::default(),
            tokens: crate::token_opt::TokenPanel::default(),
            drawn_tab: ProjectTab::default(),
            providers: None,
            ops_list: crate::operations::ListState::default(),
            ops_view: None,
            requirements: None,
        }
    }

    /// The backend refused the change.
    pub(crate) fn rejected(&mut self, error: String) {
        self.error = Some(error);
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ProjectSettingsAction {
    Close,
    /// Save the whole updated settings.
    Save(ProjectSettings),
    Refresh,
    /// Show another section. The app keeps the choice for the next sheet.
    SelectTab(ProjectTab),
    /// A change in the project's model providers dialog.
    Providers(ProvidersAction),
    /// Start a terminal of this agent in the project (it has none yet).
    Start(ToolKind),
    /// Something asked of the project's operations.
    Ops(connecto_core::ops::OpsAction),
    /// Look again for the tools a package needs.
    CheckRequirements(String),
    /// Run a tool's install command in a new shell terminal of the project:
    /// the user has seen the command and said yes.
    InstallRequirement {
        package: String,
        requirement: String,
        /// The command as the user saw it: run only if it is still that.
        command: String,
    },
}

impl From<ProvidersAction> for ProjectSettingsAction {
    fn from(action: ProvidersAction) -> Self {
        Self::Providers(action)
    }
}

pub(crate) struct ProjectSettingsInput<'a> {
    pub(crate) view: &'a PackagesView,
    pub(crate) project_name: &'a str,
    /// Settings submitted that the backend's view does not show yet. Edits
    /// build on these, so a second change never undoes the first.
    pub(crate) pending: Option<&'a ProjectSettings>,
    pub(crate) now_ms: u64,
    pub(crate) utc_offset_s: i32,
    pub(crate) memory: &'a dyn connecto_core::MemoryControl,
    /// The section on show.
    pub(crate) tab: ProjectTab,
    /// The agents, for the project's own model providers.
    pub(crate) agents: &'a dyn connecto_core::AgentsControl,
    /// Agent Token Optimization.
    pub(crate) tokens: &'a dyn connecto_core::TokenControl,
    /// Functional memory, shown under Project Memory (ADR 0033).
    pub(crate) functional_memory: &'a dyn connecto_core::functional_memory::FunctionalMemoryControl,
    /// The agents a terminal can be started with now, when the project has no
    /// terminal yet (a new project); empty otherwise.
    pub(crate) start: &'a [ToolKind],
    /// The project's operations.
    pub(crate) ops: &'a dyn connecto_core::ops::OpsControl,
    /// The project is another machine's: its packages, tools and tokens are
    /// that machine's (read and changed over the link); its memory and model
    /// providers are set up there.
    pub(crate) remote: bool,
}

pub(crate) fn sheet(
    ctx: &egui::Context,
    area: Rect,
    state: &mut ProjectSettingsSheet,
    input: &ProjectSettingsInput<'_>,
    out: &mut Out<ProjectSettingsAction>,
) {
    let title = format!("Project Settings — {}", input.project_name);
    let closed = crate::sheet::show(
        ctx,
        area,
        "project-settings-panel",
        &title,
        |ui, content| {
            body(ui, content, state, input, out);
        },
    );
    if closed {
        out.actions.push(ProjectSettingsAction::Close);
    }
    if let Some(dialog) = &mut state.requirements {
        let mut done = false;
        let error = state.error.as_deref();
        crate::dialog::card(ctx, "package-requirements-dialog", |ui| {
            if let Some(error) = error {
                out.say(ui, problem(error));
            }
            requirements_dialog(ui, dialog, input, out, &mut done);
        });
        if done {
            state.requirements = None;
        }
    }
    if let Some(dialog) = &mut state.providers {
        let (claude, pi) = match dialog.tool {
            ToolKind::Pi => (
                connecto_core::ClaudeProvidersView::default(),
                input.agents.pi_providers(&dialog.scope),
            ),
            _ => (
                input.agents.claude_providers(&dialog.scope),
                connecto_core::PiProvidersView::default(),
            ),
        };
        let mut done = false;
        crate::dialog::card(ctx, "project-providers-dialog", |ui| {
            crate::providers::dialog(ui, dialog, &claude, &pi, out, &mut done);
        });
        if done {
            state.providers = None;
        }
    }
}

fn body(
    ui: &mut Ui,
    rect: Rect,
    state: &mut ProjectSettingsSheet,
    input: &ProjectSettingsInput<'_>,
    out: &mut Out<ProjectSettingsAction>,
) {
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        // Escape closes what is on top: a dialog, then the sheet.
        if state.requirements.take().is_none() && state.providers.take().is_none() {
            out.actions.push(ProjectSettingsAction::Close);
        }
    }
    forms::column(ui, rect, |ui| {
        let Some(shown) = input.view.project(&state.project) else {
            ui.add_space(8.0);
            if let Some(error) = &input.view.last_error {
                out.say(ui, problem(error));
            } else if input.remote {
                out.say(
                    ui,
                    muted("Asking that machine for this project's settings…"),
                );
            } else {
                out.say(ui, muted("Loading the settings of this project…"));
            }
            return;
        };
        // What the user chose outranks the view until the view catches up.
        let mut project = shown.clone();
        if let Some(set) = input.pending {
            project.settings = set.clone();
        }
        let project = &project;
        if state.drawn_tab != input.tab {
            if state.drawn_tab == ProjectTab::Memory {
                crate::memory::leave(&mut state.memory);
            }
            state.drawn_tab = input.tab;
        }
        // Statement results reach the memory whichever tab is on show.
        if !input.remote {
            crate::memory::poll(ui.ctx(), &mut state.memory, input.memory, &state.project);
        }
        if let Some(error) = &state.error {
            out.say(ui, problem(error));
        }
        if let Some(error) = &input.view.last_error {
            out.say(ui, problem(format!("Last error: {error}")));
        }
        let tabs = ProjectTab::ALL.map(|tab| (tab, tab.key(), tab.label()));
        if let Some(tab) = forms::tab_row(ui, out, input.tab, tabs) {
            out.actions.push(ProjectSettingsAction::SelectTab(tab));
        }
        egui::ScrollArea::vertical()
            .id_salt("project-settings-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut next = None;
                match input.tab {
                    ProjectTab::Packages => {
                        default_section(ui, input.view, project, &mut next, out);
                        added_section(ui, input.view, project, &mut next, out);
                        // A package that needs a tool that is not there is
                        // not added: the dialog says what to install.
                        if let Some(package) = next
                            .as_ref()
                            .and_then(|settings| needs_tools(input.view, project, settings))
                            && let Some(settings) = next.take()
                        {
                            // Look again at once: the user may have installed
                            // a tool since the last look.
                            for id in package_chain(input.view, &package) {
                                out.actions.push(ProjectSettingsAction::CheckRequirements(id));
                            }
                            state.requirements = Some(RequirementsDialog {
                                package,
                                settings,
                                confirming: None,
                            });
                        }
                    }
                    ProjectTab::Tools => {
                        start_section(ui, input.start, out);
                        tools_section(
                            ui,
                            project,
                            input.view.tools_one_or_all,
                            (!input.remote).then_some(&mut state.providers),
                            &mut next,
                            out,
                        );
                    }
                    ProjectTab::Status => status_section(ui, project, input, out),
                    // A remote project's operations are its host's, through
                    // `input.ops`; its memory stays there.
                    // Its functional memory is shown and updated from here,
                    // through that machine.
                    ProjectTab::Memory if input.remote => {
                        ui.add_space(8.0);
                        out.say(
                            ui,
                            muted(
                                "This project's memory is kept on the machine it runs on. Open Project Settings > Project Memory there to change it.",
                            ),
                        );
                        functional_memory_section(
                            ui,
                            input.functional_memory,
                            &state.project,
                            (input.now_ms, input.utc_offset_s),
                            true,
                            &mut state.error,
                            out,
                        );
                    }
                    ProjectTab::Operations => {
                        let fresh = state.ops_view.as_ref().is_some_and(|(at, _)| {
                            at.elapsed() < std::time::Duration::from_secs(1)
                        });
                        if !fresh {
                            state.ops_view =
                                Some((std::time::Instant::now(), input.ops.view(&state.project)));
                        }
                        ui.ctx()
                            .request_repaint_after(std::time::Duration::from_secs(1));
                        if let Some((_, view)) = &state.ops_view {
                            let mut ops_out = Out::default();
                            crate::operations::settings_section(
                                ui,
                                &mut ops_out,
                                view,
                                &mut state.ops_list,
                                input.now_ms,
                                input.utc_offset_s,
                            );
                            #[cfg(test)]
                            {
                                out.marks.rects.extend(ops_out.marks.rects);
                                out.marks.texts.extend(ops_out.marks.texts);
                            }
                            if !ops_out.actions.is_empty() {
                                state.ops_view = None;
                            }
                            out.actions.extend(
                                ops_out.actions.into_iter().map(ProjectSettingsAction::Ops),
                            );
                        }
                    }
                    ProjectTab::Memory => {
                        let (now, offset) = (input.now_ms, input.utc_offset_s);
                        functional_memory_section(
                            ui,
                            input.functional_memory,
                            &state.project,
                            (now, offset),
                            false,
                            &mut state.error,
                            out,
                        );
                        crate::memory::section(
                            ui,
                            &mut state.memory,
                            input.memory,
                            &state.project,
                            now,
                            offset,
                        );
                    }
                    ProjectTab::Tokens => crate::token_opt::section(
                        ui,
                        &mut state.tokens,
                        input.tokens,
                        &state.project,
                    ),
                }
                if let Some(settings) = next {
                    state.error = None;
                    out.actions.push(ProjectSettingsAction::Save(settings));
                }
            });
    });
}

/// Functional memory (ADR 0033): where `.functionalMemory/` stands, Update
/// now, and the switch for automatic updates when main advances.
fn functional_memory_section(
    ui: &mut Ui,
    control: &dyn connecto_core::functional_memory::FunctionalMemoryControl,
    project: &str,
    (now_ms, offset_s): (u64, i32),
    remote: bool,
    error: &mut Option<String>,
    out: &mut Out<ProjectSettingsAction>,
) {
    use connecto_core::functional_memory::FmState;
    let status = control.status(project);
    ui.add_space(8.0);
    card(ui, |ui| {
        ui.label(heading("Functional memory"));
        out.say(
            ui,
            muted(
                "A few Markdown files in .functionalMemory/ that say what this project does, for voice calls about new features. Updated and committed on their own when main advances.",
            ),
        );
        let when = status
            .updated_ms
            .map(|ms| format!(", updated {}", ago(ms, now_ms, offset_s)));
        let commit = status
            .commit
            .as_deref()
            .map(|c| format!(" (describes {})", &c[..c.len().min(7)]));
        let line = match status.state {
            FmState::None => "Not built yet.".to_owned(),
            FmState::Idle => format!(
                "Up to date{}{}.",
                commit.unwrap_or_default(),
                when.unwrap_or_default()
            ),
            FmState::Waiting => {
                "Main moved: updating once it has been still for a while.".to_owned()
            }
            FmState::Running => "Updating now…".to_owned(),
            FmState::Deferred => "Waiting to update.".to_owned(),
            FmState::Failed => "The last update failed.".to_owned(),
            FmState::Off => "Main moved; automatic updates are off.".to_owned(),
            // The message below says why (asking, or that machine's answer).
            FmState::Unknown => String::new(),
        };
        if !line.is_empty() {
            out.say(ui, RichText::new(line));
        }
        if let Some(message) = &status.message {
            let text = muted(message.clone());
            out.say(
                ui,
                if status.state == FmState::Failed {
                    problem(message.clone())
                } else {
                    text
                },
            );
        }
        ui.horizontal(|ui| {
            let busy = matches!(status.state, FmState::Running);
            if out.button_hint(
                ui,
                "fm-update",
                "Update now",
                !busy,
                "An update is running.",
            ) && let Err(why) = control.update_now(project)
            {
                *error = Some(why);
            }
            let mut on = status.auto;
            // Another machine's is changed there, by its host, as here.
            let response = ui.checkbox(&mut on, "Update when main advances");
            out.mark("fm-auto", response.rect);
            if response.changed()
                && let Err(why) = control.set_auto(project, on)
            {
                *error = Some(why);
            }
        });
        model_row(ui, control, project, &status, error, out);
        // Asked again while shown: often while it works, and for another
        // machine's, every few seconds even when idle (nothing else wakes
        // the window when that machine starts an update).
        let busy = matches!(
            status.state,
            FmState::Running | FmState::Waiting | FmState::Unknown
        );
        if busy {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(1));
        } else if remote {
            ui.ctx()
                .request_repaint_after(std::time::Duration::from_secs(5));
        }
    });
}

/// The model updates run on: the default (named), or one of what the
/// project's CLIs offer (on another machine: that machine's).
fn model_row(
    ui: &mut Ui,
    control: &dyn connecto_core::functional_memory::FunctionalMemoryControl,
    project: &str,
    status: &connecto_core::functional_memory::FmStatus,
    error: &mut Option<String>,
    out: &mut Out<ProjectSettingsAction>,
) {
    use connecto_core::functional_memory::FmState;
    if status.state == FmState::Unknown {
        return;
    }
    if !status.model_choice {
        out.say(
            ui,
            muted("The model is chosen on that machine once its Connecto is updated."),
        );
        return;
    }
    if !status.models_known {
        out.say(ui, muted("Model: checking which CLIs can run it here…"));
        return;
    }
    if status.models.is_empty() && status.chosen.is_none() {
        out.say(
            ui,
            problem("No CLI can run it here: install Claude or Codex and sign in to it."),
        );
        return;
    }
    ui.add_space(4.0);
    out.say(ui, muted("Model"));
    let mut picked: Option<Option<connecto_core::functional_memory::FmModel>> = None;
    ui.horizontal_wrapped(|ui| {
        let text = match (&status.chosen, &status.model) {
            (None, Some(model)) => format!("Default ({})", model.label()),
            _ => "Default".to_owned(),
        };
        let chosen = status.chosen.as_ref();
        if out.choice(ui, "fm-model-default", chosen.is_none(), &text) && chosen.is_some() {
            picked = Some(None);
        }
        // The choice stays shown while its CLI cannot run here, so it can be
        // changed or cleared then too.
        let mut shown: Vec<&connecto_core::functional_memory::FmModel> =
            status.models.iter().collect();
        if let Some(chosen) = chosen
            && !shown.contains(&chosen)
        {
            shown.push(chosen);
        }
        for model in shown {
            let on = chosen == Some(model);
            let key = format!("fm-model:{}:{}", model.provider.word(), model.model);
            if out.choice(ui, &key, on, &model.label()) && !on {
                picked = Some(Some(model.clone()));
            }
        }
    });
    if let Some(chosen) = &status.chosen
        && !status.models.contains(chosen)
    {
        let now = status.model.as_ref().map_or_else(String::new, |m| {
            format!(": {} is used until it can", m.label())
        });
        out.say(
            ui,
            muted(format!("{} cannot run here now{now}.", chosen.label())),
        );
    }
    if let Some(model) = picked
        && let Err(why) = control.set_model(project, model)
    {
        *error = Some(why);
    }
}

fn default_section(
    ui: &mut Ui,
    view: &PackagesView,
    project: &ProjectPackagesView,
    next: &mut Option<ProjectSettings>,
    out: &mut Out<ProjectSettingsAction>,
) {
    out.say(ui, heading("DEFAULT PACKAGE"));
    let default = view
        .default_package
        .as_deref()
        .and_then(|id| view.packages.iter().find(|p| p.id == id));
    match default {
        Some(p) => out.say(
            ui,
            RichText::new(format!("{} v{}", p.name, p.version)).color(theme::TEXT),
        ),
        None => out.say(ui, muted("No default package is set.")),
    }
    let mut on = project.settings.use_default_package;
    let response = ui.checkbox(&mut on, "Use default package");
    out.mark("use-default", response.rect);
    if response.changed() {
        let mut settings = project.settings.clone();
        settings.use_default_package = on;
        *next = Some(settings);
    }
    forms::separator(ui);
}

/// The package `settings` adds (or turns on as the default) whose tools are
/// not all there, if any.
fn needs_tools(
    view: &PackagesView,
    project: &ProjectPackagesView,
    settings: &ProjectSettings,
) -> Option<String> {
    let mut added: Vec<&str> = settings
        .packages
        .iter()
        .filter(|id| !project.settings.packages.contains(id))
        .map(String::as_str)
        .collect();
    if settings.use_default_package && !project.settings.use_default_package {
        added.extend(view.default_package.as_deref());
    }
    added
        .into_iter()
        .find(|id| connecto_core::package_refusal(view, id).is_some())
        .map(str::to_owned)
}

/// The package and the packages it extends, in that order.
fn package_chain(view: &PackagesView, package: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    let mut id = Some(package.to_owned());
    while let Some(current) = id.take().filter(|i| !out.contains(i) && out.len() < 8) {
        id = view
            .packages
            .iter()
            .find(|p| p.id == current)
            .and_then(|p| p.extends.clone());
        out.push(current);
    }
    out
}

/// The tools a package needs, and the package with what it extends, each
/// with the id of the package that names it.
fn chain_requirements<'a>(
    view: &'a PackagesView,
    package: &str,
) -> Vec<(&'a str, &'a RequirementView)> {
    let mut out = Vec::new();
    for id in package_chain(view, package) {
        if let Some(item) = view.packages.iter().find(|p| p.id == id) {
            out.extend(item.requires.iter().map(|r| (item.id.as_str(), r)));
        }
    }
    out
}

fn requirements_dialog(
    ui: &mut Ui,
    dialog: &mut RequirementsDialog,
    input: &ProjectSettingsInput<'_>,
    out: &mut Out<ProjectSettingsAction>,
    done: &mut bool,
) {
    let view = input.view;
    let name = view
        .packages
        .iter()
        .find(|p| p.id == dialog.package)
        .map_or_else(|| dialog.package.clone(), |p| p.name.clone());
    out.say(ui, heading(&format!("{} NEEDS TOOLS", name.to_uppercase())));
    let machine = if input.remote {
        "the machine this project runs on"
    } else {
        "this computer"
    };
    out.say(
        ui,
        muted(format!(
            "{name} can be added once these are installed on {machine}. If you don't install them, it stays off for this project."
        )),
    );
    let requires = chain_requirements(view, &dialog.package);
    let mut checking = false;
    for (owner, req) in &requires {
        ui.push_id((owner, &req.id), |ui| {
            card(ui, |ui| {
                let (state, color) = match &req.state {
                    RequirementState::Found { version } => (
                        version
                            .as_ref()
                            .map_or_else(|| "installed".to_owned(), |v| format!("installed · {v}")),
                        theme::GREEN,
                    ),
                    RequirementState::Checking => {
                        checking = true;
                        ("looking for it…".to_owned(), theme::FAINT)
                    }
                    RequirementState::Missing => ("not installed".to_owned(), theme::AMBER),
                    RequirementState::NoVersion { wanted } => (
                        format!("installed, but its version could not be read; {wanted} is needed"),
                        theme::AMBER,
                    ),
                    RequirementState::WrongVersion { version, wanted } => (
                        format!("{version} is installed; {wanted} is needed"),
                        theme::AMBER,
                    ),
                };
                ui.horizontal_wrapped(|ui| {
                    out.say(ui, RichText::new(&req.name).strong().color(theme::TEXT));
                    out.say(ui, RichText::new(state).size(11.5).color(color));
                });
                if req.state.met() || req.state == RequirementState::Checking {
                    return;
                }
                if !req.why.is_empty() {
                    out.say(ui, muted(&req.why));
                }
                requirement_steps(ui, dialog, owner, req, input.remote, out);
            });
        });
    }
    if checking {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(300));
    }
    let ready = requires.iter().all(|(_, r)| r.state.met());
    ui.add_space(6.0);
    ui.horizontal_wrapped(|ui| {
        if out.button(
            ui,
            "requirements-check",
            "I've installed it, check again",
            !ready,
        ) {
            for id in package_chain(view, &dialog.package) {
                out.actions
                    .push(ProjectSettingsAction::CheckRequirements(id));
            }
        }
        if out.button(ui, "requirements-add", &format!("Add {name}"), ready) {
            out.actions
                .push(ProjectSettingsAction::Save(dialog.settings.clone()));
            *done = true;
        }
        if out.button(ui, "requirements-cancel", "Cancel", true) {
            *done = true;
        }
    });
}

/// How to get one missing tool: Connecto's command (after a yes), or the
/// steps to follow by hand.
fn requirement_steps(
    ui: &mut Ui,
    dialog: &mut RequirementsDialog,
    owner: &str,
    req: &RequirementView,
    remote: bool,
    out: &mut Out<ProjectSettingsAction>,
) {
    let key = format!("{owner}/{}", req.id);
    // A yes is for the command as it was shown: if the package changed it
    // since, ask again.
    if dialog
        .confirming
        .as_ref()
        .is_some_and(|(o, r, c)| o == owner && *r == req.id && req.install.as_ref() != Some(c))
    {
        dialog.confirming = None;
    }
    if req.licence {
        out.say(
            ui,
            warning(
                "Installing it means signing in or accepting a licence. Connecto never accepts terms for you: do these steps yourself.",
            ),
        );
    }
    if let Some(command) = &req.install {
        let confirming = dialog
            .confirming
            .as_ref()
            .is_some_and(|(o, r, _)| o == owner && *r == req.id);
        if confirming {
            let terminal = if remote {
                "a new shell terminal of this project, on that machine"
            } else {
                "a new shell terminal of this project"
            };
            out.say(
                ui,
                muted(format!(
                    "Connecto will run this in {terminal}, and shows it so you can answer its questions. Come back here when it is done:"
                )),
            );
            out.say(ui, RichText::new(command).monospace().color(theme::TEXT));
            ui.horizontal_wrapped(|ui| {
                if out.button(ui, &format!("install-run:{key}"), "Run it", true) {
                    out.actions.push(ProjectSettingsAction::InstallRequirement {
                        package: owner.to_owned(),
                        requirement: req.id.clone(),
                        command: command.clone(),
                    });
                    dialog.confirming = None;
                }
                if out.button(ui, &format!("install-back:{key}"), "Back", true) {
                    dialog.confirming = None;
                }
            });
        } else {
            ui.horizontal_wrapped(|ui| {
                out.say(ui, RichText::new(command).monospace().color(theme::TEXT));
                if out.button(ui, &format!("install:{key}"), "Install…", true) {
                    dialog.confirming = Some((owner.to_owned(), req.id.clone(), command.clone()));
                }
            });
        }
    }
    if !req.manual.is_empty() {
        let lead = if req.install.is_some() {
            "Or by hand: "
        } else {
            ""
        };
        out.say(ui, muted(format!("{lead}{}", req.manual)));
    }
    if let Some(url) = &req.url {
        ui.hyperlink_to(url, url);
    }
}

/// Where a package stands in this project.
fn standing(installed: Option<&InstalledPackage>) -> (String, egui::Color32) {
    match installed.map(|p| (p.installed_version, p.available_version)) {
        Some((Some(have), Some(avail))) if have == avail => {
            (format!("installed v{have} · up to date"), theme::GREEN)
        }
        Some((Some(have), Some(avail))) => (
            format!("installed v{have} · v{avail} available"),
            theme::AMBER,
        ),
        Some((None, Some(avail))) => (
            format!("not installed yet · v{avail} available"),
            theme::AMBER,
        ),
        Some((_, None)) => ("this package no longer exists".to_owned(), theme::ERROR),
        None => ("not installed yet".to_owned(), theme::FAINT),
    }
}

fn added_section(
    ui: &mut Ui,
    view: &PackagesView,
    project: &ProjectPackagesView,
    next: &mut Option<ProjectSettings>,
    out: &mut Out<ProjectSettingsAction>,
) {
    out.say(ui, heading("ADDED PACKAGES"));
    if project.settings.packages.is_empty() {
        out.say(ui, muted("None. Add packages on top of the default one."));
    }
    for id in &project.settings.packages {
        let item: Option<&PackageItem> = view.packages.iter().find(|p| p.id == *id);
        let installed = project.status.packages.iter().find(|p| p.id == *id);
        ui.push_id(id, |ui| {
            card(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    let name = item.map_or(id.as_str(), |p| p.name.as_str());
                    out.say(ui, RichText::new(name).strong().color(theme::TEXT));
                    let (text, color) = if item.is_none() {
                        ("this package no longer exists".to_owned(), theme::ERROR)
                    } else {
                        standing(installed)
                    };
                    out.say(ui, RichText::new(text).size(11.5).color(color));
                    if out.button(ui, &format!("remove:{id}"), "Remove", true) {
                        let mut settings = project.settings.clone();
                        settings.packages.retain(|p| p != id);
                        *next = Some(settings);
                    }
                });
            });
        });
    }
    let addable: Vec<&PackageItem> = view
        .packages
        .iter()
        .filter(|p| !project.settings.packages.contains(&p.id))
        .collect();
    out.say(ui, muted("Add package"));
    if addable.is_empty() {
        out.say(ui, muted("Every package is already added."));
    }
    ui.horizontal_wrapped(|ui| {
        for item in addable {
            let label = format!("+ {}", item.name);
            if out.button(ui, &format!("add:{}", item.id), &label, true) {
                let mut settings = project.settings.clone();
                settings.packages.push(item.id.clone());
                *next = Some(settings);
            }
        }
    });
    forms::separator(ui);
}

/// A project with no terminal yet: say how it is used, then start its agents
/// from here.
fn start_section(ui: &mut Ui, start: &[ToolKind], out: &mut Out<ProjectSettingsAction>) {
    if start.is_empty() {
        return;
    }
    card(ui, |ui| {
        out.say(ui, heading("START"));
        out.say(
            ui,
            muted("This project has no terminal yet. Choose below which agents it uses and how each reads its configuration, then start one."),
        );
        ui.horizontal_wrapped(|ui| {
            for tool in start {
                let label = format!("Start {}", tool.label());
                if out.button(
                    ui,
                    &format!("start:{}", tool.label().to_lowercase()),
                    &label,
                    true,
                ) {
                    out.actions.push(ProjectSettingsAction::Start(*tool));
                }
            }
        });
    });
}

/// One checkbox per CLI, and All. One or more: the last one checked cannot
/// be unchecked.
fn tool_checkboxes(
    ui: &mut Ui,
    settings: &ProjectSettings,
    next: &mut Option<ProjectSettings>,
    out: &mut Out<ProjectSettingsAction>,
) {
    ui.horizontal(|ui| {
        for tool in ProjectTools::CHOICES {
            let text = tool.label();
            let mut on = settings.tools.allows(tool);
            let alone = on && settings.tools.len() == 1;
            let response = ui
                .add_enabled(!alone, egui::Checkbox::new(&mut on, text))
                .on_disabled_hover_text("A project uses at least one tool.");
            out.mark(&format!("tools:{text}"), response.rect);
            if response.changed()
                && let Some(tools) = settings.tools.with(tool, on)
            {
                let mut changed = settings.clone();
                changed.tools = tools;
                *next = Some(changed);
            }
        }
        if out.button(ui, "tools:All", "All", !settings.tools.is_all()) {
            let mut changed = settings.clone();
            changed.tools = ProjectTools::All;
            *next = Some(changed);
        }
    });
    out.say(
        ui,
        muted("Only the checked tools are offered for new terminals in this project, and only their folders are installed. At least one stays checked."),
    );
}

/// A host from before sets of tools saves one tool or all: those are the
/// choices, as that host offers them.
fn one_or_all_choices(
    ui: &mut Ui,
    settings: &ProjectSettings,
    next: &mut Option<ProjectSettings>,
    out: &mut Out<ProjectSettingsAction>,
) {
    ui.horizontal(|ui| {
        let choices = ProjectTools::CHOICES
            .map(|tool| (ProjectTools::from_tools([tool]), tool.label()))
            .into_iter()
            .filter_map(|(tools, text)| Some((tools?, text)))
            .chain([(ProjectTools::All, "All")]);
        for (tools, text) in choices {
            if out.choice(ui, &format!("tools:{text}"), settings.tools == tools, text)
                && settings.tools != tools
            {
                let mut changed = settings.clone();
                changed.tools = tools;
                *next = Some(changed);
            }
        }
    });
    out.say(
        ui,
        muted("Only the chosen tools are offered for new terminals in this project, and only their folders are installed. That machine's Connecto takes one tool or all of them: update it there to choose several."),
    );
}

fn tools_section(
    ui: &mut Ui,
    project: &ProjectPackagesView,
    // The project is on a host from before sets of tools: one or all.
    one_or_all: bool,
    // `None`: the project's model providers are set up on its own machine.
    mut providers: Option<&mut Option<ProvidersDialog>>,
    next: &mut Option<ProjectSettings>,
    out: &mut Out<ProjectSettingsAction>,
) {
    let settings = &project.settings;
    out.say(ui, heading("TOOLS"));
    if one_or_all {
        one_or_all_choices(ui, settings, next, out);
    } else {
        tool_checkboxes(ui, settings, next, out);
    }
    out.say(
        ui,
        muted("Pi works with the model providers set in Settings > Agents (OpenRouter, Together AI, a local LM Studio). When its config here is Isolated, this project can have providers and keys of its own instead: Set up model providers… under it. It reads AGENTS.md and the skills in .agents/skills."),
    );
    if settings.tools.allows(ToolKind::Antigravity) {
        out.say(
            ui,
            muted("Antigravity CLI always uses your own ~/.gemini folder and Google sign-in: it has no isolated config. Connecto adds one entry, named connecto, to ~/.gemini/config/hooks.json so its terminals report their turns, and one rule to ~/.gemini/antigravity-cli/settings.json so they can read the files you attach or paste. It reads AGENTS.md and GEMINI.md, and the skills in .agents/skills."),
        );
    }
    if settings.tools.allows(ToolKind::OpenCode) {
        out.say(
            ui,
            muted("OpenCode signs in to its model providers itself: type /connect in an OpenCode terminal. Isolated gives this project its own OpenCode settings, sign-ins and sessions; System default uses your own. Connecto writes nothing into OpenCode's folders: its plugin is loaded for its terminals only. It reads AGENTS.md (or CLAUDE.md) and the skills in .agents/skills, .claude/skills and .opencode/skills."),
        );
    }
    for (tool, label, mode) in [
        (ToolKind::Claude, "Claude", settings.claude_config),
        (ToolKind::Codex, "Codex", settings.codex_config),
        (ToolKind::Pi, "Pi", settings.pi_config),
        (ToolKind::OpenCode, "OpenCode", settings.opencode_config),
    ] {
        if !settings.tools.allows(tool) {
            continue;
        }
        ui.horizontal(|ui| {
            out.say(
                ui,
                RichText::new(format!("{label} config")).color(theme::TEXT),
            );
            for (value, text) in [
                (ConfigMode::System, "System default"),
                (ConfigMode::Isolated, "Isolated (advanced)"),
            ] {
                if out.choice(ui, &format!("{label}-config:{text}"), mode == value, text)
                    && mode != value
                {
                    let mut changed = settings.clone();
                    match tool {
                        ToolKind::Claude => changed.claude_config = value,
                        ToolKind::Codex => changed.codex_config = value,
                        ToolKind::Pi => changed.pi_config = value,
                        ToolKind::OpenCode => changed.opencode_config = value,
                        // Neither has a profile of its own to choose:
                        // Antigravity always runs on the user's own folder.
                        ToolKind::Antigravity | ToolKind::Shell => {}
                    }
                    *next = Some(changed);
                }
            }
        });
        if let Some(providers) = providers.as_deref_mut()
            && crate::providers::project_setup_offered(
                crate::providers::CLAUDE_SUPPORTED,
                tool,
                mode,
            )
            && out.button(
                ui,
                &format!("{}-providers-open", label.to_lowercase()),
                "Set up model providers…",
                true,
            )
        {
            *providers = Some(ProvidersDialog::new(
                connecto_core::ProviderScope::Project(project.project_id.clone()),
                tool,
            ));
        }
    }
    out.say(
        ui,
        muted("System default uses your own ~/.claude, ~/.codex or ~/.pi/agent, with the login you already have, and the model providers set in Settings > Agents. Isolated (advanced) keeps a separate profile and login for this project, and can have model providers of its own."),
    );
    forms::separator(ui);
}

fn status_section(
    ui: &mut Ui,
    project: &ProjectPackagesView,
    input: &ProjectSettingsInput<'_>,
    out: &mut Out<ProjectSettingsAction>,
) {
    let status = &project.status;
    out.say(ui, heading("STATUS"));
    ui.horizontal_wrapped(|ui| {
        let (word, color) = match &status.state {
            InstallState::None => ("Nothing installed".to_owned(), theme::FAINT),
            InstallState::Working => ("Working…".to_owned(), theme::AMBER),
            InstallState::UpToDate => ("Up to date".to_owned(), theme::GREEN),
            InstallState::Updated => ("Updated".to_owned(), theme::CYAN),
            InstallState::Error(why) => (format!("Error: {why}"), theme::ERROR),
        };
        out.say(ui, RichText::new(word).color(color));
        if let Some(ms) = status.checked_ms {
            let when = ago(ms, input.now_ms, input.utc_offset_s);
            out.say(ui, muted(format!("checked {when}")));
        }
        let working = status.state == InstallState::Working;
        if out.button(ui, "refresh", "Refresh", !working) {
            out.actions.push(ProjectSettingsAction::Refresh);
        }
    });
    if status.restart_needed {
        out.say(ui, warning("Restart terminals to apply."));
    }
    if !status.skipped_modified.is_empty() {
        out.say(ui, warning("Left alone because you edited them:"));
        for path in &status.skipped_modified {
            out.say(ui, warning(format!("  {path}")));
        }
    }
    for text in &status.warnings {
        out.say(ui, warning(text));
    }
}
