//! The New terminal chooser: Claude, Codex, Pi, Antigravity, OpenCode or a plain shell, with an optional name
//! (project-tree-ui R17).

use connecto_core::ToolKind;
use egui::{
    Align2, Color32, FontId, Id, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::theme::{self, FieldStyle as _};

pub(crate) const POPOVER_WIDTH: f32 = 244.0;
const CHOICE_HEIGHT: f32 = 46.0;
pub(crate) const TOOLS: [ToolKind; 6] = ToolKind::ALL;

pub(crate) struct Chooser {
    pub(crate) project: String,
    pub(crate) highlighted: ToolKind,
    pub(crate) name: String,
    /// What is typed to choose Pi's model. Empty is the default one.
    pub(crate) model: String,
    frames: u8,
}

impl Chooser {
    pub(crate) fn new(project: String) -> Self {
        Self {
            project,
            highlighted: ToolKind::Claude,
            name: String::new(),
            model: String::new(),
            frames: 0,
        }
    }
}

pub(crate) enum ChooserEnd {
    /// The kind, its name, and for Pi the model chosen (`None`: the default).
    Pick(ToolKind, Option<String>, Option<String>),
    Cancel,
}

/// What the chooser shows about the project.
pub(crate) struct ChooserInfo<'a> {
    pub(crate) project_name: &'a str,
    /// What to say under each kind, in the order of [`TOOLS`].
    pub(crate) logins: [&'a str; TOOLS.len()],
    /// The tools the project allows, Claude first. Never empty.
    pub(crate) tools: &'a [ToolKind],
    /// Why nothing can be added, if so.
    pub(crate) blocked: Option<&'a str>,
    /// A remote host's Tools setting is still on its way: its agents are
    /// shown as loading and cannot be chosen yet (a shell can).
    pub(crate) agents_loading: bool,
    /// Every model Pi offers now, as `provider/model`.
    pub(crate) pi_models: &'a [String],
    /// The model a Pi terminal starts on when none is chosen here.
    pub(crate) pi_default: Option<&'a str>,
}

/// How many models are suggested under the model field.
const SUGGESTIONS: usize = 5;

/// The model a Pi terminal is opened on, from what was typed: nothing typed
/// is the default (`Ok(None)`); otherwise the model of that name, or the
/// first whose name holds the words. `Err` when Pi offers none like it. With
/// no list to check against (Pi was not asked yet) the text is taken as it is.
fn chosen_model(typed: &str, models: &[String]) -> Result<Option<String>, ()> {
    let typed = typed.trim();
    if typed.is_empty() {
        return Ok(None);
    }
    if models.is_empty() || models.iter().any(|model| model == typed) {
        return Ok(Some(typed.to_owned()));
    }
    connecto_core::pi::matching_models(models, typed, 1)
        .first()
        .map(|model| Some((*model).to_owned()))
        .ok_or(())
}

#[derive(Default)]
pub(crate) struct ChooserHits {
    #[cfg(test)]
    pub(crate) choices: Vec<Rect>,
    #[cfg(test)]
    pub(crate) name: Option<Rect>,
}

fn glyph(tool: ToolKind) -> &'static str {
    match tool {
        ToolKind::Claude => "CC",
        ToolKind::Codex => "CX",
        ToolKind::Pi => "PI",
        ToolKind::Antigravity => "AG",
        ToolKind::OpenCode => "OC",
        ToolKind::Shell => "SH",
    }
}

fn label(tool: ToolKind) -> &'static str {
    tool.label()
}

fn shortcut(tool: ToolKind) -> &'static str {
    match tool {
        ToolKind::Claude => crate::platform::kbd!(alt "1"),
        ToolKind::Codex => crate::platform::kbd!(alt "2"),
        ToolKind::Shell => crate::platform::kbd!(alt "3"),
        // After the shell, which keeps the number it has always had.
        ToolKind::Pi => crate::platform::kbd!(alt "4"),
        ToolKind::Antigravity => crate::platform::kbd!(alt "5"),
        ToolKind::OpenCode => crate::platform::kbd!(alt "6"),
    }
}

