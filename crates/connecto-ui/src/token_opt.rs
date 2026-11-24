//! Agent Token Optimization, as the Tokens tab of Project Settings
//! (docs/specs/token-optimization.md §8).
//!
//! As the memory section: everything here reads [`TokenControl::view`] (an
//! `Arc`, never blocking) and sends the user's choices as commands, which
//! return at once. While the host works, the tab is redrawn every 500 ms.

use std::collections::BTreeSet;
use std::time::Duration;

use connecto_core::tokens::{
    AUTOCOMPACT_CHOICES, Confirmation, ProjectTokenView, Proposal, ProposalKind, StudyState,
    TokenControl, TokenReport,
};
use egui::{RichText, Ui};

use crate::forms::{Out, card, heading, muted, problem, separator, warning};
use crate::theme;

/// What the tab asks the control for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum TokenAction {
    Analyze,
    Apply {
        plugins: Vec<String>,
        skills: Vec<String>,
    },
    Undo,
    Autocompact(Option<u32>),
}

/// The tab's own state, one per Project Settings sheet.
#[derive(Default)]
pub(crate) struct TokenPanel {
    /// The ticked proposals, by [`key`].
    ticked: BTreeSet<String>,
    /// The report the ticks belong to (its `analyzed_ms`).
    ticks_for: Option<u64>,
    /// The project this panel serves.
    project: Option<String>,
    /// Why the last command was refused.
    error: Option<String>,
    /// What the last pass drew, for tests.
    #[cfg(test)]
    pub(crate) marks: crate::forms::Marks,
    /// What the last pass asked for, for tests.
    #[cfg(test)]
    pub(crate) sent: Vec<TokenAction>,
}

/// A proposal's key: its kind and name.
fn key(p: &Proposal) -> String {
    format!("{:?}:{}", p.kind, p.name)
}

