//! The container every menu-opened sheet shares: it fills the terminal area
//! (the main content next to the sidebar), has a header with the title and a
//! close button, and gives each sheet one content rectangle to lay out in.
//!
//! The sheets (History, Utility Sessions, Automations) draw their own bodies
//! and handle Esc themselves, as they always did. The close button reports a
//! click to the sheet, which pushes the same `Close` action Esc does.

use egui::{Align2, Color32, FontId, Id, Pos2, Rect, Sense, Stroke, Ui, pos2, vec2};

use crate::theme;

/// The title row, above each sheet's content.
pub(crate) const HEADER_HEIGHT: f32 = 44.0;
/// The inner padding left and right of the header and of most content.
pub(crate) const PAD: f32 = 16.0;
const CLOSE_SIZE: f32 = 26.0;

/// Where the close button sits in a sheet whose header spans `sheet`.
pub(crate) fn close_center(sheet: Rect) -> Pos2 {
    pos2(
        sheet.max.x - PAD - CLOSE_SIZE / 2.0 + 4.0,
        sheet.min.y + HEADER_HEIGHT / 2.0,
    )
}

/// The rectangle of a sheet's header, given its content rectangle.
pub(crate) fn header_of(content: Rect) -> Rect {
    Rect::from_min_max(
        pos2(content.min.x, content.min.y - HEADER_HEIGHT),
        pos2(content.max.x, content.min.y),
    )
}

/// Where the last close button was drawn, for tests.
#[cfg(test)]
pub(crate) fn close_button_id() -> Id {
    static ID: std::sync::OnceLock<Id> = std::sync::OnceLock::new();
    *ID.get_or_init(|| Id::unique("sheet-close-rect"))
}

/// Where the last sheet was drawn, for tests.
#[cfg(test)]
pub(crate) fn sheet_rect_id() -> Id {
    static ID: std::sync::OnceLock<Id> = std::sync::OnceLock::new();
    *ID.get_or_init(|| Id::unique("sheet-rect"))
}

/// Draw the sheet over `area`: background, header, then `body` with the
/// content rectangle below the header. Returns whether the close button was
/// clicked. The sheet takes every pointer event over `area`, so nothing under
/// it is reachable.
pub(crate) fn show(
    ctx: &egui::Context,
    area: Rect,
    name: &str,
    title: &str,
    body: impl FnOnce(&mut Ui, Rect),
) -> bool {
    let mut closed = false;
    egui::Area::new(Id::unique(name.to_owned()))
        .order(egui::Order::Foreground)
        .fixed_pos(area.min)
        .show(ctx, |ui| {
            ui.set_clip_rect(area);
            #[cfg(test)]
            ui.data_mut(|data| data.insert_temp(sheet_rect_id(), area));
            ui.painter().rect_filled(area, 0.0, theme::SHEET_BG);
            // Swallow the pointer over the whole area.
            ui.interact(area, Id::unique("sheet-blocker"), Sense::click_and_drag());
            let header = Rect::from_min_size(area.min, vec2(area.width(), HEADER_HEIGHT));
            ui.painter().hline(
                header.x_range(),
                header.max.y - 0.5,
                Stroke::new(1.0, theme::BORDER),
            );
            ui.painter().text(
                pos2(area.min.x + PAD, header.center().y),
                Align2::LEFT_CENTER,
                title,
                FontId::proportional(14.0),
                Color32::WHITE,
            );
            closed = close_button(ui, area);
            let content = Rect::from_min_max(pos2(area.min.x, header.max.y), area.max);
            ui.scope_builder(egui::UiBuilder::new().max_rect(content), |ui| {
                body(ui, content);
            });
        });
    closed
}

/// The frameless "×" in the header's right corner.
fn close_button(ui: &mut Ui, sheet: Rect) -> bool {
    let center = close_center(sheet);
    let rect = Rect::from_center_size(center, vec2(CLOSE_SIZE, CLOSE_SIZE));
    let response = ui.interact(rect, Id::unique("sheet-close"), Sense::click());
    #[cfg(test)]
    ui.data_mut(|data| data.insert_temp(close_button_id(), rect));
    let hovered = response.hovered();
    let painter = ui.painter();
    if hovered {
        painter.circle_filled(center, CLOSE_SIZE / 2.0, theme::RAISED_BG);
    }
    let color = if hovered {
        Color32::WHITE
    } else {
        theme::TEXT_MUTED
    };
    let arm = 5.0;
    let stroke = Stroke::new(1.5, color);
    painter.line_segment([center + vec2(-arm, -arm), center + vec2(arm, arm)], stroke);
    painter.line_segment([center + vec2(-arm, arm), center + vec2(arm, -arm)], stroke);
    response
        .widget_info(|| egui::WidgetInfo::labeled(egui::accesskit::Role::Button, true, "Close"));
    response.on_hover_text("Close (Esc)").clicked()
}
