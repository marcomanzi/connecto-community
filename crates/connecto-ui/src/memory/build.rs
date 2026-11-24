//! The initial build (spec §5.9): its state and progress, the coverage of the
//! sources, and before a start the plan: sources, exclusions, limits, and the
//! characters and runs they make. Never a figure in money.

use connecto_core::memory::{
    BootstrapOptions, BootstrapState, BootstrapView, CoverageGap, RunOutcome, SourceClass,
    SourceCoverage,
};
use egui::{RichText, Ui};

use super::{Cx, MemoryAction, MemoryPanel, field, number, row, text};
use crate::forms::{self, Out, heading, muted, warning};
use crate::theme;

/// Not a promise of knowing everything (spec §5.9).
pub(crate) const NOT_EXHAUSTIVE: &str = "A finished build is not complete knowledge of the project: the memory knows the sources analysed above, within the limits chosen.";

/// A contract range of `u32`, widened for the number field.
fn wide(range: &std::ops::RangeInclusive<u32>) -> std::ops::RangeInclusive<u64> {
    u64::from(*range.start())..=u64::from(*range.end())
}

pub(super) fn show(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let b = &cx.project.bootstrap;
    out.say(ui, heading("INITIAL BUILD"));
    out.say(
        ui,
        muted("Builds the memory from what the project already has. On a memory that has content it reconciles, never resets."),
    );
    state(ui, b, cx, out);
    if cx.project.git_note.is_some() {
        forms::separator(ui);
        super::report::git_note(ui, cx, out);
    }
    if !b.coverage.is_empty() || !b.gaps.is_empty() {
        forms::separator(ui);
        out.say(ui, heading("SOURCE COVERAGE"));
        coverage(ui, &b.coverage, "Analysed", out, |c| c.analysed);
        gaps(ui, &b.gaps, out);
        out.say(ui, warning(NOT_EXHAUSTIVE));
    }
    if !matches!(b.state, BootstrapState::Running | BootstrapState::Queued) {
        forms::separator(ui);
        plan(ui, panel, cx, out);
    }
}

fn state(ui: &mut Ui, b: &BootstrapView, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let (word, color) = text::bootstrap_state(&b.state);
    if let BootstrapState::Failed { reason } = &b.state {
        super::held(ui, out, super::Stop::Failed, "Failed", reason);
    }
    let last = cx
        .project
        .cycle(connecto_core::memory::Cycle::Bootstrap)
        .and_then(|c| c.last.as_ref())
        .and_then(|r| match &r.outcome {
            RunOutcome::Held { reason } => Some((super::Stop::Held, "On hold", reason)),
            RunOutcome::Failed { reason } => {
                Some((super::Stop::Failed, "Last build run failed", reason))
            }
            RunOutcome::Refused { reason, .. } => {
                Some((super::Stop::Refused, "Last build run kept nothing", reason))
            }
            _ => None,
        })
        .filter(|(_, _, reason)| {
            !matches!(&b.state, BootstrapState::Failed { reason: r } if r == *reason)
        });
    // A build that completed with nothing kept is not "Completed".
    let kept_nothing = matches!(last, Some((super::Stop::Refused, _, _)));
    let (word, color) = if kept_nothing && b.state == BootstrapState::Completed {
        ("Finished with nothing kept".to_owned(), theme::AMBER)
    } else {
        (word, color)
    };
    if let Some((stop, what, reason)) = last {
        super::held(ui, out, stop, what, reason);
    }
    ui.horizontal_wrapped(|ui| {
        if !matches!(b.state, BootstrapState::Failed { .. }) {
            out.say(ui, RichText::new(word).color(color));
        }
        if let Some(ms) = b.started_ms {
            out.say(
                ui,
                muted(format!(
                    "started {}",
                    crate::utility::ago(ms, cx.now_ms, cx.offset_s)
                )),
            );
        }
        if let Some(ms) = b.updated_ms {
            out.say(
                ui,
                muted(format!(
                    "updated {}",
                    crate::utility::ago(ms, cx.now_ms, cx.offset_s)
                )),
            );
        }
    });
    match &b.state {
        BootstrapState::Queued | BootstrapState::Running => {
            progress(ui, b, out);
            out.say(
                ui,
                muted("Events from your terminals wait in the queue meanwhile. Agents are told the build is in progress."),
            );
            let stop = ui.add(theme::destructive("Cancel the build"));
            out.mark("mem-boot-cancel", stop.rect);
            if stop.clicked() {
                out.actions.push(MemoryAction::CancelBootstrap);
            }
        }
        BootstrapState::Interrupted | BootstrapState::Failed { .. } => {
            if b.packets_total > 0 {
                progress(ui, b, out);
            }
            if matches!(b.state, BootstrapState::Failed { .. }) {
                out.say(ui, muted("Nothing partial was published as complete."));
            }
            ui.horizontal_wrapped(|ui| {
                if out.button(ui, "mem-boot-resume", "Resume", true) {
                    out.actions.push(MemoryAction::ResumeBootstrap);
                }
                out.say(ui, muted("Continues from the last checkpoint."));
            });
        }
        BootstrapState::Completed => {
            if b.packets_total > 0 {
                progress(ui, b, out);
            }
        }
        BootstrapState::NotStarted => {}
    }
}

