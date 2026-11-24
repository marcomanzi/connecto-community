//! The overview: what the memory holds (status), how up to date it is
//! (freshness and backlog, kept apart from status), and the actions.

use connecto_core::memory::{Cycle, ExportFormat};
use egui::{RichText, Ui};

use super::{Confirm, Cx, MemoryAction, MemoryPanel, text};
use crate::forms::{self, Out, heading, muted, problem, warning};
use crate::theme;

/// One line on the state of the memory, in plain words: what it holds, when
/// the last run was and what it kept, and what waits.
pub(super) fn summary(ui: &mut Ui, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let p = cx.project;
    let version = if p.version == 0 {
        "Nothing published yet".to_owned()
    } else {
        format!("Version {}", p.version)
    };
    let mut line = format!("{version} · {} memories", p.totals.total());
    if p.totals.to_verify > 0 {
        line.push_str(&format!(" · {} to verify", p.totals.to_verify));
    }
    if p.totals.conflict > 0 {
        line.push_str(&format!(" · {} in conflict", p.totals.conflict));
    }
    if p.open_annotations > 0 {
        line.push_str(&format!(" · {} open notes", p.open_annotations));
    }
    out.say(ui, RichText::new(line).size(13.0).color(theme::TEXT));
    let last = p
        .cycles
        .iter()
        .filter_map(|c| c.last.as_ref())
        .max_by_key(|r| (r.started_ms, r.id));
    match last {
        Some(run) => {
            let (word, color) = text::outcome(&run.outcome);
            out.say(
                ui,
                RichText::new(format!(
                    "Last run {}: {word}",
                    crate::utility::ago(run.started_ms, cx.now_ms, cx.offset_s)
                ))
                .size(12.0)
                .color(color),
            );
        }
        None => out.say(ui, muted("No run yet.")),
    }
    if p.open_contradictions > 0 {
        out.say(
            ui,
            problem(format!(
                "{} open contradictions: memories that disagree, shown in the Memories tab",
                p.open_contradictions
            )),
        );
    }
    if p.backlog.queued > 0 {
        out.say(
            ui,
            warning(format!(
                "{} events waiting to be considered",
                p.backlog.queued
            )),
        );
    }
}

/// What deleting memory does not reach.
pub(super) const DELETE_LEAVES: &str = "Memory runs also leave their transcripts in the CLI's own session logs (Claude, Codex). Deleting the memory does not remove those.";

/// Memory is off: say so, and keep export and delete, since notes, evidence
/// or queued events may be kept even with nothing published.
pub(super) fn disabled(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    cx: &Cx<'_>,
    out: &mut Out<MemoryAction>,
) {
    out.say(
        ui,
        RichText::new("Memory is off for this project.")
            .size(12.5)
            .color(theme::TEXT_MUTED),
    );
    out.say(
        ui,
        muted(format!(
            "Capture and both cycles are stopped. What is kept stays: version {}, {} memories, {} open notes, {} queued events.",
            cx.project.version,
            cx.project.totals.total(),
            cx.project.open_annotations,
            cx.project.backlog.queued
        )),
    );
    forms::separator(ui);
    export(ui, panel, cx, out);
    forms::separator(ui);
    delete(ui, panel, out);
}
/// Dream now, when no dream or build is running.
pub(super) fn dream_now(ui: &mut Ui, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let dreaming = cx.project.cycle(Cycle::Dream).is_some_and(|c| c.running);
    let building = cx.project.initializing();
    let why_not = if dreaming {
        "A dream is running."
    } else if building {
        "The initial build is in progress."
    } else {
        ""
    };
    ui.horizontal_wrapped(|ui| {
        if out.button_hint(
            ui,
            "mem-dream-now",
            "Dream now",
            why_not.is_empty(),
            why_not,
        ) {
            out.actions.push(MemoryAction::DreamNow);
        }
        out.say(
            ui,
            muted(
                "Consolidates notes and evidence into memories now, as a new version you can undo.",
            ),
        );
    });
}

