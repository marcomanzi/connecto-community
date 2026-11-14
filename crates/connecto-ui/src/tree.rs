//! The sidebar: the tree of open projects and their terminals, or the narrow
//! rail of project tiles when it is collapsed (project-tree-ui R15).
//!
//! Drawing only reads the views and returns `Command`s. Nothing here talks to
//! a session, so a click can never type into a terminal.

use std::collections::{HashMap, HashSet};
use std::sync::Arc;

use connecto_core::{
    MAX_TERMINALS, ProjectView, TermActivity, TerminalEntry, TerminalsView, ToolKind,
};
use egui::epaint::text::TextWrapping;
use egui::text::LayoutJob;
use egui::{
    Align2, Color32, FontId, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, Vec2, pos2,
    vec2,
};

use crate::app::Snapshot;
use crate::commands::Command;
use crate::dots::Dot;
use crate::header;
use crate::icons;
use crate::rename::{Rename, RenameEnd, RenameTarget};
use crate::theme;

pub(crate) const SIDEBAR_WIDTH: f32 = 286.0;
pub(crate) const RAIL_WIDTH: f32 = 60.0;
/// The band the macOS traffic lights sit in; none off macOS.
pub(crate) const LIGHTS_BAND: f32 = crate::platform::LIGHTS_BAND;
const HEAD_ROW: f32 = 36.0;
const FOOT_HEIGHT: f32 = 46.0;
const PROJECT_ROW: f32 = 32.0;
const TERMINAL_ROW: f32 = 40.0;
const CHILD_ROW: f32 = 38.0;
const NEW_ROW: f32 = 28.0;
const SIDE_PAD: f32 = 8.0;
const ICON_BUTTON: f32 = 24.0;

