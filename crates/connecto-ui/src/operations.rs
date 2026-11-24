//! The project's operations (ADR 0031): the Operations dialog behind the
//! bolt on a project row, and the list it shares with Project Settings >
//! Operations.

use std::time::{Duration, Instant};

use connecto_core::ops::{HookMode, OpRow, OpStanding, OpsAction, OpsControl, OpsView};
use egui::{Context, Rect, RichText, Ui};

use crate::forms::{self, Out, card, heading, muted, problem, warning};
use crate::theme;
use crate::utility::ago;

/// How often a shown list is read again.
const REFRESH: Duration = Duration::from_secs(1);

/// What the list remembers between frames.
#[derive(Debug, Default)]
pub(crate) struct ListState {
    /// The operation whose details are open.
    pub(crate) expanded: Option<String>,
    /// An operation about to run: its arguments being typed, or the
    /// person's yes awaited when it asks first. Bound to the hash shown.
    pub(crate) prepare: Option<Prepare>,
    /// An operation waiting for the person's yes to delete it.
    pub(crate) confirm_delete: Option<String>,
}

/// A run being prepared in the dialog.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Prepare {
    pub(crate) slug: String,
    pub(crate) hash: String,
    pub(crate) args: Vec<String>,
}

/// What a frame of the dialog came to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SheetEnd {
    Open,
    Close,
    /// Close, and open Project Settings > Operations.
    Settings,
}

pub(crate) struct OpsSheet {
    pub(crate) project: String,
    name: String,
    view: OpsView,
    refreshed: Instant,
    list: ListState,
}

impl OpsSheet {
    pub(crate) fn new(project: &str, name: &str, control: &dyn OpsControl) -> Self {
        Self {
            project: project.to_owned(),
            name: name.to_owned(),
            view: control.view(project),
            refreshed: Instant::now(),
            list: ListState::default(),
        }
    }

    pub(crate) fn draw(
        &mut self,
        ctx: &Context,
        area: Rect,
        control: &dyn OpsControl,
        now_ms: u64,
        offset_s: i32,
    ) -> SheetEnd {
        if self.refreshed.elapsed() >= REFRESH {
            self.view = control.view(&self.project);
            self.refreshed = Instant::now();
        }
        ctx.request_repaint_after(REFRESH);
        let mut settings = false;
        let mut out = Out::<OpsAction>::default();
        let title = format!("Operations — {}", self.name);
        let closed = crate::sheet::show(ctx, area, "operations", &title, |ui, rect| {
            forms::column(ui, rect, |ui| {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        suggestion(ui, &mut out, &self.view);
                        list(ui, &mut out, &self.view, &mut self.list, now_ms, offset_s);
                        forms::separator(ui);
                        out.say(
                            ui,
                            muted(
                                "Operations are scripts in the project's .connecto/ops folder, kept out of git. Project Settings > Operations finds new ones and sets how agents are told about them.",
                            ),
                        );
                        if out.button(ui, "ops-settings", "Project Settings…", true) {
                            settings = true;
                        }
                    });
            });
        });
        let acted = !out.actions.is_empty();
        for action in out.actions {
            control.submit(self.project.clone(), action);
        }
        if acted {
            self.refreshed = Instant::now() - REFRESH;
        }
        if settings {
            SheetEnd::Settings
        } else if closed || ctx.input(|i| i.key_pressed(egui::Key::Escape)) {
            SheetEnd::Close
        } else {
            SheetEnd::Open
        }
    }
}