/// How up to date the memory is: not how reliable (spec §12.6).
pub(super) fn freshness(ui: &mut Ui, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let p = cx.project;
    let b = &p.backlog;
    out.say(ui, heading("FRESHNESS AND BACKLOG"));
    out.say(
        ui,
        muted("How up to date the memory is with your work. This is not its reliability."),
    );
    out.say(
        ui,
        RichText::new(match p.considered_through_ms {
            Some(ms) => format!(
                "Considered work through {}",
                crate::utility::ago(ms, cx.now_ms, cx.offset_s)
            ),
            None => "No work considered yet".to_owned(),
        })
        .color(theme::TEXT),
    );
    let queued = match b.oldest_queued_ms {
        Some(ms) if b.queued > 0 => format!(
            "{} events waiting · oldest {} old",
            b.queued,
            text::age(ms, cx.now_ms)
        ),
        _ => format!("{} events waiting", b.queued),
    };
    let color = if b.queued > 0 {
        theme::AMBER
    } else {
        theme::TEXT_ROW
    };
    out.say(ui, RichText::new(queued).color(color));
    if b.failed_batches > 0 {
        out.say(
            ui,
            problem(format!(
                "{} batches failed after every attempt",
                b.failed_batches
            )),
        );
    } else {
        out.say(ui, muted("No failed batches"));
    }
    if b.dropped_on_full > 0 {
        out.say(
            ui,
            warning(format!(
                "{} events lost because the queue from the terminals was full (since Connecto started)",
                b.dropped_on_full
            )),
        );
    }
    if let Some(why) = &b.last_failure {
        out.say(ui, problem(format!("Last failure: {why}")));
    }
    for cycle in [Cycle::Development, Cycle::Dream] {
        let Some(state) = p.cycle(cycle) else {
            continue;
        };
        let line = if state.running {
            format!("{}: running", cycle.label())
        } else if let Some(last) = &state.last {
            format!(
                "{}: last run {} · {}",
                cycle.label(),
                crate::utility::ago(last.started_ms, cx.now_ms, cx.offset_s),
                text::outcome(&last.outcome).0
            )
        } else {
            format!("{}: no run yet", cycle.label())
        };
        out.say(ui, muted(line));
    }
    if let Some(ms) = p.next_dream_ms {
        out.say(
            ui,
            muted(format!(
                "Next dream {} (only while Connecto is open)",
                text::ahead(ms, cx.now_ms, cx.offset_s)
            )),
        );
    }
}
pub(super) fn export(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    cx: &Cx<'_>,
    out: &mut Out<MemoryAction>,
) {
    out.say(ui, heading("EXPORT"));
    ui.horizontal_wrapped(|ui| {
        for format in ExportFormat::ALL {
            let key = match format {
                ExportFormat::Json => "mem-export:json",
                ExportFormat::Markdown => "mem-export:markdown",
                ExportFormat::Both => "mem-export:both",
            };
            if out.choice(ui, key, panel.export == *format, format.label()) {
                panel.export = *format;
            }
        }
        if out.button(ui, "mem-export", "Export", true) {
            out.actions.push(MemoryAction::Export(panel.export));
        }
    });
    out.say(
        ui,
        muted("Written to Connecto's data folder for this project, never into the repository."),
    );
    if let Some(last) = &cx.project.last_export {
        let when = crate::utility::ago(last.at_ms, cx.now_ms, cx.offset_s);
        match &last.result {
            Ok(paths) => {
                out.say(
                    ui,
                    RichText::new(format!("Exported {} {when}:", last.format.label()))
                        .size(11.5)
                        .color(theme::GREEN),
                );
                for path in paths {
                    out.say(ui, muted(format!("  {}", path.display())));
                }
            }
            Err(why) => out.say(ui, problem(format!("Export failed {when}: {why}"))),
        }
    }
}

pub(super) fn delete(ui: &mut Ui, panel: &mut MemoryPanel, out: &mut Out<MemoryAction>) {
    out.say(ui, heading("DELETE"));
    if panel.confirm == Some(Confirm::Delete) {
        forms::card(ui, |ui| {
            out.say(
                ui,
                RichText::new("Delete this project's memory?")
                    .strong()
                    .color(theme::TEXT),
            );
            out.say(
                ui,
                RichText::new("The memories, notes, evidence, queue and exports of this project are deleted and memory is turned off. This cannot be undone: export first to keep a copy.")
                    .size(12.0)
                    .color(theme::TEXT),
            );
            out.say(
                ui,
                RichText::new(DELETE_LEAVES).size(12.0).color(theme::TEXT),
            );
            ui.horizontal_wrapped(|ui| {
                let yes = ui.add(theme::destructive("Delete memory"));
                out.mark("mem-delete-confirm", yes.rect);
                if yes.clicked() {
                    out.actions.push(MemoryAction::Delete);
                }
                if out.button(ui, "mem-delete-keep", "Keep it", true) {
                    panel.confirm = None;
                }
            });
        });
    } else {
        ui.horizontal_wrapped(|ui| {
            let ask = ui.add(theme::destructive("Delete memory…"));
            out.mark("mem-delete", ask.rect);
            if ask.clicked() {
                panel.confirm = Some(Confirm::Delete);
            }
            out.say(
                ui,
                muted("Separate from turning memory off, which keeps the data."),
            );
        });
    }
}