fn progress(ui: &mut Ui, b: &BootstrapView, out: &mut Out<MemoryAction>) {
    let total = b.packets_total.max(1);
    #[allow(clippy::cast_precision_loss)]
    let fraction = (b.packets_done.min(total) as f32) / (total as f32);
    let label = format!("{} of {} packets", b.packets_done, b.packets_total);
    ui.add(
        egui::ProgressBar::new(fraction)
            .desired_width(280.0)
            .text(label.clone()),
    );
    #[cfg(test)]
    out.marks.texts.push(label);
    #[cfg(not(test))]
    let _ = (out, label);
}

/// A table of source classes: `first` names the first number column.
fn coverage(
    ui: &mut Ui,
    rows: &[SourceCoverage],
    first: &str,
    out: &mut Out<MemoryAction>,
    first_of: impl Fn(&SourceCoverage) -> u32,
) {
    if rows.is_empty() {
        return;
    }
    egui::Grid::new(("memory-coverage", first))
        .num_columns(4)
        .spacing([16.0, 4.0])
        .show(ui, |ui| {
            for title in ["Source", first, "Excluded", "Remaining"] {
                out.say(ui, heading(title));
            }
            ui.end_row();
            for c in rows {
                out.say(ui, RichText::new(c.class.label()).color(theme::TEXT));
                out.say(
                    ui,
                    RichText::new(text::count(u64::from(first_of(c)))).color(theme::TEXT_ROW),
                );
                out.say(ui, muted(text::count(u64::from(c.excluded))));
                let color = if c.remaining > 0 {
                    theme::AMBER
                } else {
                    theme::TEXT_ROW
                };
                out.say(
                    ui,
                    RichText::new(text::count(u64::from(c.remaining))).color(color),
                );
                ui.end_row();
            }
        });
}

fn gaps(ui: &mut Ui, gaps: &[CoverageGap], out: &mut Out<MemoryAction>) {
    for gap in gaps {
        out.say(ui, warning(format!("• {}", gap.label())));
    }
}