/// Project Settings > Operations: finding operations, how agents are told
/// about them, and the list.
pub(crate) fn settings_section(
    ui: &mut Ui,
    out: &mut Out<OpsAction>,
    view: &OpsView,
    state: &mut ListState,
    now_ms: u64,
    offset_s: i32,
) {
    if !view.available {
        // Nothing here can be changed: no control that would send an action
        // the operations cannot take (a host too old to share them).
        list(ui, out, view, state, now_ms, offset_s);
        return;
    }
    out.say(ui, heading("FIND COMMON OPERATIONS"));
    out.say(
        ui,
        muted("An agent terminal reads what you asked for in this project (your prompts, kept on this machine) and the project's scripts, and proposes an operation for each request that repeats: a script that needs no model, or one that asks a small model for one step. Nothing runs until you approve it."),
    );
    if let Some(at) = view.last_discovery_ms {
        out.say(
            ui,
            muted(format!(
                "Last looked {}; {} prompts since.",
                ago(at, now_ms, offset_s),
                view.prompts_since_discovery
            )),
        );
    }
    ui.horizontal_wrapped(|ui| {
        let running = view.discovery_terminal.clone();
        let why = running
            .as_ref()
            .map(|id| format!("It is running in {id}."))
            .unwrap_or_default();
        if out.button_hint(
            ui,
            "ops-discover",
            "Find common operations",
            view.available && running.is_none(),
            &why,
        ) {
            out.actions.push(OpsAction::Discover);
        }
        if let Some(id) = &running {
            out.say(ui, muted(format!("Running in {id}.")));
        }
    });
    let mut weekly = view.weekly;
    let response = ui.checkbox(
        &mut weekly,
        RichText::new("Suggest it once a week, when there are new prompts").size(12.0),
    );
    out.mark("ops-weekly", response.rect);
    if response.changed() {
        out.actions.push(OpsAction::SetWeekly(weekly));
    }
    forms::separator(ui);
    out.say(ui, heading("WHEN A PROMPT MATCHES AN OPERATION"));
    ui.horizontal_wrapped(|ui| {
        for (mode, key, label) in [
            (
                HookMode::Suggest,
                "ops-hook-suggest",
                "Tell the agent to run it",
            ),
            (HookMode::Off, "ops-hook-off", "Do nothing"),
        ] {
            if out.choice(ui, key, view.hook == mode, label) && view.hook != mode {
                out.actions.push(OpsAction::SetHook(mode));
            }
        }
    });
    out.say(
        ui,
        muted(if view.jev {
            "Connecto compares the prompt with each operation's phrases, then asks Jev (your key in Settings > Automations) to confirm. An operation can also run in place of the prompt: switch that on in its details. Only Claude terminals can be answered that way."
        } else {
            "Connecto compares the prompt with each operation's phrases. With a Jev key (Settings > Automations) it also confirms the match first, and operations can run in place of the prompt."
        }),
    );
    forms::separator(ui);
    // Summed wide: each count may be anything a host sent.
    let (runs, by_agent, suggested, intercepted) =
        view.ops
            .iter()
            .fold((0_u64, 0_u64, 0_u64, 0_u64), |(r, a, s, i), op| {
                (
                    r + u64::from(op.stats.runs),
                    a + u64::from(op.stats.by_agent),
                    s + u64::from(op.stats.suggested),
                    i + u64::from(op.stats.intercepted),
                )
            });
    if runs + suggested + intercepted > 0 {
        out.say(
            ui,
            muted(format!(
                "So far: {runs} runs ({by_agent} by agents), {suggested} prompts answered with a suggestion, {intercepted} run in place of the prompt (no agent turn). Connecto shows no token figures: the CLIs report none it can trust."
            )),
        );
        ui.add_space(4.0);
    }
    suggestion(ui, out, view);
    list(ui, out, view, state, now_ms, offset_s);
}

