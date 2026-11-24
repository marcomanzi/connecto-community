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
    pub(crate) message: std::borrow::Cow<'static, str>,
}

impl Notice {
    /// Creating a project while a remote machine is shown: not built yet.
    pub(crate) fn remote_project() -> Self {
        Self {
            title: "NEW PROJECT",
            message: "Project creation on remote machines is still in development".into(),
        }
    }

    /// Paired machines changed secrets here: what each did, never a value.
    pub(crate) fn remote_secret_changes(lines: &[String]) -> Self {
        Self {
            title: "SECRETS CHANGED FROM A PAIRED MACHINE",
            // Every one, however many: the notice scrolls.
            message: lines.join("\n").into(),
        }
    }

    /// A command about this machine's projects (history, settings, names,
    /// closing), asked while a remote host is shown.
    pub(crate) fn remote_local_only() -> Self {
        Self {
            title: "REMOTE PROJECT",
            message: "This works on this machine's projects. A remote project is kept on its own machine.".into(),
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
    // A long message (many changes from a paired machine) scrolls, so
    // every line stays readable and the OK button stays in reach.
    egui::ScrollArea::vertical()
        .max_height(ui.ctx().content_rect().height() * 0.6)
        .auto_shrink([false, true])
        .show(ui, |ui| {
            out.say(
                ui,
                RichText::new(state.message.as_ref())
                    .size(15.0)
                    .strong()
                    .color(theme::TEXT),
            );
        });
    ui.add_space(12.0);
    if out.button(ui, "notice-ok", "OK", true) {
        out.actions.push(NoticeAction::Ok);
    }
}