/// The options of the next build, its plan, and the start.
fn plan(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let b = &cx.project.bootstrap;
    out.say(ui, heading("PLAN"));
    out.say(
        ui,
        muted(format!(
            "Runs with the dream cycle's model: {}. It reads the project and may open files to check a claim; it does not change the project or run its tests.",
            cx.settings.dream.choice
        )),
    );
    out.say(ui, RichText::new("Sources").color(theme::TEXT));
    ui.horizontal_wrapped(|ui| {
        for class in SourceClass::ALL {
            let mut on = panel.boot.sources.contains(class);
            let response = ui.checkbox(&mut on, class.label());
            out.mark(source_key(*class), response.rect);
            if response.changed() {
                if on {
                    panel.boot.sources.push(*class);
                    panel
                        .boot
                        .sources
                        .sort_by_key(|c| SourceClass::ALL.iter().position(|a| a == c));
                } else {
                    panel.boot.sources.retain(|c| c != class);
                }
            }
        }
    });
    let exclude = panel.boot.exclude.join(", ");
    let mut typed = None;
    row(ui, out, "Exclude", |ui, out| {
        typed = field(ui, panel, out, "mem-boot-exclude", &exclude, 320.0);
        out.say(ui, muted("paths or globs, comma separated"));
    });
    if let Some(typed) = typed {
        panel.boot.exclude = typed
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_owned)
            .collect();
    }
    let (history_days, max_minutes) = (
        wide(&BootstrapOptions::HISTORY_DAYS),
        wide(&BootstrapOptions::MAX_MINUTES),
    );
    let numbers: [(
        &str,
        &'static str,
        u64,
        &std::ops::RangeInclusive<u64>,
        &str,
    ); 3] = [
        (
            "History",
            "mem-boot-days",
            u64::from(panel.boot.history_days),
            &history_days,
            "days of git history and conversations",
        ),
        (
            "Input limit",
            "mem-boot-input",
            panel.boot.max_input_chars,
            &BootstrapOptions::MAX_INPUT_CHARS,
            "characters read in total",
        ),
        (
            "Time limit",
            "mem-boot-minutes",
            u64::from(panel.boot.max_minutes),
            &max_minutes,
            "minutes",
        ),
    ];
    for (label, key, value, range, hint) in numbers {
        let mut result = None;
        row(ui, out, label, |ui, out| {
            result = number(ui, panel, out, key, value, range);
            out.say(
                ui,
                muted(format!(
                    "{hint} ({}–{})",
                    text::count(*range.start()),
                    text::count(*range.end())
                )),
            );
        });
        match result {
            Some(Ok(n)) => match key {
                "mem-boot-days" => panel.boot.history_days = u32::try_from(n).unwrap_or(u32::MAX),
                "mem-boot-input" => panel.boot.max_input_chars = n,
                _ => panel.boot.max_minutes = u32::try_from(n).unwrap_or(u32::MAX),
            },
            Some(Err(why)) => panel.error = Some(why),
            None => {}
        }
    }
    let none = panel.boot.sources.is_empty();
    ui.horizontal_wrapped(|ui| {
        let why = if none {
            "Pick at least one kind of source."
        } else {
            ""
        };
        if out.button_hint(
            ui,
            "mem-boot-plan",
            "Preview the plan",
            !none && !b.planning,
            why,
        ) {
            out.actions
                .push(MemoryAction::PlanBootstrap(panel.boot.clone()));
        }
        if b.planning {
            out.say(ui, muted("Working out the plan…"));
        }
    });
    let current = b.plan.as_ref().filter(|p| p.options == panel.boot);
    match (&b.plan, current) {
        (_, Some(plan)) => {
            out.say(ui, RichText::new("What it would read").color(theme::TEXT));
            coverage(ui, &plan.coverage, "To read", out, |c| c.remaining);
            gaps(ui, &plan.gaps, out);
            out.say(
                ui,
                RichText::new(format!(
                    "About {} characters, in about {} model runs.",
                    text::count(plan.chars),
                    plan.runs
                ))
                .color(theme::TEXT),
            );
            out.say(
                ui,
                muted("Measured in characters and runs: the CLIs report no reliable cost, so none is shown."),
            );
        }
        (Some(_), None) => {
            out.say(
                ui,
                warning(
                    "The options changed since the last plan: preview it again before starting.",
                ),
            );
        }
        (None, None) => {
            out.say(
                ui,
                muted("Preview the plan to see what it reads before it starts."),
            );
        }
    }
    let again = cx.project.version > 0 || b.state == BootstrapState::Completed;
    let label = if again {
        "Reconcile with the project"
    } else {
        "Start the build"
    };
    let ready = current.is_some() && !b.planning;
    let start = ui.add_enabled(ready, theme::primary(label));
    let start = if ready {
        start
    } else {
        start.on_disabled_hover_text("Preview the plan for these options first.")
    };
    out.mark("mem-boot-start", start.rect);
    if start.clicked() {
        out.actions
            .push(MemoryAction::Bootstrap(panel.boot.clone()));
    }
}

const fn source_key(class: SourceClass) -> &'static str {
    match class {
        SourceClass::Documentation => "mem-boot-source:documentation",
        SourceClass::Manifests => "mem-boot-source:manifests",
        SourceClass::Structure => "mem-boot-source:structure",
        SourceClass::Code => "mem-boot-source:code",
        SourceClass::TestConfig => "mem-boot-source:test_config",
        SourceClass::GitState => "mem-boot-source:git_state",
        SourceClass::GitHistory => "mem-boot-source:git_history",
        SourceClass::Conversations => "mem-boot-source:conversations",
    }
}
