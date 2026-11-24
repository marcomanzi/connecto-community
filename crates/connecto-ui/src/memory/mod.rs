//! Project memory, as a section of the Project Settings sheet (spec §5.9,
//! §5.10, §7, §8; tasks T10 and T11).
//!
//! Everything here reads [`MemoryControl::view`] (a cheap `Arc`, never
//! blocking) and calls the control's commands, which return at once and touch
//! no disk. Heavy reads (a room's index, a search, one memory) are requests:
//! the section keeps the request id and draws the answer from the project's
//! `reads` when it arrives, pending or failed until then.
//!
//! The section is split in tabs: an overview (status, freshness and backlog,
//! actions), the initial build, the two cycles' settings, runs and versions,
//! and the memory itself. No figure in money is shown anywhere (§8.4): only
//! runs, characters in and out, and durations.

mod activity;
mod ask;
mod browse;
mod build;
mod cycles;
mod fetch;
mod overview;
mod report;
mod runs;
mod settings;
mod statement;
#[cfg(test)]
mod tests;
mod text;

use std::collections::HashMap;
use std::ops::RangeInclusive;
use std::time::Duration;

use connecto_core::memory::{
    BootstrapOptions, BootstrapState, Cycle, DreamReport, ExportFormat, LocalTime, MemoryControl,
    MemorySettings, MemoryView, ProjectMemoryView, ReadRequest, ReadState, Room, RunOutcome,
};
use egui::{Id, RichText, TextEdit, Ui};

use crate::forms::{Out, heading, muted, problem};
use crate::theme;

/// The tabs of the section, once memory is on: the memories to read, what
/// the memory has done, and how it is configured.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub(crate) enum Tab {
    #[default]
    Memories,
    Activity,
    Settings,
}

impl Tab {
    const ALL: [Self; 3] = [Self::Memories, Self::Activity, Self::Settings];

    const fn label(self) -> &'static str {
        match self {
            Self::Memories => "Memories",
            Self::Activity => "Activity",
            Self::Settings => "Settings",
        }
    }

    const fn key(self) -> &'static str {
        match self {
            Self::Memories => "mem-tab:memories",
            Self::Activity => "mem-tab:activity",
            Self::Settings => "mem-tab:settings",
        }
    }
}

/// A destructive step waiting for the user's yes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Confirm {
    Revert(u64),
    Delete,
}

/// Settings sent that the view does not show yet. Edits build on them, so a
/// second change never undoes the first.
#[derive(Debug, Clone)]
struct Pending {
    settings: MemorySettings,
    /// The project's `last_error` when they were sent: a new one means the
    /// engine turned them down.
    error: Option<String>,
}

/// Where a read's answer is shown.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Slot {
    /// A room's index or search results.
    List,
    /// One memory (or annotation), over the list.
    Detail,
}

/// What the section asks the control for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum MemoryAction {
    Settings(MemorySettings),
    PlanBootstrap(BootstrapOptions),
    Bootstrap(BootstrapOptions),
    CancelBootstrap,
    ResumeBootstrap,
    DreamNow,
    Revert(u64),
    Export(ExportFormat),
    Delete,
    Read(ReadRequest, Slot),
    /// The entrance's brief, asked for at the memory state `key`.
    ReadEntrance {
        key: browse::BriefKey,
        at_ms: u64,
    },
    /// Forget the search: its text, room, scope and archive choice, and
    /// close what it showed.
    ClearSearch,
    /// Something the user states from this window.
    Statement(connecto_core::memory::UserStatement),
    /// A question to the memory.
    Ask(connecto_core::memory::AskRequest),
    /// One room's memories, asked for at the palace state `key`.
    ReadRoomSlot {
        room: Room,
        key: browse::BriefKey,
        at_ms: u64,
    },
}