/// Draw the chooser into `ui`. `keys` is whether Up, Down, Enter and Esc are
/// for it: false while something else, such as a rename, has the keyboard.
pub(crate) fn body(
    ui: &mut Ui,
    chooser: &mut Chooser,
    info: &ChooserInfo<'_>,
    keys: bool,
    focus_name: bool,
    hits: &mut ChooserHits,
) -> Option<ChooserEnd> {
    let _ = &hits;
    if !info.tools.contains(&chooser.highlighted)
        && let Some(first) = info.tools.first()
    {
        chooser.highlighted = *first;
    }
    let width = ui.available_width().min(POPOVER_WIDTH - 12.0);
    let (head, _) = ui.allocate_exact_size(vec2(width, 22.0), Sense::hover());
    ui.painter().text(
        head.left_center() + vec2(8.0, 0.0),
        Align2::LEFT_CENTER,
        format!("New terminal in {}", info.project_name),
        FontId::proportional(11.5),
        theme::TEXT_MUTED,
    );
    let mut end = None;
    let waiting = |tool: ToolKind| info.agents_loading && tool.is_agent();
    for &tool in info.tools {
        let (rect, _) = ui.allocate_exact_size(vec2(width, CHOICE_HEIGHT), Sense::hover());
        let response = ui.interact(
            rect,
            Id::unique(("chooser-choice", tool.name())),
            Sense::click(),
        );
        let on = chooser.highlighted == tool;
        let painter = ui.painter();
        if on {
            painter.rect_filled(rect, 7.0, theme::TEAL);
        } else if response.hovered() {
            painter.rect_filled(rect, 7.0, theme::RAISED_BG);
        }
        let glyph_rect = Rect::from_min_size(rect.min + vec2(8.0, 9.0), vec2(28.0, 28.0));
        painter.rect_filled(glyph_rect, 7.0, theme::WINDOW_BG);
        painter.rect_stroke(
            glyph_rect,
            7.0,
            Stroke::new(1.0, theme::POPOVER_BORDER),
            StrokeKind::Inside,
        );
        painter.text(
            glyph_rect.center(),
            Align2::CENTER_CENTER,
            glyph(tool),
            FontId::monospace(12.0),
            theme::CYAN,
        );
        painter.text(
            rect.min + vec2(46.0, 15.0),
            Align2::LEFT_CENTER,
            label(tool),
            FontId::proportional(13.0),
            if waiting(tool) {
                theme::TEXT_MUTED
            } else {
                Color32::WHITE
            },
        );
        painter.text(
            rect.min + vec2(46.0, 32.0),
            Align2::LEFT_CENTER,
            if waiting(tool) {
                "loading the project's settings…"
            } else {
                info.logins[TOOLS.iter().position(|t| *t == tool).unwrap_or(0)]
            },
            FontId::proportional(11.0),
            if on {
                Color32::from_rgb(0xdf, 0xe8, 0xee)
            } else {
                theme::TEXT_MUTED
            },
        );
        painter.text(
            rect.right_center() - vec2(10.0, 0.0),
            Align2::RIGHT_CENTER,
            if on { "↵" } else { shortcut(tool) },
            FontId::monospace(11.0),
            if on {
                Color32::WHITE
            } else {
                theme::TEXT_MUTED
            },
        );
        response.widget_info(|| {
            egui::WidgetInfo::selected(
                egui::accesskit::Role::Button,
                info.blocked.is_none() && !waiting(tool),
                on,
                format!("New {} terminal", label(tool)),
            )
        });
        #[cfg(test)]
        hits.choices.push(rect);
        if response.clicked() && info.blocked.is_none() && !waiting(tool) {
            chooser.highlighted = tool;
            // A model that Pi does not offer keeps the chooser open, on Pi,
            // where the field says so.
            if let Ok(model) = model_for(tool, chooser, info) {
                end = Some(ChooserEnd::Pick(tool, name_of(&chooser.name), model));
            }
        }
    }

    let (field, _) = ui.allocate_exact_size(vec2(width, 30.0), Sense::hover());
    let field = field.shrink2(vec2(4.0, 2.0));
    let edit = egui::TextEdit::singleline(&mut chooser.name)
        .padded()
        .id(Id::unique("chooser-name"))
        .hint_text("Name (optional)")
        .font(FontId::monospace(11.5))
        .desired_width(field.width());
    let response = ui.put(field, edit);
    #[cfg(test)]
    {
        hits.name = Some(field);
    }
    if focus_name && chooser.frames == 0 {
        response.request_focus();
    }
    chooser.frames = chooser.frames.saturating_add(1);

    if chooser.highlighted == ToolKind::Pi {
        model_field(ui, chooser, info, width);
    }

    if let Some(reason) = info.blocked {
        let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
        ui.painter().text(
            rect.left_center() + vec2(8.0, 0.0),
            Align2::LEFT_CENTER,
            reason,
            FontId::proportional(11.0),
            theme::CORAL_SOFT,
        );
    }

    // Keys steer the chooser only while the keyboard is its own: a focused
    // control elsewhere, such as a sidebar row, keeps its Enter.
    let name_id = Id::unique("chooser-name");
    let own = ui.ctx().memory(|memory| memory.focused()).is_none_or(|id| {
        id == name_id
            || id == Id::unique("chooser-model")
            || TOOLS
                .iter()
                .any(|tool| id == Id::unique(("chooser-choice", tool.name())))
    });
    if keys && own && end.is_none() {
        let (down, up, enter, escape) = ui.input(|input| {
            (
                input.key_pressed(Key::ArrowDown),
                input.key_pressed(Key::ArrowUp),
                input.key_pressed(Key::Enter),
                input.key_pressed(Key::Escape),
            )
        });
        if down || up {
            let at = info
                .tools
                .iter()
                .position(|tool| *tool == chooser.highlighted)
                .unwrap_or(0);
            if let Some(tool) = info.tools.get((at + 1) % info.tools.len().max(1)) {
                chooser.highlighted = *tool;
            }
        }
        if enter && info.blocked.is_none() && !waiting(chooser.highlighted) {
            if let Ok(model) = model_for(chooser.highlighted, chooser, info) {
                end = Some(ChooserEnd::Pick(
                    chooser.highlighted,
                    name_of(&chooser.name),
                    model,
                ));
            }
        } else if escape {
            end = Some(ChooserEnd::Cancel);
        }
    }
    end
}

