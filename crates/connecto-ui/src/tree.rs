//! The sidebar: the tree of open projects and their terminals, or the narrow
//! rail of project tiles when it is collapsed (project-tree-ui R15).
//!
//! Drawing only reads the views and returns `Command`s. Nothing here talks to
//! a session, so a click can never type into a terminal.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Instant;

use connecto_core::{
    BackgroundWork, LONG_RUNNING, MAX_TERMINALS, ProjectView, TermActivity, TerminalEntry,
    TerminalsView, ToolKind, short_age,
};
use egui::epaint::text::TextWrapping;
use egui::text::LayoutJob;
use egui::{
    Align2, Color32, FontId, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, pos2,
    vec2,
};

use crate::app::Snapshot;
use crate::commands::{Command, GroupFilter};
use crate::dots::Dot;
use crate::header;
use crate::icons;
use crate::rename::{Rename, RenameEnd, RenameTarget};
use crate::theme;

pub(crate) const SIDEBAR_WIDTH: f32 = 286.0;
pub(crate) const RAIL_WIDTH: f32 = 60.0;
/// The band the macOS traffic lights sit in; none off macOS.
pub(crate) const LIGHTS_BAND: f32 = crate::platform::LIGHTS_BAND;
const FOOT_HEIGHT: f32 = 46.0;
const PROJECT_ROW: f32 = 32.0;
const TERMINAL_ROW: f32 = 40.0;
const CHILD_ROW: f32 = 38.0;
/// One task of an open background list.
const BACKGROUND_ROW: f32 = 20.0;
const NEW_ROW: f32 = 28.0;
const GROUP_ROW: f32 = 26.0;
/// How far right a project sits under its group's header.
const GROUP_INDENT: f32 = 14.0;
const FILTER_ROW: f32 = 28.0;
const SIDE_PAD: f32 = 8.0;
/// Room left of the group filter and the group headers.
const LEFT_INSET: f32 = crate::hosts::LEFT_INSET;
const ICON_BUTTON: f32 = 24.0;

/// A control, named so tests can find where it was drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hit {
    Project(String),
    Twisty(String),
    Clock(String),
    Gear(String),
    Ops(String),
    Terminal(String, String),
    Close(String, String),
    Robot(String, String),
    Kids(String, String),
    /// A terminal's stop button for its background shells, or one task's.
    StopBackground(String, String, Option<String>),
    /// The arrow that opens a terminal's list of background tasks.
    BackgroundList(String, String),
    NewRow(String),
    HeaderHistory,
    HeaderPlus,
    /// The items of the + menu.
    PlusOpenProject,
    PlusNewProject,
    PlusNewGroup,
    /// The group filter above the projects, and each of its choices: `all`,
    /// `ungrouped` or a group id.
    GroupFilter,
    GroupFilterItem(String),
    /// A group's header row, by id.
    Group(String),
    /// The drop zone that takes a dragged project out of its group.
    Ungrouped,
    Collapse,
    Pause,
    Tile(String),
    Header,
}

#[derive(Default)]
pub(crate) struct TreeOut {
    pub(crate) commands: Vec<Command>,
    /// A parent whose children the user folded or unfolded.
    pub(crate) toggled_kids: Vec<(String, String)>,
    pub(crate) rename_end: Option<RenameEnd>,
    /// A rename field was drawn this pass.
    pub(crate) rename_drawn: bool,
    /// Where each project's "New terminal" row is, to anchor the chooser.
    pub(crate) new_rows: HashMap<String, Rect>,
    #[cfg(test)]
    pub(crate) hits: Vec<(Hit, Rect)>,
    /// Where the host rows and their add button were drawn.
    #[cfg(test)]
    pub(crate) host_rects: crate::hosts::RowRects,
}

impl TreeOut {
    #[cfg(test)]
    fn note(&mut self, hit: impl FnOnce() -> Hit, rect: Rect) {
        self.hits.push((hit(), rect));
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    fn note(&mut self, _hit: impl FnOnce() -> Hit, _rect: Rect) {}
}

pub(crate) struct TreeInput<'a> {
    pub(crate) snap: &'a Snapshot,
    /// Index of the project shown.
    pub(crate) active: Option<usize>,
    /// The terminal shown in the active project.
    pub(crate) shown_terminal: Option<&'a str>,
    /// Turns each terminal showed when the user last saw it.
    pub(crate) seen: &'a HashMap<(String, String), u64>,
    pub(crate) collapsed_kids: &'a HashSet<(String, String)>,
    pub(crate) paused: bool,
    pub(crate) history_open: bool,
    /// Terminals with a title asked for that has not finished.
    pub(crate) titling: &'a HashMap<(String, String), u64>,
    pub(crate) time: f64,
    /// The saved remote hosts, drawn above the projects.
    pub(crate) hosts: &'a [connecto_core::HostRow],
    /// The remote host shown; `None` is this machine.
    pub(crate) host: Option<&'a str>,
    /// Which projects are listed when there are groups.
    pub(crate) group_filter: &'a GroupFilter,
    /// Groups whose projects are folded away.
    pub(crate) folded_groups: &'a HashSet<String>,
    /// Projects with operations, by id: whether something waits for the
    /// person (proposals, or the weekly suggestion).
    pub(crate) ops: &'a HashMap<String, bool>,
}

impl TreeInput<'_> {
    /// Groups apply: a group made, on this machine or on the host shown.
    fn grouping(&self) -> bool {
        !self.snap.ws.groups.is_empty()
    }

    /// Groups can be changed here: this machine's. A host's groups are its
    /// own, shown as it has them.
    fn regrouping(&self) -> bool {
        self.grouping() && !self.snap.is_remote()
    }

    /// The group a project counts as in: none when its group is gone.
    fn group_of<'p>(&self, project: &'p ProjectView) -> Option<&'p str> {
        project
            .group
            .as_deref()
            .filter(|id| self.snap.ws.groups.iter().any(|g| g.id == *id))
    }

    /// Whether the filter lists a project.
    fn listed(&self, project: &ProjectView) -> bool {
        if !self.grouping() {
            return true;
        }
        match self.group_filter {
            GroupFilter::All => true,
            GroupFilter::Group(id) => self.group_of(project) == Some(id.as_str()),
            GroupFilter::Ungrouped => self.group_of(project).is_none(),
        }
    }
}

/// What dragging a project row carries.
#[derive(Debug, Clone)]
struct ProjectDrag {
    id: String,
    group: Option<String>,
}

/// A terminal in the order the tree draws it.
pub(crate) struct Node<'a> {
    pub(crate) entry: &'a TerminalEntry,
    pub(crate) child: bool,
}

/// Top-level terminals in list order, each followed by its children. A child
/// whose parent is gone is drawn at top level.
pub(crate) fn order(view: &TerminalsView) -> Vec<Node<'_>> {
    let mut nodes = Vec::with_capacity(view.terminals.len());
    for top in view
        .terminals
        .iter()
        .filter(|entry| parent_of(view, entry).is_none())
    {
        nodes.push(Node {
            entry: top,
            child: false,
        });
        nodes.extend(
            view.terminals
                .iter()
                .filter(|entry| parent_of(view, entry) == Some(top.id.as_str()))
                .map(|entry| Node { entry, child: true }),
        );
    }
    nodes
}

pub(crate) fn is_unseen(
    seen: &HashMap<(String, String), u64>,
    project: &str,
    entry: &TerminalEntry,
) -> bool {
    seen.get(&(project.to_owned(), entry.id.clone()))
        .is_some_and(|seen| entry.finished_turns > *seen)
}

pub(crate) fn terminal_dot(
    seen: &HashMap<(String, String), u64>,
    project: &str,
    entry: &TerminalEntry,
) -> Dot {
    Dot::of(entry.state, is_unseen(seen, project, entry))
}

/// The dot a project row shows: its most urgent terminal.
pub(crate) fn project_dot(
    seen: &HashMap<(String, String), u64>,
    project: &ProjectView,
    view: Option<&TerminalsView>,
) -> Dot {
    if project.error.is_some() {
        return Dot::Failed;
    }
    let Some(view) = view else {
        return Dot::Starting;
    };
    Dot::most_urgent(
        view.terminals
            .iter()
            .map(|entry| terminal_dot(seen, &project.id, entry)),
    )
    .unwrap_or(Dot::Hollow)
}

/// The display name of a terminal.
pub(crate) fn title(entry: &TerminalEntry) -> &str {
    entry.name.as_deref().unwrap_or_else(|| entry.display_id())
}

/// Fit one line of text to `width`, ending in an ellipsis if it is cut.
pub(crate) fn fit(
    painter: &egui::Painter,
    text: &str,
    font: FontId,
    color: Color32,
    width: f32,
) -> Arc<egui::Galley> {
    let mut job = LayoutJob::simple_singleline(text.to_owned(), font, color);
    job.wrap = TextWrapping::truncate_at_width(width.max(1.0));
    painter.layout_job(job)
}

/// The two letters on a project's tile.
pub(crate) fn monogram(name: &str) -> String {
    let mut letters = name
        .split(|c: char| !c.is_alphanumeric())
        .filter_map(|word| word.chars().next());
    let first = letters.next();
    let second = letters.next();
    match (first, second) {
        (Some(a), Some(b)) => format!("{a}{b}").to_uppercase(),
        (Some(a), None) => {
            let rest: String = name
                .chars()
                .filter(|c| c.is_alphanumeric())
                .skip(1)
                .take(1)
                .collect();
            format!("{a}{rest}").to_uppercase()
        }
        _ => "?".to_owned(),
    }
}

fn state_words(entry: &TerminalEntry, dot: Dot) -> &'static str {
    match entry.state {
        TermActivity::NotStarted => "not started",
        TermActivity::Exited => "exited",
        _ => dot.word(),
    }
}

