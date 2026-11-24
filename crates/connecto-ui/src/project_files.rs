//! The New Project dialog (a folder, and optionally a git repository cloned
//! into it), and the confirmation before a project is deleted with its folder.
//!
//! Both are cards over the window. The work itself (git, the trash) runs in
//! the workspace, off the UI thread; these follow it through a
//! [`ProjectTask`].

use std::path::PathBuf;
use std::sync::Arc;

use connecto_core::project_files::{Cloning, DeletePlan, ProjectTask, TaskState};
use egui::{Context, Id, RichText, TextEdit, Ui};

use crate::dialog;
use crate::forms::{Out, heading, muted, problem, warning};
use crate::theme;

// ---------- New Project ----------

#[derive(Default)]
pub(crate) struct NewProjectSheet {
    pub(crate) folder: Option<PathBuf>,
    pub(crate) url: String,
    pub(crate) cloning: Option<Cloning>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NewAction {
    /// Pick the folder.
    Choose,
    /// Open the folder, or clone the repository into it.
    Create,
    /// Close the dialog, or stop the clone under way.
    Cancel,
}

pub(crate) fn new_dialog(
    ctx: &Context,
    state: &mut NewProjectSheet,
    esc: bool,
    out: &mut Out<NewAction>,
) {
    if esc {
        out.actions.push(NewAction::Cancel);
        return;
    }
    dialog::card(ctx, "new-project-dialog", |ui| new_body(ui, state, out));
}

fn new_body(ui: &mut Ui, state: &mut NewProjectSheet, out: &mut Out<NewAction>) {
    let busy = state.cloning.is_some();
    out.say(ui, heading("NEW PROJECT"));
    out.say(ui, heading("FOLDER"));
    ui.horizontal_wrapped(|ui| {
        let shown = state.folder.as_ref().map_or_else(
            || "No folder chosen".to_owned(),
            |folder| folder.display().to_string(),
        );
        out.say(ui, RichText::new(shown).size(13.0).color(theme::TEXT));
        if out.button(ui, "new-project-choose", "Choose…", !busy) {
            out.actions.push(NewAction::Choose);
        }
    });
    ui.add_space(8.0);
    out.say(ui, heading("GIT REPOSITORY (OPTIONAL)"));
    let field = ui.add_enabled(
        !busy,
        TextEdit::singleline(&mut state.url)
            .id(Id::unique("new-project-url"))
            .hint_text("https://github.com/owner/project.git")
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    );
    out.mark("new-project-url", field.rect);
    out.say(
        ui,
        muted(
            "Leave it empty to open the folder as it is. With an address, the repository is cloned into a new folder inside the one chosen, named after the repository. Your own git does the clone, with your credential helper and SSH keys: Connecto never asks for or keeps a password.",
        ),
    );
    if let Some(cloning) = &state.cloning {
        ui.add_space(8.0);
        out.say(
            ui,
            RichText::new(format!("Cloning into {}", cloning.target.display()))
                .size(13.0)
                .strong()
                .color(theme::TEXT),
        );
        if let TaskState::Running(line) = cloning.task.state()
            && !line.is_empty()
        {
            out.say(ui, muted(line));
        }
    }
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        out.say(ui, problem(error.clone()));
    }
    ui.add_space(12.0);
    let cloning = !state.url.trim().is_empty();
    let ready = state.folder.is_some() && !busy;
    let enter = field.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter));
    ui.horizontal_wrapped(|ui| {
        let label = if cloning { "Clone and open" } else { "Open" };
        if out.button(ui, "new-project-create", label, ready) || (enter && ready) {
            out.actions.push(NewAction::Create);
        }
        let cancel = if busy { "Cancel the clone" } else { "Cancel" };
        if out.button(ui, "new-project-cancel", cancel, true) {
            out.actions.push(NewAction::Cancel);
        }
    });
}

// ---------- Delete Project ----------

pub(crate) enum PlanState {
    /// The folder is being looked at (git status, a count of the files).
    Loading(crossbeam_channel::Receiver<QueuedPlan>),
    Ready(DeletePlan),
    /// Refused, or failed after the project was closed: said, with OK only.
    Refused(String),
}

/// A plan on its way to the dialog. The workspace holds the plan's folder
/// open until it is used or given back, so a plan that is never taken (the
/// dialog closed first, the message dropped with the channel or never sent)
/// is given back when this is dropped.
pub(crate) struct QueuedPlan {
    result: Option<Result<DeletePlan, String>>,
    workspace: Arc<dyn connecto_core::WorkspaceControl>,
}

impl QueuedPlan {
    pub(crate) fn new(
        result: Result<DeletePlan, String>,
        workspace: Arc<dyn connecto_core::WorkspaceControl>,
    ) -> Self {
        Self {
            result: Some(result),
            workspace,
        }
    }