/// The model to open `tool` on: only Pi's is chosen here.
fn model_for(
    tool: ToolKind,
    chooser: &Chooser,
    info: &ChooserInfo<'_>,
) -> Result<Option<String>, ()> {
    if tool == ToolKind::Pi {
        chosen_model(&chooser.model, info.pi_models)
    } else {
        Ok(None)
    }
}

/// Under the name, while Pi is the kind: the model to start on. Empty is the
/// default; typing finds one among Pi's, and a suggestion fills the field.
fn model_field(ui: &mut Ui, chooser: &mut Chooser, info: &ChooserInfo<'_>, width: f32) {
    let (field, _) = ui.allocate_exact_size(vec2(width, 30.0), Sense::hover());
    let field = field.shrink2(vec2(4.0, 2.0));
    let hint = match info.pi_default {
        Some(model) => format!("Model: {model}"),
        None => "Model: Pi's choice (type to pick one)".to_owned(),
    };
    let edit = egui::TextEdit::singleline(&mut chooser.model)
        .padded()
        .id(Id::unique("chooser-model"))
        .hint_text(hint)
        .font(FontId::monospace(11.5))
        .desired_width(field.width());
    ui.put(field, edit);
    let typed = chooser.model.trim().to_owned();
    if typed.is_empty() || info.pi_models.contains(&typed) {
        return;
    }
    let found = connecto_core::pi::matching_models(info.pi_models, &typed, SUGGESTIONS);
    if found.is_empty() && !info.pi_models.is_empty() {
        let (rect, _) = ui.allocate_exact_size(vec2(width, 18.0), Sense::hover());
        ui.painter().text(
            rect.left_center() + vec2(8.0, 0.0),
            Align2::LEFT_CENTER,
            "Pi offers no model like that",
            FontId::proportional(11.0),
            theme::CORAL_SOFT,
        );
        return;
    }
    let mut picked = None;
    for (index, model) in found.iter().enumerate() {
        let (rect, _) = ui.allocate_exact_size(vec2(width, 20.0), Sense::hover());
        let response = ui.interact(
            rect,
            Id::unique(("chooser-model-suggestion", index)),
            Sense::click(),
        );
        if response.hovered() {
            ui.painter().rect_filled(rect, 5.0, theme::RAISED_BG);
        }
        ui.painter().text(
            rect.left_center() + vec2(8.0, 0.0),
            Align2::LEFT_CENTER,
            *model,
            FontId::monospace(11.0),
            // The first is the one Enter takes.
            if index == 0 {
                Color32::WHITE
            } else {
                theme::TEXT_MUTED
            },
        );
        if response.clicked() {
            picked = Some((*model).to_owned());
        }
    }
    if let Some(model) = picked {
        chooser.model = model;
    }
}