/// The section's own state, one per Project Settings sheet.
#[derive(Default)]
pub(crate) struct MemoryPanel {
    pub(crate) tab: Tab,
    /// The toggle was turned on and the start is being chosen.
    pub(crate) enabling: bool,
    /// Why the last command was refused.
    pub(crate) error: Option<String>,
    pending: Option<Pending>,
    /// Text being typed in a field, by field key, while it has focus.
    drafts: HashMap<&'static str, String>,
    /// The options of the next initial build.
    pub(crate) boot: BootstrapOptions,
    pub(crate) export: ExportFormat,
    pub(crate) confirm: Option<Confirm>,
    pub(crate) browse: browse::Browse,
    /// A statement being written (accept, reject, correct, note).
    pub(crate) composer: Option<statement::Composer>,
    /// The last statement was recorded.
    pub(crate) said: Option<String>,
    /// The Ask row's state.
    pub(crate) ask: ask::AskDraft,
    /// The runs whose details are open, by run id.
    pub(crate) open_runs: std::collections::BTreeSet<u64>,
    /// The project this panel serves.
    pub(crate) project: Option<String>,
    /// What the last pass drew, for tests.
    #[cfg(test)]
    pub(crate) marks: crate::forms::Marks,
}

/// What every part of the section reads.
pub(super) struct Cx<'a> {
    pub(super) view: &'a MemoryView,
    pub(super) project: &'a ProjectMemoryView,
    /// The settings to show: the pending ones if any.
    pub(super) settings: &'a MemorySettings,
    pub(super) now_ms: u64,
    pub(super) offset_s: i32,
}

/// Draw the Memory section of a project's settings and send what the user
/// asked for to `control`.
pub(crate) fn section(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    control: &dyn MemoryControl,
    project_id: &str,
    now_ms: u64,
    offset_s: i32,
) {
    let view = control.view();
    // A panel serves one project: anything kept for another is dropped.
    if panel.project.as_deref() != Some(project_id) {
        if panel.project.is_some() {
            *panel = MemoryPanel::default();
        }
        panel.project = Some(project_id.to_owned());
    }
    // A result may have closed the draft or asked for a reload: draw from a
    // view that includes what it did.
    let view = if follow(panel, control, view.project(project_id), project_id) {
        control.view()
    } else {
        view
    };
    let mut out = Out::default();
    ui.push_id("project-memory", |ui| {
        draw(ui, panel, &view, project_id, now_ms, offset_s, &mut out);
    });
    let busy = view.project(project_id).is_some_and(|p| {
        p.bootstrap.planning
            || p.initializing()
            || p.cycles.iter().any(|c| c.running)
            || p.reads.iter().any(|r| r.state == ReadState::Pending)
            || p.asks.iter().any(|a| a.state.is_pending())
    });
    let sending = panel.composer.as_ref().is_some_and(|c| c.pending.is_some());
    if busy || sending || panel.pending.is_some() {
        ui.ctx().request_repaint_after(Duration::from_millis(500));
    }
    #[cfg(test)]
    {
        panel.marks = std::mem::take(&mut out.marks);
    }
    let last_error = view.project(project_id).and_then(|p| p.last_error.clone());
    // The data this frame saw: what every read sent now is stamped with.
    let asked = view
        .project(project_id)
        .map(browse::Stamp::of)
        .unwrap_or_default();
    for action in out.actions {
        ui.ctx().request_repaint();
        apply(
            panel,
            control,
            project_id,
            last_error.clone(),
            asked,
            action,
        );
    }
}

