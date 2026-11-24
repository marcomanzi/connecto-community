//! Settings: every setting of the memory in one scrolling form, in plain
//! sections. The master switch is at the top of the section, on every tab.
//! Export and delete are here too, as the last two sections.

use egui::Ui;

use super::{Cx, MemoryAction, MemoryPanel, cycles, overview};
use crate::forms::{self, Out, muted};

pub(super) fn show(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    out.say(
        ui,
        muted("Changes apply from the next run. Each setting says what it does in a short line."),
    );
    forms::separator(ui);
    cycles::show(ui, panel, cx, out);
    forms::separator(ui);
    overview::export(ui, panel, cx, out);
    forms::separator(ui);
    overview::delete(ui, panel, out);
}
