//! The Skills & Plugins sheet (Terminal menu): the packages in Connecto Home,
//! which one is the default, and creating, editing and deleting them.

use connecto_core::{PackageItem, PackagesView};
use egui::{Id, Key, Rect, RichText, TextEdit, Ui};

use crate::forms::{self, Out, card, heading, muted, problem};
use crate::theme::{self, FieldStyle as _};

#[derive(Default)]
pub(crate) struct SkillsSheet {
    /// The package whose Delete awaits its confirmation.
    confirm_delete: Option<String>,
    /// The New package form is open.
    creating: bool,
    /// The name field takes the keyboard once, when the form opens.
    focus_name: bool,
    name: String,
    extends: Option<String>,
    /// Why the last request was refused.
    pub(crate) error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum SkillsAction {
    Close,
    OpenSettings,
    SetDefault(Option<String>),
    Edit(String),
    Delete(String),
    Create(String, Option<String>),
}

fn name_id() -> Id {
    static ID: std::sync::OnceLock<Id> = std::sync::OnceLock::new();
    *ID.get_or_init(|| Id::unique("skills-new-name"))
}

pub(crate) fn sheet(
    ctx: &egui::Context,
    area: Rect,
    state: &mut SkillsSheet,
    view: &PackagesView,
    out: &mut Out<SkillsAction>,
) {
    let closed = crate::sheet::show(
        ctx,
        area,
        "skills-panel",
        "Skills & Plugins",
        |ui, content| {
            body(ui, content, state, view, out);
        },
    );
    if closed {
        out.actions.push(SkillsAction::Close);
    }
}

fn body(
    ui: &mut Ui,
    rect: Rect,
    state: &mut SkillsSheet,
    view: &PackagesView,
    out: &mut Out<SkillsAction>,
) {
    // Esc first cancels a confirmation or the form, and only then closes.
    if ui.input(|i| i.key_pressed(Key::Escape))
        && state.confirm_delete.take().is_none()
        && !std::mem::take(&mut state.creating)
    {
        out.actions.push(SkillsAction::Close);
    }
    forms::column(ui, rect, |ui| {
        if view.home.is_none() {
            empty_home(ui, out);
            errors(ui, state, view, out);
            return;
        }
        top(ui, state, view, out);
        errors(ui, state, view, out);
        if state.creating {
            new_form(ui, state, view, out);
        }
        forms::separator(ui);
        egui::ScrollArea::vertical()
            .id_salt("skills-scroll")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                if view.packages.is_empty() {
                    out.say(ui, muted("No packages yet. Create one with New package…."));
                }
                for item in &view.packages {
                    ui.push_id(&item.id, |ui| package_card(ui, state, item, out));
                }
            });
    });
}

fn errors(ui: &mut Ui, state: &SkillsSheet, view: &PackagesView, out: &mut Out<SkillsAction>) {
    if let Some(error) = &state.error {
        out.say(ui, problem(error));
    }
    if let Some(error) = &view.last_error {
        out.say(ui, problem(format!("Last error: {error}")));
    }
}

fn empty_home(ui: &mut Ui, out: &mut Out<SkillsAction>) {
    ui.add_space(8.0);
    out.say(
        ui,
        RichText::new("Connecto Home is not set")
            .size(15.0)
            .color(theme::TEXT),
    );
    out.say(
        ui,
        muted(
            "Packages (skills and plugins) live in Connecto Home. Choose that folder in Settings first.",
        ),
    );
    ui.add_space(6.0);
    if out.button(ui, "open-settings", "Open Settings…", true) {
        out.actions.push(SkillsAction::OpenSettings);
    }
}

fn top(ui: &mut Ui, state: &mut SkillsSheet, view: &PackagesView, out: &mut Out<SkillsAction>) {
    let default = view
        .default_package
        .as_deref()
        .and_then(|id| view.packages.iter().find(|p| p.id == id));
    ui.horizontal_wrapped(|ui| {
        let line = default.map_or_else(
            || "Default package: none".to_owned(),
            |p| format!("Default package: {} v{}", p.name, p.version),
        );
        out.say(ui, RichText::new(line).color(theme::TEXT));
        if out.button(ui, "new-package", "New package…", !state.creating) {
            state.creating = true;
            state.focus_name = true;
            state.error = None;
        }
    });
    out.say(
        ui,
        muted("A default package is installed into every project that uses it. Projects can add more in their settings (gear in the sidebar)."),
    );
}

