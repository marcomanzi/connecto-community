//! Activity: what the memory has done, in plain words. The state in one line,
//! what needs attention, Dream now, the initial build, the runs (each with its
//! refusals when it had any), the versions to undo, and what is waiting. Only
//! a build's own start options are set here; the settings are in Settings.

use egui::Ui;

use super::{Cx, MemoryAction, MemoryPanel, build, overview, runs};
use crate::forms::{self, Out, heading};

pub(super) fn show(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let stops = super::stops(cx);
    if !stops.is_empty() {
        out.say(ui, heading("NEEDS ATTENTION"));
        for (stop, what, reason) in &stops {
            super::held(ui, out, *stop, what, reason);
        }
        forms::separator(ui);
    }
    overview::summary(ui, cx, out);
    ui.add_space(4.0);
    overview::dream_now(ui, cx, out);
    forms::separator(ui);
    build::show(ui, panel, cx, out);
    forms::separator(ui);
    runs::show(ui, panel, cx, out);
    forms::separator(ui);
    overview::freshness(ui, cx, out);
    if cx.project.git_note.is_some() {
        forms::separator(ui);
        super::report::git_note(ui, cx, out);
    }
}