/// An icon-sized button with a hover or pressed background.
fn icon_button(
    ui: &mut Ui,
    center: Pos2,
    id: &'static str,
    tip: &str,
    on: bool,
    paint: impl FnOnce(&egui::Painter, Pos2, Color32),
) -> Response {
    let rect = Rect::from_center_size(center, Vec2::splat(ICON_BUTTON));
    let response = ui.interact(rect, Id::unique(id), Sense::click());
    let painter = ui.painter();
    if on {
        painter.rect_filled(rect, 6.0, theme::RAISED_BG);
        painter.rect_stroke(rect, 6.0, Stroke::new(1.0, theme::TEAL), StrokeKind::Inside);
    } else if response.hovered() {
        painter.rect_filled(rect, 6.0, theme::ROW_HOVER);
    }
    let color = if on || response.hovered() {
        theme::TEXT
    } else {
        theme::TEXT_MUTED
    };
    paint(painter, center, color);
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::accesskit::Role::Button, true, tip.to_owned())
    });
    response.on_hover_text(tip)
}

pub(crate) fn sidebar(
    ui: &mut Ui,
    rect: Rect,
    input: &TreeInput<'_>,
    rename: Option<&mut Rename>,
    out: &mut TreeOut,
) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::SIDEBAR_BG);
    painter.vline(
        rect.max.x - 0.5,
        rect.y_range(),
        Stroke::new(1.0, theme::SIDEBAR_BORDER),
    );

    // The header is the window's drag area. It is registered first, so the
    // buttons on it stay on top and keep their clicks.
    // History, + and the host switcher share one row above the scrolling
    // list. The switcher names what the list shows, so the list needs no
    // heading of its own.
    let hosts_height = crate::hosts::section_height();
    let header_rect = Rect::from_min_size(rect.min, vec2(rect.width(), LIGHTS_BAND + hosts_height));
    let drag = ui.interact(
        header_rect,
        Id::unique("sidebar-drag"),
        Sense::click_and_drag(),
    );
    out.note(|| Hit::Header, header_rect);

    let row_y = rect.min.y + LIGHTS_BAND + crate::hosts::DROPDOWN_HEIGHT / 2.0;
    let last_x = rect.max.x - 12.0 - ICON_BUTTON / 2.0;
    // The buttons left of the switcher, from the list's left edge.
    let mut next_x = rect.min.x + SIDE_PAD + LEFT_INSET + ICON_BUTTON / 2.0;
    // Without a band for the traffic lights (off macOS), the collapse button
    // joins the row too, first.
    let collapse_at = if LIGHTS_BAND > 0.0 {
        pos2(last_x, rect.min.y + LIGHTS_BAND / 2.0)
    } else {
        let at = pos2(next_x, row_y);
        next_x += ICON_BUTTON + 2.0;
        at
    };
    let collapse = icon_button(
        ui,
        collapse_at,
        "sidebar-collapse",
        concat!("Hide sidebar  ", crate::platform::kbd!("\\")),
        false,
        icons::sidebar,
    );
    let clock = icon_button(
        ui,
        pos2(next_x, row_y),
        "sidebar-history",
        concat!("History  ", crate::platform::kbd!("Y")),
        input.history_open,
        icons::clock,
    );
    let plus = icon_button(
        ui,
        pos2(next_x + ICON_BUTTON + 2.0, row_y),
        "sidebar-open",
        "Open project or new group",
        false,
        |p, c, col| icons::plus(p, c, 5.0, col),
    );
    out.note(|| Hit::Collapse, collapse.rect);
    out.note(|| Hit::HeaderPlus, plus.rect);
    out.note(|| Hit::HeaderHistory, clock.rect);
    if collapse.clicked() {
        out.commands.push(Command::ToggleSidebar);
    }
    let plus_rect = plus.rect;
    egui::Popup::menu(&plus).show(|ui| {
        let open = ui.button(concat!("Open project…  ", crate::platform::kbd!("O")));
        out.note(|| Hit::PlusOpenProject, open.rect);
        if open.clicked() {
            out.commands.push(Command::OpenProject);
        }
        let new = ui.button("New project…");
        out.note(|| Hit::PlusNewProject, new.rect);
        if new.clicked() {
            out.commands.push(Command::NewProject);
        }
        let group = ui.button("New group…");
        out.note(|| Hit::PlusNewGroup, group.rect);
        if group.clicked() {
            out.commands.push(Command::NewGroup);
        }
    });
    if clock.clicked() {
        out.commands.push(Command::History);
    }
    header::window_gestures(ui, &drag, &[collapse.rect, plus_rect, clock.rect]);
    // The switcher keeps its own inset as the gap after +.
    let hosts_rect = Rect::from_min_max(
        pos2(plus_rect.max.x + 2.0, rect.min.y + LIGHTS_BAND),
        pos2(
            rect.max.x - SIDE_PAD - 1.0,
            rect.min.y + LIGHTS_BAND + hosts_height,
        ),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(hosts_rect), |ui| {
        ui.spacing_mut().item_spacing.y = 1.0;
        crate::hosts::section(
            ui,
            input.hosts,
            input.host,
            &mut out.commands,
            #[cfg(test)]
            &mut out.host_rects,
        );
    });

    let body = Rect::from_min_max(
        pos2(rect.min.x, header_rect.max.y),
        pos2(rect.max.x - 1.0, rect.max.y - FOOT_HEIGHT),
    );
    let mut tree = Tree { input, out, rename };
    ui.scope_builder(egui::UiBuilder::new().max_rect(body), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("project-tree")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // The same margin on both sides, so the list lines up with the
                // host switcher above it.
                egui::Frame::NONE
                    .inner_margin(egui::Margin {
                        left: SIDE_PAD as i8,
                        ..egui::Margin::ZERO
                    })
                    .show(ui, |ui| {
                        ui.spacing_mut().item_spacing.y = 1.0;
                        ui.set_width(body.width() - SIDE_PAD * 2.0);
                        tree.body(ui);
                    });
            });
    });

    footer(ui, rect, input, out);
}

fn footer(ui: &mut Ui, rect: Rect, input: &TreeInput<'_>, out: &mut TreeOut) {
    let foot = Rect::from_min_max(
        pos2(rect.min.x, rect.max.y - FOOT_HEIGHT),
        pos2(rect.max.x - 1.0, rect.max.y),
    );
    let painter = ui.painter_at(foot);
    painter.hline(foot.x_range(), foot.min.y, Stroke::new(1.0, theme::HAIR));
    let toggle = Rect::from_center_size(
        pos2(foot.max.x - 12.0 - 15.0, foot.center().y),
        vec2(30.0, 17.0),
    );
    let (label, color) = if input.paused {
        ("Agents paused", theme::AMBER)
    } else {
        ("Pause agents", theme::TEXT_MUTED)
    };
    painter.text(
        pos2(foot.min.x + 16.0, foot.center().y),
        Align2::LEFT_CENTER,
        label,
        FontId::proportional(12.0),
        color,
    );
    pause_toggle(ui, toggle, input.paused, out);
}

fn pause_toggle(ui: &mut Ui, rect: Rect, paused: bool, out: &mut TreeOut) {
    let response = ui.interact(rect, Id::unique("pause-toggle"), Sense::click());
    let (fill, border, knob, x) = if paused {
        (theme::TEAL, theme::CYAN, theme::CYAN, rect.max.x - 8.5)
    } else {
        (
            theme::RAISED_BG,
            theme::TEAL,
            theme::TEXT_MUTED,
            rect.min.x + 8.5,
        )
    };
    let painter = ui.painter();
    painter.rect_filled(rect, 99.0, fill);
    painter.rect_stroke(rect, 99.0, Stroke::new(1.0, border), StrokeKind::Inside);
    painter.circle_filled(pos2(x, rect.center().y), 5.5, knob);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::accesskit::Role::Switch,
            true,
            paused,
            "Pause agents".to_owned(),
        )
    });
    out.note(|| Hit::Pause, rect);
    if response
        .on_hover_text(concat!("Pause all agents  ", crate::platform::kbd!(".")))
        .clicked()
    {
        out.commands.push(Command::TogglePause);
    }
}

struct Tree<'a, 'b> {
    input: &'a TreeInput<'a>,
    out: &'b mut TreeOut,
    rename: Option<&'b mut Rename>,
}