/// Draw the tab and send what the user asked for to `control`.
pub(crate) fn section(
    ui: &mut Ui,
    panel: &mut TokenPanel,
    control: &dyn TokenControl,
    project_id: &str,
) {
    if panel.project.as_deref() != Some(project_id) {
        *panel = TokenPanel {
            project: Some(project_id.to_owned()),
            ..TokenPanel::default()
        };
        control.open(project_id);
    }
    let view = control.view();
    let project = view.project(project_id);
    let mut out = Out::default();
    ui.push_id("token-optimization", |ui| {
        draw(ui, panel, view.available, project, &mut out);
    });
    if project.is_some_and(|p| p.busy.is_some()) {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
    #[cfg(test)]
    {
        panel.marks = std::mem::take(&mut out.marks);
        panel.sent.clone_from(&out.actions);
    }
    for action in out.actions {
        ui.ctx().request_repaint();
        let result = match action {
            TokenAction::Analyze => control.analyze(project_id),
            TokenAction::Apply { plugins, skills } => control.apply(project_id, plugins, skills),
            TokenAction::Undo => control.undo(project_id),
            TokenAction::Autocompact(tokens) => control.set_autocompact(project_id, tokens),
        };
        panel.error = result.err();
    }
}

fn k(tokens: u64) -> String {
    #[allow(clippy::cast_precision_loss)]
    if tokens >= 1_000_000 {
        format!("{:.1}M", tokens as f64 / 1_000_000.0)
    } else {
        format!("{}k", tokens.div_ceil(1000))
    }
}

fn pct(value: f64) -> String {
    if value > 0.0 && value < 1.0 {
        "<1%".to_owned()
    } else {
        format!("{value:.0}%")
    }
}

fn draw(
    ui: &mut Ui,
    panel: &mut TokenPanel,
    available: bool,
    project: Option<&ProjectTokenView>,
    out: &mut Out<TokenAction>,
) {
    out.say(ui, heading("AGENT TOKEN OPTIMIZATION"));
    out.say(
        ui,
        muted("Connecto reads this project's Claude sessions of the last 30 days, shows where the tokens go and what enters every session without being used, and can leave that out of the Claude terminals you start from now on. It never changes a running terminal or your own files."),
    );
    if !available {
        out.say(
            ui,
            muted("Token optimization is not available in this build."),
        );
        return;
    }
    let Some(project) = project else {
        out.say(ui, muted("Loading…"));
        return;
    };
    if let Some(error) = panel.error.as_ref().or(project.error.as_ref()) {
        out.say(ui, problem(error.clone()));
    }
    ui.horizontal_wrapped(|ui| {
        let busy = project.busy.is_some();
        if out.button(ui, "tok-analyze", "Analyze past sessions", !busy) {
            out.actions.push(TokenAction::Analyze);
        }
        if let Some(busy) = &project.busy {
            out.say(ui, RichText::new(busy).size(11.5).color(theme::AMBER));
        }
    });
    separator(ui);
    match &project.report {
        Some(report) => {
            if panel.ticks_for != Some(report.analyzed_ms) {
                panel.ticks_for = Some(report.analyzed_ms);
                panel.ticked = report
                    .proposals
                    .iter()
                    .filter(|p| p.preselected && p.kind.applicable())
                    .map(key)
                    .collect();
            }
            where_tokens_go(ui, report, out);
            separator(ui);
            unused(ui, panel, project, report, out);
        }
        None => {
            out.say(ui, heading("WHERE YOUR TOKENS GO"));
            out.say(ui, muted("Analyze past sessions to see where the tokens go and what is loaded but never used."));
            if project.settings.has_cuts() {
                separator(ui);
                cuts(ui, project, out);
            }
        }
    }
    separator(ui);
    before_after(ui, project, out);
    separator(ui);
    compact(ui, project, out);
    separator(ui);
    study(ui, project, out);
}

fn where_tokens_go(ui: &mut Ui, report: &TokenReport, out: &mut Out<TokenAction>) {
    let t = &report.totals;
    out.say(ui, heading("WHERE YOUR TOKENS GO"));
    if t.sessions == 0 {
        out.say(
            ui,
            muted("No Claude session of this project in the last 30 days had at least 3 calls."),
        );
        return;
    }
    let text = |s: String| RichText::new(s).color(theme::TEXT);
    out.say(
        ui,
        text(format!(
            "{} sessions, {} calls in the last 30 days.",
            t.sessions, t.calls
        )),
    );
    out.say(
        ui,
        text(format!(
            "{} of input was read from the cache. Median context per call {}; median start context {}.",
            pct(t.cache_read_share),
            k(t.median_context),
            k(t.median_start_context)
        )),
    );
    out.say(
        ui,
        text(format!(
            "Calls above 200k made {} of the cache reads.",
            pct(t.reads_above_200k)
        )),
    );
    out.say(
        ui,
        text(format!(
            "Relative cost: cache reads {}, cache writes {}, output {}, fresh input {}.",
            pct(t.cost_reads),
            pct(t.cost_writes),
            pct(t.cost_output),
            pct(t.cost_input)
        )),
    );
    out.say(
        ui,
        muted("Weights: 0.1× a cache read, 2× a 1-hour cache write, 1.25× a 5-minute write, 5× output."),
    );
    out.say(
        ui,
        text(format!(
            "{} cold rebuilds after pauses over an hour; {} compactions.",
            t.cold_rebuilds, t.compactions
        )),
    );
    if let Some(limited) = &t.limited {
        out.say(ui, warning(limited.clone()));
    }
    match &report.loaded {
        Some(l) => out.say(
            ui,
            muted(format!(
                "The latest session loaded {} skills (~{} tokens), {} deferred MCP tools (~{}), {} agents (~{}) and {} memory files (~{}). Token counts are estimates: characters / 4.",
                l.skills,
                k(l.skill_tokens),
                l.mcp_tools,
                k(l.mcp_tokens),
                l.agents,
                k(l.agent_tokens),
                l.memory_files,
                k(l.memory_tokens)
            )),
        ),
        None => out.say(
            ui,
            muted("What the sessions load is not recorded in their transcripts."),
        ),
    }
}

const fn kind_label(kind: ProposalKind) -> &'static str {
    match kind {
        ProposalKind::Plugin => "Plugin",
        ProposalKind::Skill => "Skill",
        ProposalKind::McpServer => "MCP server",
        ProposalKind::MemoryFile => "Memory file",
    }
}

