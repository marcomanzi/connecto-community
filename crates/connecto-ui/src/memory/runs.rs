//! Runs, skipped dream dates and published versions, with "undo version".
//! Only measured quantities: runs, characters in and out, durations (§8.4).

use connecto_core::memory::{Cycle, RunEntry, RunOutcome, VersionEntry};
use egui::{RichText, Ui};

use super::{Confirm, Cx, MemoryAction, MemoryPanel, text};
use crate::forms::{self, Out, card, heading, muted, warning};
use crate::theme;

pub(super) fn show(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    schedule(ui, cx, out);
    forms::separator(ui);
    out.say(ui, heading("RUNS"));
    if cx.project.runs.is_empty() {
        out.say(ui, muted("No runs yet."));
    }
    let report = cx.project.last_report.as_deref();
    for run in cx.project.runs.iter() {
        ui.push_id(("run", run.id), |ui| {
            run_row(ui, panel, run, report, cx, out)
        });
    }
    // A report whose run is not in the list is still shown, on its own.
    if let Some(report) = report.filter(|r| !cx.project.runs.iter().any(|run| run.id == r.run))
        && super::report::has_entries(report)
    {
        super::report::standalone(ui, panel, report, out);
    }
    forms::separator(ui);
    versions(ui, panel, cx, out);
}

fn schedule(ui: &mut Ui, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    out.say(ui, heading("SCHEDULE"));
    let p = cx.project;
    match p.next_dream_ms {
        Some(ms) => out.say(
            ui,
            RichText::new(format!(
                "Next dream {}",
                text::ahead(ms, cx.now_ms, cx.offset_s)
            ))
            .color(theme::TEXT),
        ),
        None if cx.settings.dream.days.is_empty() => {
            out.say(ui, muted("No dream scheduled: no days are chosen."));
        }
        None => out.say(ui, muted("No dream scheduled yet.")),
    }
    out.say(
        ui,
        warning("Nothing runs while Connecto is closed. A missed date runs once, as a catch-up, when it opens again."),
    );
    for c in &p.cycles {
        if c.running {
            out.say(
                ui,
                RichText::new(format!("{} cycle running now", c.cycle.label())).color(theme::CYAN),
            );
        }
    }
    if p.skipped.is_empty() {
        return;
    }
    out.say(ui, RichText::new("Skipped dream dates").color(theme::TEXT));
    for skipped in &p.skipped {
        let line = match skipped.recovered_by {
            Some(run) => format!("{} · recovered by run #{run}", skipped.date),
            None => format!("{} · not recovered yet", skipped.date),
        };
        out.say(ui, muted(line));
    }
}

/// One run as plain lines: what it was, when, and what came of it. Its
/// refusals and weaker forms, when it had any, open on request.
fn run_row(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    run: &RunEntry,
    report: Option<&connecto_core::memory::DreamReport>,
    cx: &Cx<'_>,
    out: &mut Out<MemoryAction>,
) {
    ui.horizontal_wrapped(|ui| {
        out.say(
            ui,
            RichText::new(format!("#{} {}", run.id, run.cycle.label())).color(theme::TEXT),
        );
        out.say(
            ui,
            muted(format!(
                "· {}",
                crate::utility::ago(run.started_ms, cx.now_ms, cx.offset_s)
            )),
        );
        let (word, color) = text::outcome(&run.outcome);
        out.say(
            ui,
            RichText::new(format!("· {word}")).size(12.0).color(color),
        );
    });
    let took = match (run.duration_ms, &run.outcome) {
        (Some(ms), _) => format!("took {}", text::duration(ms)),
        (None, RunOutcome::Running) => format!(
            "running for {}",
            text::duration(cx.now_ms.saturating_sub(run.started_ms))
        ),
        (None, _) => "duration unknown".to_owned(),
    };
    out.say(
        ui,
        muted(format!(
            "{} characters in · {} characters out · {took} · {}",
            text::count(run.chars_in),
            text::count(run.chars_out),
            run.choice
        )),
    );
    if !run.recovers.is_empty() {
        let dates: Vec<String> = run.recovers.iter().map(ToString::to_string).collect();
        out.say(
            ui,
            muted(format!(
                "Also stands for the skipped dates {}",
                dates.join(", ")
            )),
        );
    }
    if let Some(report) = report.filter(|r| r.run == run.id && super::report::has_entries(r)) {
        let open = panel.open_runs.contains(&run.id);
        let label = if open { "Hide details" } else { "Details" };
        if out.button(ui, &format!("mem-run-details:{}", run.id), label, true) {
            if open {
                panel.open_runs.remove(&run.id);
            } else {
                panel.open_runs.insert(run.id);
            }
        }
        if panel.open_runs.contains(&run.id) {
            ui.indent(("run-details", run.id), |ui| {
                super::report::details(ui, panel, report, out);
            });
        }
    }
    ui.add_space(8.0);
}

fn versions(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    out.say(ui, heading("VERSIONS"));
    out.say(
        ui,
        muted("Undoing a version restores what it changed, as a new version. Notes that arrived after it are kept."),
    );
    if cx.project.versions.is_empty() {
        out.say(ui, muted("No version published yet."));
    }
    let busy =
        cx.project.initializing() || cx.project.cycle(Cycle::Dream).is_some_and(|c| c.running);
    for v in cx.project.versions.iter() {
        ui.push_id(("version", v.version), |ui| {
            version_row(ui, panel, v, busy, cx, out);
        });
    }
}

fn version_row(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    v: &VersionEntry,
    busy: bool,
    cx: &Cx<'_>,
    out: &mut Out<MemoryAction>,
) {
    ui.horizontal_wrapped(|ui| {
        out.say(
            ui,
            RichText::new(format!("Version {}", v.version)).color(theme::TEXT),
        );
        let mut what = format!(
            "{} · {} operation{} · {}",
            v.cycle.label(),
            v.operations,
            if v.operations == 1 { "" } else { "s" },
            crate::utility::ago(v.published_ms, cx.now_ms, cx.offset_s)
        );
        if let Some(undone) = v.reverts {
            what.push_str(&format!(" · undoes version {undone}"));
        }
        out.say(ui, muted(what));
        let key = format!("mem-revert:{}", v.version);
        let why = if busy {
            "Wait for the running dream or build to finish."
        } else {
            ""
        };
        if out.button_hint(ui, &key, "Undo…", !busy, why) {
            panel.confirm = Some(Confirm::Revert(v.version));
        }
    });
    if panel.confirm == Some(Confirm::Revert(v.version)) {
        card(ui, |ui| {
            out.say(
                ui,
                RichText::new(format!("Undo version {}?", v.version))
                    .strong()
                    .color(theme::TEXT),
            );
            out.say(
                ui,
                RichText::new("What it changed goes back as it was, in a new version. Notes it absorbed open again; notes that arrived after it are not touched.")
                    .size(12.0)
                    .color(theme::TEXT),
            );
            ui.horizontal_wrapped(|ui| {
                let yes = ui.add(theme::destructive(&format!("Undo version {}", v.version)));
                out.mark("mem-revert-confirm", yes.rect);
                if yes.clicked() {
                    out.actions.push(MemoryAction::Revert(v.version));
                }
                if out.button(ui, "mem-revert-keep", "Keep it", true) {
                    panel.confirm = None;
                }
            });
        });
    }
}
