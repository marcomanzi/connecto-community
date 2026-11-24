//! The history sheet: closed terminals, searchable, with Continue and a
//! read-only transcript (project-tree-ui R18).

use std::ops::Range;

use connecto_core::{HistoryEntry, ProjectView, TerminalsView, ToolKind};
use egui::{
    Align2, Color32, FontId, Id, Key, Rect, Sense, Stroke, StrokeKind, Ui, pos2, text::LayoutJob,
    vec2,
};

use crate::app::Snapshot;
use crate::icons;
use crate::theme::{self, FieldStyle as _};
use crate::tree::fit;

/// The most transcript text the viewer lays out, from the end. A transcript is
/// kept up to 2 MiB, and egui lays out a whole text edit each time it changes.
const VIEW_LIMIT: usize = 512 * 1024;

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Scope {
    /// The project with this id.
    This(String),
    All,
}

pub(crate) struct History {
    pub(crate) scope: Scope,
    pub(crate) tool: Option<ToolKind>,
    pub(crate) query: String,
    selected: Option<(String, String)>,
    /// The transcript open: project id, history id.
    pub(crate) viewing: Option<(String, String)>,
    frames: u8,
    /// The transcript text has been given the keyboard.
    viewer_focused: bool,
}

impl History {
    /// Open the transcript of a closed terminal in place of the list.
    pub(crate) fn open_viewer(&mut self, project: String, history: String) {
        self.viewing = Some((project, history));
        self.viewer_focused = false;
    }

    pub(crate) fn new(scope: Scope) -> Self {
        Self {
            scope,
            tool: None,
            query: String::new(),
            selected: None,
            viewing: None,
            frames: 0,
            viewer_focused: false,
        }
    }
}

/// What the sheet asks the window to do.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum HistoryAction {
    Continue { project: String, history: String },
    ViewTranscript { project: String, history: String },
    Close,
}

#[derive(Default)]
pub(crate) struct HistoryOut {
    pub(crate) actions: Vec<HistoryAction>,
    #[cfg(test)]
    pub(crate) hits: HistoryHits,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct HistoryHits {
    pub(crate) search: Option<Rect>,
    pub(crate) filters: Vec<Rect>,
    pub(crate) rows: Vec<((String, String), Rect)>,
    pub(crate) continue_button: Option<Rect>,
    pub(crate) view_button: Option<Rect>,
    pub(crate) continue_id: Option<Id>,
    pub(crate) view_id: Option<Id>,
    pub(crate) transcript: Option<Rect>,
    pub(crate) back: Option<Rect>,
}

pub(crate) struct Row<'a> {
    pub(crate) project: &'a ProjectView,
    pub(crate) view: &'a TerminalsView,
    pub(crate) entry: &'a HistoryEntry,
}

impl Row<'_> {
    fn key(&self) -> (String, String) {
        (self.project.id.clone(), self.entry.id.clone())
    }

    fn title(&self) -> &str {
        self.entry
            .name
            .as_deref()
            .unwrap_or(&self.entry.terminal_id)
    }
}

/// Whether `entry` matches the query: a case-insensitive substring of its name,
/// terminal id, task or last reply (A4).
pub(crate) fn matches(entry: &HistoryEntry, query: &str) -> bool {
    let needle = query.trim().to_lowercase();
    if needle.is_empty() {
        return true;
    }
    [
        entry.name.as_deref(),
        Some(entry.terminal_id.as_str()),
        entry.task.as_deref(),
        entry.last_reply.as_deref(),
    ]
    .into_iter()
    .flatten()
    .any(|field| field.to_lowercase().contains(&needle))
}

/// The entries the sheet lists, newest first within each project.
pub(crate) fn rows<'a>(snap: &'a Snapshot, history: &History) -> Vec<Row<'a>> {
    let mut rows = Vec::new();
    for (index, project) in snap.ws.projects.iter().enumerate() {
        if let Scope::This(id) = &history.scope
            && *id != project.id
        {
            continue;
        }
        let Some(view) = snap.views.get(index).and_then(|view| view.as_deref()) else {
            continue;
        };
        rows.extend(
            view.history
                .iter()
                .filter(|entry| history.tool.is_none_or(|tool| entry.tool == tool))
                .filter(|entry| matches(entry, &history.query))
                .map(|entry| Row {
                    project,
                    view,
                    entry,
                }),
        );
    }
    rows.sort_by_key(|row| std::cmp::Reverse(row.entry.closed_at_ms));
    rows
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Group {
    Today,
    Yesterday,
    Earlier,
}

