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

    /// A tab of a second row, under the top-level tabs: smaller, and muted
    /// unless it is the one on show, so the two levels never read alike.
    pub(crate) fn sub_choice(&mut self, ui: &mut Ui, key: &str, on: bool, text: &str) -> bool {
        let color = if on { theme::TEXT } else { theme::TEXT_MUTED };
        let response = ui.selectable_label(on, RichText::new(text).size(11.5).color(color));
        self.mark(key, response.rect);
        response.clicked()
    }

    /// A line of text drawn by hand (not through `say`), recorded for tests.
    pub(crate) fn text_mark(&mut self, text: &str) {
        #[cfg(test)]
        self.marks.texts.push(text.to_owned());
        #[cfg(not(test))]
        let _ = text;
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

/// A row of top-level tabs, drawn the same wherever a sheet has sections. Each
/// entry is the tab, its test key and its label. Returns the tab clicked, when
/// it is not the one on show.
pub(crate) fn tab_row<A, T: Copy + PartialEq>(
    ui: &mut Ui,
    out: &mut Out<A>,
    current: T,
    tabs: impl IntoIterator<Item = (T, &'static str, &'static str)>,
) -> Option<T> {
    let mut picked = None;
    ui.horizontal_wrapped(|ui| {
        for (tab, key, label) in tabs {
            let on = tab == current;
            if out.choice(ui, key, on, label) && !on {
                picked = Some(tab);
            }
        }
    });
    ui.add_space(6.0);
    picked
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
