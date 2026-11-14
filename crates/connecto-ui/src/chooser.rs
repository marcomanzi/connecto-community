//! The New terminal chooser: Claude, Codex or a plain shell, with an optional name
//! (project-tree-ui R17).

use connecto_core::ToolKind;
use egui::{
    Align2, Color32, FontId, Id, Key, Pos2, Rect, Sense, Stroke, StrokeKind, Ui, pos2, vec2,
};

use crate::theme::{self, FieldStyle as _};

pub(crate) const POPOVER_WIDTH: f32 = 244.0;
const CHOICE_HEIGHT: f32 = 46.0;
pub(crate) const TOOLS: [ToolKind; 3] = ToolKind::ALL;

pub(crate) struct Chooser {
    pub(crate) project: String,
    pub(crate) highlighted: ToolKind,
    pub(crate) name: String,
    frames: u8,
}

impl Chooser {
    pub(crate) fn new(project: String) -> Self {
        Self {
            project,
            highlighted: ToolKind::Claude,
            name: String::new(),
            frames: 0,
        }
    }
}

pub(crate) enum ChooserEnd {
    Pick(ToolKind, Option<String>),
    Cancel,
}

/// What the chooser shows about the project.
pub(crate) struct ChooserInfo<'a> {
    pub(crate) project_name: &'a str,
    /// What to say under each kind, in the order of [`TOOLS`].
    pub(crate) logins: [&'a str; 3],
    /// The tools the project allows, Claude first. Never empty.
    pub(crate) tools: &'a [ToolKind],
    /// Why nothing can be added, if so.
    pub(crate) blocked: Option<&'a str>,
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
            Color32::WHITE,
        );
        painter.text(
            rect.min + vec2(46.0, 32.0),
            Align2::LEFT_CENTER,
            info.logins[TOOLS.iter().position(|t| *t == tool).unwrap_or(0)],
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
                info.blocked.is_none(),
                on,
                format!("New {} terminal", label(tool)),
            )
        });
        #[cfg(test)]
        hits.choices.push(rect);
        if response.clicked() && info.blocked.is_none() {
            chooser.highlighted = tool;
            end = Some(ChooserEnd::Pick(tool, name_of(&chooser.name)));
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
        if enter && info.blocked.is_none() {
            end = Some(ChooserEnd::Pick(
                chooser.highlighted,
                name_of(&chooser.name),
            ));
        } else if escape {
            end = Some(ChooserEnd::Cancel);
        }
    }
    end
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