impl Group {
    const fn label(self) -> &'static str {
        match self {
            Self::Today => "TODAY",
            Self::Yesterday => "YESTERDAY",
            Self::Earlier => "EARLIER",
        }
    }
}

fn local_day(ms: u64, offset_s: i32) -> i64 {
    let seconds = i64::try_from(ms / 1000).unwrap_or(i64::MAX) + i64::from(offset_s);
    seconds.div_euclid(86_400)
}

pub(crate) fn group_of(closed_ms: u64, now_ms: u64, offset_s: i32) -> Group {
    match local_day(now_ms, offset_s) - local_day(closed_ms, offset_s) {
        ..=0 => Group::Today,
        1 => Group::Yesterday,
        _ => Group::Earlier,
    }
}

/// Year, month (1..=12) and day for days since 1970-01-01.
fn civil(days: i64) -> (i64, u32, u32) {
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let day = u32::try_from(doy - (153 * mp + 2) / 5 + 1).unwrap_or(1);
    let month = u32::try_from(if mp < 10 { mp + 3 } else { mp - 9 }).unwrap_or(1);
    let year = yoe + era * 400 + i64::from(month <= 2);
    (year, month, day)
}

/// "14:02" today, "Fri 18:20" within the last week, "12 Oct" before.
pub(crate) fn when(closed_ms: u64, now_ms: u64, offset_s: i32) -> String {
    const WEEKDAYS: [&str; 7] = ["Thu", "Fri", "Sat", "Sun", "Mon", "Tue", "Wed"];
    const MONTHS: [&str; 12] = [
        "Jan", "Feb", "Mar", "Apr", "May", "Jun", "Jul", "Aug", "Sep", "Oct", "Nov", "Dec",
    ];
    let seconds = i64::try_from(closed_ms / 1000).unwrap_or(i64::MAX) + i64::from(offset_s);
    let day = seconds.div_euclid(86_400);
    let secs_of_day = seconds.rem_euclid(86_400);
    let clock = format!("{:02}:{:02}", secs_of_day / 3600, secs_of_day % 3600 / 60);
    let age = local_day(now_ms, offset_s) - day;
    if age <= 0 {
        clock
    } else if age < 7 {
        let weekday = WEEKDAYS[usize::try_from(day.rem_euclid(7)).unwrap_or(0)];
        format!("{weekday} {clock}")
    } else {
        let (_, month, date) = civil(day);
        format!(
            "{date} {}",
            MONTHS[usize::try_from(month - 1).unwrap_or(0).min(11)]
        )
    }
}

/// Where `query` occurs in `text`, ignoring case. Empty when the lowercase form
/// changes length, which only odd scripts do.
fn highlights(text: &str, query: &str) -> Vec<Range<usize>> {
    let needle: Vec<char> = query.trim().chars().flat_map(char::to_lowercase).collect();
    if needle.is_empty() {
        return Vec::new();
    }
    let chars: Vec<(usize, char)> = text.char_indices().collect();
    let mut lowered = Vec::with_capacity(chars.len());
    for (_, c) in &chars {
        let mut lower = c.to_lowercase();
        match (lower.next(), lower.next()) {
            (Some(one), None) => lowered.push(one),
            _ => return Vec::new(),
        }
    }
    let mut found = Vec::new();
    let mut at = 0;
    while at + needle.len() <= lowered.len() {
        if lowered[at..at + needle.len()] == needle[..] {
            let start = chars[at].0;
            let end = chars
                .get(at + needle.len())
                .map_or(text.len(), |(index, _)| *index);
            found.push(start..end);
            at += needle.len();
        } else {
            at += 1;
        }
    }
    found
}

