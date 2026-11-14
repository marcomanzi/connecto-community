//! The Project Settings sheet (gear in the sidebar): which packages a project
//! gets, which tools it uses and where each tool reads its configuration.

use connecto_core::{
    ConfigMode, InstallState, InstalledPackage, PackageItem, PackagesView, ProjectPackagesView,
    ProjectSettings, ProjectTools, ToolKind,
};
use egui::{Key, Rect, RichText, Ui};

use crate::forms::{self, Out, card, heading, muted, problem, warning};
use crate::theme;
use crate::utility::ago;

pub(crate) struct ProjectSettingsSheet {
    pub(crate) project: String,
    /// Why the last change was refused.
    pub(crate) error: Option<String>,
}

impl ProjectSettingsSheet {
    pub(crate) fn new(project: String) -> Self {
        Self {
            project,
            error: None,
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
}

pub(crate) struct ProjectSettingsInput<'a> {
    pub(crate) view: &'a PackagesView,
    pub(crate) project_name: &'a str,
    /// Settings submitted that the backend's view does not show yet. Edits
    /// build on these, so a second change never undoes the first.
    pub(crate) pending: Option<&'a ProjectSettings>,
    pub(crate) now_ms: u64,
    pub(crate) utc_offset_s: i32,
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
}

fn body(
    ui: &mut Ui,
    rect: Rect,
    state: &mut ProjectSettingsSheet,
    input: &ProjectSettingsInput<'_>,
    out: &mut Out<ProjectSettingsAction>,
) {
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        out.actions.push(ProjectSettingsAction::Close);
    }
    forms::column(ui, rect, |ui| {
        let Some(shown) = input.view.project(&state.project) else {
            ui.add_space(8.0);
            out.say(ui, muted("Loading the settings of this project…"));
            return;
        };
        // What the user chose outranks the view until the view catches up.
        let mut project = shown.clone();
        if let Some(set) = input.pending {
            project.settings = set.clone();
        }
        let project = &project;
        if let Some(error) = &state.error {
            out.say(ui, problem(error));
        }
        if let Some(error) = &input.view.last_error {
            out.say(ui, problem(format!("Last error: {error}")));
        }
        egui::ScrollArea::vertical()
            .id_salt("project-settings-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                let mut next = None;
                default_section(ui, input.view, project, &mut next, out);
                added_section(ui, input.view, project, &mut next, out);
                tools_section(ui, project, &mut next, out);
                status_section(ui, project, input, out);
                if let Some(settings) = next {
                    state.error = None;
                    out.actions.push(ProjectSettingsAction::Save(settings));
                }
            });
    });
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

fn tools_section(
    ui: &mut Ui,
    project: &ProjectPackagesView,
    next: &mut Option<ProjectSettings>,
    out: &mut Out<ProjectSettingsAction>,
) {
    let settings = &project.settings;
    out.say(ui, heading("TOOLS"));
    ui.horizontal(|ui| {
        for (tools, text) in [
            (ProjectTools::Claude, "Claude"),
            (ProjectTools::Codex, "Codex"),
            (ProjectTools::Both, "Both"),
        ] {
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
        muted("Only these tools are offered for new terminals in this project, and only their folders are installed."),
    );
    for (tool, label, mode) in [
        (ToolKind::Claude, "Claude", settings.claude_config),
        (ToolKind::Codex, "Codex", settings.codex_config),
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
                        // A shell has no profile of its own to choose.
                        ToolKind::Shell => {}
                    }
                    *next = Some(changed);
                }
            }
        });
    }
    out.say(
        ui,
        muted("System default uses your own ~/.claude or ~/.codex, with the login you already have. Isolated (advanced) keeps a separate profile and login for this project."),
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
