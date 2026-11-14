//! The confirmation before a machine's pairing is removed, or a host forgotten.
//! Both say the same thing: the connection ends now and the record goes.

use egui::{Context, RichText, Ui};

use crate::dialog;
use crate::forms::{Out, heading, problem};
use crate::theme;

/// What is removed when the user confirms.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    /// A paired machine, by its id: `disconnect_machine`.
    Machine(String),
    /// A saved host, by its name: `forget_host`.
    Host(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Confirm {
    pub(crate) name: String,
    pub(crate) target: Target,
    /// Why the removal was refused. The dialog stays open to say so.
    pub(crate) error: Option<String>,
}

impl Confirm {
    pub(crate) fn machine(id: &str, name: &str) -> Self {
        Self {
            name: name.to_owned(),
            target: Target::Machine(id.to_owned()),
            error: None,
        }
    }

    pub(crate) fn host(name: &str) -> Self {
        Self {
            name: name.to_owned(),
            target: Target::Host(name.to_owned()),
            error: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ConfirmAction {
    /// Remove it.
    Confirm,
    /// Keep it.
    Keep,
}

pub(crate) fn dialog(ctx: &Context, state: &Confirm, esc: bool, out: &mut Out<ConfirmAction>) {
    if esc {
        out.actions.push(ConfirmAction::Keep);
        return;
    }
    dialog::card(ctx, "confirm-dialog", |ui| body(ui, state, out));
}

/// The wording both removals share.
pub(crate) fn explanation(name: &str) -> String {
    format!(
        "This ends any connection with {name} now and removes the pairing: its key, its saved host and the key kept for it in Connecto's {}. To connect again, pair the two machines again. {name} is not told, and its next attempt is refused.",
        crate::platform::KEY_STORE
    )
}

fn body(ui: &mut Ui, state: &Confirm, out: &mut Out<ConfirmAction>) {
    out.say(ui, heading("REMOVE THIS MACHINE"));
    out.say(
        ui,
        RichText::new(format!("Remove {}?", state.name))
            .size(15.0)
            .strong()
            .color(theme::TEXT),
    );
    out.say(
        ui,
        RichText::new(explanation(&state.name))
            .size(13.0)
            .color(theme::TEXT),
    );
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        out.say(ui, problem(error.clone()));
    }
    ui.add_space(12.0);
    ui.horizontal_wrapped(|ui| {
        if out.button(ui, "confirm-remove", "Remove and disconnect", true) {
            out.actions.push(ConfirmAction::Confirm);
        }
        if out.button(ui, "confirm-keep", "Keep it", true) {
            out.actions.push(ConfirmAction::Keep);
        }
    });
}