fn name_of(text: &str) -> Option<String> {
    let trimmed = text.trim();
    (!trimmed.is_empty()).then(|| trimmed.to_owned())
}

/// The chooser as a popover at `anchor`, with a backdrop that dismisses it.
pub(crate) fn popover(
    ctx: &egui::Context,
    anchor: Pos2,
    chooser: &mut Chooser,
    info: &ChooserInfo<'_>,
    hits: &mut ChooserHits,
) -> Option<ChooserEnd> {
    let screen = ctx.content_rect();
    let mut end = None;
    egui::Area::new(Id::unique("chooser-backdrop"))
        .order(egui::Order::Middle)
        .fixed_pos(screen.min)
        .show(ctx, |ui| {
            let (rect, response) = ui.allocate_exact_size(screen.size(), Sense::click());
            let _ = rect;
            if response.clicked() {
                end = Some(ChooserEnd::Cancel);
            }
        });
    let pos = pos2(
        anchor.x.clamp(
            screen.min.x + 8.0,
            (screen.max.x - POPOVER_WIDTH - 8.0).max(screen.min.x),
        ),
        anchor
            .y
            .clamp(screen.min.y + 8.0, (screen.max.y - 190.0).max(screen.min.y)),
    );
    egui::Area::new(Id::unique("chooser-popover"))
        .order(egui::Order::Foreground)
        .fixed_pos(pos)
        .show(ctx, |ui| {
            egui::Frame::new()
                .fill(theme::POPOVER_BG)
                .stroke(Stroke::new(1.0, theme::POPOVER_BORDER))
                .corner_radius(10)
                .inner_margin(6)
                .shadow(egui::epaint::Shadow {
                    offset: [0, 10],
                    blur: 30,
                    spread: 0,
                    color: Color32::from_black_alpha(160),
                })
                .show(ui, |ui| {
                    ui.set_width(POPOVER_WIDTH - 12.0);
                    if let Some(result) = body(ui, chooser, info, true, true, hits) {
                        end = Some(result);
                    }
                });
        });
    end
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_model_typed_is_the_default_an_exact_name_or_the_first_like_it() {
        let models: Vec<String> = ["lmstudio/google/gemma-4-e2b", "openrouter/openai/gpt-5"]
            .map(str::to_owned)
            .to_vec();
        assert_eq!(chosen_model("  ", &models), Ok(None));
        assert_eq!(
            chosen_model("openrouter/openai/gpt-5", &models),
            Ok(Some("openrouter/openai/gpt-5".to_owned()))
        );
        assert_eq!(
            chosen_model("gemma", &models),
            Ok(Some("lmstudio/google/gemma-4-e2b".to_owned()))
        );
        // Pi offers none like it: nothing is opened on a guess.
        assert_eq!(chosen_model("llama", &models), Err(()));
        // Pi was not asked yet: the name is taken as typed.
        assert_eq!(
            chosen_model("together/x", &[]),
            Ok(Some("together/x".to_owned()))
        );
    }
}
