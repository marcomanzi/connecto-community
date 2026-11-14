//! A notice that says why an action is not done, with one OK button. The
//! person reads it and dismisses it; nothing else changes.

use egui::{Context, RichText, Ui};

use crate::dialog;
use crate::forms::{Out, heading};
use crate::theme;

/// The message shown, and the heading above it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Notice {
    pub(crate) title: &'static str,
    pub(crate) message: &'static str,
}

impl Notice {
    /// Creating a project while a remote machine is shown: not built yet.
    pub(crate) fn remote_project() -> Self {
        Self {
            title: "NEW PROJECT",
            message: "Project creation on remote machines is still in development",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum NoticeAction {
    /// Dismiss it.
    Ok,
}

pub(crate) fn dialog(ctx: &Context, state: &Notice, esc: bool, out: &mut Out<NoticeAction>) {
    if esc {
        out.actions.push(NoticeAction::Ok);
        return;
    }
    dialog::card(ctx, "notice-dialog", |ui| body(ui, state, out));
}

fn body(ui: &mut Ui, state: &Notice, out: &mut Out<NoticeAction>) {
    out.say(ui, heading(state.title));
    out.say(
        ui,
        RichText::new(state.message)
            .size(15.0)
            .strong()
            .color(theme::TEXT),
    );
    ui.add_space(12.0);
    if out.button(ui, "notice-ok", "OK", true) {
        out.actions.push(NoticeAction::Ok);
    }
}
