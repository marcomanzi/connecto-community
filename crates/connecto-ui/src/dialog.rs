//! A modal card drawn over the whole window: above the sheets, dimming them,
//! and taking every pointer event so nothing under it is reachable.

use egui::{
    Color32, Context, Frame, Id, Margin, Order, Rect, Sense, Stroke, Ui, UiBuilder, pos2, vec2,
};

use crate::theme;

const WIDTH: f32 = 460.0;

/// Draw `body` in a card centred over the window. `id` names the layer.
pub(crate) fn card(ctx: &Context, id: &'static str, body: impl FnOnce(&mut Ui)) {
    let screen = ctx.content_rect();
    let width = (screen.width() - 32.0).clamp(120.0, WIDTH);
    let height = (screen.height() - 80.0).max(160.0);
    let place = Rect::from_min_size(
        pos2(screen.center().x - width / 2.0, screen.min.y + 40.0),
        vec2(width, height),
    );
    // Tooltip order is above every sheet (Foreground), so a sheet can never
    // cover the dialog, even one opened after it.
    let area = egui::Area::new(Id::unique(id))
        .order(Order::Tooltip)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            ui.set_clip_rect(screen);
            ui.painter()
                .rect_filled(screen, 0.0, Color32::from_black_alpha(150));
            // Swallow the pointer over the whole window.
            ui.interact(
                screen,
                Id::unique("dialog-blocker"),
                Sense::click_and_drag(),
            );
            ui.scope_builder(UiBuilder::new().max_rect(place), |ui| {
                Frame::NONE
                    .fill(theme::POPOVER_BG)
                    .stroke(Stroke::new(1.0, theme::POPOVER_BORDER))
                    .corner_radius(f32::from(theme::RADIUS))
                    .inner_margin(Margin::same(16))
                    .show(ui, |ui| {
                        ui.set_width(width - 32.0);
                        egui::ScrollArea::vertical()
                            .auto_shrink([false, true])
                            .max_height(height - 32.0)
                            .show(ui, body);
                    });
            });
        });
    // Focus stays inside the dialog: Tab does not reach what is under it.
    ctx.memory_mut(|memory| memory.set_modal_layer(area.response.layer_id));
}

/// A request dialog's heading, with a Minimize button at its right that
/// shrinks the request to a card in the window's bottom-right corner.
/// Returns whether it was clicked.
pub(crate) fn heading_with_minimize<A>(
    ui: &mut Ui,
    out: &mut crate::forms::Out<A>,
    title: &str,
    key: &str,
) -> bool {
    let mut clicked = false;
    ui.horizontal(|ui| {
        out.say(ui, crate::forms::heading(title));
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let button = ui
                .add(egui::Button::new(egui::RichText::new("\u{2013}  Minimize").size(11.5)).small())
                .on_hover_text(
                    "Shrink it to the bottom-right corner to read and use the terminal; click it there to answer. Esc does the same.",
                );
            out.mark(key, button.rect);
            clicked = button.clicked();
        });
    });
    clicked
}