fn draw(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    view: &MemoryView,
    project_id: &str,
    now_ms: u64,
    offset_s: i32,
    out: &mut Out<MemoryAction>,
) {
    out.say(ui, heading("PROJECT MEMORY"));
    let Some(project) = view.project(project_id) else {
        if view.projects.is_empty() && view.capabilities.is_empty() {
            out.say(ui, muted("Project memory is not available in this build."));
        } else {
            out.say(ui, muted("Loading the memory of this project…"));
        }
        return;
    };
    settle(panel, project);
    let settings = panel
        .pending
        .as_ref()
        .map_or(&project.settings, |p| &p.settings)
        .clone();
    let cx = Cx {
        view,
        project,
        settings: &settings,
        now_ms,
        offset_s,
    };
    if let Some(error) = &panel.error {
        out.say(ui, problem(error.clone()));
    }
    if let Some(error) = &project.last_error {
        held(ui, out, Stop::Failed, "Last error", error);
    }
    toggle(ui, panel, &cx, out);
    if panel.enabling && !cx.settings.enabled {
        enable_choice(ui, panel, &cx, out);
        return;
    }
    if !cx.settings.enabled {
        // Any draft waits, with its text, until memory is on again.
        if let Some(c) = &mut panel.composer {
            c.suspend("Memory was turned off. Your text is kept.");
        }
        overview::disabled(ui, panel, &cx, out);
        return;
    }
    // The second level: smaller and indented under the Project Memory tab.
    ui.add_space(4.0);
    ui.indent("mem-tabs", |ui| {
        ui.horizontal_wrapped(|ui| {
            for tab in Tab::ALL {
                if out.sub_choice(ui, tab.key(), panel.tab == tab, tab.label()) {
                    panel.tab = tab;
                }
            }
        });
    });
    ui.add_space(6.0);
    if project.initializing() {
        out.say(
            ui,
            RichText::new("Initial build in progress: the memory is not complete yet, and agents are told so.")
                .size(11.5)
                .color(theme::AMBER),
        );
    }
    if panel.tab != Tab::Memories
        && let Some(c) = &mut panel.composer
        && c.bound.is_some()
    {
        c.suspend("You left the Memories tab.");
    }
    match panel.tab {
        Tab::Memories => browse::show(ui, panel, &cx, out),
        Tab::Activity => activity::show(ui, panel, &cx, out),
        Tab::Settings => settings::show(ui, panel, &cx, out),
    }
}

/// Drop the pending settings once the view shows them, or once the engine
/// reported a new error (it turned them down).
fn settle(panel: &mut MemoryPanel, project: &ProjectMemoryView) {
    let done = panel.pending.as_ref().is_some_and(|p| {
        p.settings == project.settings
            || (project.last_error.is_some() && project.last_error != p.error)
    });
    if done {
        panel.pending = None;
    }
    if project.settings.enabled {
        panel.enabling = false;
    }
}

fn toggle(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let mut on = cx.settings.enabled || panel.enabling;
    let response = ui.checkbox(&mut on, "Keep a memory for this project");
    out.mark("mem-toggle", response.rect);
    if response.changed() {
        if on {
            panel.enabling = true;
        } else if cx.settings.enabled {
            let mut settings = cx.settings.clone();
            settings.enabled = false;
            out.actions.push(MemoryAction::Settings(settings));
        } else {
            panel.enabling = false;
        }
    }
    out.say(
        ui,
        muted("Agents in this project read it with `connecto memory` and add notes to it. It lives outside the repository and is off until you turn it on. Turning it off stops capture and both cycles; the data stays."),
    );
}

/// The start the user picks when turning memory on (spec §5.9, §8.3).
fn enable_choice(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    crate::forms::card(ui, |ui| {
        out.say(
            ui,
            RichText::new("How should the memory start?")
                .strong()
                .color(theme::TEXT),
        );
        let mut on = cx.settings.clone();
        on.enabled = true;
        let build = ui.add(theme::primary(
            "Build the palace from the current project (recommended)",
        ));
        out.mark("mem-enable-build", build.rect);
        out.say(
            ui,
            muted("Reads documentation, manifests, code, test setup, git state and history, and Connecto conversations, within limits you see before it starts. Uses the dream cycle's model."),
        );
        if build.clicked() {
            out.actions.push(MemoryAction::Settings(on.clone()));
            out.actions
                .push(MemoryAction::PlanBootstrap(panel.boot.clone()));
            panel.tab = Tab::Activity;
        }
        ui.add_space(4.0);
        if out.button(ui, "mem-enable-empty", "Start empty", true) {
            out.actions.push(MemoryAction::Settings(on));
            panel.tab = Tab::Memories;
        }
        out.say(
            ui,
            muted("The memory fills as you work. You can build it from the project later, from Initial build."),
        );
        ui.add_space(4.0);
        if out.button(ui, "mem-enable-cancel", "Cancel", true) {
            panel.enabling = false;
        }
    });
}