    /// Take the plan: from now on the dialog gives it back.
    pub(crate) fn take(mut self) -> Result<DeletePlan, String> {
        self.result
            .take()
            .unwrap_or_else(|| Err("The folder could not be checked.".to_owned()))
    }
}

impl Drop for QueuedPlan {
    fn drop(&mut self) {
        if let Some(Ok(plan)) = self.result.take() {
            self.workspace.release_delete_plan(&plan);
        }
    }
}

pub(crate) struct DeleteSheet {
    pub(crate) project: String,
    /// The name the sidebar shows.
    pub(crate) shown: String,
    pub(crate) plan: PlanState,
    /// What the person typed to confirm: the folder's name exactly (a name
    /// may end in a space, so nothing is trimmed).
    pub(crate) typed: String,
    pub(crate) task: Option<Arc<dyn ProjectTask>>,
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum DeleteAction {
    Delete,
    Keep,
}

impl DeleteSheet {
    /// Whether what was typed lets the deletion go ahead.
    pub(crate) fn confirmed(&self) -> bool {
        match &self.plan {
            PlanState::Ready(plan) => self.task.is_none() && self.typed == plan.name,
            _ => false,
        }
    }
}

pub(crate) fn delete_dialog(
    ctx: &Context,
    state: &mut DeleteSheet,
    esc: bool,
    out: &mut Out<DeleteAction>,
) {
    // Once it has started it runs to the end: the dialog stays to say how.
    if esc && state.task.is_none() {
        out.actions.push(DeleteAction::Keep);
        return;
    }
    dialog::card(ctx, "delete-project-dialog", |ui| {
        delete_body(ui, state, out);
    });
}

fn delete_body(ui: &mut Ui, state: &mut DeleteSheet, out: &mut Out<DeleteAction>) {
    out.say(ui, heading("DELETE PROJECT"));
    let plan = match &state.plan {
        PlanState::Loading(_) => {
            out.say(
                ui,
                RichText::new(format!("Checking {}…", state.shown))
                    .size(13.0)
                    .color(theme::TEXT),
            );
            ui.add_space(12.0);
            if out.button(ui, "delete-keep", "Cancel", true) {
                out.actions.push(DeleteAction::Keep);
            }
            return;
        }
        PlanState::Refused(why) => {
            out.say(ui, problem(why.clone()));
            ui.add_space(12.0);
            if out.button(ui, "delete-keep", "OK", true) {
                out.actions.push(DeleteAction::Keep);
            }
            return;
        }
        PlanState::Ready(plan) => plan.clone(),
    };
    out.say(
        ui,
        RichText::new(format!("Delete {} and its folder?", state.shown))
            .size(15.0)
            .strong()
            .color(theme::TEXT),
    );
    let what = format!(
        "This closes the project, stops its terminals and their background work, and moves this folder to {}, from where it can be put back:",
        plan.trash
    );
    out.say(ui, RichText::new(what).size(13.0).color(theme::TEXT));
    out.say(
        ui,
        RichText::new(plan.path.clone())
            .monospace()
            .size(12.5)
            .color(theme::TEXT),
    );
    out.say(
        ui,
        muted(format!("With everything in it: {}.", plan.contents)),
    );
    for line in &plan.warnings {
        out.say(ui, warning(line.clone()));
    }
    out.say(
        ui,
        muted("Connecto also forgets the project: it leaves the recent projects, and its terminal list and history are removed."),
    );
    ui.add_space(8.0);
    out.say(
        ui,
        RichText::new(format!(
            "Type the folder's name exactly, \"{}\", to confirm.",
            plan.name
        ))
        .size(13.0)
        .color(theme::TEXT),
    );
    let running = state.task.is_some();
    let field = ui.add_enabled(
        !running,
        TextEdit::singleline(&mut state.typed)
            .id(Id::unique("delete-confirm-name"))
            .hint_text(plan.name.as_str())
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    );
    out.mark("delete-confirm-name", field.rect);
    if let Some(task) = &state.task
        && let TaskState::Running(line) = task.state()
    {
        ui.add_space(8.0);
        out.say(ui, muted(line));
    }
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        out.say(ui, problem(error.clone()));
    }
    ui.add_space(12.0);
    let ready = state.confirmed();
    ui.horizontal_wrapped(|ui| {
        if out.button_hint(
            ui,
            "delete-confirm",
            "Move to Trash",
            ready,
            "Type the folder's name first.",
        ) {
            out.actions.push(DeleteAction::Delete);
        }
        if out.button(ui, "delete-keep", "Keep it", !running) {
            out.actions.push(DeleteAction::Keep);
        }
    });
}
