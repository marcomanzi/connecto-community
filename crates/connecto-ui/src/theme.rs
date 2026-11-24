//! The brand palette and the egui style built from it.
//!
//! Every value was sampled from the logo (`assets/brand/logo-soft.png`):
//! navy from the tile, cyan from the block tops and connector lines, teal from
//! the block sides, coral from the cube. Chrome only: the terminal keeps its
//! own ANSI colours and background.

use egui::{Align2, Color32, CornerRadius, Margin, Stroke, TextEdit, Vec2, Visuals};

/// Window background, the navy at the middle of the tile.
pub(crate) const WINDOW_BG: Color32 = Color32::from_rgb(0x0f, 0x1b, 0x29);
/// Panels and pane headers.
pub(crate) const PANEL_BG: Color32 = Color32::from_rgb(0x15, 0x24, 0x34);
/// Raised controls and the selected tab.
pub(crate) const RAISED_BG: Color32 = Color32::from_rgb(0x1d, 0x32, 0x44);
/// The dark teal of the block sides: secondary surfaces.
pub(crate) const TEAL_DEEP: Color32 = Color32::from_rgb(0x18, 0x35, 0x45);
/// The lit teal of the block sides: borders and strokes.
pub(crate) const TEAL: Color32 = Color32::from_rgb(0x2d, 0x66, 0x74);
/// The idle border of a pane.
pub(crate) const BORDER: Color32 = Color32::from_rgb(0x24, 0x44, 0x55);

/// The primary accent: selection, focus, primary buttons, activity.
pub(crate) const CYAN: Color32 = Color32::from_rgb(0x73, 0xf8, 0xfc);

/// The lit face of the cube: destructive fills and outlines.
pub(crate) const CORAL: Color32 = Color32::from_rgb(0xe5, 0x4b, 0x4b);
/// The top of the cube: warnings, as text.
pub(crate) const CORAL_SOFT: Color32 = Color32::from_rgb(0xed, 0x82, 0x7f);
/// Errors, as text.
pub(crate) const ERROR: Color32 = Color32::from_rgb(0xf5, 0x78, 0x76);
/// Fill of a destructive control, coral on navy.
pub(crate) const CORAL_BG: Color32 = Color32::from_rgb(0x3a, 0x22, 0x2c);
/// Text on a destructive control.
pub(crate) const CORAL_ON_BG: Color32 = Color32::from_rgb(0xf6, 0xa7, 0xa4);

/// Body text.
pub(crate) const TEXT: Color32 = Color32::from_rgb(0xe9, 0xef, 0xf3);
/// Secondary text.
pub(crate) const TEXT_MUTED: Color32 = Color32::from_rgb(0x9d, 0xb1, 0xbf);
/// Text on a cyan fill.
pub(crate) const ON_CYAN: Color32 = Color32::from_rgb(0x0c, 0x17, 0x22);

/// The sidebar, a step darker than the window.
pub(crate) const SIDEBAR_BG: Color32 = Color32::from_rgb(0x0c, 0x17, 0x23);
/// The line between the sidebar and the terminal, and under the headers.
pub(crate) const SIDEBAR_BORDER: Color32 = Color32::from_rgb(0x1d, 0x32, 0x44);
/// A hairline inside the sidebar.
pub(crate) const HAIR: Color32 = Color32::from_rgb(0x17, 0x2a, 0x3a);
/// Tertiary text and quiet icons.
pub(crate) const FAINT: Color32 = Color32::from_rgb(0x6d, 0x84, 0x94);
/// Text a step brighter than muted, for rows.
pub(crate) const TEXT_ROW: Color32 = Color32::from_rgb(0xc9, 0xd6, 0xdf);
/// Waiting for the user.
pub(crate) const AMBER: Color32 = Color32::from_rgb(0xf5, 0xb8, 0x5a);
/// Background work running for a long time: a muted sand, apart from the
/// amber that wants the user, and calm.
pub(crate) const LONG_RUNNING: Color32 = Color32::from_rgb(0xd4, 0xb4, 0x83);
/// An idle terminal: a subtle light gray, quiet against the dark sidebar.
pub(crate) const IDLE: Color32 = Color32::from_rgb(0xa9, 0xb5, 0xbf);
/// Done, finished.
pub(crate) const GREEN: Color32 = Color32::from_rgb(0x5f, 0xd3, 0xa6);
/// A terminal at work: the pulsing dot. A truer green than `GREEN` (mint) so a
/// running terminal reads apart from a finished one.
pub(crate) const WORKING: Color32 = Color32::from_rgb(0x3d, 0xdc, 0x6f);
/// The terminal's own background before it reports one.
pub(crate) const TERM_BG: Color32 = Color32::from_rgb(0x0a, 0x12, 0x1b);
/// The project row of the project shown.
pub(crate) const ROW_ACTIVE: Color32 = Color32::from_rgb(0x1a, 0x2d, 0x3e);
/// A row under the pointer.
pub(crate) const ROW_HOVER: Color32 = Color32::from_rgb(0x15, 0x24, 0x34);
/// The terminal row shown.
pub(crate) const TERM_ON: Color32 = Color32::from_rgb(0x18, 0x35, 0x45);
/// The connector line from a parent terminal to its children.
pub(crate) const TREE_LINE: Color32 = Color32::from_rgb(0x22, 0x39, 0x4a);
/// Popovers and menus.
pub(crate) const POPOVER_BG: Color32 = Color32::from_rgb(0x1b, 0x2a, 0x39);
pub(crate) const POPOVER_BORDER: Color32 = Color32::from_rgb(0x2c, 0x42, 0x53);
/// The history sheet.
pub(crate) const SHEET_BG: Color32 = Color32::from_rgb(0x11, 0x1f, 0x2e);