impl Tree<'_, '_> {
    fn body(&mut self, ui: &mut Ui) {
        let snap = self.input.snap;

        let new_group = self
            .rename
            .as_deref()
            .is_some_and(|rename| rename.target == RenameTarget::NewGroup);
        if snap.ws.projects.is_empty() {
            ui.add_space(8.0);
            let (rect, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), 40.0), Sense::hover());
            ui.painter().text(
                rect.left_top() + vec2(8.0, 0.0),
                Align2::LEFT_TOP,
                "No project is open.",
                FontId::proportional(12.5),
                theme::TEXT_MUTED,
            );
            ui.painter().text(
                rect.left_top() + vec2(8.0, 18.0),
                Align2::LEFT_TOP,
                crate::platform::NO_PROJECT_HINT,
                FontId::proportional(12.0),
                theme::FAINT,
            );
            // Groups can be made before any project is open.
            if !new_group && !self.input.grouping() {
                return;
            }
        }
        if !self.input.grouping() {
            if new_group {
                self.new_group_row(ui);
            }
            for index in 0..snap.ws.projects.len() {
                self.entry(ui, index);
            }
            ui.add_space(8.0);
            return;
        }
        self.filter_row(ui);
        match self.input.group_filter {
            GroupFilter::All => {
                if new_group {
                    self.new_group_row(ui);
                }
                for group in &snap.ws.groups {
                    let members: Vec<usize> = (0..snap.ws.projects.len())
                        .filter(|i| self.input.group_of(&snap.ws.projects[*i]) == Some(&group.id))
                        .collect();
                    let folded = self.input.folded_groups.contains(&group.id);
                    self.group_row(ui, Some(group), members.len(), folded);
                    if !folded {
                        for index in members {
                            self.grouped_entry(ui, index);
                        }
                    }
                }
                let loose: Vec<usize> = (0..snap.ws.projects.len())
                    .filter(|i| self.input.group_of(&snap.ws.projects[*i]).is_none())
                    .collect();
                // The projects in no group follow without a header. While a
                // project in a group is dragged, a drop zone is the way out.
                let leaving = egui::DragAndDrop::payload::<ProjectDrag>(ui.ctx())
                    .is_some_and(|drag| drag.group.is_some());
                if leaving {
                    self.group_row(ui, None, loose.len(), false);
                }
                for index in loose {
                    self.entry(ui, index);
                }
            }
            GroupFilter::Group(_) | GroupFilter::Ungrouped => {
                let mut any = false;
                for (index, project) in snap.ws.projects.iter().enumerate() {
                    if self.input.listed(project) {
                        any = true;
                        self.entry(ui, index);
                    }
                }
                if !any {
                    ui.add_space(6.0);
                    let (rect, _) =
                        ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
                    ui.painter().text(
                        rect.left_center() + vec2(8.0, 0.0),
                        Align2::LEFT_CENTER,
                        "No project here.",
                        FontId::proportional(12.0),
                        theme::FAINT,
                    );
                }
            }
        }
        ui.add_space(8.0);
    }

    /// A project under its group's header: drawn a step to the right, so it
    /// reads as part of the group.
    fn grouped_entry(&mut self, ui: &mut Ui, index: usize) {
        ui.scope(|ui| {
            ui.spacing_mut().indent = GROUP_INDENT;
            ui.indent(("group-member", index), |ui| {
                ui.spacing_mut().item_spacing.y = 1.0;
                self.entry(ui, index);
            });
        });
    }

    /// One project and, when it is the open one, its terminals.
    fn entry(&mut self, ui: &mut Ui, index: usize) {
        let snap = self.input.snap;
        let project = &snap.ws.projects[index];
        ui.add_space(5.0);
        let view = snap.views.get(index).and_then(|view| view.as_deref());
        self.project(ui, index, project, view);
        // Only the selected project lists its terminals.
        if !project.expanded || self.input.active != Some(index) {
            return;
        }
        if let Some(error) = &project.error {
            self.error_line(ui, error);
        } else if let Some(view) = view {
            self.terminals(ui, index, project, view);
        }
    }

    /// The group filter: All projects, each group, No group.
    fn filter_row(&mut self, ui: &mut Ui) {
        let snap = self.input.snap;
        let label = match self.input.group_filter {
            GroupFilter::All => "All projects".to_owned(),
            GroupFilter::Ungrouped => "No group".to_owned(),
            GroupFilter::Group(id) => snap
                .ws
                .groups
                .iter()
                .find(|g| g.id == *id)
                .map_or_else(|| "All projects".to_owned(), |g| g.name.clone()),
        };
        let (full, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), FILTER_ROW), Sense::hover());
        let rect = inset(full);
        let button = ui.interact(rect, Id::unique("group-filter"), Sense::click());
        let painter = ui.painter();
        let fill = if button.hovered() {
            theme::ROW_HOVER
        } else {
            theme::RAISED_BG
        };
        painter.rect_filled(rect.shrink2(vec2(0.0, 2.0)), 7.0, fill);
        painter.rect_stroke(
            rect.shrink2(vec2(0.0, 2.0)),
            7.0,
            Stroke::new(1.0, theme::BORDER),
            StrokeKind::Inside,
        );
        let galley = fit(
            painter,
            &format!("Show: {label}"),
            FontId::proportional(12.0),
            theme::TEXT,
            rect.width() - 34.0,
        );
        painter.galley(
            pos2(rect.min.x + 10.0, rect.center().y - galley.size().y / 2.0),
            galley,
            theme::TEXT,
        );
        icons::twisty(
            painter,
            pos2(rect.max.x - 14.0, rect.center().y),
            true,
            theme::TEXT_MUTED,
        );
        button.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::accesskit::Role::ComboBox,
                true,
                format!("Show projects: {label}"),
            )
        });
        self.out.note(|| Hit::GroupFilter, rect);
        let out = &mut *self.out;
        egui::Popup::menu(&button).show(|ui| {
            let mut item = |ui: &mut Ui, key: String, text: &str, filter: GroupFilter, on: bool| {
                let response = ui.selectable_label(on, text);
                out.note(|| Hit::GroupFilterItem(key), response.rect);
                if response.clicked() {
                    out.commands.push(Command::FilterGroups(filter));
                }
            };
            let current = self.input.group_filter;
            item(
                ui,
                "all".to_owned(),
                "All projects",
                GroupFilter::All,
                *current == GroupFilter::All,
            );
            for group in &snap.ws.groups {
                let filter = GroupFilter::Group(group.id.clone());
                let on = *current == filter;
                item(ui, group.id.clone(), &group.name, filter, on);
            }
            item(
                ui,
                "ungrouped".to_owned(),
                "No group",
                GroupFilter::Ungrouped,
                *current == GroupFilter::Ungrouped,
            );
        });
    }

    /// The field that names a new group, where its header will be.
    fn new_group_row(&mut self, ui: &mut Ui) {
        ui.add_space(5.0);
        let (full, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), GROUP_ROW), Sense::hover());
        let rect = inset(full);
        if let Some(rename) = self.rename.as_deref_mut() {
            self.out.rename_drawn = true;
            let field = Rect::from_min_max(
                pos2(rect.min.x + 26.0, rect.min.y + 2.0),
                pos2(rect.max.x - 6.0, rect.max.y - 2.0),
            );
            self.out.rename_end = rename
                .edit(ui, field, FontId::proportional(12.5))
                .or(self.out.rename_end.take());
        }
    }

    /// A group's header. Click folds it; a project dropped on it joins it.
    /// `None` is the drop zone that takes a dragged project out of its group.
    fn group_row(
        &mut self,
        ui: &mut Ui,
        group: Option<&connecto_core::ProjectGroup>,
        count: usize,
        folded: bool,
    ) {
        ui.add_space(8.0);
        let (full, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), GROUP_ROW), Sense::hover());
        let rect = inset(full);
        let key = group.map_or("group-ungrouped", |g| g.id.as_str());
        let row = ui.interact(rect, Id::unique(("tree-group", key)), Sense::click());
        let target = group.map(|g| g.id.clone());
        let dragged = row.dnd_hover_payload::<ProjectDrag>();
        let accepts = dragged.as_ref().is_some_and(|drag| drag.group != target);
        let painter = ui.painter();
        if accepts {
            painter.rect_filled(rect, 7.0, theme::CYAN.gamma_multiply(0.15));
            painter.rect_stroke(rect, 7.0, Stroke::new(1.0, theme::CYAN), StrokeKind::Inside);
        } else if row.hovered() && group.is_some() {
            painter.rect_filled(rect, 7.0, theme::ROW_HOVER);
        }
        let center_y = rect.center().y;
        if group.is_some() {
            icons::twisty(
                painter,
                pos2(rect.min.x + 14.0, center_y),
                !folded,
                theme::FAINT,
            );
        }
        let name = group.map_or("Remove from group", |g| g.name.as_str());
        let renaming = group.and_then(|g| {
            self.rename
                .as_deref_mut()
                .filter(|rename| rename.target == RenameTarget::Group(g.id.clone()))
        });
        if let Some(rename) = renaming {
            self.out.rename_drawn = true;
            let field = Rect::from_min_max(
                pos2(rect.min.x + 26.0, rect.min.y + 2.0),
                pos2(rect.max.x - 6.0, rect.max.y - 2.0),
            );
            self.out.rename_end = rename
                .edit(ui, field, FontId::proportional(12.5))
                .or(self.out.rename_end.take());
        } else {
            let painter = ui.painter();
            // The drop zone shows no count.
            let count_x = if group.is_some() {
                let count_galley = painter.layout_no_wrap(
                    count.to_string(),
                    FontId::monospace(10.5),
                    theme::FAINT,
                );
                let count_x = rect.max.x - 8.0 - count_galley.size().x;
                painter.galley(
                    pos2(count_x, center_y - count_galley.size().y / 2.0),
                    count_galley,
                    theme::FAINT,
                );
                count_x
            } else {
                rect.max.x - 8.0
            };
            let galley = fit(
                painter,
                &name.to_uppercase(),
                FontId::monospace(10.5),
                theme::TEXT_MUTED,
                count_x - rect.min.x - 34.0,
            );
            painter.galley(
                pos2(rect.min.x + 26.0, center_y - galley.size().y / 2.0),
                galley,
                theme::TEXT_MUTED,
            );
        }
        self.out.note(
            || group.map_or(Hit::Ungrouped, |g| Hit::Group(g.id.clone())),
            rect,
        );
        row.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::accesskit::Role::Button,
                true,
                !folded,
                if group.is_some() {
                    format!("Group {name}, {count} projects")
                } else {
                    name.to_owned()
                },
            )
        });
        if let Some(drag) = row.dnd_release_payload::<ProjectDrag>()
            && drag.group != target
        {
            self.out
                .commands
                .push(Command::SetProjectGroup(drag.id.clone(), target.clone()));
        }
        let Some(group) = group else { return };
        // A host's group folds here; it is renamed and deleted there.
        if self.input.snap.is_remote() {
            if row.clicked() {
                self.out
                    .commands
                    .push(Command::ToggleGroup(group.id.clone()));
            }
            return;
        }
        if row.double_clicked() {
            self.out
                .commands
                .push(Command::RenameGroupAt(group.id.clone()));
        } else if row.clicked() {
            self.out
                .commands
                .push(Command::ToggleGroup(group.id.clone()));
        }
        row.context_menu(|ui| {
            for (label, command) in [
                ("Rename group…", Command::RenameGroupAt(group.id.clone())),
                ("Delete group", Command::DeleteGroupAt(group.id.clone())),
            ] {
                if ui.button(label).clicked() {
                    self.out.commands.push(command);
                    ui.close();
                }
            }
        });
    }

    fn error_line(&mut self, ui: &mut Ui, error: &str) {
        let (rect, response) =
            ui.allocate_exact_size(vec2(ui.available_width(), 18.0), Sense::hover());
        let width = rect.width() - 28.0;
        let galley = fit(
            ui.painter(),
            error,
            FontId::proportional(11.5),
            theme::ERROR,
            width,
        );
        ui.painter().galley(
            rect.left_center() + vec2(28.0, -galley.size().y / 2.0),
            galley,
            theme::ERROR,
        );
        response.on_hover_text(error);
    }

    fn project(
        &mut self,
        ui: &mut Ui,
        index: usize,
        project: &ProjectView,
        view: Option<&TerminalsView>,
    ) {
        let (rect, _) =
            ui.allocate_exact_size(vec2(ui.available_width(), PROJECT_ROW), Sense::hover());
        let id = project.id.as_str();
        let active = self.input.active == Some(index);
        let open = project.expanded && active;
        // A remote project's name and closing are its machine's: neither is
        // offered here. Its history and settings are read from that machine,
        // unless the host lists no projects (then there is none to name).
        let remote = self.input.snap.is_remote();
        let panels = !id.starts_with("remote-host:");
        // With groups, a project is dragged onto a group (or a project in it)
        // to join that group.
        let grouping = self.input.regrouping();
        let sense = if grouping {
            Sense::click_and_drag()
        } else {
            Sense::click()
        };
        let row = ui.interact(rect, Id::unique(("tree-project", id)), sense);
        let group = self.input.group_of(project).map(str::to_owned);
        if grouping {
            row.dnd_set_drag_payload(ProjectDrag {
                id: id.to_owned(),
                group: group.clone(),
            });
            if row.dragged() {
                drag_ghost(ui.ctx(), &project.name);
            }
        }
        let drop_here = row
            .dnd_hover_payload::<ProjectDrag>()
            .filter(|drag| drag.id != id && drag.group != group);
        let painter = ui.painter();
        if active {
            painter.rect_filled(rect, 8.0, theme::ROW_ACTIVE);
            painter.rect_stroke(
                rect,
                8.0,
                Stroke::new(1.0, theme::BORDER),
                StrokeKind::Inside,
            );
            painter.rect_filled(
                Rect::from_min_max(
                    pos2(rect.min.x - SIDE_PAD, rect.min.y + 10.0),
                    pos2(rect.min.x - SIDE_PAD + 2.0, rect.max.y - 10.0),
                ),
                1.0,
                theme::CYAN,
            );
        } else if row.contains_pointer() {
            painter.rect_filled(rect, 8.0, theme::ROW_HOVER);
        }
        if drop_here.is_some() {
            painter.rect_stroke(rect, 8.0, Stroke::new(1.0, theme::CYAN), StrokeKind::Inside);
        }
        if let Some(drag) = row.dnd_release_payload::<ProjectDrag>()
            && drag.id != id
            && drag.group != group
        {
            self.out
                .commands
                .push(Command::SetProjectGroup(drag.id.clone(), group.clone()));
        }
        let center_y = rect.center().y;

        let twisty_rect = Rect::from_min_size(rect.min, vec2(26.0, PROJECT_ROW));
        let twisty = ui.interact(twisty_rect, Id::unique(("tree-twisty", id)), Sense::click());
        let painter = ui.painter();
        // A project is a folder: open while its terminals are listed.
        icons::folder(
            painter,
            pos2(rect.min.x + 15.0, center_y),
            open,
            if twisty.hovered() {
                theme::TEXT
            } else if active {
                theme::TEXT_MUTED
            } else {
                theme::FAINT
            },
        );
        twisty.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::accesskit::Role::Button,
                true,
                open,
                format!("Show terminals of {}", project.name),
            )
        });
        let dot = project_dot(self.input.seen, project, view);
        dot.paint(painter, pos2(rect.min.x + 36.0, center_y), self.input.time);

        // Right side: history on hover or when active, the count when folded.
        let mut right = rect.max.x - 6.0;
        let count = view.map_or(0, |view| view.terminals.len());
        if !open {
            let text = count.to_string();
            let galley = painter.layout_no_wrap(text, FontId::monospace(10.5), theme::FAINT);
            right -= galley.size().x;
            painter.galley(
                pos2(right, center_y - galley.size().y / 2.0),
                galley,
                theme::FAINT,
            );
            right -= 6.0;
        }
        let show_clock = (active || row.contains_pointer()) && panels;
        let mut clock_rect = None;
        if show_clock {
            let center = pos2(right - ICON_BUTTON / 2.0, center_y);
            let clock = ui.interact(
                Rect::from_center_size(center, Vec2::splat(ICON_BUTTON)),
                Id::unique(("tree-clock", id)),
                Sense::click(),
            );
            let painter = ui.painter();
            if clock.hovered() {
                painter.rect_filled(clock.rect, 6.0, theme::RAISED_BG);
            }
            icons::clock(
                painter,
                center,
                if clock.hovered() {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                },
            );
            clock.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::accesskit::Role::Button,
                    true,
                    format!("History of {}", project.name),
                )
            });
            self.out.note(|| Hit::Clock(id.to_owned()), clock.rect);
            let clock_hit = clock.rect;
            if clock
                .on_hover_text(concat!("History  ", crate::platform::kbd!("Y")))
                .clicked()
            {
                self.out.commands.push(Command::HistoryOf(id.to_owned()));
            }
            right -= ICON_BUTTON + 2.0;
            clock_rect = Some(clock_hit);
        }
        // The project's settings, just left of the history clock.
        let mut gear_rect = None;
        if show_clock {
            let center = pos2(right - ICON_BUTTON / 2.0, center_y);
            let gear = ui.interact(
                Rect::from_center_size(center, Vec2::splat(ICON_BUTTON)),
                Id::unique(("tree-gear", id)),
                Sense::click(),
            );
            let painter = ui.painter();
            if gear.hovered() {
                painter.rect_filled(gear.rect, 6.0, theme::RAISED_BG);
            }
            icons::gear(
                painter,
                center,
                if gear.hovered() {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                },
            );
            gear.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::accesskit::Role::Button,
                    true,
                    format!("Settings of {}", project.name),
                )
            });
            self.out.note(|| Hit::Gear(id.to_owned()), gear.rect);
            gear_rect = Some(gear.rect);
            if gear.on_hover_text("Project settings").clicked() {
                self.out
                    .commands
                    .push(Command::ProjectSettingsAt(id.to_owned()));
            }
            right -= ICON_BUTTON + 2.0;
        }
        // The project's operations, left of the gear, once it has any.
        let mut ops_rect = None;
        if show_clock && let Some(waiting) = self.input.ops.get(id).copied() {
            let center = pos2(right - ICON_BUTTON / 2.0, center_y);
            let bolt = ui.interact(
                Rect::from_center_size(center, Vec2::splat(ICON_BUTTON)),
                Id::unique(("tree-ops", id)),
                Sense::click(),
            );
            let painter = ui.painter();
            if bolt.hovered() {
                painter.rect_filled(bolt.rect, 6.0, theme::RAISED_BG);
            }
            icons::bolt(
                painter,
                center,
                if bolt.hovered() {
                    theme::TEXT
                } else {
                    theme::TEXT_MUTED
                },
            );
            if waiting {
                painter.circle_filled(center + vec2(5.5, -5.5), 2.5, theme::CYAN);
            }
            bolt.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::accesskit::Role::Button,
                    true,
                    format!("Operations of {}", project.name),
                )
            });
            self.out.note(|| Hit::Ops(id.to_owned()), bolt.rect);
            ops_rect = Some(bolt.rect);
            if bolt.on_hover_text("Operations").clicked() {
                self.out.commands.push(Command::OperationsOf(id.to_owned()));
            }
            right -= ICON_BUTTON + 2.0;
        }

        // The name, or the field that renames it.
        let name_left = rect.min.x + 49.0;
        let name_rect = Rect::from_min_max(
            pos2(name_left, rect.min.y + 3.0),
            pos2(right, rect.max.y - 3.0),
        );
        let renaming = self
            .rename
            .as_deref_mut()
            .filter(|rename| rename.target == RenameTarget::Project(id.to_owned()));
        if let Some(rename) = renaming {
            self.out.rename_drawn = true;
            self.out.rename_end = rename
                .edit(ui, name_rect, FontId::proportional(13.0))
                .or(self.out.rename_end.take());
        } else {
            let painter = ui.painter();
            let color = if active {
                Color32::WHITE
            } else {
                theme::TEXT_ROW
            };
            let galley = fit(
                painter,
                &project.name,
                FontId::proportional(13.0),
                color,
                name_rect.width(),
            );
            painter.galley(
                pos2(name_left, center_y - galley.size().y / 2.0),
                galley,
                color,
            );
        }

        self.out.note(|| Hit::Project(id.to_owned()), rect);
        self.out.note(|| Hit::Twisty(id.to_owned()), twisty.rect);
        row.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::accesskit::Role::Button,
                true,
                active,
                format!("{}, {}", project.name, dot.word()),
            )
        });
        let on_control = |pos: Option<Pos2>| {
            pos.is_some_and(|pos| {
                twisty.rect.contains(pos)
                    || clock_rect.is_some_and(|rect| rect.contains(pos))
                    || gear_rect.is_some_and(|rect| rect.contains(pos))
                    || ops_rect.is_some_and(|rect| rect.contains(pos))
            })
        };
        let row = row.on_hover_text(project.path.as_str());
        if twisty.clicked() {
            // On a background project the twisty selects it, which opens it.
            if !active {
                self.out.commands.push(Command::Activate(id.to_owned()));
            }
            self.out
                .commands
                .push(Command::SetExpanded(id.to_owned(), !open));
        } else if row.double_clicked() && !on_control(row.interact_pointer_pos()) {
            if !remote {
                self.out
                    .commands
                    .push(Command::RenameProjectAt(id.to_owned()));
            }
        } else if row.clicked() && !on_control(row.interact_pointer_pos()) {
            self.out.commands.push(Command::Activate(id.to_owned()));
        }
        if remote && panels {
            row.context_menu(|ui| {
                for (label, command) in [
                    ("History", Command::HistoryOf(id.to_owned())),
                    (
                        "Project settings…",
                        Command::ProjectSettingsAt(id.to_owned()),
                    ),
                ] {
                    if ui.button(label).clicked() {
                        self.out.commands.push(command);
                        ui.close();
                    }
                }
            });
        }
        if !remote {
            row.context_menu(|ui| {
                for (label, command) in [
                    ("Rename project…", Command::RenameProjectAt(id.to_owned())),
                    ("History", Command::HistoryOf(id.to_owned())),
                    (
                        "Project settings…",
                        Command::ProjectSettingsAt(id.to_owned()),
                    ),
                    ("Close project", Command::CloseProjectAt(id.to_owned())),
                    ("Delete project…", Command::DeleteProjectAt(id.to_owned())),
                ] {
                    if ui.button(label).clicked() {
                        self.out.commands.push(command);
                        ui.close();
                    }
                }
                let groups = &self.input.snap.ws.groups;
                if !groups.is_empty() {
                    ui.menu_button("Move to group", |ui| {
                        let choices = groups
                            .iter()
                            .map(|g| (Some(g.id.clone()), g.name.as_str()))
                            .chain([(None, "No group")]);
                        for (target, label) in choices {
                            if ui.selectable_label(group == target, label).clicked() {
                                if group != target {
                                    self.out
                                        .commands
                                        .push(Command::SetProjectGroup(id.to_owned(), target));
                                }
                                ui.close();
                            }
                        }
                    });
                }
            });
        }
    }

    fn terminals(
        &mut self,
        ui: &mut Ui,
        index: usize,
        project: &ProjectView,
        view: &TerminalsView,
    ) {
        let nodes = order(view);
        let pid = project.id.as_str();
        let active = self.input.active == Some(index);
        let kid_counts: HashMap<&str, usize> = {
            let mut counts = HashMap::new();
            for node in nodes.iter().filter(|node| node.child) {
                if let Some(top) = parent_of(view, node.entry) {
                    *counts.entry(top).or_insert(0) += 1;
                }
            }
            counts
        };
        // The connector down the left of a parent's children: x, top, bottom.
        let mut line: Option<(f32, f32, f32)> = None;
        let mut parent_id: Option<&str> = None;
        for node in &nodes {
            let entry = node.entry;
            if node.child {
                let folded = parent_id.is_some_and(|top| {
                    self.input
                        .collapsed_kids
                        .contains(&(pid.to_owned(), top.to_owned()))
                });
                if folded {
                    continue;
                }
            } else {
                finish_line(ui, line.take());
                parent_id = Some(entry.id.as_str());
            }
            let shown = active && self.input.shown_terminal == Some(entry.id.as_str());
            let kids = if node.child {
                0
            } else {
                kid_counts.get(entry.id.as_str()).copied().unwrap_or(0)
            };
            let rect = self.terminal_row(ui, pid, entry, node.child, shown, kids);
            if background_open(ui, pid, entry) {
                self.background_rows(ui, pid, entry, node.child);
            }
            if !node.child && kids > 0 {
                line = Some((rect.min.x + 10.0, rect.max.y, f32::NAN));
            } else if let Some(line) = line.as_mut() {
                line.0 = rect.min.x - 9.0;
                line.2 = rect.center().y;
            }
        }
        finish_line(ui, line.take());
        self.new_row(ui, pid, project.name.as_str(), view);
    }

    fn terminal_row(
        &mut self,
        ui: &mut Ui,
        pid: &str,
        entry: &TerminalEntry,
        child: bool,
        shown: bool,
        kids: usize,
    ) -> Rect {
        let height = if child { CHILD_ROW } else { TERMINAL_ROW };
        let (full, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
        let indent = if child { 39.0 } else { 20.0 };
        let rect = Rect::from_min_max(pos2(full.min.x + indent, full.min.y), full.max);
        let tid = entry.id.as_str();
        let row = ui.interact(
            rect,
            Id::unique(("tree-terminal", pid, tid)),
            Sense::click(),
        );
        let dot = terminal_dot(self.input.seen, pid, entry);
        let done_child = child && dot == Dot::Done;
        let hovered = row.contains_pointer();
        let painter = ui.painter();
        if shown {
            painter.rect_filled(rect, 7.0, theme::TERM_ON);
            painter.rect_stroke(rect, 7.0, Stroke::new(1.0, theme::TEAL), StrokeKind::Inside);
        } else if hovered {
            painter.rect_filled(rect, 7.0, theme::ROW_HOVER);
        }
        if child {
            // The short connector from the parent's line.
            painter.hline(
                (rect.min.x - 9.0)..=(rect.min.x - 2.0),
                rect.center().y,
                Stroke::new(1.0, theme::TREE_LINE),
            );
        }
        let center_y = rect.center().y;
        dot.paint(painter, pos2(rect.min.x + 13.0, center_y), self.input.time);

        // Right side, from the edge in: the children toggle, then close.
        let mut right = rect.max.x - 8.0;
        let mut kids_rect = None;
        if kids > 0 {
            let folded = self
                .input
                .collapsed_kids
                .contains(&(pid.to_owned(), tid.to_owned()));
            let zone = Rect::from_min_max(
                pos2(right - 32.0, rect.min.y),
                pos2(right + 4.0, rect.max.y),
            );
            let toggle = ui.interact(zone, Id::unique(("tree-kids", pid, tid)), Sense::click());
            let painter = ui.painter();
            let color = if shown || toggle.hovered() {
                theme::TEXT_MUTED
            } else {
                theme::FAINT
            };
            painter.text(
                pos2(right - 12.0, center_y),
                Align2::RIGHT_CENTER,
                kids.to_string(),
                FontId::monospace(10.5),
                color,
            );
            icons::twisty(painter, pos2(right - 5.0, center_y), !folded, color);
            toggle.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::accesskit::Role::Button,
                    true,
                    format!("{} child terminals of {}", kids, title(entry)),
                )
            });
            self.out
                .note(|| Hit::Kids(pid.to_owned(), tid.to_owned()), toggle.rect);
            if toggle
                .on_hover_text("Fold or unfold child terminals")
                .clicked()
            {
                self.out.toggled_kids.push((pid.to_owned(), tid.to_owned()));
            }
            kids_rect = Some(zone);
            right -= 36.0;
        }
        // The arrow that opens the list of background tasks, shown while any
        // runs. The tasks, each with its own stop, are rows under this one,
        // so the state line keeps saying the state.
        let mut list_rect = None;
        if !entry.background.is_empty() {
            let open = background_open(ui, pid, entry);
            let center = pos2(right - 8.0, center_y);
            let zone = Rect::from_center_size(center, Vec2::splat(20.0));
            let toggle = ui.interact(zone, Id::unique(("tree-bg-list", pid, tid)), Sense::click());
            let painter = ui.painter();
            if toggle.hovered() {
                painter.circle_filled(center, 9.0, theme::RAISED_BG);
            }
            let color = if toggle.hovered() {
                theme::TEXT
            } else if long_running(entry, Instant::now()) {
                theme::LONG_RUNNING
            } else {
                theme::TEXT_MUTED
            };
            icons::twisty(painter, center, open, color);
            let count = entry.background.len();
            toggle.widget_info(|| {
                egui::WidgetInfo::selected(
                    egui::accesskit::Role::Button,
                    true,
                    open,
                    format!("{count} background tasks of {}", title(entry)),
                )
            });
            self.out
                .note(|| Hit::BackgroundList(pid.to_owned(), tid.to_owned()), zone);
            let tip = match (open, count) {
                (true, _) => "Hide the background tasks".to_owned(),
                (false, 1) => "Show the background task".to_owned(),
                (false, n) => format!("Show the {n} background tasks"),
            };
            if toggle.on_hover_text(tip).clicked() {
                set_background_open(ui, pid, entry, !open);
            }
            list_rect = Some(zone);
            right -= 24.0;
        }
        let mut close_rect = None;
        if hovered || done_child || shown {
            let center = pos2(right - 8.0, center_y);
            let close = ui.interact(
                Rect::from_center_size(center, Vec2::splat(20.0)),
                Id::unique(("tree-close", pid, tid)),
                Sense::click(),
            );
            let painter = ui.painter();
            if close.hovered() {
                painter.circle_filled(center, 9.0, theme::CORAL_BG);
            }
            icons::cross(
                painter,
                center,
                3.5,
                if close.hovered() {
                    theme::CORAL_SOFT
                } else if done_child || hovered {
                    theme::TEXT_MUTED
                } else {
                    theme::FAINT
                },
            );
            close.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::accesskit::Role::Button,
                    true,
                    format!("Close {}", title(entry)),
                )
            });
            self.out
                .note(|| Hit::Close(pid.to_owned(), tid.to_owned()), close.rect);
            let close_hit = close.rect;
            if close
                .on_hover_text("Close: stops it and keeps it in history")
                .clicked()
            {
                self.out.commands.push(Command::CloseTerminalAt {
                    project: pid.to_owned(),
                    terminal: tid.to_owned(),
                });
            }
            close_rect = Some(close_hit);
            right -= 24.0;
        }
        // The robot sits left of the close button, and shows with it, and
        // pulses while a title is being made.
        let titling = self.input.snap.titling(pid, tid)
            || self
                .input
                .titling
                .contains_key(&(pid.to_owned(), tid.to_owned()));
        let mut robot_rect = None;
        // A shell has no conversation to title.
        if (hovered || done_child || shown) && entry.tool.is_agent() {
            let center = pos2(right - 8.0, center_y);
            let robot = ui.interact(
                Rect::from_center_size(center, Vec2::splat(20.0)),
                Id::unique(("tree-robot", pid, tid)),
                Sense::click(),
            );
            let painter = ui.painter();
            if robot.hovered() && !titling {
                painter.circle_filled(center, 9.0, theme::RAISED_BG);
            }
            let color = if titling {
                let pulse = (self.input.time * 4.0).sin() as f32 * 0.5 + 0.5;
                theme::CYAN.gamma_multiply(0.35 + 0.5 * pulse)
            } else if robot.hovered() {
                theme::CYAN
            } else if done_child || hovered {
                theme::TEXT_MUTED
            } else {
                theme::FAINT
            };
            icons::robot(painter, center, color);
            robot.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::accesskit::Role::Button,
                    !titling,
                    format!("Generate title for {}", title(entry)),
                )
            });
            self.out
                .note(|| Hit::Robot(pid.to_owned(), tid.to_owned()), robot.rect);
            robot_rect = Some(robot.rect);
            let tip = if titling {
                "Generating a title…"
            } else {
                "Generate title"
            };
            if robot.on_hover_text(tip).clicked() && !titling {
                self.out.commands.push(Command::GenerateTitleAt {
                    project: pid.to_owned(),
                    terminal: tid.to_owned(),
                });
            }
            right -= 24.0;
        }

        // Two lines: the name, then the id and the state.
        let text_left = rect.min.x + 26.0;
        let width = (right - text_left).max(20.0);
        let name_color = if shown {
            Color32::WHITE
        } else if done_child {
            Color32::from_rgb(0x8a, 0xa0, 0xae)
        } else if child {
            Color32::from_rgb(0xb9, 0xc8, 0xd2)
        } else {
            Color32::from_rgb(0xdf, 0xe8, 0xee)
        };
        let name_font = FontId::proportional(if child { 12.0 } else { 12.5 });
        let name_rect = Rect::from_min_size(pos2(text_left, rect.min.y + 4.0), vec2(width, 18.0));
        let renaming = self.rename.as_deref_mut().filter(|rename| {
            rename.target
                == RenameTarget::Terminal {
                    project: pid.to_owned(),
                    terminal: tid.to_owned(),
                }
        });
        if let Some(rename) = renaming {
            self.out.rename_drawn = true;
            self.out.rename_end = rename
                .edit(ui, name_rect, name_font)
                .or(self.out.rename_end.take());
        } else {
            let painter = ui.painter();
            let galley = fit(painter, title(entry), name_font, name_color, width);
            painter.galley(pos2(text_left, rect.min.y + 6.0), galley, name_color);
        }
        let now = Instant::now();
        let meta = meta_line(entry, dot, now);
        let meta_color = if long_running(entry, now) {
            theme::LONG_RUNNING
        } else if shown || hovered {
            theme::TEXT_MUTED
        } else {
            theme::FAINT
        };
        let meta_y = rect.min.y + (height - 15.0);
        let mut meta_left = text_left;
        let painter = ui.painter();
        // The id agents use (`claude-41`), left of the state, so a row can be
        // matched to what `connecto list` prints. The state comes first: the
        // id takes what the state leaves, and is left out (it stays in the
        // tooltip) when too little is left; the state is cut only when it
        // alone does not fit.
        let meta_font = FontId::monospace(10.5);
        let room = (right - meta_left).max(20.0);
        // The longest form of the state that fits, else the shortest.
        let forms = meta_lines(entry, dot, now);
        let (meta, state_width) = forms
            .iter()
            .map(|form| {
                let galley = painter.layout_no_wrap(form.clone(), meta_font.clone(), meta_color);
                (form.clone(), galley.size().x)
            })
            .find(|(_, width)| *width <= room)
            .unwrap_or_else(|| (forms.last().cloned().unwrap_or(meta), room));
        if let Some(width) = id_room(room, state_width) {
            let id = row_id(entry);
            let id_color = if shown { theme::CYAN } else { theme::TEXT_ROW };
            let galley = fit(painter, &id, meta_font.clone(), id_color, width);
            let used = galley.size().x;
            painter.galley(pos2(meta_left, meta_y), galley, id_color);
            meta_left += used + ID_GAP;
        }
        let galley = fit(
            painter,
            &meta,
            meta_font,
            meta_color,
            (right - meta_left).max(20.0),
        );
        painter.galley(pos2(meta_left, meta_y), galley, meta_color);

        self.out
            .note(|| Hit::Terminal(pid.to_owned(), tid.to_owned()), rect);
        row.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::accesskit::Role::Button,
                true,
                shown,
                format!(
                    "{}, {}, {}",
                    title(entry),
                    entry.tool.name(),
                    state_line(entry, dot, now)
                ),
            )
        });
        let on_control = |pos: Option<Pos2>| {
            pos.is_some_and(|pos| {
                kids_rect.is_some_and(|rect| rect.contains(pos))
                    || list_rect.is_some_and(|rect| rect.contains(pos))
                    || close_rect.is_some_and(|rect| rect.contains(pos))
                    || robot_rect.is_some_and(|rect| rect.contains(pos))
            })
        };
        if row.double_clicked() && !on_control(row.interact_pointer_pos()) {
            self.out.commands.push(Command::RenameTerminalAt {
                project: pid.to_owned(),
                terminal: tid.to_owned(),
            });
        } else if row.clicked() && !on_control(row.interact_pointer_pos()) {
            self.out.commands.push(Command::Show {
                project: pid.to_owned(),
                terminal: tid.to_owned(),
            });
        }
        let has_kids = kids > 0;
        let restartable = matches!(
            entry.state,
            TermActivity::Exited | TermActivity::Failed | TermActivity::NotStarted
        );
        let tip = row_tip(entry, now);
        if !tip.is_empty() {
            row.clone().on_hover_text(tip);
        }
        let background = !entry.background.is_empty();
        let shells = entry.background.iter().any(|work| work.is_shell());
        row.context_menu(|ui| {
            let mut commands = vec![(
                "Rename…",
                Command::RenameTerminalAt {
                    project: pid.to_owned(),
                    terminal: tid.to_owned(),
                },
            )];
            if restartable {
                commands.push((
                    "Restart",
                    Command::RestartAt {
                        project: pid.to_owned(),
                        terminal: tid.to_owned(),
                    },
                ));
            }
            if has_kids {
                commands.push((
                    "Close finished children",
                    Command::CloseFinishedChildrenOf {
                        project: pid.to_owned(),
                        terminal: tid.to_owned(),
                    },
                ));
            }
            commands.push((
                "Close",
                Command::CloseTerminalAt {
                    project: pid.to_owned(),
                    terminal: tid.to_owned(),
                },
            ));
            for (label, command) in commands {
                if ui.button(label).clicked() {
                    self.out.commands.push(command);
                    ui.close();
                }
            }
            if background {
                ui.separator();
                let stop = ui
                    .add_enabled(shells, egui::Button::new("Stop background shells"))
                    .on_disabled_hover_text(NOT_A_SHELL);
                if stop.clicked() {
                    self.out.commands.push(Command::StopBackgroundAt {
                        project: pid.to_owned(),
                        terminal: tid.to_owned(),
                        task: None,
                    });
                    ui.close();
                }
            }
        });
        rect
    }

    /// The square stop button centred on `center`; `task` names the one
    /// background task it stops, `None` all the terminal's shells. Returns
    /// where it is.
    fn stop_button(
        &mut self,
        ui: &mut Ui,
        center: Pos2,
        pid: &str,
        tid: &str,
        task: Option<String>,
        what: &str,
    ) -> Rect {
        let zone = Rect::from_center_size(center, Vec2::splat(20.0));
        let button = ui.interact(
            zone,
            Id::unique(("tree-bg-stop", pid, tid, task.as_deref().unwrap_or(""))),
            Sense::click(),
        );
        let painter = ui.painter();
        if button.hovered() {
            painter.circle_filled(center, 9.0, theme::CORAL_BG);
        }
        let color = if button.hovered() {
            theme::CORAL_SOFT
        } else {
            theme::TEXT_MUTED
        };
        icons::stop(painter, center, 3.5, color);
        button.widget_info(|| {
            egui::WidgetInfo::labeled(egui::accesskit::Role::Button, true, what.to_owned())
        });
        self.out.note(
            || Hit::StopBackground(pid.to_owned(), tid.to_owned(), task.clone()),
            zone,
        );
        if button.on_hover_text(what).clicked() {
            self.out.commands.push(Command::StopBackgroundAt {
                project: pid.to_owned(),
                terminal: tid.to_owned(),
                task,
            });
        }
        zone
    }

    /// The open list of a terminal's background tasks: one short row each,
    /// what it is for, how long it has run, and a stop for a shell.
    fn background_rows(&mut self, ui: &mut Ui, pid: &str, entry: &TerminalEntry, child: bool) {
        let now = Instant::now();
        let indent = if child { 39.0 } else { 20.0 } + 26.0;
        for work in &entry.background {
            let (full, _) =
                ui.allocate_exact_size(vec2(ui.available_width(), BACKGROUND_ROW), Sense::hover());
            let rect = Rect::from_min_max(pos2(full.min.x + indent, full.min.y), full.max);
            let row = ui.interact(
                rect,
                Id::unique(("tree-bg-row", pid, entry.id.as_str(), work.id.as_str())),
                Sense::hover(),
            );
            let center_y = rect.center().y;
            let mut right = rect.max.x - 8.0;
            if work.is_shell() {
                let what = format!("Stop {}", task_title(work));
                self.stop_button(
                    ui,
                    pos2(right - 8.0, center_y),
                    pid,
                    &entry.id,
                    Some(work.id.clone()),
                    &what,
                );
            }
            right -= 24.0;
            let color = if row.hovered() {
                theme::TEXT_MUTED
            } else {
                theme::FAINT
            };
            let painter = ui.painter();
            let age =
                painter.layout_no_wrap(short_age(work.age(now)), FontId::monospace(10.0), color);
            let age_left = right - age.size().x;
            painter.galley(pos2(age_left, center_y - age.size().y / 2.0), age, color);
            let text = fit(
                painter,
                &task_title(work),
                FontId::proportional(11.0),
                color,
                (age_left - 6.0 - rect.min.x).max(20.0),
            );
            painter.galley(
                pos2(rect.min.x, center_y - text.size().y / 2.0),
                text,
                color,
            );
            row.on_hover_text(task_tip(work, now));
        }
    }

    fn new_row(&mut self, ui: &mut Ui, pid: &str, project_name: &str, view: &TerminalsView) {
        let (full, _) = ui.allocate_exact_size(vec2(ui.available_width(), NEW_ROW), Sense::hover());
        let rect = Rect::from_min_max(pos2(full.min.x + 20.0, full.min.y), full.max);
        let enabled = view.can_add();
        let row = ui.interact(rect, Id::unique(("tree-new", pid)), Sense::click());
        let hot = enabled && row.hovered();
        let painter = ui.painter();
        if hot {
            painter.rect_filled(rect, 7.0, theme::ROW_HOVER);
        }
        let color = if !enabled {
            theme::FAINT.gamma_multiply(0.6)
        } else if hot {
            theme::TEXT
        } else {
            theme::FAINT
        };
        icons::plus(
            painter,
            pos2(rect.min.x + 13.0, rect.center().y),
            4.0,
            color,
        );
        painter.text(
            pos2(rect.min.x + 26.0, rect.center().y),
            Align2::LEFT_CENTER,
            "New terminal",
            FontId::proportional(12.0),
            color,
        );
        let kbd = painter.layout_no_wrap(
            crate::platform::kbd!("T").to_owned(),
            FontId::monospace(10.5),
            theme::FAINT,
        );
        let kbd_rect = Rect::from_center_size(
            pos2(
                rect.max.x - 8.0 - (kbd.size().x + 10.0) / 2.0,
                rect.center().y,
            ),
            vec2(kbd.size().x + 10.0, 16.0),
        );
        painter.rect_stroke(
            kbd_rect,
            4.0,
            Stroke::new(1.0, theme::BORDER),
            StrokeKind::Inside,
        );
        painter.galley(kbd_rect.center() - kbd.size() / 2.0, kbd, theme::FAINT);
        self.out.new_rows.insert(pid.to_owned(), rect);
        self.out.note(|| Hit::NewRow(pid.to_owned()), rect);
        row.widget_info(|| {
            egui::WidgetInfo::labeled(
                egui::accesskit::Role::Button,
                enabled,
                format!("New terminal in {project_name}"),
            )
        });
        let tip = match view.save {
            connecto_core::SaveState::ReadOnly => {
                "The terminal list could not be read, so it cannot change.".to_owned()
            }
            _ => format!("Up to {MAX_TERMINALS} terminals."),
        };
        let row = if enabled { row } else { row.on_hover_text(tip) };
        if enabled && row.clicked() {
            self.out
                .commands
                .push(Command::NewTerminalIn(pid.to_owned()));
        }
        ui.add_space(2.0);
    }
}