/// A control, named so tests can find where it was drawn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Hit {
    Project(String),
    Twisty(String),
    Clock(String),
    Gear(String),
    Terminal(String, String),
    Close(String, String),
    Robot(String, String),
    Kids(String, String),
    NewRow(String),
    HeaderHistory,
    HeaderPlus,
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
    entry.name.as_deref().unwrap_or(&entry.id)
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
    // The host switcher sits above the PROJECTS row, not inside the scrolling
    // list, so "PROJECTS" and its buttons head the projects and nothing else.
    let hosts_height = crate::hosts::section_height(input.hosts.len());
    let header_rect = Rect::from_min_size(
        rect.min,
        vec2(rect.width(), LIGHTS_BAND + hosts_height + HEAD_ROW),
    );
    let drag = ui.interact(
        header_rect,
        Id::unique("sidebar-drag"),
        Sense::click_and_drag(),
    );
    out.note(|| Hit::Header, header_rect);

    let row_y = rect.min.y + LIGHTS_BAND + hosts_height + HEAD_ROW / 2.0 - 2.0;
    let last_x = rect.max.x - 12.0 - ICON_BUTTON / 2.0;
    // Without a band for the traffic lights (off macOS), the collapse button
    // joins the PROJECTS row, left of History.
    let collapse_at = if LIGHTS_BAND > 0.0 {
        pos2(last_x, rect.min.y + LIGHTS_BAND / 2.0)
    } else {
        pos2(last_x - 2.0 * (ICON_BUTTON + 2.0), row_y)
    };
    let collapse = icon_button(
        ui,
        collapse_at,
        "sidebar-collapse",
        concat!("Hide sidebar  ", crate::platform::kbd!("\\")),
        false,
        icons::sidebar,
    );
    painter.text(
        pos2(rect.min.x + 16.0, row_y),
        Align2::LEFT_CENTER,
        "PROJECTS",
        FontId::monospace(10.5),
        theme::FAINT,
    );
    let plus = icon_button(
        ui,
        pos2(last_x, row_y),
        "sidebar-open",
        concat!("Open project  ", crate::platform::kbd!("O")),
        false,
        |p, c, col| icons::plus(p, c, 5.0, col),
    );
    let clock = icon_button(
        ui,
        pos2(plus.rect.center().x - ICON_BUTTON - 2.0, row_y),
        "sidebar-history",
        concat!("History  ", crate::platform::kbd!("Y")),
        input.history_open,
        icons::clock,
    );
    out.note(|| Hit::Collapse, collapse.rect);
    out.note(|| Hit::HeaderPlus, plus.rect);
    out.note(|| Hit::HeaderHistory, clock.rect);
    if collapse.clicked() {
        out.commands.push(Command::ToggleSidebar);
    }
    if plus.clicked() {
        out.commands.push(Command::OpenProject);
    }
    if clock.clicked() {
        out.commands.push(Command::History);
    }
    header::window_gestures(ui, &drag, &[collapse.rect, plus.rect, clock.rect]);
    let hosts_rect = Rect::from_min_size(
        pos2(rect.min.x + SIDE_PAD, rect.min.y + LIGHTS_BAND),
        vec2(rect.width() - SIDE_PAD * 2.0 - 1.0, hosts_height),
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
                ui.spacing_mut().item_spacing.y = 1.0;
                ui.set_width(body.width() - SIDE_PAD * 2.0);
                ui.add_space(0.0);
                tree.body(ui);
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
            return;
        }
        for (index, project) in snap.ws.projects.iter().enumerate() {
            if index > 0 {
                ui.add_space(5.0);
            }
            let view = snap.views.get(index).and_then(|view| view.as_deref());
            self.project(ui, index, project, view);
            // Only the selected project lists its terminals.
            if !project.expanded || self.input.active != Some(index) {
                continue;
            }
            if let Some(error) = &project.error {
                self.error_line(ui, error);
            } else if let Some(view) = view {
                self.terminals(ui, index, project, view);
            }
        }
        ui.add_space(8.0);
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
        let row = ui.interact(rect, Id::unique(("tree-project", id)), Sense::click());
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
        let center_y = rect.center().y;

        let twisty_rect = Rect::from_min_size(rect.min, vec2(26.0, PROJECT_ROW));
        let twisty = ui.interact(twisty_rect, Id::unique(("tree-twisty", id)), Sense::click());
        let painter = ui.painter();
        icons::twisty(
            painter,
            pos2(rect.min.x + 14.0, center_y),
            open,
            if twisty.hovered() {
                theme::TEXT
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
        dot.paint(painter, pos2(rect.min.x + 33.0, center_y), self.input.time);

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
        let show_clock = active || row.contains_pointer();
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

        // The name, or the field that renames it.
        let name_left = rect.min.x + 46.0;
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
            self.out
                .commands
                .push(Command::RenameProjectAt(id.to_owned()));
        } else if row.clicked() && !on_control(row.interact_pointer_pos()) {
            self.out.commands.push(Command::Activate(id.to_owned()));
        }
        row.context_menu(|ui| {
            for (label, command) in [
                ("Rename project…", Command::RenameProjectAt(id.to_owned())),
                ("History", Command::HistoryOf(id.to_owned())),
                (
                    "Project settings…",
                    Command::ProjectSettingsAt(id.to_owned()),
                ),
                ("Close project", Command::CloseProjectAt(id.to_owned())),
            ] {
                if ui.button(label).clicked() {
                    self.out.commands.push(command);
                    ui.close();
                }
            }
        });
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

        // Two lines: the name, then the state (the id is on hover).
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
        let meta = meta_line(entry, dot);
        let meta_color = if shown || hovered {
            theme::TEXT_MUTED
        } else {
            theme::FAINT
        };
        let painter = ui.painter();
        let galley = fit(painter, &meta, FontId::monospace(10.5), meta_color, width);
        painter.galley(
            pos2(text_left, rect.min.y + (height - 15.0)),
            galley,
            meta_color,
        );

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
                    state_words(entry, dot)
                ),
            )
        });
        let on_control = |pos: Option<Pos2>| {
            pos.is_some_and(|pos| {
                kids_rect.is_some_and(|rect| rect.contains(pos))
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
        if entry.name.is_some() {
            row.clone()
                .on_hover_text(format!("Terminal id: {}", entry.id));
        }
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
        });
        rect
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
pub(crate) fn meta_line(entry: &TerminalEntry, dot: Dot) -> String {
    match &entry.asked_by {
        Some(asker) => format!("asked by {asker}"),
        None => state_words(entry, dot).to_owned(),
    }
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
        assert_eq!(monogram("orbit-worker"), "OW");
        assert_eq!(monogram("connecto"), "CO");
        assert_eq!(monogram("x"), "X");
        assert_eq!(monogram("payments api"), "PA");
        assert_eq!(monogram("--"), "?");
    }
}