/// The weekly suggestion, when it is due, with its two buttons.
pub(crate) fn suggestion(ui: &mut Ui, out: &mut Out<OpsAction>, view: &OpsView) {
    if !view.suggest_discovery {
        return;
    }
    card(ui, |ui| {
        out.say(
            ui,
            RichText::new("Look for common operations?")
                .size(13.0)
                .color(theme::TEXT),
        );
        out.say(
            ui,
            muted(format!(
                "{} prompts since the last look. An agent reads what you asked for in this project and proposes scripts for the requests that repeat. You review each one before it can run.",
                view.prompts_since_discovery
            )),
        );
        ui.horizontal(|ui| {
            if out.button(ui, "ops-suggest-find", "Find operations", true) {
                out.actions.push(OpsAction::Discover);
            }
            if out.button(ui, "ops-suggest-later", "Later", true) {
                out.actions.push(OpsAction::Later);
            }
        });
    });
    ui.add_space(8.0);
}

/// The operations: the approved ones first, then the ones waiting.
pub(crate) fn list(
    ui: &mut Ui,
    out: &mut Out<OpsAction>,
    view: &OpsView,
    state: &mut ListState,
    now_ms: u64,
    offset_s: i32,
) {
    if let Some((text, error)) = &view.notice {
        out.say(ui, if *error { problem(text) } else { muted(text) });
        ui.add_space(4.0);
    }
    if !view.available {
        let why = view
            .unavailable
            .as_deref()
            .unwrap_or("Open the project on this machine to see its operations.");
        out.say(ui, warning(why));
        return;
    }
    let (ready, waiting): (Vec<&OpRow>, Vec<&OpRow>) =
        view.ops.iter().partition(|op| op.standing.runs());
    out.say(ui, heading("READY"));
    if ready.is_empty() {
        out.say(
            ui,
            muted("No approved operations yet. Approve one below, or find some in Project Settings > Operations."),
        );
    }
    for op in &ready {
        row(ui, out, op, state, now_ms, offset_s);
    }
    if !waiting.is_empty() {
        forms::separator(ui);
        out.say(ui, heading("WAITING FOR YOU"));
        out.say(
            ui,
            muted("Read what each one runs before you approve it. A change to its files takes the approval back."),
        );
        for op in &waiting {
            row(ui, out, op, state, now_ms, offset_s);
        }
    }
}

fn kind_and_danger(op: &OpRow) -> String {
    match (op.kind, op.danger) {
        (Some(kind), Some(danger)) => format!("{} · {}", kind.label(), danger.label()),
        _ => String::new(),
    }
}