/// The name of the project being dragged, following the pointer.
/// A row moved right by [`LEFT_INSET`], so the dropdowns and group headers
/// line up with the host switcher instead of the list's edge.
fn inset(rect: Rect) -> Rect {
    Rect::from_min_max(pos2(rect.min.x + LEFT_INSET, rect.min.y), rect.max)
}

fn drag_ghost(ctx: &egui::Context, name: &str) {
    static GHOST: std::sync::OnceLock<Id> = std::sync::OnceLock::new();
    ctx.set_cursor_icon(egui::CursorIcon::Grabbing);
    let Some(pos) = ctx.pointer_interact_pos() else {
        return;
    };
    egui::Area::new(*GHOST.get_or_init(|| Id::unique("project-drag-ghost")))
        .order(egui::Order::Tooltip)
        .fixed_pos(pos + vec2(12.0, 12.0))
        .interactable(false)
        .show(ctx, |ui| {
            egui::Frame::popup(ui.style()).show(ui, |ui| {
                ui.label(egui::RichText::new(name).color(Color32::WHITE));
            });
        });
}

fn finish_line(ui: &Ui, line: Option<(f32, f32, f32)>) {
    if let Some((x, top, bottom)) = line
        && bottom.is_finite()
    {
        ui.painter()
            .vline(x, top..=bottom, Stroke::new(1.0, theme::TREE_LINE));
    }
}

