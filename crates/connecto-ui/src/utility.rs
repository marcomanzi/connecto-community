//! The utility sessions sheet: the latest hidden titling runs of the project on
//! show, and the model and effort they use.

use connecto_core::{UtilityEffort, UtilityModel, UtilityRun, UtilitySettings, UtilityStatus};
use egui::{Align2, Color32, FontId, Id, Key, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::history::when;
use crate::icons;
use crate::theme;
use crate::tree::fit;

const ROW_HEIGHT: f32 = 62.0;

#[derive(Default)]
pub(crate) struct UtilitySheet {
    /// The model and effort popover is open.
    pub(crate) settings_open: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum UtilityAction {
    Settings(UtilitySettings),
    /// Show this run's session in the main area.
    Open(u64),
    Close,
}

pub(crate) struct UtilityInput<'a> {
    pub(crate) runs: &'a [UtilityRun],
    pub(crate) settings: UtilitySettings,
    pub(crate) now_ms: u64,
    pub(crate) utc_offset_s: i32,
    /// Runs whose session is still there to open.
    pub(crate) openable: &'a std::collections::HashSet<u64>,
}

#[derive(Default)]
pub(crate) struct UtilityOut {
    pub(crate) actions: Vec<UtilityAction>,
    #[cfg(test)]
    pub(crate) hits: UtilityHits,
}

#[cfg(test)]
#[derive(Default)]
pub(crate) struct UtilityHits {
    pub(crate) gear: Option<Rect>,
    pub(crate) models: Vec<(UtilityModel, Rect)>,
    pub(crate) efforts: Vec<(UtilityEffort, Rect)>,
    /// Each run drawn, with the lines of text of its row.
    pub(crate) rows: Vec<(u64, Vec<String>)>,
    pub(crate) row_rects: Vec<(u64, Rect)>,
    pub(crate) empty: bool,
    pub(crate) empty_text: Option<Rect>,
}

/// "just now", "5 min ago", "3 h ago", then the clock or date.
pub(crate) fn ago(then_ms: u64, now_ms: u64, offset_s: i32) -> String {
    let secs = now_ms.saturating_sub(then_ms) / 1000;
    if secs < 45 {
        "just now".to_owned()
    } else if secs < 3600 {
        format!("{} min ago", (secs / 60).max(1))
    } else if secs < 86_400 {
        format!("{} h ago", secs / 3600)
    } else {
        when(then_ms, now_ms, offset_s)
    }
}

pub(crate) fn sheet(
    ctx: &egui::Context,
    area: Rect,
    state: &mut UtilitySheet,
    input: &UtilityInput<'_>,
    out: &mut UtilityOut,
) {
    let closed = crate::sheet::show(
        ctx,
        area,
        "utility-panel",
        "Utility Sessions",
        |ui, content| {
            body(ui, content, state, input, out);
        },
    );
    if closed {
        out.actions.push(UtilityAction::Close);
    }
}

fn body(
    ui: &mut Ui,
    rect: Rect,
    state: &mut UtilitySheet,
    input: &UtilityInput<'_>,
    out: &mut UtilityOut,
) {
    if ui.input(|i| i.key_pressed(Key::Escape)) {
        if state.settings_open {
            state.settings_open = false;
        } else {
            out.actions.push(UtilityAction::Close);
        }
    }
    // The gear sits left of the sheet's close button, in the header.
    let close = crate::sheet::close_center(crate::sheet::header_of(rect));
    let gear_center = pos2(close.x - 32.0, close.y);
    let head_y = rect.min.y + 12.0;
    let gear = ui.interact(
        Rect::from_center_size(gear_center, vec2(26.0, 26.0)),
        Id::unique("utility-gear"),
        Sense::click(),
    );
    {
        let painter = ui.painter();
        if gear.hovered() || state.settings_open {
            painter.circle_filled(gear_center, 13.0, theme::RAISED_BG);
        }
        icons::gear(
            painter,
            gear_center,
            if state.settings_open {
                theme::CYAN
            } else {
                theme::TEXT_MUTED
            },
        );
    }
    gear.widget_info(|| {
        egui::WidgetInfo::labeled(egui::accesskit::Role::Button, true, "Utility settings")
    });
    #[cfg(test)]
    {
        out.hits.gear = Some(gear.rect);
    }
    if gear.on_hover_text("Model and effort").clicked() {
        state.settings_open = !state.settings_open;
    }
    let s = input.settings;
    ui.painter().text(
        pos2(rect.min.x + 16.0, head_y),
        Align2::LEFT_CENTER,
        format!("{} · {}", s.model.label(), s.effort.label()),
        FontId::monospace(11.0),
        theme::FAINT,
    );

    let list_top = head_y + 16.0;
    let list = Rect::from_min_max(pos2(rect.min.x, list_top), rect.max);
    if input.runs.is_empty() {
        #[cfg(test)]
        {
            out.hits.empty = true;
        }
        let painter = ui.painter();
        let galley = painter.layout(
            "No utility sessions yet. Use the robot on a terminal to generate a title.".to_owned(),
            FontId::proportional(12.0),
            theme::FAINT,
            rect.width() - 32.0,
        );
        #[cfg(test)]
        {
            out.hits.empty_text = Some(Rect::from_min_size(
                pos2(rect.min.x + 16.0, list_top + 10.0),
                galley.size(),
            ));
        }
        painter.galley(
            pos2(rect.min.x + 16.0, list_top + 10.0),
            galley,
            theme::FAINT,
        );
    } else {
        let mut child = ui.new_child(egui::UiBuilder::new().max_rect(list));
        egui::ScrollArea::vertical()
            .id_salt("utility-runs")
            .auto_shrink([false, false])
            .show(&mut child, |ui| {
                for run in input.runs {
                    run_row(ui, rect.width(), run, input, out);
                }
            });
    }

    if state.settings_open {
        popover(ui, rect, head_y, state, input, out);
    }
}

fn run_row(
    ui: &mut Ui,
    width: f32,
    run: &UtilityRun,
    input: &UtilityInput<'_>,
    out: &mut UtilityOut,
) {
    let openable = input.openable.contains(&run.id);
    let sense = if openable {
        Sense::click()
    } else {
        Sense::focusable_noninteractive()
    };
    let (rect, response) = ui.allocate_exact_size(vec2(width, ROW_HEIGHT), sense);
    let dim = |color: Color32| {
        if openable {
            color
        } else {
            color.gamma_multiply(0.55)
        }
    };
    let painter = ui.painter();
    if openable && response.hovered() {
        painter.rect_filled(rect, 0.0, theme::ROW_HOVER);
    }
    painter.hline(
        rect.x_range().shrink(14.0),
        rect.max.y - 0.5,
        Stroke::new(1.0, theme::HAIR),
    );
    let left = rect.min.x + 16.0;
    let text_width = rect.width() - 32.0;
    let time = ago(run.started_ms, input.now_ms, input.utc_offset_s);
    let time_galley =
        painter.layout_no_wrap(time.clone(), FontId::monospace(10.5), dim(theme::FAINT));
    let name = fit(
        painter,
        &run.terminal_label,
        FontId::proportional(12.5),
        dim(Color32::WHITE),
        text_width - time_galley.size().x - 10.0,
    );
    painter.galley(pos2(left, rect.min.y + 8.0), name, dim(Color32::WHITE));
    painter.galley(
        pos2(rect.max.x - 16.0 - time_galley.size().x, rect.min.y + 9.0),
        time_galley,
        dim(theme::FAINT),
    );
    let (result, color, status) = match &run.status {
        UtilityStatus::Running => ("Generating a title…".to_owned(), theme::AMBER, "running"),
        UtilityStatus::Done { title } => (title.clone(), theme::TEXT_ROW, "done"),
        UtilityStatus::Failed { reason } => (reason.clone(), theme::ERROR, "failed"),
    };
    let galley = fit(
        painter,
        &result,
        FontId::proportional(12.0),
        dim(color),
        text_width,
    );
    painter.galley(pos2(left, rect.min.y + 26.0), galley, dim(color));
    let meta = format!(
        "{} · {} · {} · {}",
        run.terminal_id,
        run.model.label(),
        run.effort.label(),
        status
    );
    let galley = fit(
        painter,
        &meta,
        FontId::monospace(10.5),
        dim(theme::FAINT),
        text_width - if openable { 100.0 } else { 0.0 },
    );
    painter.galley(pos2(left, rect.min.y + 43.0), galley, dim(theme::FAINT));
    // A run that stopped for the user to finish is the one to open.
    let blocked = matches!(&run.status, UtilityStatus::Failed { reason }
        if reason.to_lowercase().contains("open the session"));
    if openable {
        if blocked {
            painter.rect_filled(
                Rect::from_min_size(rect.min, vec2(3.0, rect.height() - 1.0)),
                0.0,
                theme::AMBER,
            );
        }
        painter.text(
            pos2(rect.max.x - 16.0, rect.min.y + 49.0),
            Align2::RIGHT_CENTER,
            "Open session ›",
            FontId::monospace(10.5),
            if blocked { theme::AMBER } else { theme::CYAN },
        );
    }
    let spoken = if openable {
        format!("Open session of {}, {result}, {status}", run.terminal_label)
    } else {
        format!("{}, {result}, {status}, session closed", run.terminal_label)
    };
    let response = response.on_hover_text(if openable {
        "Open this session"
    } else {
        "Session closed"
    });
    response.widget_info(|| {
        egui::WidgetInfo::labeled(egui::accesskit::Role::Button, openable, spoken.clone())
    });
    if openable && response.clicked() {
        out.actions.push(UtilityAction::Open(run.id));
    }
    #[cfg(test)]
    out.hits.row_rects.push((run.id, rect));
    #[cfg(test)]
    out.hits
        .rows
        .push((run.id, vec![run.terminal_label.clone(), result, meta, time]));
}

fn option_chip(ui: &mut Ui, pos: egui::Pos2, text: &str, on: bool, id: Id) -> (Rect, bool) {
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
    (rect, response.clicked())
}

fn popover(
    ui: &mut Ui,
    rect: Rect,
    top: f32,
    state: &mut UtilitySheet,
    input: &UtilityInput<'_>,
    out: &mut UtilityOut,
) {
    let panel = Rect::from_min_size(
        pos2(rect.min.x + 14.0, top + 16.0),
        vec2((rect.width() - 28.0).min(420.0), 118.0),
    );
    // Swallow clicks that land on the popover's background.
    ui.interact(panel, Id::unique("utility-popover"), Sense::click());
    let painter = ui.painter();
    painter.rect_filled(panel, 8.0, theme::POPOVER_BG);
    painter.rect_stroke(
        panel,
        8.0,
        Stroke::new(1.0, theme::POPOVER_BORDER),
        StrokeKind::Inside,
    );
    let mut settings = input.settings;
    painter.text(
        panel.min + vec2(12.0, 16.0),
        Align2::LEFT_CENTER,
        "Model",
        FontId::proportional(11.5),
        theme::TEXT_MUTED,
    );
    let mut x = panel.min.x + 12.0;
    for model in UtilityModel::ALL {
        let (chip, clicked) = option_chip(
            ui,
            pos2(x, panel.min.y + 30.0),
            model.label(),
            model == input.settings.model,
            Id::unique(("utility-model", model.cli_value())),
        );
        x = chip.max.x + 6.0;
        #[cfg(test)]
        out.hits.models.push((model, chip));
        if clicked {
            settings.model = model;
        }
    }
    ui.painter().text(
        panel.min + vec2(12.0, 74.0),
        Align2::LEFT_CENTER,
        "Effort",
        FontId::proportional(11.5),
        theme::TEXT_MUTED,
    );
    let mut x = panel.min.x + 12.0;
    for effort in UtilityEffort::ALL {
        let (chip, clicked) = option_chip(
            ui,
            pos2(x, panel.min.y + 88.0),
            effort.label(),
            effort == input.settings.effort,
            Id::unique(("utility-effort", effort.cli_value())),
        );
        x = chip.max.x + 6.0;
        #[cfg(test)]
        out.hits.efforts.push((effort, chip));
        if clicked {
            settings.effort = effort;
        }
    }
    if settings != input.settings {
        out.actions.push(UtilityAction::Settings(settings));
    }
    let _ = state;
}