fn job(text: &str, query: &str, font: FontId, color: Color32, width: f32) -> LayoutJob {
    let mut job = LayoutJob::default();
    let mut last = 0;
    for range in highlights(text, query) {
        job.append(
            &text[last..range.start],
            0.0,
            egui::TextFormat::simple(font.clone(), color),
        );
        job.append(
            &text[range.clone()],
            0.0,
            egui::TextFormat {
                font_id: font.clone(),
                color: theme::CYAN,
                background: theme::CYAN.gamma_multiply(0.2),
                ..Default::default()
            },
        );
        last = range.end;
    }
    job.append(&text[last..], 0.0, egui::TextFormat::simple(font, color));
    job.wrap = egui::epaint::text::TextWrapping::truncate_at_width(width.max(1.0));
    job
}

pub(crate) struct HistoryInput<'a> {
    pub(crate) snap: &'a Snapshot,
    pub(crate) now_ms: u64,
    pub(crate) utc_offset_s: i32,
    pub(crate) project_name: &'a dyn Fn(&str) -> String,
    /// Why a project's history could not be read or acted on (a remote
    /// project's host did not answer), by project id.
    pub(crate) error: &'a dyn Fn(&str) -> Option<String>,
}

/// The sheet over `area`. It closes with Esc, a click on the dimmed part, or a
/// Continue.
pub(crate) fn sheet(
    ctx: &egui::Context,
    area: Rect,
    history: &mut History,
    input: &HistoryInput<'_>,
    out: &mut HistoryOut,
) {
    let closed = crate::sheet::show(ctx, area, "history-panel", "History", |ui, content| {
        if history.viewing.is_some() {
            viewer(ui, content, history, input, out);
        } else {
            list(ui, content, history, input, out);
        }
    });
    if closed {
        out.actions.push(HistoryAction::Close);
    }
    history.frames = history.frames.saturating_add(1);
}

fn chip(ui: &mut Ui, pos: egui::Pos2, text: &str, on: bool, id: Id) -> Rect {
    let galley =
        ui.painter()
            .layout_no_wrap(text.to_owned(), FontId::monospace(11.0), theme::TEXT_MUTED);
    let rect = Rect::from_min_size(pos, vec2(galley.size().x + 18.0, 22.0));
    let response = ui.interact(rect, id, Sense::click());
    let painter = ui.painter();
    if on {
        painter.rect_filled(rect, 99.0, theme::RAISED_BG);
    }
    painter.rect_stroke(
        rect,
        99.0,
        Stroke::new(1.0, if on { theme::TEAL } else { theme::BORDER }),
        StrokeKind::Inside,
    );
    let color = if on {
        Color32::WHITE
    } else {
        theme::TEXT_MUTED
    };
    painter.galley(
        rect.min + vec2(9.0, 11.0 - galley.size().y / 2.0),
        galley,
        color,
    );
    let label = text.to_owned();
    response.widget_info(|| {
        egui::WidgetInfo::selected(egui::accesskit::Role::Button, true, on, label.clone())
    });
    if response.clicked() {
        ui.data_mut(|data| data.insert_temp(Id::unique("history-chip"), id));
    }
    rect
}