/// Why work stopped, as the overview and the build tab call it out.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Stop {
    /// A run failed, or the engine reported an error.
    Failed,
    /// A run is on hold ([`RunOutcome::Held`]): it used no attempt and
    /// resumes once the cause is gone, typically a CLI that has not trusted
    /// the project folder yet.
    Held,
    /// A run proposed operations and none was kept ([`RunOutcome::Refused`]).
    /// Not success: the palace is as it was, and the report says why.
    Refused,
}

/// A reason the engine gave for failing or holding work, drawn so it cannot
/// be missed. A held run says what to do about it.
pub(super) fn held(ui: &mut Ui, out: &mut Out<MemoryAction>, stop: Stop, what: &str, reason: &str) {
    let color = match stop {
        Stop::Failed => theme::ERROR,
        Stop::Held | Stop::Refused => theme::AMBER,
    };
    out.say(
        ui,
        RichText::new(format!("{what}: {reason}"))
            .size(13.0)
            .strong()
            .color(color),
    );
    match stop {
        Stop::Held => out.say(ui, crate::forms::warning(HELD_HINT)),
        Stop::Refused => out.say(ui, crate::forms::warning(REFUSED_HINT)),
        Stop::Failed => {}
    }
}

/// What a refused run means, and where its reasons are.
pub(super) const REFUSED_HINT: &str = "Nothing was added to the memory: the model's proposals were checked, and none passed. While the app stays open, the reasons are listed in the last report under Runs and versions.";

/// What to do about a run on hold.
pub(super) const HELD_HINT: &str = "It used no attempt and resumes by itself once the cause is solved. If the CLI has not trusted this project's folder yet, open a terminal for this project with that CLI once and accept its question about trusting the folder.";

/// What stopped work in this project, by what stopped: the initial build's
/// state, then each cycle's latest run (held or failed).
pub(super) fn stops(cx: &Cx<'_>) -> Vec<(Stop, String, String)> {
    let mut list: Vec<(Stop, String, String)> = Vec::new();
    if let BootstrapState::Failed { reason } = &cx.project.bootstrap.state {
        list.push((
            Stop::Failed,
            "Initial build failed".to_owned(),
            reason.clone(),
        ));
    }
    for state in &cx.project.cycles {
        let found = match state.last.as_ref().map(|r| &r.outcome) {
            Some(RunOutcome::Held { reason }) => Some((Stop::Held, "on hold", reason)),
            Some(RunOutcome::Failed { reason }) => Some((Stop::Failed, "last run failed", reason)),
            Some(RunOutcome::Refused { reason, .. }) => {
                Some((Stop::Refused, "nothing kept", reason))
            }
            _ => None,
        };
        if let Some((stop, word, reason)) = found
            && !list.iter().any(|(_, _, r)| r == reason)
        {
            let what = if state.cycle == Cycle::Bootstrap {
                format!("Initial build, {word}")
            } else {
                format!("{} cycle, {word}", state.cycle.label())
            };
            list.push((stop, what, reason.clone()));
        }
    }
    list
}

/// The latest run's refusal: the run it is, how many it proposed, and why. The
/// latest run is the newest by start across all cycles; an older refusal is
/// not described as the last run.
pub(super) struct Refusal<'a> {
    pub(super) run: u64,
    pub(super) proposed: u32,
    pub(super) reason: &'a str,
}