fn row(
    ui: &mut Ui,
    out: &mut Out<OpsAction>,
    op: &OpRow,
    state: &mut ListState,
    now_ms: u64,
    offset_s: i32,
) {
    let slug = op.slug.as_str();
    card(ui, |ui| {
        ui.horizontal_wrapped(|ui| {
            out.say(
                ui,
                RichText::new(&op.name)
                    .size(13.0)
                    .strong()
                    .color(theme::TEXT),
            );
            out.say(ui, muted(slug));
            match &op.standing {
                OpStanding::Approved => {}
                OpStanding::Proposed => out.pill(ui, "proposed", theme::CYAN),
                OpStanding::Changed => out.pill(ui, "changed", theme::AMBER),
                OpStanding::Invalid(_) => out.pill(ui, "invalid", theme::ERROR),
            }
            if op.asks_first {
                out.pill(ui, "asks first", theme::AMBER);
            }
            if op.intercept && op.standing.runs() {
                out.pill(ui, "runs in place of prompts", theme::GREEN);
            }
        });
        if !op.description.is_empty() {
            out.say(
                ui,
                RichText::new(&op.description)
                    .size(12.0)
                    .color(theme::TEXT_ROW),
            );
        }
        if let OpStanding::Invalid(why) = &op.standing {
            out.say(ui, problem(why));
        }
        let detail = kind_and_danger(op);
        if !detail.is_empty() {
            out.say(ui, muted(detail));
        }
        let stats = &op.stats;
        if let Some((at, outcome)) = &stats.last {
            out.say(
                ui,
                muted(format!(
                    "Ran {} time{} ({} by agents), last {}: {outcome}. Suggested {}×, ran in place of a prompt {}×.",
                    stats.runs,
                    if stats.runs == 1 { "" } else { "s" },
                    stats.by_agent,
                    ago(*at, now_ms, offset_s),
                    stats.suggested,
                    stats.intercepted,
                )),
            );
        }
        let last_failed = stats.last.as_ref().is_some_and(|(_, o)| o != "exit 0");
        ui.horizontal_wrapped(|ui| {
            if op.standing.runs() && out.button(ui, &format!("ops-run:{slug}"), "Run", true) {
                if op.asks_first || !op.args.is_empty() {
                    state.prepare = Some(Prepare {
                        slug: slug.to_owned(),
                        hash: op.hash.clone(),
                        args: vec![String::new(); op.args.len()],
                    });
                } else {
                    out.actions.push(OpsAction::Run {
                        slug: slug.to_owned(),
                        hash: op.hash.clone(),
                        args: Vec::new(),
                    });
                }
            }
            if matches!(op.standing, OpStanding::Proposed | OpStanding::Changed)
                && out.button(ui, &format!("ops-approve:{slug}"), "Approve", true)
            {
                out.actions.push(OpsAction::Approve {
                    slug: slug.to_owned(),
                    hash: op.hash.clone(),
                });
            }
            if (last_failed || matches!(op.standing, OpStanding::Invalid(_)))
                && out.button(ui, &format!("ops-fix:{slug}"), "Fix with an agent", true)
            {
                out.actions.push(OpsAction::Fix(slug.to_owned()));
            }
            let open = state.expanded.as_deref() == Some(slug);
            let label = if open { "Hide details" } else { "Details" };
            if out.button(ui, &format!("ops-details:{slug}"), label, true) {
                state.expanded = if open { None } else { Some(slug.to_owned()) };
            }
        });
        if state.prepare.as_ref().is_some_and(|p| p.slug == slug) {
            prepare(ui, out, op, state);
        }
        if state.expanded.as_deref() == Some(slug) {
            details(ui, out, op, state);
        }
    });
    ui.add_space(6.0);
}

/// The arguments of a run, and the yes of an operation that asks first.
fn prepare(ui: &mut Ui, out: &mut Out<OpsAction>, op: &OpRow, state: &mut ListState) {
    let slug = op.slug.as_str();
    let Some(prep) = state.prepare.as_mut() else {
        return;
    };
    if prep.hash != op.hash {
        // The files changed under the panel: what was shown is gone.
        state.prepare = None;
        return;
    }
    ui.add_space(4.0);
    for (i, arg) in op.args.iter().enumerate() {
        let Some(value) = prep.args.get_mut(i) else {
            continue;
        };
        ui.horizontal(|ui| {
            let label = if arg.required {
                arg.name.clone()
            } else {
                format!("{} (optional)", arg.name)
            };
            out.say(ui, RichText::new(label).size(12.0).color(theme::TEXT_ROW));
            let response = ui.add(
                egui::TextEdit::singleline(value)
                    .id(egui::Id::unique(("ops-arg", slug, i)))
                    .hint_text(&arg.description)
                    .desired_width(260.0),
            );
            out.mark(&format!("ops-arg:{slug}:{i}"), response.rect);
        });
    }
    if op.asks_first {
        out.say(
            ui,
            warning(format!(
                "{} asks first: {}. Run it now?",
                op.name,
                op.danger
                    .map_or("it may change things", |d| d.label())
                    .to_lowercase()
            )),
        );
    }
    // Arguments are given in order: an empty optional one ends the list.
    let missing = op
        .args
        .iter()
        .zip(&prep.args)
        .find(|(a, v)| a.required && v.trim().is_empty())
        .map(|(a, _)| format!("Enter {} first.", a.name));
    let mut given: Vec<String> = Vec::new();
    for value in &prep.args {
        if value.trim().is_empty() {
            break;
        }
        given.push(value.clone());
    }
    // Arguments go in order: one after an empty field would be lost.
    let gap = prep.args[given.len()..]
        .iter()
        .any(|v| !v.trim().is_empty());
    let missing = missing.or_else(|| gap.then(|| "Fill the arguments in order.".to_owned()));
    let mut close = false;
    ui.horizontal(|ui| {
        let why = missing.clone().unwrap_or_default();
        if out.button_hint(
            ui,
            &format!("ops-run-yes:{slug}"),
            "Run it",
            missing.is_none(),
            &why,
        ) {
            out.actions.push(OpsAction::Run {
                slug: slug.to_owned(),
                hash: op.hash.clone(),
                args: given.clone(),
            });
            close = true;
        }
        if out.button(ui, &format!("ops-run-no:{slug}"), "Cancel", true) {
            close = true;
        }
    });
    if close {
        state.prepare = None;
    }
}