fn unused(
    ui: &mut Ui,
    panel: &mut TokenPanel,
    project: &ProjectTokenView,
    report: &TokenReport,
    out: &mut Out<TokenAction>,
) {
    out.say(ui, heading("LOADED BUT NEVER USED"));
    if report.loaded.is_none() {
        out.say(
            ui,
            muted("Not recorded: the transcripts do not say what was loaded."),
        );
    } else if report.proposals.is_empty() {
        out.say(ui, muted("Nothing that was loaded went unused."));
    }
    for p in &report.proposals {
        let id = key(p);
        ui.push_id(&id, |ui| {
            card(ui, |ui| {
                ui.horizontal_wrapped(|ui| {
                    if p.kind.applicable() {
                        let mut on = panel.ticked.contains(&id);
                        let response = ui.checkbox(&mut on, "");
                        out.mark(&format!("tok-tick:{}", p.name), response.rect);
                        if response.changed() {
                            if on {
                                panel.ticked.insert(id.clone());
                            } else {
                                panel.ticked.remove(&id);
                            }
                        }
                    }
                    out.say(
                        ui,
                        RichText::new(format!("{} {}", kind_label(p.kind), p.name))
                            .strong()
                            .color(theme::TEXT),
                    );
                    out.say(
                        ui,
                        muted(format!("~{} tokens · {}", k(p.tokens), p.evidence)),
                    );
                });
                if let Some(hint) = &p.hint {
                    out.say(ui, muted(hint.clone()));
                }
            });
        });
    }
    let chosen: Vec<&Proposal> = report
        .proposals
        .iter()
        .filter(|p| p.kind.applicable() && panel.ticked.contains(&key(p)))
        .collect();
    let busy = project.busy.is_some();
    // Something ticked that new terminals do not leave out yet.
    let adds = chosen.iter().any(|p| {
        let have = match p.kind {
            ProposalKind::Plugin => &project.settings.plugins_off,
            _ => &project.settings.skills_off,
        };
        p.ids.iter().any(|id| !have.contains(id))
    });
    ui.horizontal_wrapped(|ui| {
        if out.button(ui, "tok-apply", "Apply to new terminals", adds && !busy) {
            let pick = |kind| {
                chosen
                    .iter()
                    .filter(|p| p.kind == kind)
                    .flat_map(|p| p.ids.iter().cloned())
                    .collect()
            };
            out.actions.push(TokenAction::Apply {
                plugins: pick(ProposalKind::Plugin),
                skills: pick(ProposalKind::Skill),
            });
        }
        if out.button(ui, "tok-undo", "Undo all", project.settings.has_cuts()) {
            out.actions.push(TokenAction::Undo);
        }
    });
    out.say(
        ui,
        muted("Applies to Claude terminals started from now on; running terminals keep their setup. A resumed conversation gets the current setup, and its prompt cache is written again once."),
    );
    cuts(ui, project, out);
}

/// What new terminals leave out now.
fn cuts(ui: &mut Ui, project: &ProjectTokenView, out: &mut Out<TokenAction>) {
    let s = &project.settings;
    if !s.has_cuts() {
        return;
    }
    if !s.plugins_off.is_empty() {
        out.say(
            ui,
            RichText::new(format!("Plugins off: {}", s.plugins_off.join(", ")))
                .size(11.5)
                .color(theme::CYAN),
        );
    }
    if !s.skills_off.is_empty() {
        out.say(
            ui,
            RichText::new(format!("Skills off: {}", s.skills_off.join(", ")))
                .size(11.5)
                .color(theme::CYAN),
        );
    }
    // With a report, Undo all sits under the list.
    if project.report.is_some() {
        return;
    }
    if out.button(ui, "tok-undo", "Undo all", true) {
        out.actions.push(TokenAction::Undo);
    }
}

