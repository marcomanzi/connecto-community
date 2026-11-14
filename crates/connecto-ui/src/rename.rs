//! Inline rename of a terminal or a project (project-tree-ui R16).

use egui::{Color32, FontId, Id, Key, Rect, Stroke, StrokeKind, Ui};

use crate::theme::{self, FieldStyle as _};

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum RenameTarget {
    Terminal { project: String, terminal: String },
    Project(String),
}

pub(crate) enum RenameEnd {
    /// Enter. `None` when the field is empty: clear the name.
    Save(Option<String>),
    /// Esc, or a click elsewhere.
    Cancel,
}

pub(crate) struct Rename {
    pub(crate) target: RenameTarget,
    pub(crate) text: String,
    frames: u8,
    /// Where the editor was laid out, for tests.
    #[cfg(test)]
    pub(crate) editor: Option<Rect>,
}

impl Rename {
    pub(crate) fn new(target: RenameTarget, current: &str) -> Self {
        Self {
            target,
            text: current.to_owned(),
            frames: 0,
            #[cfg(test)]
            editor: None,
        }
    }

    /// Passes the field has been drawn for.
    pub(crate) fn age(&self) -> u8 {
        self.frames
    }

    /// Draw the field in `rect`. Returns how it ended, once it does.
    pub(crate) fn edit(&mut self, ui: &mut Ui, rect: Rect, font: FontId) -> Option<RenameEnd> {
        let id = Id::unique("rename-field");
        let painter = ui.painter();
        painter.rect_filled(rect.expand(3.0), 7.0, theme::CYAN.gamma_multiply(0.15));
        painter.rect_filled(rect, 5.0, theme::TERM_BG);
        painter.rect_stroke(rect, 5.0, Stroke::new(1.0, theme::CYAN), StrokeKind::Inside);
        let edit = egui::TextEdit::singleline(&mut self.text)
            .compact()
            .id(id)
            .font(font)
            .text_color(Color32::WHITE)
            .desired_width(rect.width());
        // The theme floors a field at the height of a button; this one is
        // only as tall as its row.
        let response = ui
            .scope(|ui| {
                ui.spacing_mut().interact_size.y = 0.0;
                ui.put(rect, edit)
            })
            .inner;
        #[cfg(test)]
        {
            self.editor = Some(response.rect);
        }
        if self.frames == 0 {
            response.request_focus();
            let end = self.text.chars().count();
            if let Some(mut state) = egui::TextEdit::load_state(ui.ctx(), id) {
                state
                    .cursor
                    .set_char_range(Some(egui::text::CCursorRange::two(
                        egui::text::CCursor::new(0),
                        egui::text::CCursor::new(end),
                    )));
                state.store(ui.ctx(), id);
            }
        }
        let live = self.frames > 0;
        self.frames = self.frames.saturating_add(1);
        if !(live && response.lost_focus()) {
            return None;
        }
        let pressed = |key| ui.input(|input| input.key_pressed(key));
        if pressed(Key::Escape) {
            return Some(RenameEnd::Cancel);
        }
        if pressed(Key::Enter) {
            let trimmed = self.text.trim();
            return Some(RenameEnd::Save(
                (!trimmed.is_empty()).then(|| trimmed.to_owned()),
            ));
        }
        Some(RenameEnd::Cancel)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use egui::{Pos2, Vec2, pos2, vec2};

    /// The editor stays inside the border drawn around its rectangle, for the
    /// row heights the sidebar gives it (18 px for a terminal, 26 for a
    /// project) and the fonts it uses.
    #[test]
    fn the_editor_fits_inside_the_border_of_a_sidebar_row() {
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        for (height, size) in [(18.0, 12.0), (18.0, 12.5), (26.0, 13.0)] {
            let mut rename = Rename::new(
                RenameTarget::Project("p".into()),
                "Connecto and a much longer name than fits",
            );
            let rect = Rect::from_min_size(pos2(30.0, 10.0), vec2(180.0, height));
            let input = egui::RawInput {
                screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 200.0))),
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                let _ = rename.edit(ui, rect, FontId::proportional(size));
            });
            output.textures_delta.clear();
            let editor = rename.editor.unwrap_or(Rect::NOTHING);
            assert!(
                rect.contains_rect(editor),
                "{size}pt in {height}px: editor {editor:?} spills out of {rect:?}"
            );
        }
    }
}