fn parent_of<'a>(view: &'a TerminalsView, entry: &'a TerminalEntry) -> Option<&'a str> {
    let mut current = entry;
    for _ in 0..8 {
        match current
            .parent
            .as_deref()
            .and_then(|id| view.terminals.iter().find(|candidate| candidate.id == id))
        {
            Some(parent) => current = parent,
            None => break,
        }
    }
    (current.id != entry.id).then_some(current.id.as_str())
}

/// The second line of a row: the state, or who asked for a child. The id is
/// the title of an unnamed terminal; a named one shows it on hover and in
/// `connecto list`.
/// What the stop says when only subagents, monitors or workflows run.
pub(crate) const NOT_A_SHELL: &str =
    "Only background shells can be stopped. Subagents, monitors and workflows end on their own.";

/// The gap between a row's id and its state.
const ID_GAP: f32 = 7.0;
/// The least room an id is shown in; less would leave two letters and a `…`.
const MIN_ID: f32 = 32.0;

/// The room a row's id gets on the second line: what a state `state` wide
/// leaves of `room`. `None` when that is too little to show it.
pub(crate) fn id_room(room: f32, state: f32) -> Option<f32> {
    let left = room - state - ID_GAP;
    (left >= MIN_ID).then_some(left)
}

/// The id a terminal row shows left of its state, on every row: the one the
/// CLI and agents use (`claude`, `codex-27`), never a second numbering.
/// Antigravity's long tool name is shortened to `agy` (`agy-3`) to fit.
pub(crate) fn row_id(entry: &TerminalEntry) -> std::borrow::Cow<'_, str> {
    let id = entry.display_id();
    match id.strip_prefix("antigravity") {
        Some(rest) if rest.is_empty() || rest.starts_with('-') => format!("agy{rest}").into(),
        _ => id.into(),
    }
}