/// Corner radius of controls and tabs.
pub(crate) const RADIUS: u8 = 8;

/// The egui style: dark navy surfaces, cyan interaction, rounded corners.
pub(crate) fn apply(ctx: &egui::Context) {
    let mut visuals = Visuals::dark();
    visuals.panel_fill = WINDOW_BG;
    visuals.window_fill = PANEL_BG;
    visuals.extreme_bg_color = WINDOW_BG;
    visuals.faint_bg_color = PANEL_BG;
    visuals.hyperlink_color = CYAN;
    visuals.warn_fg_color = CORAL_SOFT;
    visuals.error_fg_color = ERROR;
    visuals.selection.bg_fill = TEAL;
    visuals.selection.stroke = Stroke::new(1.0, CYAN);
    visuals.window_stroke = Stroke::new(1.0, BORDER);
    visuals.window_corner_radius = CornerRadius::same(RADIUS + 2);
    visuals.menu_corner_radius = CornerRadius::same(RADIUS);
    let radius = CornerRadius::same(RADIUS - 2);
    let widgets = &mut visuals.widgets;
    widgets.noninteractive.bg_fill = PANEL_BG;
    widgets.noninteractive.bg_stroke = Stroke::new(1.0, BORDER);
    widgets.noninteractive.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.inactive.bg_fill = TEAL_DEEP;
    widgets.inactive.weak_bg_fill = TEAL_DEEP;
    widgets.inactive.bg_stroke = Stroke::new(1.0, BORDER);
    widgets.inactive.fg_stroke = Stroke::new(1.0, TEXT);
    widgets.hovered.bg_fill = RAISED_BG;
    widgets.hovered.weak_bg_fill = RAISED_BG;
    widgets.hovered.bg_stroke = Stroke::new(1.0, CYAN);
    widgets.hovered.fg_stroke = Stroke::new(1.5, TEXT);
    widgets.active.bg_fill = TEAL;
    widgets.active.weak_bg_fill = TEAL;
    widgets.active.bg_stroke = Stroke::new(1.0, CYAN);
    widgets.active.fg_stroke = Stroke::new(1.5, TEXT);
    widgets.open.bg_fill = RAISED_BG;
    widgets.open.weak_bg_fill = RAISED_BG;
    widgets.open.bg_stroke = Stroke::new(1.0, TEAL);
    for state in [
        &mut widgets.noninteractive,
        &mut widgets.inactive,
        &mut widgets.hovered,
        &mut widgets.active,
        &mut widgets.open,
    ] {
        state.corner_radius = radius;
    }
    ctx.set_visuals(visuals);
    ctx.global_style_mut(|style| {
        style.spacing.item_spacing = Vec2::new(8.0, 5.0);
        style.spacing.button_padding = Vec2::new(10.0, 4.0);
        style.spacing.interact_size.y = 24.0;
    });
}

/// The space between a text field's border and its text.
pub(crate) const FIELD_MARGIN: Margin = Margin::symmetric(8, 5);

/// One look for every text input: the same inner padding, and the text
/// centered vertically. Call `.padded()` on a framed field, `.bare()` on one
/// that draws its own border or none. egui ignores `.margin()` on a field
/// whose frame was replaced, so `bare` puts the margin into that frame.
pub(crate) trait FieldStyle: Sized {
    fn padded(self) -> Self;
    fn bare(self) -> Self;
    /// For a field inside a row that is only as tall as its text: the same
    /// horizontal padding and little vertical padding. The caller also zeroes
    /// `interact_size.y` in its scope, which the theme uses as a minimum height.
    fn compact(self) -> Self;
}