fn list(
    ui: &mut Ui,
    rect: Rect,
    history: &mut History,
    input: &HistoryInput<'_>,
    out: &mut HistoryOut,
) {
    let _ = &out;
    // Search.
    let search = Rect::from_min_size(rect.min + vec2(14.0, 14.0), vec2(rect.width() - 28.0, 32.0));
    let painter = ui.painter();
    painter.rect_filled(search, 8.0, theme::TERM_BG);
    painter.rect_stroke(
        search,
        8.0,
        Stroke::new(1.0, theme::TEAL),
        StrokeKind::Inside,
    );
    icons::search(
        painter,
        search.left_center() + vec2(16.0, -1.0),
        theme::FAINT,
    );
    let edit = egui::TextEdit::singleline(&mut history.query)
        .bare()
        .id(Id::unique("history-search"))
        .hint_text("Search name, task or reply")
        .font(FontId::monospace(12.5))
        .desired_width(search.width() - 38.0);
    let field = Rect::from_min_max(search.min + vec2(30.0, 0.0), search.max);
    let search_response = ui.put(field, edit);
    if history.frames == 0 {
        search_response.request_focus();
    }
    #[cfg(test)]
    {
        out.hits.search = Some(search);
    }

    // Filters.
    ui.data_mut(|data| data.remove::<Id>(Id::unique("history-chip")));
    let mut x = rect.min.x + 14.0;
    let y = search.max.y + 10.0;
    let this_project = match &history.scope {
        Scope::This(id) => Some(id.clone()),
        Scope::All => None,
    };
    // One chip per agent: a click shows only that agent's, a second all again.
    let agent_chips: Vec<(String, ToolKind)> = ToolKind::ALL
        .into_iter()
        .filter(|tool| tool.is_agent())
        .map(|tool| (format!("history-chip-{}", tool.name()), tool))
        .collect();
    let mut chips: Vec<(&str, bool, &str)> = vec![
        ("This project", this_project.is_some(), "history-chip-this"),
        ("All projects", this_project.is_none(), "history-chip-all"),
    ];
    for (id, tool) in &agent_chips {
        chips.push((tool.label(), history.tool == Some(*tool), id.as_str()));
    }
    for (label, on, id) in chips {
        let rect = chip(ui, pos2(x, y), label, on, Id::unique(id));
        #[cfg(test)]
        out.hits.filters.push(rect);
        x = rect.max.x + 6.0;
    }
    let clicked: Option<Id> = ui.data(|data| data.get_temp(Id::unique("history-chip")));
    if let Some(id) = clicked {
        let active = input.snap.ws.active.clone();
        if id == Id::unique("history-chip-this") {
            if let Some(project) =
                active.or_else(|| input.snap.ws.projects.first().map(|p| p.id.clone()))
            {
                history.scope = Scope::This(project);
            }
        } else if id == Id::unique("history-chip-all") {
            history.scope = Scope::All;
        } else if let Some((_, tool)) = agent_chips
            .iter()
            .find(|(chip_id, _)| id == Id::unique(chip_id.as_str()))
        {
            history.tool = if history.tool == Some(*tool) {
                None
            } else {
                Some(*tool)
            };
        }
    }

    let rows = rows(input.snap, history);
    let selected = history
        .selected
        .clone()
        .filter(|key| rows.iter().any(|row| row.key() == *key))
        .or_else(|| rows.first().map(Row::key));
    let list_rect = Rect::from_min_max(pos2(rect.min.x, y + 32.0), rect.max);
    if rows.is_empty() {
        let error = input
            .snap
            .ws
            .projects
            .iter()
            .filter(|p| match &history.scope {
                Scope::This(id) => *id == p.id,
                Scope::All => true,
            })
            .find_map(|p| (input.error)(&p.id));
        let (message, color) = match &error {
            Some(error) => (error.as_str(), theme::ERROR),
            None if history.query.trim().is_empty() => (
                "Nothing closed yet. A terminal you close is kept here.",
                theme::TEXT_MUTED,
            ),
            None => ("No closed terminal matches.", theme::TEXT_MUTED),
        };
        ui.painter().text(
            list_rect.min + vec2(16.0, 18.0),
            Align2::LEFT_TOP,
            message,
            FontId::proportional(12.5),
            color,
        );
        key_nav(ui, history, &rows, &selected, out);
        return;
    }
    let all_projects = history.scope == Scope::All;
    ui.scope_builder(egui::UiBuilder::new().max_rect(list_rect), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("history-list")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.spacing_mut().item_spacing.y = 0.0;
                let mut group = None;
                for row in &rows {
                    let this = group_of(row.entry.closed_at_ms, input.now_ms, input.utc_offset_s);
                    if group != Some(this) {
                        group = Some(this);
                        let (rect, _) = ui
                            .allocate_exact_size(vec2(ui.available_width(), 28.0), Sense::hover());
                        ui.painter().text(
                            rect.left_bottom() + vec2(16.0, -4.0),
                            Align2::LEFT_BOTTOM,
                            this.label(),
                            FontId::monospace(10.5),
                            theme::FAINT,
                        );
                    }
                    let on = selected.as_ref() == Some(&row.key());
                    history_row(ui, row, on, all_projects, history, input, out);
                }
                ui.add_space(16.0);
            });
    });
    key_nav(ui, history, &rows, &selected, out);
}