pub(super) fn refusal<'a>(cx: &'a Cx<'_>) -> Option<Refusal<'a>> {
    let latest = cx
        .project
        .runs
        .iter()
        .chain(cx.project.cycles.iter().filter_map(|c| c.last.as_ref()))
        .max_by_key(|r| (r.started_ms, r.id))?;
    match &latest.outcome {
        RunOutcome::Refused { proposed, reason } => Some(Refusal {
            run: latest.id,
            proposed: *proposed,
            reason,
        }),
        _ => None,
    }
}

/// The last report, only when it is the report of `run`: a report of another
/// run is not offered as the reasons for this one.
pub(super) fn report_for<'a>(cx: &'a Cx<'_>, run: u64) -> Option<&'a DreamReport> {
    cx.project.last_report.as_deref().filter(|r| r.run == run)
}

/// The user left the Project Memory tab: a bound draft waits there, with its
/// text, until it is resumed on the Memories tab.
pub(crate) fn leave(panel: &mut MemoryPanel) {
    if let Some(c) = &mut panel.composer
        && c.bound.is_some()
    {
        c.suspend("You left Project Memory. Your text is kept.");
    }
}

/// Work the section does on every frame, whichever tab is on show: a
/// statement's result reaches its draft, and a pending one keeps the window
/// repainting. Without it a result waits until the Memory tab is drawn again.
pub(crate) fn poll(
    ctx: &egui::Context,
    panel: &mut MemoryPanel,
    control: &dyn MemoryControl,
    project_id: &str,
) {
    let view = control.view();
    follow(panel, control, view.project(project_id), project_id);
    if panel.composer.as_ref().is_some_and(|c| c.pending.is_some()) {
        ctx.request_repaint_after(Duration::from_millis(500));
    }
}

/// Apply the result of the statement a draft waits for, by its typed state.
/// Runs every frame, on any tab, so a result that arrives while the user is
/// elsewhere reaches the (suspended) draft. A result for a draft already
/// closed is ignored: only the id the draft holds is looked at.
fn follow(
    panel: &mut MemoryPanel,
    control: &dyn MemoryControl,
    project: Option<&ProjectMemoryView>,
    project_id: &str,
) -> bool {
    use connecto_core::memory::{StatementRefusal as R, StatementState as S};
    let Some(id) = panel.composer.as_ref().and_then(|c| c.pending) else {
        return false;
    };
    let Some(p) = project else {
        if let Some(c) = &mut panel.composer {
            c.pending = None;
            c.error = Some(
                "The memory of this project went away before an answer came: your statement may not have been recorded. Your text is kept.".to_owned(),
            );
        }
        return true;
    };
    let state = match p.statement(id) {
        Some(r) if r.state.is_final() => r.state.clone(),
        Some(_) => return false,
        None => {
            if let Some(c) = &mut panel.composer {
                c.pending = None;
                c.error = Some(
                    "No answer came for your statement: it may not have been recorded. Your text is kept.".to_owned(),
                );
            }
            return true;
        }
    };
    let Some(c) = &mut panel.composer else {
        return true;
    };
    c.pending = None;
    match state {
        S::Pending => {}
        S::Recorded {
            applied: true,
            version,
            ..
        } => {
            panel.composer = None;
            panel.said = Some(match version {
                Some(v) => format!("Recorded as your statement, in version {v}."),
                None => "Recorded as your statement.".to_owned(),
            });
        }
        S::Recorded {
            applied: false,
            note,
            ..
        } => {
            panel.composer = None;
            let mut said =
                "Recorded as an open note; it will be considered at the next dream.".to_owned();
            if let Some(note) = note {
                said.push(' ');
                said.push_str(&note);
            }
            panel.said = Some(said);
        }
        S::Refused(R::Changed) => {
            let shown = c.bound.as_ref().map(|b| b.shown.clone());
            c.error = None;
            c.confirming = false;
            c.draft = statement::Draft::Suspended {
                why: format!(
                    "{} It is being reloaded: choose the action again under it as it is now. Your text is kept.",
                    R::Changed.sentence()
                ),
                changed: None,
            };
            // Reload the memory once, if it is the one on show.
            let on_show = panel.browse.detail.clone().filter(|d| {
                matches!(&d.request, ReadRequest::Open(connecto_core::memory::Lookup::Memory(m)) if Some(m) == shown.as_ref())
            });
            if let Some(d) = on_show
                && let Ok(read) = control.request_read(project_id, d.request.clone())
            {
                panel
                    .browse
                    .landed(Slot::Detail, d.request, read, browse::Stamp::of(p));
            }
        }
        S::Refused(R::AlreadyStated) => {
            c.confirming = false;
            c.error = Some(format!(
                "{} Change the text, or cancel the draft.",
                R::AlreadyStated.sentence()
            ));
        }
        S::Refused(R::Busy) => {
            c.confirming = false;
            c.error = Some("A run is publishing; try again when it finishes.".to_owned());
        }
        S::Refused(other) => {
            c.confirming = false;
            c.error = Some(other.sentence());
        }
    }
    true
}