fn new_form(
    ui: &mut Ui,
    state: &mut SkillsSheet,
    view: &PackagesView,
    out: &mut Out<SkillsAction>,
) {
    card(ui, |ui| {
        out.say(ui, heading("NEW PACKAGE"));
        let field = ui.add(
            TextEdit::singleline(&mut state.name)
                .padded()
                .id(name_id())
                .hint_text("Name, e.g. Rust tools")
                .desired_width(f32::INFINITY),
        );
        out.mark("field:name", field.rect);
        if std::mem::take(&mut state.focus_name) {
            field.request_focus();
        }
        out.say(ui, muted("Extends (installed first, optional)"));
        ui.horizontal_wrapped(|ui| {
            if out.choice(ui, "extends:none", state.extends.is_none(), "None") {
                state.extends = None;
            }
            for item in &view.packages {
                let on = state.extends.as_deref() == Some(item.id.as_str());
                if out.choice(ui, &format!("extends:{}", item.id), on, &item.name) {
                    state.extends = Some(item.id.clone());
                }
            }
        });
        let ready = !state.name.trim().is_empty();
        let submit = field.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
        ui.horizontal(|ui| {
            let create = out.button(ui, "create", "Create", ready);
            if ready && (create || submit) {
                out.actions.push(SkillsAction::Create(
                    state.name.trim().to_owned(),
                    state.extends.clone(),
                ));
                state.error = None;
            }
            if out.button(ui, "cancel-create", "Cancel", true) {
                state.creating = false;
            }
        });
        out.say(
            ui,
            muted("A Claude terminal opens in Connecto Home to ask which skills and plugins to include."),
        );
    });
}

fn package_card(
    ui: &mut Ui,
    state: &mut SkillsSheet,
    item: &PackageItem,
    out: &mut Out<SkillsAction>,
) {
    card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            out.say(
                ui,
                RichText::new(&item.name)
                    .size(13.5)
                    .strong()
                    .color(theme::TEXT),
            );
            out.say(
                ui,
                RichText::new(format!("v{}", item.version))
                    .monospace()
                    .size(11.0)
                    .color(theme::FAINT),
            );
            if item.is_default {
                out.pill(ui, "Default", theme::CYAN);
            }
            if item.builtin {
                out.pill(ui, "Built-in", theme::FAINT);
            }
            if let Some(base) = &item.extends {
                out.pill(ui, &format!("extends {base}"), theme::TEXT_MUTED);
            }
            if item.has_claude {
                out.pill(ui, "Claude", theme::GREEN);
            }
            if item.has_codex {
                out.pill(ui, "Codex", theme::GREEN);
            }
        });
        if !item.description.is_empty() {
            out.say(ui, muted(&item.description));
        }
        for error in &item.errors {
            out.say(ui, problem(format!("Error: {error}")));
        }
        ui.horizontal_wrapped(|ui| {
            let id = &item.id;
            if item.is_default {
                if out.button(ui, &format!("no-default:{id}"), "No default", true) {
                    state.error = None;
                    out.actions.push(SkillsAction::SetDefault(None));
                }
            } else if out.button(ui, &format!("default:{id}"), "Set as default", true) {
                state.error = None;
                out.actions.push(SkillsAction::SetDefault(Some(id.clone())));
            }
            let why = if item.builtin {
                "The built-in package cannot be changed"
            } else {
                ""
            };
            if out.button_hint(ui, &format!("edit:{id}"), "Edit", !item.builtin, why) {
                state.error = None;
                out.actions.push(SkillsAction::Edit(id.clone()));
            }
            if out.button_hint(ui, &format!("delete:{id}"), "Delete", !item.builtin, why) {
                state.confirm_delete = Some(id.clone());
            }
        });
        if state.confirm_delete.as_deref() == Some(item.id.as_str()) {
            out.say(
                ui,
                problem(format!(
                    "Delete {} and its folder? Projects keep the files it installed until their next update.",
                    item.name
                )),
            );
            ui.horizontal_wrapped(|ui| {
                if out.button(ui, &format!("confirm-delete:{}", item.id), "Delete", true) {
                    state.error = None;
                    out.actions.push(SkillsAction::Delete(item.id.clone()));
                    state.confirm_delete = None;
                }
                if out.button(ui, &format!("cancel-delete:{}", item.id), "Keep", true) {
                    state.confirm_delete = None;
                }
            });
        }
    });
}
