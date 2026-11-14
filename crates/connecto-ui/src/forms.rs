//! Small pieces the Skills & Plugins, Settings and Project Settings sheets
//! share: text styles, pills, buttons, and the record of what was drawn that
//! tests read.

use egui::{Color32, Margin, Rect, RichText, Stroke, Ui};

use crate::theme;

/// What the last pass drew, for tests.
#[cfg(test)]
#[derive(Default)]
pub(crate) struct Marks {
    /// Each control drawn, by name.
    pub(crate) rects: std::collections::HashMap<String, Rect>,
    /// Every line of text drawn.
    pub(crate) texts: Vec<String>,
}

/// What a sheet asks for, and where it drew.
pub(crate) struct Out<A> {
    pub(crate) actions: Vec<A>,
    #[cfg(test)]
    pub(crate) marks: Marks,
}

impl<A> Default for Out<A> {
    fn default() -> Self {
        Self {
            actions: Vec::new(),
            #[cfg(test)]
            marks: Marks::default(),
        }
    }
}

impl<A> Out<A> {
    pub(crate) fn say(&mut self, ui: &mut Ui, text: RichText) {
        #[cfg(test)]
        self.marks.texts.push(text.text().to_owned());
        ui.label(text);
    }

    #[cfg(test)]
    pub(crate) fn mark(&mut self, key: &str, rect: Rect) {
        self.marks.rects.insert(key.to_owned(), rect);
    }

    #[cfg(not(test))]
    #[allow(clippy::unused_self)]
    pub(crate) fn mark(&mut self, _key: &str, _rect: Rect) {}

    pub(crate) fn button(&mut self, ui: &mut Ui, key: &str, text: &str, enabled: bool) -> bool {
        self.button_hint(ui, key, text, enabled, "")
    }

    /// A button that says why it is off, when it is.
    pub(crate) fn button_hint(
        &mut self,
        ui: &mut Ui,
        key: &str,
        text: &str,
        enabled: bool,
        why_not: &str,
    ) -> bool {
        let mut response = ui.add_enabled(enabled, egui::Button::new(text));
        if !enabled && !why_not.is_empty() {
            response = response.on_disabled_hover_text(why_not);
        }
        self.mark(key, response.rect);
        response.clicked()
    }

    /// One of a set of choices. Returns whether it was clicked.
    pub(crate) fn choice(&mut self, ui: &mut Ui, key: &str, on: bool, text: &str) -> bool {
        let response = ui.selectable_label(on, text);
        self.mark(key, response.rect);
        response.clicked()
    }

    /// A small rounded tag.
    pub(crate) fn pill(&mut self, ui: &mut Ui, text: &str, color: Color32) {
        egui::Frame::NONE
            .fill(color.gamma_multiply(0.18))
            .stroke(Stroke::new(1.0, color.gamma_multiply(0.6)))
            .corner_radius(9.0)
            .inner_margin(Margin::symmetric(8, 1))
            .show(ui, |ui| {
                self.say(ui, RichText::new(text).size(11.0).color(color));
            });
    }
}

pub(crate) fn muted(text: impl Into<String>) -> RichText {
    RichText::new(text).size(11.5).color(theme::TEXT_MUTED)
}

pub(crate) fn heading(text: &str) -> RichText {
    RichText::new(text)
        .size(11.0)
        .strong()
        .color(theme::TEXT_MUTED)
}

pub(crate) fn problem(text: impl Into<String>) -> RichText {
    RichText::new(text).size(11.5).color(theme::ERROR)
}

pub(crate) fn warning(text: impl Into<String>) -> RichText {
    RichText::new(text).size(11.5).color(theme::AMBER)
}

pub(crate) fn separator(ui: &mut Ui) {
    ui.add_space(6.0);
    ui.separator();
    ui.add_space(6.0);
}

/// The padded column every sheet body lays out in.
pub(crate) fn column(ui: &mut Ui, rect: Rect, add: impl FnOnce(&mut Ui)) {
    egui::Frame::NONE
        .inner_margin(Margin::symmetric(crate::sheet::PAD as i8, 10))
        .show(ui, |ui| {
            ui.set_width(rect.width() - 2.0 * crate::sheet::PAD);
            add(ui);
        });
}

/// A raised card around one item of a list.
pub(crate) fn card(ui: &mut Ui, add: impl FnOnce(&mut Ui)) {
    egui::Frame::NONE
        .fill(theme::RAISED_BG)
        .stroke(Stroke::new(1.0, theme::BORDER))
        .corner_radius(f32::from(theme::RADIUS))
        .inner_margin(Margin::same(10))
        .show(ui, |ui| {
            ui.set_width(ui.available_width());
            add(ui);
        });
    ui.add_space(6.0);
}