/// Send one action to the control. A refusal shows at the top of the section.
fn apply(
    panel: &mut MemoryPanel,
    control: &dyn MemoryControl,
    project_id: &str,
    last_error: Option<String>,
    asked: browse::Stamp,
    action: MemoryAction,
) {
    panel.error = None;
    let result = match action {
        MemoryAction::Settings(settings) => {
            let settings = settings.clamped();
            let result = control.set_settings(project_id, settings.clone());
            if result.is_ok() {
                panel.pending = Some(Pending {
                    settings,
                    error: last_error,
                });
            } else {
                panel.enabling = false;
            }
            result
        }
        MemoryAction::PlanBootstrap(options) => {
            control.plan_bootstrap(project_id, options.clamped())
        }
        MemoryAction::Bootstrap(options) => control.bootstrap(project_id, options.clamped()),
        MemoryAction::CancelBootstrap => {
            control.cancel_bootstrap(project_id);
            Ok(())
        }
        MemoryAction::ResumeBootstrap => control.resume_bootstrap(project_id),
        MemoryAction::DreamNow => control.run_dream_now(project_id),
        MemoryAction::Revert(version) => {
            panel.confirm = None;
            control.revert(project_id, version)
        }
        MemoryAction::Export(format) => control.export(project_id, format),
        MemoryAction::Delete => {
            panel.confirm = None;
            control.delete_memory(project_id)
        }
        MemoryAction::Read(request, slot) => {
            let result = control.request_read(project_id, request.clone());
            match &result {
                Ok(id) => panel.browse.landed(slot, request, *id, asked),
                // A draft waiting for its memory to reopen can try again.
                Err(why) => {
                    if let Some(c) = &mut panel.composer
                        && c.draft == statement::Draft::Resuming
                    {
                        c.draft = statement::Draft::Suspended {
                            why: format!("Could not reopen it: {why}"),
                            changed: None,
                        };
                    }
                }
            }
            result.map(|_| ())
        }
        MemoryAction::ReadEntrance { key, at_ms } => {
            let started = control.request_read(project_id, ReadRequest::Brief);
            panel.browse.cache.entrance.started(started, key, at_ms);
            Ok(())
        }
        MemoryAction::ClearSearch => {
            panel.browse.clear_search();
            Ok(())
        }
        MemoryAction::ReadRoomSlot { room, key, at_ms } => {
            let started = control.request_read(project_id, ReadRequest::Room(room));
            panel
                .browse
                .cache
                .room_mut(room)
                .started(started, key, at_ms);
            Ok(())
        }
        MemoryAction::Ask(request) => {
            // The engine refuses a question it cannot take (memory off, too
            // many waiting); a refusal stays in the row, with the text.
            match control.ask(project_id, request) {
                Ok(_) => {
                    panel.ask.text.clear();
                    panel.ask.error = None;
                }
                Err(why) => panel.ask.error = Some(why),
            }
            Ok(())
        }
        MemoryAction::Statement(statement) => {
            // Checked again here: nothing invalid, and nothing from a draft
            // that is not on show, leaves the window.
            // Never a second send while one waits for its result.
            let on_show = panel.composer.as_ref().is_some_and(|c| {
                c.is_active() && c.pending.is_none() && c.statement() == statement
            });
            let result = if on_show {
                statement
                    .check()
                    .map_err(|why| format!("{why}."))
                    .and_then(|()| control.user_statement(project_id, statement))
            } else {
                Err("This draft is not on show any more.".to_owned())
            };
            if let Some(c) = &mut panel.composer {
                c.sending = false;
                match result {
                    // Accepted for processing: the draft waits for the result.
                    Ok(id) => {
                        c.pending = Some(id);
                        c.error = None;
                    }
                    // Known at once: the text stays.
                    Err(why) => {
                        c.error = Some(why);
                        c.confirming = false;
                    }
                }
            } else if let Err(why) = result {
                panel.error = Some(why);
            }
            Ok(())
        }
    };
    if let Err(error) = result {
        panel.error = Some(error);
    }
}