fn details(ui: &mut Ui, out: &mut Out<OpsAction>, op: &OpRow, state: &mut ListState) {
    let slug = op.slug.as_str();
    ui.add_space(4.0);
    if !op.phrases.is_empty() {
        out.say(
            ui,
            muted(format!("Asked for as: {}", op.phrases.join(" · "))),
        );
    }
    if !op.args.is_empty() {
        let args: Vec<String> = op
            .args
            .iter()
            .map(|a| {
                let optional = if a.required { "" } else { " (optional)" };
                if a.description.is_empty() {
                    format!("{}{optional}", a.name)
                } else {
                    format!("{}{optional}: {}", a.name, a.description)
                }
            })
            .collect();
        out.say(ui, muted(format!("Arguments: {}", args.join("; "))));
    }
    out.say(ui, muted(format!("Stops after {} s.", op.timeout_s)));
    if !op.body.is_empty() {
        egui::Frame::NONE
            .fill(theme::TERM_BG)
            .corner_radius(4.0)
            .inner_margin(egui::Margin::same(6))
            .show(ui, |ui| {
                ui.set_width(ui.available_width());
                egui::ScrollArea::both()
                    .id_salt(("ops-body", slug))
                    .max_height(220.0)
                    .show(ui, |ui| {
                        out.say(
                            ui,
                            RichText::new(&op.body)
                                .monospace()
                                .size(11.5)
                                .color(theme::TEXT_ROW),
                        );
                    });
            });
    }
    ui.add_space(4.0);
    if op.standing.runs() {
        if op.can_intercept() {
            let mut on = op.intercept;
            let response = ui.checkbox(
                &mut on,
                RichText::new(
                    "Run in place of a matching prompt to Claude (the agent never sees it)",
                )
                .size(12.0),
            );
            out.mark(&format!("ops-intercept:{slug}"), response.rect);
            if response.changed() {
                out.actions
                    .push(OpsAction::SetIntercept(slug.to_owned(), on));
            }
        } else {
            out.say(
                ui,
                muted("Only suggested to agents: it asks first, or is a recipe."),
            );
        }
    }
    ui.horizontal_wrapped(|ui| {
        if op.standing.runs()
            && out.button(
                ui,
                &format!("ops-withdraw:{slug}"),
                "Withdraw approval",
                true,
            )
        {
            out.actions.push(OpsAction::Withdraw(slug.to_owned()));
        }
        if out.button(ui, &format!("ops-delete:{slug}"), "Delete…", true) {
            state.confirm_delete = Some(slug.to_owned());
        }
    });
    if state.confirm_delete.as_deref() == Some(slug) {
        out.say(
            ui,
            warning(format!(
                "Delete .connecto/ops/{slug}? Its files are removed."
            )),
        );
        ui.horizontal(|ui| {
            if out.button(ui, &format!("ops-delete-yes:{slug}"), "Delete", true) {
                out.actions.push(OpsAction::Delete(slug.to_owned()));
                state.confirm_delete = None;
                state.expanded = None;
            }
            if out.button(ui, &format!("ops-delete-no:{slug}"), "Cancel", true) {
                state.confirm_delete = None;
            }
        });
    }
}