fn history_row(
    ui: &mut Ui,
    row: &Row<'_>,
    on: bool,
    all_projects: bool,
    history: &mut History,
    input: &HistoryInput<'_>,
    out: &mut HistoryOut,
) {
    let entry = row.entry;
    let height = if on { 76.0 } else { 52.0 };
    let (rect, _) = ui.allocate_exact_size(vec2(ui.available_width(), height), Sense::hover());
    let response = ui.interact(
        rect,
        Id::unique(("history-row", &row.project.id, &entry.id)),
        Sense::click(),
    );
    let painter = ui.painter();
    painter.hline(rect.x_range(), rect.min.y, Stroke::new(1.0, theme::HAIR));
    if on {
        painter.rect_filled(rect, 0.0, theme::TERM_ON);
    } else if response.hovered() {
        painter.rect_filled(rect, 0.0, theme::ROW_HOVER);
    }
    let when = when(entry.closed_at_ms, input.now_ms, input.utc_offset_s);
    let when_galley = painter.layout_no_wrap(when, FontId::monospace(11.0), theme::FAINT);
    let when_width = when_galley.size().x;
    painter.galley(
        pos2(rect.max.x - 16.0 - when_width, rect.min.y + 11.0),
        when_galley,
        if on { theme::TEXT_MUTED } else { theme::FAINT },
    );
    let text_width = rect.width() - 32.0 - when_width - 12.0;
    let name = painter.layout_job(job(
        row.title(),
        &history.query,
        FontId::proportional(13.0),
        Color32::from_rgb(0xe9, 0xef, 0xf3),
        text_width,
    ));
    painter.galley(
        pos2(rect.min.x + 16.0, rect.min.y + 9.0),
        name,
        Color32::WHITE,
    );

    let mut line = entry.terminal_id.clone();
    if let Some(parent) = &entry.parent {
        let parent_name = row
            .view
            .terminals
            .iter()
            .find(|candidate| candidate.id == *parent)
            .map_or_else(
                || parent.clone(),
                |candidate| crate::tree::title(candidate).to_owned(),
            );
        line.push_str(&format!(" · under {parent_name}"));
    }
    if all_projects {
        line.push_str(&format!(" · {}", (input.project_name)(&row.project.id)));
    }
    if let Some(text) = entry.last_reply.as_ref().or(entry.task.as_ref()) {
        let one: String = text.lines().next().unwrap_or_default().to_owned();
        line.push_str(&format!(" · \"{one}\""));
    }
    let line_width = rect.width() - 32.0;
    let line_galley = painter.layout_job(job(
        &line,
        &history.query,
        FontId::monospace(11.0),
        Color32::from_rgb(0x8a, 0xa0, 0xae),
        line_width,
    ));
    painter.galley(
        pos2(rect.min.x + 16.0, rect.min.y + 30.0),
        line_galley,
        theme::TEXT_MUTED,
    );
    let _ = fit;

    #[cfg(test)]
    out.hits.rows.push((row.key(), rect));
    let mut acted = false;
    if on {
        let mut x = rect.min.x + 16.0;
        let y = rect.max.y - 28.0;
        if entry.resumable {
            let button = Rect::from_min_size(pos2(x, y), vec2(76.0, 22.0));
            let continue_button = ui.put(button, theme::primary("Continue").small());
            #[cfg(test)]
            {
                out.hits.continue_button = Some(continue_button.rect);
                out.hits.continue_id = Some(continue_button.id);
            }
            if continue_button.clicked() {
                out.actions.push(HistoryAction::Continue {
                    project: row.project.id.clone(),
                    history: entry.id.clone(),
                });
                acted = true;
            }
            x = continue_button.rect.max.x + 6.0;
        }
        let view = ui.put(
            Rect::from_min_size(pos2(x, y), vec2(112.0, 22.0)),
            theme::button("View transcript").small(),
        );
        #[cfg(test)]
        {
            out.hits.view_button = Some(view.rect);
            out.hits.view_id = Some(view.id);
        }
        if view.clicked() {
            out.actions.push(HistoryAction::ViewTranscript {
                project: row.project.id.clone(),
                history: entry.id.clone(),
            });
            acted = true;
        }
    }
    response.widget_info(|| {
        egui::WidgetInfo::selected(
            egui::accesskit::Role::Button,
            true,
            on,
            format!("{}, closed {}", row.title(), entry.terminal_id),
        )
    });
    if response.clicked() && !acted {
        history.selected = Some(row.key());
    }
}

