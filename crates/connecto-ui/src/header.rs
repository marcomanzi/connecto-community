//! The strip a static shell (`--pane`) keeps for switching between its panes,
//! and the window-drag gesture the sidebar header and the rail share.
//!
//! A workspace window has no terminal header (the sidebar already says where
//! you are), so nothing over the terminal starts a window drag.

use egui::{Color32, FontId, Id, Pos2, Rect, Response, Sense, Stroke, StrokeKind, Ui, pos2, vec2};

use crate::theme;

pub(crate) const HEADER_HEIGHT: f32 = 44.0;
const CHIP_HEIGHT: f32 = 22.0;
/// Room for the macOS traffic lights when nothing else sits under them.
const LIGHTS_WIDTH: f32 = 80.0;

pub(crate) struct HeadInput {
    /// A static shell's chip.
    pub(crate) status: Option<(String, Color32)>,
    /// The labels to switch between and the one shown.
    pub(crate) tabs: Option<(Vec<String>, usize)>,
}

#[derive(Default)]
pub(crate) struct HeadOut {
    pub(crate) tab: Option<usize>,
    /// The key that chose a tab, which is not for the terminal.
    pub(crate) activating: Option<egui::Key>,
    #[cfg(test)]
    pub(crate) tab_rects: Vec<Rect>,
}

/// Move or zoom the window from a drag area. A gesture that starts on a
/// control belongs to that control, so only empty area drags.
pub(crate) fn window_gestures(ui: &Ui, area: &Response, controls: &[Rect]) {
    let on_control =
        |pos: Option<Pos2>| pos.is_some_and(|p| controls.iter().any(|r| r.contains(p)));
    let origin = ui.input(|input| input.pointer.press_origin());
    if area.drag_started_by(egui::PointerButton::Primary) && !on_control(origin) {
        ui.ctx().send_viewport_cmd(egui::ViewportCommand::StartDrag);
    }
    if area.double_clicked() && !on_control(area.interact_pointer_pos()) {
        let zoomed = ui
            .input(|input| input.viewport().maximized)
            .unwrap_or(false);
        ui.ctx()
            .send_viewport_cmd(egui::ViewportCommand::Maximized(!zoomed));
    }
}

pub(crate) fn header(ui: &mut Ui, rect: Rect, input: &HeadInput, out: &mut HeadOut) {
    let painter = ui.painter_at(rect);
    painter.rect_filled(rect, 0.0, theme::WINDOW_BG);
    painter.hline(
        rect.x_range(),
        rect.max.y - 0.5,
        Stroke::new(1.0, theme::SIDEBAR_BORDER),
    );
    let drag = ui.interact(rect, Id::unique("header-drag"), Sense::click_and_drag());
    let mut controls: Vec<Rect> = Vec::new();
    let mid = rect.center().y;

    let mut x = rect.min.x + LIGHTS_WIDTH;
    if let Some((labels, shown)) = &input.tabs {
        for (index, label) in labels.iter().enumerate() {
            let galley =
                painter.layout_no_wrap(label.clone(), FontId::monospace(12.0), theme::TEXT);
            let tab = Rect::from_min_size(pos2(x, mid - 13.0), vec2(galley.size().x + 20.0, 26.0));
            let response = ui.interact(tab, Id::unique(("static-tab", index)), Sense::click());
            let on = index == *shown;
            if on {
                painter.rect_filled(tab, 7.0, theme::TERM_ON);
                painter.rect_stroke(tab, 7.0, Stroke::new(1.0, theme::TEAL), StrokeKind::Inside);
            } else if response.hovered() {
                painter.rect_filled(tab, 7.0, theme::ROW_HOVER);
            }
            if response.has_focus() {
                painter.rect_stroke(tab, 7.0, Stroke::new(2.0, theme::CYAN), StrokeKind::Inside);
            }
            let color = if on {
                Color32::WHITE
            } else {
                theme::TEXT_MUTED
            };
            painter.galley(
                tab.min + vec2(10.0, 13.0 - galley.size().y / 2.0),
                galley,
                color,
            );
            let name = label.clone();
            response.widget_info(|| {
                egui::WidgetInfo::selected(egui::accesskit::Role::Tab, true, on, name.clone())
            });
            #[cfg(test)]
            out.tab_rects.push(tab);
            controls.push(tab);
            if response.clicked() {
                out.tab = Some(index);
                out.activating = crate::pane::activating_key(ui, response.has_focus());
            }
            x = tab.max.x + 4.0;
        }
    }
    if let Some((text, color)) = &input.status {
        let galley = painter.layout_no_wrap(text.clone(), FontId::monospace(11.5), *color);
        let width = galley.size().x + 16.0;
        let chip = Rect::from_min_size(
            pos2(rect.max.x - 14.0 - width, mid - CHIP_HEIGHT / 2.0),
            vec2(width, CHIP_HEIGHT),
        );
        painter.rect_stroke(
            chip,
            99.0,
            Stroke::new(1.0, theme::BORDER),
            StrokeKind::Inside,
        );
        painter.galley(
            pos2(chip.min.x + 8.0, mid - galley.size().y / 2.0),
            galley,
            *color,
        );
        controls.push(chip);
    }
    window_gestures(ui, &drag, &controls);
}