fn before_after(ui: &mut Ui, project: &ProjectTokenView, out: &mut Out<TokenAction>) {
    out.say(ui, heading("BEFORE / AFTER"));
    let Some(before) = project.settings.before else {
        out.say(
            ui,
            muted("Apply a cut to measure its effect on new sessions."),
        );
        return;
    };
    out.say(
        ui,
        RichText::new(format!(
            "Before: median start context {} ({} sessions).",
            k(before.start_context_median),
            before.sessions
        ))
        .color(theme::TEXT),
    );
    let after = project.report.as_ref().and_then(|r| r.after.as_ref());
    let Some(after) = after else {
        out.say(ui, muted("After: analyze past sessions to measure."));
        return;
    };
    match after.start_context_median {
        None => out.say(ui, muted("After: start a new terminal to measure.")),
        Some(median) => {
            let n = if after.sessions < 3 {
                format!(" ({} sessions)", after.sessions)
            } else {
                String::new()
            };
            out.say(
                ui,
                RichText::new(format!("After: median start context {}{n}.", k(median)))
                    .color(theme::TEXT),
            );
        }
    }
    match &after.confirmation {
        Confirmation::NotYet => {}
        Confirmation::Confirmed => out.say(
            ui,
            RichText::new("The cuts are confirmed in the transcript: none of them was loaded.")
                .size(11.5)
                .color(theme::GREEN),
        ),
        Confirmation::StillLoaded(still) => out.say(
            ui,
            warning(format!(
                "Still loaded in the first session after the cuts: {}.",
                still.join(", ")
            )),
        ),
        Confirmation::NotRecorded => out.say(
            ui,
            muted("The first session after the cuts does not record what it loaded."),
        ),
    }
}

fn compact(ui: &mut Ui, project: &ProjectTokenView, out: &mut Out<TokenAction>) {
    out.say(ui, heading("COMPACT EARLIER"));
    let current = project.settings.autocompact;
    ui.horizontal_wrapped(|ui| {
        let auto = "Auto (Claude's default)";
        if out.choice(ui, "tok-compact:auto", current.is_none(), auto) && current.is_some() {
            out.actions.push(TokenAction::Autocompact(None));
        }
        for choice in AUTOCOMPACT_CHOICES {
            let label = k(u64::from(choice));
            let on = current == Some(choice);
            if out.choice(ui, &format!("tok-compact:{choice}"), on, &label) && !on {
                out.actions.push(TokenAction::Autocompact(Some(choice)));
            }
        }
    });
    let estimate = project.report.as_ref().map(|r| &r.compact);
    if let Some(e) = estimate {
        out.say(
            ui,
            muted(format!(
                "Calls above 200k used {} of your cache reads in the last 30 days.",
                pct(e.reads_above_200k)
            )),
        );
        if let Some(saving) = current.and_then(|c| e.savings.iter().find(|(t, _)| *t == c)) {
            out.say(
                ui,
                RichText::new(format!(
                    "Estimated saving: ~{} of reads; compaction summarizes earlier turns, so agents may lose detail from long sessions.",
                    pct(saving.1)
                ))
                .size(11.5)
                .color(theme::TEXT),
            );
        }
    }
    out.say(
        ui,
        muted("Passed to Claude terminals started from now on (--autocompact)."),
    );
}

fn study(ui: &mut Ui, project: &ProjectTokenView, out: &mut Out<TokenAction>) {
    out.say(ui, heading("BIGGEST SESSION STUDY"));
    if let Some(biggest) = project.report.as_ref().and_then(|r| r.biggest.as_ref()) {
        out.say(
            ui,
            muted(format!("The session with the most cache reads: {biggest}.")),
        );
    }
    match &project.study {
        StudyState::NotRun => out.say(
            ui,
            muted("A utility session studies the biggest session after the analysis. What it finds is advice to read: it changes nothing."),
        ),
        StudyState::Running => out.say(ui, warning("Studying the biggest session…")),
        StudyState::Unavailable(why) => {
            out.say(ui, warning(format!("Study unavailable: {why}")));
        }
        StudyState::Done(findings) if findings.is_empty() => {
            out.say(ui, muted("No habit stood out."));
        }
        StudyState::Done(findings) => {
            for f in findings {
                card(ui, |ui| {
                    out.say(ui, RichText::new(&f.title).strong().color(theme::TEXT));
                    out.say(ui, muted(&f.evidence));
                    out.say(ui, RichText::new(&f.advice).size(11.5).color(theme::TEXT));
                });
            }
        }
    }
}

#[cfg(test)]
#[path = "token_opt_tests.rs"]
mod tests;