/// A whole-number field. Returns `Some` when the user finished typing (Enter
/// or focus left): the clamped new value, or why the text is not a number.
pub(super) fn number(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    out: &mut Out<MemoryAction>,
    key: &'static str,
    value: u64,
    range: &RangeInclusive<u64>,
) -> Option<Result<u64, String>> {
    let typed = field(ui, panel, out, key, &value.to_string(), 96.0)?;
    let cleaned: String = typed
        .chars()
        .filter(|c| !matches!(c, ',' | '_' | ' ' | '\''))
        .collect();
    match cleaned.parse::<u64>() {
        Ok(n) => {
            let n = n.clamp(*range.start(), *range.end());
            (n != value).then_some(Ok(n))
        }
        Err(_) => Some(Err(format!(
            "Type a whole number from {} to {}.",
            text::count(*range.start()),
            text::count(*range.end())
        ))),
    }
}

/// An `HH:MM` field, as [`number`].
pub(super) fn time(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    out: &mut Out<MemoryAction>,
    key: &'static str,
    value: LocalTime,
) -> Option<Result<LocalTime, String>> {
    let typed = field(ui, panel, out, key, &value.to_string(), 64.0)?;
    match LocalTime::parse(typed.trim()) {
        Some(t) => (t != value).then_some(Ok(t)),
        None => Some(Err(
            "Type a time as HH:MM, 24-hour (for example 02:30).".to_owned()
        )),
    }
}

/// A one-line field that keeps what is typed while it has focus. Returns the
/// text when typing finished.
pub(super) fn field(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    out: &mut Out<MemoryAction>,
    key: &'static str,
    value: &str,
    width: f32,
) -> Option<String> {
    field_hinted(ui, panel, out, key, value, width, "")
}

/// [`field`], with a hint shown while the field is empty.
pub(super) fn field_hinted(
    ui: &mut Ui,
    panel: &mut MemoryPanel,
    out: &mut Out<MemoryAction>,
    key: &'static str,
    value: &str,
    width: f32,
    hint: &str,
) -> Option<String> {
    let mut text = panel
        .drafts
        .get(key)
        .cloned()
        .unwrap_or_else(|| value.to_owned());
    let response = ui.add(
        TextEdit::singleline(&mut text)
            .id(Id::unique(("memory-field", key)))
            .desired_width(width)
            .margin(theme::FIELD_MARGIN)
            .hint_text(hint),
    );
    out.mark(key, response.rect);
    if response.has_focus() {
        panel.drafts.insert(key, text);
        return None;
    }
    panel.drafts.remove(key);
    response.lost_focus().then_some(text)
}

/// A labelled row: the label, then what `add` draws.
pub(super) fn row(
    ui: &mut Ui,
    out: &mut Out<MemoryAction>,
    label: &str,
    add: impl FnOnce(&mut Ui, &mut Out<MemoryAction>),
) {
    ui.horizontal_wrapped(|ui| {
        out.say(ui, RichText::new(label).color(theme::TEXT));
        add(ui, out);
    });
}