impl FieldStyle for TextEdit<'_> {
    fn padded(self) -> Self {
        self.margin(FIELD_MARGIN).align(Align2::LEFT_CENTER)
    }

    fn compact(self) -> Self {
        self.frame(egui::Frame::NONE.inner_margin(Margin::symmetric(FIELD_MARGIN.left, 1)))
            .align(Align2::LEFT_CENTER)
    }

    fn bare(self) -> Self {
        self.frame(egui::Frame::NONE.inner_margin(FIELD_MARGIN))
            .align(Align2::LEFT_CENTER)
    }
}

/// A plain button. Its label never wraps: the button grows to fit it.
pub(crate) fn button(text: &str) -> egui::Button<'_> {
    egui::Button::new(text).wrap_mode(egui::TextWrapMode::Extend)
}

/// A filled button in the primary accent.
pub(crate) fn primary(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).color(ON_CYAN).strong())
        .wrap_mode(egui::TextWrapMode::Extend)
        .fill(CYAN)
}

/// A button for an action that stops or removes something.
pub(crate) fn destructive(text: &str) -> egui::Button<'_> {
    egui::Button::new(egui::RichText::new(text).color(CORAL_ON_BG))
        .wrap_mode(egui::TextWrapMode::Extend)
        .fill(CORAL_BG)
        .stroke(Stroke::new(1.0, CORAL))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luminance(color: Color32) -> f32 {
        let channel = |value: u8| {
            let v = f32::from(value) / 255.0;
            if v <= 0.03928 {
                v / 12.92
            } else {
                ((v + 0.055) / 1.055).powf(2.4)
            }
        };
        0.2126 * channel(color.r()) + 0.7152 * channel(color.g()) + 0.0722 * channel(color.b())
    }

    fn contrast(a: Color32, b: Color32) -> f32 {
        let (la, lb) = (luminance(a), luminance(b));
        (la.max(lb) + 0.05) / (la.min(lb) + 0.05)
    }

    #[test]
    fn every_text_pairing_meets_wcag_aa() {
        let surfaces = [WINDOW_BG, PANEL_BG, RAISED_BG, TEAL_DEEP];
        for surface in surfaces {
            for (name, text) in [
                ("text", TEXT),
                ("muted", TEXT_MUTED),
                ("cyan", CYAN),
                ("warning", CORAL_SOFT),
                ("error", ERROR),
            ] {
                let ratio = contrast(text, surface);
                assert!(ratio >= 4.5, "{name} on {surface:?}: {ratio:.2}");
            }
        }
        for (name, text, fill) in [
            ("faint on sidebar", FAINT, SIDEBAR_BG),
            ("row text on sidebar", TEXT_ROW, SIDEBAR_BG),
            ("muted on selected row", TEXT_MUTED, TERM_ON),
            ("amber on sidebar", AMBER, SIDEBAR_BG),
            ("long-running on sidebar", LONG_RUNNING, SIDEBAR_BG),
            ("long-running on selected row", LONG_RUNNING, TERM_ON),
            ("text on popover", TEXT, POPOVER_BG),
            ("muted on popover", TEXT_MUTED, POPOVER_BG),
            ("primary button", ON_CYAN, CYAN),
            ("fake-model banner", ON_CYAN, CORAL),
            ("destructive button", CORAL_ON_BG, CORAL_BG),
        ] {
            let ratio = contrast(text, fill);
            assert!(ratio >= 4.5, "{name}: {ratio:.2}");
        }
    }

    #[test]
    #[allow(clippy::expect_used)]
    fn every_field_style_keeps_its_text_off_the_border_and_centered() {
        let ctx = egui::Context::default();
        apply(&ctx);
        for bare in [false, true] {
            let mut text = String::from("Connecto");
            let mut found = None;
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    Vec2::new(400.0, 200.0),
                )),
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let edit = TextEdit::singleline(&mut text).desired_width(200.0);
                let edit = if bare { edit.bare() } else { edit.padded() };
                let out = edit.show(ui);
                found = Some((out.response.rect, out.galley_pos, out.galley.size()));
            });
            output.textures_delta.clear();
            let (rect, pos, size) = found.expect("the field was drawn");
            assert!(
                pos.x - rect.min.x >= 8.0,
                "bare={bare}: left gap {}",
                pos.x - rect.min.x
            );
            let above = pos.y - rect.min.y;
            let below = rect.max.y - (pos.y + size.y);
            assert!(
                above >= 4.0 && below >= 4.0,
                "bare={bare}: {above} / {below}"
            );
            assert!(
                (above - below).abs() <= 1.0,
                "bare={bare}: {above} / {below}"
            );
        }
    }
}