/// Up and Down move the selection, Enter acts on it, Esc closes.
fn key_nav(
    ui: &Ui,
    history: &mut History,
    rows: &[Row<'_>],
    selected: &Option<(String, String)>,
    out: &mut HistoryOut,
) {
    let (down, up, enter, escape) = ui.input(|input| {
        (
            input.key_pressed(Key::ArrowDown),
            input.key_pressed(Key::ArrowUp),
            input.key_pressed(Key::Enter),
            input.key_pressed(Key::Escape),
        )
    });
    if escape {
        out.actions.push(HistoryAction::Close);
        return;
    }
    // A focused button took Enter for itself; only the search field, or
    // nothing, leaves it to the list.
    let own = ui
        .ctx()
        .memory(|memory| memory.focused())
        .is_none_or(|id| id == Id::unique("history-search"));
    let enter = enter && own;
    let Some(index) = selected
        .as_ref()
        .and_then(|key| rows.iter().position(|row| row.key() == *key))
    else {
        return;
    };
    if down && index + 1 < rows.len() {
        history.selected = Some(rows[index + 1].key());
    } else if up && index > 0 {
        history.selected = Some(rows[index - 1].key());
    } else if enter {
        let row = &rows[index];
        let (project, id) = (row.project.id.clone(), row.entry.id.clone());
        out.actions.push(if row.entry.resumable {
            HistoryAction::Continue {
                project,
                history: id,
            }
        } else {
            HistoryAction::ViewTranscript {
                project,
                history: id,
            }
        });
    }
}

fn viewer(
    ui: &mut Ui,
    rect: Rect,
    history: &mut History,
    input: &HistoryInput<'_>,
    out: &mut HistoryOut,
) {
    let _ = &out;
    let Some((project, hid)) = history.viewing.clone() else {
        return;
    };
    let found = input
        .snap
        .ws
        .projects
        .iter()
        .position(|candidate| candidate.id == project)
        .and_then(|index| input.snap.views.get(index).and_then(|view| view.as_deref()));
    let entry = found.and_then(|view| view.history.iter().find(|entry| entry.id == hid));
    let title = entry.map_or("Transcript".to_owned(), |entry| {
        format!(
            "{} · {}",
            entry.name.as_deref().unwrap_or(&entry.terminal_id),
            entry.terminal_id
        )
    });

    let back = Rect::from_min_size(rect.min + vec2(14.0, 12.0), vec2(72.0, 24.0));
    let response = ui.put(back, theme::button("← Back").small());
    #[cfg(test)]
    {
        out.hits.back = Some(response.rect);
    }
    if response.clicked() {
        history.viewing = None;
        return;
    }
    ui.painter().text(
        pos2(back.max.x + 12.0, back.center().y),
        Align2::LEFT_CENTER,
        title,
        FontId::proportional(13.0),
        Color32::WHITE,
    );
    if ui.input(|input| input.key_pressed(Key::Escape)) {
        history.viewing = None;
        return;
    }

    let body = Rect::from_min_max(
        pos2(rect.min.x + 14.0, back.max.y + 10.0),
        pos2(rect.max.x - 14.0, rect.max.y - 14.0),
    );
    let painter = ui.painter();
    painter.rect_filled(body, 8.0, theme::TERM_BG);
    painter.rect_stroke(
        body,
        8.0,
        Stroke::new(1.0, theme::BORDER),
        StrokeKind::Inside,
    );
    let loaded = found
        .and_then(|view| view.transcript.as_ref())
        .filter(|transcript| transcript.history_id == hid);
    let Some(transcript) = loaded else {
        let error = (input.error)(&project);
        ui.painter().text(
            body.min + vec2(14.0, 14.0),
            Align2::LEFT_TOP,
            error.as_deref().unwrap_or("Loading transcript…"),
            FontId::proportional(12.5),
            if error.is_some() {
                theme::ERROR
            } else {
                theme::TEXT_MUTED
            },
        );
        return;
    };
    let full = transcript.text.as_str();
    let mut start = full.len().saturating_sub(VIEW_LIMIT);
    while !full.is_char_boundary(start) {
        start += 1;
    }
    let mut shown: &str = &full[start..];
    let mut inner = body.shrink(8.0);
    if start > 0 {
        ui.painter().text(
            inner.min,
            Align2::LEFT_TOP,
            "Showing the last 512 KiB of this transcript.",
            FontId::proportional(11.5),
            theme::CORAL_SOFT,
        );
        inner.min.y += 20.0;
    }
    #[cfg(test)]
    {
        out.hits.transcript = Some(inner);
    }
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        egui::ScrollArea::vertical()
            .id_salt("transcript")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                // A `&str` buffer cannot be edited, only selected and copied.
                let edit = egui::TextEdit::multiline(&mut shown)
                    .bare()
                    .id(Id::unique("transcript-text"))
                    .font(FontId::monospace(12.0))
                    .text_color(Color32::from_rgb(0xc9, 0xd6, 0xdf))
                    .desired_width(f32::INFINITY);
                let response = ui.add(edit);
                if !history.viewer_focused {
                    history.viewer_focused = true;
                    response.request_focus();
                }
            });
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(
        name: Option<&str>,
        id: &str,
        task: Option<&str>,
        reply: Option<&str>,
    ) -> HistoryEntry {
        HistoryEntry {
            id: format!("h-{id}"),
            terminal_id: id.to_owned(),
            name: name.map(str::to_owned),
            tool: ToolKind::Claude,
            parent: None,
            closed_at_ms: 0,
            task: task.map(str::to_owned),
            last_reply: reply.map(str::to_owned),
            resumable: true,
        }
    }

    #[test]
    fn search_matches_name_id_task_and_reply_ignoring_case() {
        let e = entry(
            Some("Write 503 Test Cases"),
            "claude-3",
            Some("cover TIMEOUT"),
            Some("6 tests added"),
        );
        for query in [
            "503",
            "test cases",
            "CLAUDE-3",
            "timeout",
            "tests added",
            "  503  ",
            "",
        ] {
            assert!(matches(&e, query), "{query}");
        }
        for query in ["codex", "nothing here"] {
            assert!(!matches(&e, query), "{query}");
        }
    }

    #[test]
    fn days_group_by_local_midnight() {
        // 2026-11-08 12:00 UTC.
        let now = 1_794_139_200_000_u64;
        let hour = 3_600_000;
        assert_eq!(group_of(now - hour, now, 0), Group::Today);
        assert_eq!(group_of(now - 13 * hour, now, 0), Group::Yesterday);
        assert_eq!(group_of(now - 40 * hour, now, 0), Group::Earlier);
        // Twelve hours on, in UTC, it is the next day: 11:00 is yesterday.
        assert_eq!(group_of(now - hour, now + 14 * hour, 0), Group::Yesterday);
        assert_eq!(group_of(now, now, 14 * 3600), Group::Today);
        assert_eq!(group_of(now - 11 * hour, now, 14 * 3600), Group::Yesterday);
    }

    #[test]
    fn times_read_as_clock_weekday_or_date() {
        let now = 1_794_139_200_000_u64; // Sun 2026-11-08 12:00 UTC
        assert_eq!(when(now - 600_000, now, 0), "11:50");
        assert_eq!(when(now - 18 * 3_600_000, now, 0), "Sat 18:00");
        assert_eq!(when(now - 20 * 24 * 3_600_000, now, 0), "19 Oct");
    }

    #[test]
    fn highlights_find_every_case_insensitive_occurrence() {
        assert_eq!(highlights("Map 503s from 503", "503"), [4..7, 14..17]);
        assert_eq!(highlights("Écrire", "é"), vec![0..2]);
        assert!(highlights("abc", "").is_empty());
    }
}