/// The second line of a terminal row: who asked it, or its state. With
/// only background work at the prompt: `<n> in background · <oldest's age>`.
pub(crate) fn meta_line(entry: &TerminalEntry, dot: Dot, now: Instant) -> String {
    match &entry.asked_by {
        Some(asker) => format!("asked by {asker}"),
        None => state_line(entry, dot, now),
    }
}

/// [`meta_line`] and its shorter forms, longest first, for a narrow row:
/// background work drops its count, then its age (both stay in the list and
/// the tooltip).
pub(crate) fn meta_lines(entry: &TerminalEntry, dot: Dot, now: Instant) -> Vec<String> {
    let full = meta_line(entry, dot, now);
    match entry.background.first() {
        Some(oldest) if entry.asked_by.is_none() && only_background(entry, dot) => vec![
            full,
            format!("background · {}", short_age(oldest.age(now))),
            "background".to_owned(),
        ],
        _ => vec![full],
    }
}

/// Whether the terminal works only because background tasks run at its
/// prompt: no turn is running.
fn only_background(entry: &TerminalEntry, dot: Dot) -> bool {
    !entry.background.is_empty() && dot == Dot::Working && !entry.in_turn
}

/// The state in words. When the terminal works only because background
/// tasks run at its prompt, `<n> in background · <oldest's age>`; a task's
/// own title is never put here, it is in the list the arrow opens.
fn state_line(entry: &TerminalEntry, dot: Dot, now: Instant) -> String {
    match entry.background.first() {
        Some(oldest) if only_background(entry, dot) => format!(
            "{} in background · {}",
            entry.background.len(),
            short_age(oldest.age(now))
        ),
        _ => state_words(entry, dot).to_owned(),
    }
}

