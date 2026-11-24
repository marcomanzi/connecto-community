//! The last report: what the latest dream or undo refused, and what it kept in
//! a weaker form, and the memories it lowered. Shown under the run it belongs
//! to, in plain text. Why git is not used (or only partly) is shown here too.

use connecto_core::memory::{DreamReport, Lookup, ReadRequest, Subject};
use egui::{RichText, Ui};

use super::{Cx, MemoryAction, MemoryPanel, Slot, Tab};
use crate::forms::{self, Out, heading, muted};
use crate::theme;

/// Whether the report has anything to show beyond its run.
pub(super) fn has_entries(report: &DreamReport) -> bool {
    !(report.rejected.is_empty() && report.adjusted.is_empty() && report.lowered.is_empty())
}

/// The report's details: each refusal and each weaker form, as plain lines,
/// then the lowered memories as links that open them.
pub(super) fn details(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    report: &DreamReport,
    out: &mut Out<MemoryAction>,
) {
    if report.rejected.is_empty() && report.adjusted.is_empty() && report.lowered.is_empty() {
        out.say(ui, muted("Nothing was refused, weakened or lowered."));
        return;
    }
    if !report.rejected.is_empty() {
        out.say(
            ui,
            RichText::new(format!("Refused ({})", report.rejected.len()))
                .strong()
                .color(theme::TEXT),
        );
        for sentence in &report.rejected {
            entry(
                ui,
                sentence,
                ("Proposed", "Caught at", "Refused because"),
                out,
            );
        }
    }
    if !report.adjusted.is_empty() {
        out.say(
            ui,
            RichText::new(format!("Kept in a weaker form ({})", report.adjusted.len()))
                .strong()
                .color(theme::TEXT),
        );
        for sentence in &report.adjusted {
            entry(ui, sentence, ("Changed", "Changed", "Why"), out);
        }
    }
    if !report.lowered.is_empty() {
        out.say(ui, RichText::new("Lowered").strong().color(theme::TEXT));
        ui.horizontal_wrapped(|ui| {
            for subject in &report.lowered {
                let key = format!("mem-report-open:{subject}");
                if out.button(ui, &key, &format!("Open {subject}"), true) {
                    let lookup = match subject {
                        Subject::Memory(id) => Lookup::Memory(id.clone()),
                        Subject::Annotation(id) => Lookup::Annotation(id.clone()),
                    };
                    out.actions
                        .push(MemoryAction::Read(ReadRequest::Open(lookup), Slot::Detail));
                    panel.tab = Tab::Memories;
                }
            }
        });
    }
}

/// One entry as two plain lines. The engine words each as "place: reason"; an
/// entry without a colon is shown whole. `labels` are: the place's word for a
/// proposed operation, the place's word for a check that caught it, and the
/// word for the reason.
fn entry(ui: &mut Ui, sentence: &str, labels: (&str, &str, &str), out: &mut Out<MemoryAction>) {
    match sentence.split_once(": ") {
        Some((place, why)) => {
            let place_word = if sentence.contains("operation ") {
                labels.0
            } else {
                labels.1
            };
            out.say(
                ui,
                RichText::new(format!("{place_word}: {place}"))
                    .size(12.0)
                    .color(theme::TEXT),
            );
            out.say(
                ui,
                RichText::new(format!("{}: {why}", labels.2))
                    .size(12.0)
                    .color(theme::TEXT_ROW),
            );
        }
        None => out.say(
            ui,
            RichText::new(sentence).size(12.0).color(theme::TEXT_ROW),
        ),
    }
    ui.add_space(4.0);
}

/// The last report, when its run is not in the runs list: a heading and its
/// details.
pub(super) fn standalone(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    report: &DreamReport,
    out: &mut Out<MemoryAction>,
) {
    out.say(ui, heading("LAST REPORT"));
    out.say(
        ui,
        muted(format!(
            "Run #{} · what it refused or kept in a weaker form",
            report.run
        )),
    );
    details(ui, panel, report, out);
    forms::separator(ui);
}

/// Why git is not used, or only partly, for this project, in plain words.
pub(super) fn git_note(ui: &mut Ui, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let Some(note) = &cx.project.git_note else {
        return;
    };
    out.say(ui, heading("GIT"));
    out.say(
        ui,
        RichText::new(note.clone())
            .size(12.0)
            .color(theme::TEXT_ROW),
    );
}