/// A background task in a few words: what the agent said it is for, or else
/// what it runs.
pub(crate) fn task_title(work: &BackgroundWork) -> String {
    work.description
        .clone()
        .filter(|text| !text.trim().is_empty())
        .unwrap_or_else(|| work.label.clone())
}

/// A background task's tooltip: what it is for, what it runs, its kind and age.
fn task_tip(work: &BackgroundWork, now: Instant) -> String {
    let mut lines = Vec::new();
    if let Some(description) = &work.description {
        lines.push(description.clone());
    }
    lines.push(work.label.clone());
    lines.push(format!(
        "{}, running for {}",
        work.kind,
        short_age(work.age(now))
    ));
    if !work.is_shell() {
        lines.push(NOT_A_SHELL.to_owned());
    }
    lines.join("\n")
}

/// Whether a terminal's list of background tasks is open: closed until its
/// arrow is clicked. An open list remembers the tasks it was opened on and
/// stays open while any of them runs, so a later batch, after they all
/// ended, starts closed even if the row was not drawn in between.
fn background_open(ui: &Ui, pid: &str, entry: &TerminalEntry) -> bool {
    let key = background_key(pid, &entry.id);
    let Some(seen) = ui.data(|data| data.get_temp::<Vec<String>>(key)) else {
        return false;
    };
    let ids = task_ids(entry);
    let open = ids.iter().any(|id| seen.contains(id));
    if !open {
        ui.data_mut(|data| data.remove::<Vec<String>>(key));
    } else if seen != ids {
        ui.data_mut(|data| data.insert_temp(key, ids));
    }
    open
}

fn set_background_open(ui: &Ui, pid: &str, entry: &TerminalEntry, open: bool) {
    let key = background_key(pid, &entry.id);
    ui.data_mut(|data| {
        if open {
            data.insert_temp(key, task_ids(entry));
        } else {
            data.remove::<Vec<String>>(key);
        }
    });
}

fn task_ids(entry: &TerminalEntry) -> Vec<String> {
    entry
        .background
        .iter()
        .map(|work| work.id.clone())
        .collect()
}

fn background_key(pid: &str, tid: &str) -> Id {
    Id::unique(("tree-bg-open", pid, tid))
}

/// Whether the oldest background work has run for [`LONG_RUNNING`] or more.
pub(crate) fn long_running(entry: &TerminalEntry, now: Instant) -> bool {
    entry
        .background
        .first()
        .is_some_and(|oldest| oldest.age(now) >= LONG_RUNNING)
}

/// A terminal row's tooltip: its id when it has a name, and the background
/// work running, each with its age.
pub(crate) fn row_tip(entry: &TerminalEntry, now: Instant) -> String {
    let mut lines = Vec::new();
    if entry.name.is_some() {
        lines.push(format!("Terminal id: {}", entry.display_id()));
    }
    if !entry.background.is_empty() {
        let long = if long_running(entry, now) {
            ", running for a long time"
        } else {
            ""
        };
        lines.push(format!(
            "Background work at the prompt{long}. The turn has not finished:"
        ));
        for work in &entry.background {
            lines.push(format!(
                "• {} ({}, {})",
                task_title(work),
                work.kind,
                short_age(work.age(now))
            ));
        }
        lines.push(if entry.background.iter().any(|work| work.is_shell()) {
            "The arrow lists them; ■ stops one shell.".to_owned()
        } else {
            NOT_A_SHELL.to_owned()
        });
    }
    lines.join("\n")
}

// ---------- the collapsed rail ----------

pub(crate) fn rail(ui: &mut Ui, rect: Rect, input: &TreeInput<'_>, out: &mut TreeOut) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::SIDEBAR_BG);
    painter.vline(
        rect.max.x - 0.5,
        rect.y_range(),
        Stroke::new(1.0, theme::SIDEBAR_BORDER),
    );
    let center_x = rect.center().x - 0.5;
    let mut y = rect.min.y + LIGHTS_BAND + 14.0;
    // Everything above the first tile drags the window, except its buttons.
    let strip = Rect::from_min_max(
        rect.min,
        pos2(
            rect.max.x,
            rect.min.y + LIGHTS_BAND + 14.0 + 2.0 * (ICON_BUTTON + 4.0) + ICON_BUTTON + 12.0 - 6.0,
        ),
    );
    let drag = ui.interact(strip, Id::unique("rail-drag"), Sense::click_and_drag());
    out.note(|| Hit::Header, strip);
    let expand = icon_button(
        ui,
        pos2(center_x, y),
        "rail-expand",
        concat!("Show sidebar  ", crate::platform::kbd!("\\")),
        false,
        icons::sidebar,
    );
    out.note(|| Hit::Collapse, expand.rect);
    if expand.clicked() {
        out.commands.push(Command::ToggleSidebar);
    }
    y += ICON_BUTTON + 4.0;
    let clock = icon_button(
        ui,
        pos2(center_x, y),
        "rail-history",
        concat!("History  ", crate::platform::kbd!("Y")),
        input.history_open,
        icons::clock,
    );
    out.note(|| Hit::HeaderHistory, clock.rect);
    if clock.clicked() {
        out.commands.push(Command::History);
    }
    y += ICON_BUTTON + 4.0;
    let plus = icon_button(
        ui,
        pos2(center_x, y),
        "rail-open",
        concat!("Open project  ", crate::platform::kbd!("O")),
        false,
        |p, c, col| icons::plus(p, c, 5.0, col),
    );
    out.note(|| Hit::HeaderPlus, plus.rect);
    if plus.clicked() {
        out.commands.push(Command::OpenProject);
    }
    header::window_gestures(ui, &drag, &[expand.rect, clock.rect, plus.rect]);
    y += ICON_BUTTON + 12.0;
    painter.hline(
        (rect.min.x + 14.0)..=(rect.max.x - 14.0),
        y - 6.0,
        Stroke::new(1.0, theme::HAIR),
    );

    let tiles = Rect::from_min_max(
        pos2(rect.min.x, y),
        pos2(rect.max.x, rect.max.y - FOOT_HEIGHT),
    );
    ui.scope_builder(egui::UiBuilder::new().max_rect(tiles), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("project-rail")
            .auto_shrink([false, false])
            .scroll_bar_visibility(egui::scroll_area::ScrollBarVisibility::AlwaysHidden)
            .show(ui, |ui| {
                for (index, project) in input.snap.ws.projects.iter().enumerate() {
                    if !input.listed(project) {
                        continue;
                    }
                    let view = input.snap.views.get(index).and_then(|view| view.as_deref());
                    tile(ui, input, index, project, view, out);
                }
            });
    });

    let foot = Rect::from_min_max(
        pos2(rect.min.x, rect.max.y - FOOT_HEIGHT),
        pos2(rect.max.x - 1.0, rect.max.y),
    );
    painter.hline(foot.x_range(), foot.min.y, Stroke::new(1.0, theme::HAIR));
    pause_toggle(
        ui,
        Rect::from_center_size(pos2(center_x, foot.center().y), vec2(30.0, 17.0)),
        input.paused,
        out,
    );
}

fn tile(
    ui: &mut Ui,
    input: &TreeInput<'_>,
    index: usize,
    project: &ProjectView,
    view: Option<&TerminalsView>,
    out: &mut TreeOut,
) {
    let (full, _) = ui.allocate_exact_size(vec2(ui.available_width(), 46.0), Sense::hover());
    let rect = Rect::from_center_size(full.center(), Vec2::splat(40.0));
    let response = ui.interact(
        rect,
        Id::unique(("rail-tile", project.id.as_str())),
        Sense::click(),
    );
    let active = input.active == Some(index);
    let painter = ui.painter();
    painter.rect_filled(
        rect,
        10.0,
        if active {
            theme::ROW_ACTIVE
        } else {
            theme::PANEL_BG
        },
    );
    painter.rect_stroke(
        rect,
        10.0,
        Stroke::new(
            1.0,
            if active {
                theme::CYAN
            } else {
                theme::SIDEBAR_BORDER
            },
        ),
        StrokeKind::Inside,
    );
    painter.text(
        rect.center(),
        Align2::CENTER_CENTER,
        monogram(&project.name),
        FontId::proportional(13.0),
        if active {
            Color32::WHITE
        } else {
            theme::TEXT_ROW
        },
    );
    let dot = project_dot(input.seen, project, view);
    let at = rect.right_bottom() + vec2(-1.0, -1.0);
    painter.circle_filled(at, 7.0, theme::SIDEBAR_BG);
    dot.paint(painter, at, input.time);
    out.note(|| Hit::Tile(project.id.clone()), rect);
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::accesskit::Role::Button,
            true,
            active,
            format!("{}, {}", project.name, dot.word()),
        )
    });
    if response
        .on_hover_text(format!("{}\n{}", project.name, project.path))
        .clicked()
    {
        out.commands.push(Command::Activate(project.id.clone()));
    }
}

/// Words for the chooser: what a tool's terminals in this project say about
/// its login.
pub(crate) fn tool_login_hint(view: &TerminalsView, tool: ToolKind) -> &'static str {
    use connecto_core::{AuthState, PaneStage};
    let mut seen_any = false;
    for entry in view.terminals.iter().filter(|entry| entry.tool == tool) {
        seen_any = true;
        match entry.control.as_ref().map(|control| control.stage()) {
            Some(PaneStage::NeedsLogin {
                auth: AuthState::LoggedOut,
            }) => return "not logged in",
            Some(PaneStage::NeedsLogin {
                auth: AuthState::Unknown,
            }) => return "login status unknown",
            _ => {}
        }
    }
    if seen_any {
        "logged in"
    } else {
        "login checked when it starts"
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn monograms_take_two_letters() {
        assert_eq!(monogram("quill-worker"), "QW");
        assert_eq!(monogram("connecto"), "CO");
        assert_eq!(monogram("x"), "X");
        assert_eq!(monogram("payments api"), "PA");
        assert_eq!(monogram("--"), "?");
    }
}
