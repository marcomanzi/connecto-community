//! Requests that wait on the person, a terminal's secret request and an
//! Agentic Works job's question, open as a dialog over the window. Either can
//! be minimized to a small card stacked in the bottom-right corner, so the
//! terminal under it (where the agent often explains the request) can be read
//! and used; clicking the card opens the dialog again.
//!
//! At most one request is open at a time: the one reopened from the stack,
//! or else the oldest that is not minimized. A request that comes later opens
//! at once, unless the person minimized a request of the same terminal in the
//! last [`RECENT`] seconds, or one of its requests is minimized now: then it
//! joins the stack, marked new, rather than covering what they are reading.
//! A request that ends (answered, timed out, withdrawn) leaves the stack.

use std::collections::{HashMap, HashSet};

use egui::{
    Context, CursorIcon, Frame, Id, Label, Margin, Order, RichText, Sense, Stroke, pos2, vec2,
};

use crate::forms::Out;
use crate::theme;

/// How long, in seconds, a terminal's new requests join the stack after the
/// person minimized one of its requests.
pub(crate) const RECENT: f64 = 300.0;

/// The cards the stack shows before "+N more".
const SHOWN: usize = 3;

const WIDTH: f32 = 260.0;

/// A request, by what it is and its id.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum Key {
    /// A terminal's secret request; `host` is the paired machine it runs on,
    /// `None` for this one.
    Secret { host: Option<String>, id: u64 },
    /// An Agentic Works job's question.
    Job(u64),
}

impl Key {
    /// The name tests click it by.
    pub(crate) fn mark(&self) -> String {
        match self {
            Self::Secret { host: None, id } => format!("pending-mini:secret:{id}"),
            Self::Secret { host: Some(h), id } => format!("pending-mini:secret:{h}:{id}"),
            Self::Job(id) => format!("pending-mini:job:{id}"),
        }
    }
}

/// The terminal a request comes from. Its id is unique only in its project
/// (each project has its own `claude`), so the project is part of it, and
/// the machine.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Terminal {
    /// The paired machine it runs on; `None` for this one.
    pub(crate) host: Option<String>,
    pub(crate) project: Option<String>,
    pub(crate) id: String,
}

impl Terminal {
    pub(crate) fn new(host: Option<&str>, project: Option<&str>, id: &str) -> Self {
        Self {
            host: host.map(str::to_owned),
            project: project.map(str::to_owned),
            id: id.to_owned(),
        }
    }
}

/// One waiting request, as its card shows it. Every text is already made
/// safe to show (one line, quotes that cannot be broken out of).
#[derive(Debug, Clone)]
pub(crate) struct Waiting {
    pub(crate) key: Key,
    pub(crate) terminal: Terminal,
    /// What kind of request: "SECRET REQUEST", "JOB QUESTION".
    pub(crate) kind: &'static str,
    /// Who asks: the terminal and its project.
    pub(crate) who: String,
    /// What it asks for.
    pub(crate) what: String,
}

/// Which requests are minimized, and which one is open.
#[derive(Debug, Default)]
pub(crate) struct Pending {
    minimized: HashSet<Key>,
    /// Every request seen, so one that just came is told apart.
    known: HashSet<Key>,
    /// Came while its terminal's requests were minimized and not opened
    /// since: its card is marked new.
    fresh: HashSet<Key>,
    /// When the person last minimized a request of each terminal (egui time).
    minimized_at: HashMap<Terminal, f64>,
    /// Reopened from the stack: open before any older request.
    chosen: Option<Key>,
    /// The stack shows every card, not only the first few.
    show_all: bool,
    /// The request open over the window, as last drawn. Only it holds the
    /// keyboard and the pointer: minimized ones never do.
    pub(crate) expanded: Option<Key>,
}

/// What the stack asks for.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Action {
    /// Open this request's dialog again.
    Restore(Key),
    /// Show every card, or only the first few again.
    ToggleAll,
}

impl Pending {
    /// Take in the requests waiting now, oldest first, at `now`; the one to
    /// open over the window, if any.
    pub(crate) fn sync(&mut self, waiting: &[Waiting], now: f64) -> Option<Key> {
        let live: HashSet<&Key> = waiting.iter().map(|w| &w.key).collect();
        self.minimized.retain(|k| live.contains(k));
        self.known.retain(|k| live.contains(k));
        self.fresh.retain(|k| live.contains(k));
        if self.chosen.as_ref().is_some_and(|k| !live.contains(k)) {
            self.chosen = None;
        }
        for w in waiting {
            if self.known.contains(&w.key) {
                continue;
            }
            self.known.insert(w.key.clone());
            let recent = self
                .minimized_at
                .get(&w.terminal)
                .is_some_and(|at| now - at < RECENT);
            let sibling = waiting
                .iter()
                .any(|o| o.terminal == w.terminal && self.minimized.contains(&o.key));
            if recent || sibling {
                self.minimized.insert(w.key.clone());
                self.fresh.insert(w.key.clone());
            }
        }
        let open = self
            .chosen
            .clone()
            .filter(|k| !self.minimized.contains(k))
            .or_else(|| {
                waiting
                    .iter()
                    .find(|w| !self.minimized.contains(&w.key))
                    .map(|w| w.key.clone())
            });
        self.expanded.clone_from(&open);
        open
    }

    /// The person minimized `key`, of `terminal`, at `now`.
    pub(crate) fn minimize(&mut self, key: &Key, terminal: &Terminal, now: f64) {
        self.minimized.insert(key.clone());
        self.minimized_at.insert(terminal.clone(), now);
        if self.chosen.as_ref() == Some(key) {
            self.chosen = None;
        }
        if self.expanded.as_ref() == Some(key) {
            self.expanded = None;
        }
    }

    /// The person opened `key` from the stack: it opens now, and its
    /// terminal's next requests open as they come again.
    pub(crate) fn restore(&mut self, key: &Key, terminal: Option<&Terminal>) {
        self.minimized.remove(key);
        self.fresh.remove(key);
        if let Some(terminal) = terminal {
            self.minimized_at.remove(terminal);
        }
        self.chosen = Some(key.clone());
    }

    pub(crate) fn act(&mut self, action: &Action, waiting: &[Waiting]) {
        match action {
            Action::Restore(key) => {
                let terminal = waiting.iter().find(|w| &w.key == key).map(|w| &w.terminal);
                self.restore(key, terminal);
            }
            Action::ToggleAll => self.show_all = !self.show_all,
        }
    }

    pub(crate) fn is_minimized(&self, key: &Key) -> bool {
        self.minimized.contains(key)
    }
}

/// The minimized requests of `waiting`, as cards stacked up from the
/// bottom-right corner of the window, the oldest at the top. Past
/// [`SHOWN`], a "+N more" button shows the rest.
pub(crate) fn stack(ctx: &Context, waiting: &[Waiting], pending: &Pending, out: &mut Out<Action>) {
    let minimized: Vec<&Waiting> = waiting
        .iter()
        .filter(|w| pending.is_minimized(&w.key))
        .collect();
    if minimized.is_empty() {
        return;
    }
    // Folded, the first few, as many as the window has room for; "+N more"
    // unfolds them all into a list that scrolls when the window is short,
    // so every request stays reachable at any height. Cards are all one
    // height, so the stack is placed from what it holds this frame (an
    // anchored area would be placed from last frame's size, and a card just
    // added would start below the window).
    let screen = ctx.content_rect();
    let max_list = (screen.height() - 2.0 * MARGIN - BUTTON_STEP).max(CARD_HEIGHT);
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let room = (((max_list + GAP) / (CARD_HEIGHT + GAP)).floor().max(1.0)) as usize;
    let folded = SHOWN.min(room);
    let fit = if pending.show_all {
        minimized.len()
    } else {
        folded
    };
    let hidden = minimized.len().saturating_sub(fit);
    let shown = &minimized[..minimized.len() - hidden];
    let button = hidden > 0 || (pending.show_all && minimized.len() > folded);
    #[allow(clippy::cast_precision_loss)]
    let list = (shown.len() as f32 * (CARD_HEIGHT + GAP) - GAP).min(max_list);
    let total = list + if button { BUTTON_STEP } else { 0.0 };
    let place = pos2(screen.max.x - MARGIN - WIDTH, screen.max.y - MARGIN - total);
    egui::Area::new(Id::unique("pending-stack"))
        .order(Order::Tooltip)
        .fixed_pos(place)
        .show(ctx, |ui| {
            ui.set_width(WIDTH);
            ui.spacing_mut().item_spacing.y = 0.0;
            egui::ScrollArea::vertical()
                .id_salt("pending-stack-list")
                .max_height(list)
                .min_scrolled_height(list)
                .auto_shrink([false, true])
                .show(ui, |ui| {
                    ui.spacing_mut().item_spacing.y = GAP;
                    for w in shown {
                        card(ui, w, pending.fresh.contains(&w.key), out);
                    }
                });
            if button {
                ui.add_space(BUTTON_STEP - BUTTON_HEIGHT);
                ui.allocate_ui_with_layout(
                    vec2(WIDTH, BUTTON_HEIGHT),
                    egui::Layout::right_to_left(egui::Align::Center),
                    |ui| {
                        let text = if pending.show_all {
                            "Show fewer".to_owned()
                        } else {
                            format!("+{hidden} more")
                        };
                        if out.button(ui, "pending-more", &text, true) {
                            out.actions.push(Action::ToggleAll);
                        }
                    },
                );
            }
        });
}

/// The space between the stack and the window's edges.
const MARGIN: f32 = 16.0;

/// The space between two cards.
const GAP: f32 = 6.0;

/// A card's height, whatever it says: each line is cut to one.
const CARD_HEIGHT: f32 = 74.0;

/// The "+N more" button's height, and with the space above it.
const BUTTON_HEIGHT: f32 = 22.0;
const BUTTON_STEP: f32 = 28.0;

fn card(ui: &mut egui::Ui, w: &Waiting, fresh: bool, out: &mut Out<Action>) {
    let border = if fresh {
        theme::AMBER
    } else {
        theme::POPOVER_BORDER
    };
    let frame = Frame::NONE
        .fill(theme::POPOVER_BG)
        .stroke(Stroke::new(if fresh { 1.5 } else { 1.0 }, border))
        .corner_radius(f32::from(theme::RADIUS))
        .inner_margin(Margin::symmetric(12, 8))
        .show(ui, |ui| {
            ui.set_width(WIDTH - 24.0);
            ui.set_height(CARD_HEIGHT - 16.0);
            ui.spacing_mut().item_spacing.y = 2.0;
            ui.horizontal(|ui| {
                line(
                    ui,
                    out,
                    RichText::new(w.kind)
                        .size(10.5)
                        .strong()
                        .color(theme::TEXT_MUTED),
                );
                if fresh {
                    line(
                        ui,
                        out,
                        RichText::new("NEW").size(10.5).strong().color(theme::AMBER),
                    );
                }
            });
            line(
                ui,
                out,
                RichText::new(&w.who).size(12.5).strong().color(theme::TEXT),
            );
            line(
                ui,
                out,
                RichText::new(&w.what).size(12.0).color(theme::TEXT_ROW),
            );
        });
    let rect = frame.response.rect;
    let response = ui
        .interact(rect, Id::unique(("pending-card", &w.key)), Sense::click())
        .on_hover_cursor(CursorIcon::PointingHand)
        .on_hover_text("Open to answer");
    if response.hovered() {
        ui.painter().rect_stroke(
            rect,
            f32::from(theme::RADIUS),
            Stroke::new(1.0, theme::CYAN.gamma_multiply(0.7)),
            egui::StrokeKind::Inside,
        );
    }
    out.mark(&w.key.mark(), rect);
    if response.clicked() {
        out.actions.push(Action::Restore(w.key.clone()));
    }
}

/// A line of the card, one line only, that never takes the card's click.
fn line(ui: &mut egui::Ui, out: &mut Out<Action>, text: RichText) {
    out.text_mark(text.text());
    ui.add(Label::new(text).selectable(false).truncate());
}

#[cfg(test)]
mod tests {
    use super::*;

    fn waiting(key: Key, terminal: &str) -> Waiting {
        in_project(key, "quill", terminal)
    }

    fn in_project(key: Key, project: &str, terminal: &str) -> Waiting {
        Waiting {
            key,
            terminal: Terminal::new(None, Some(project), terminal),
            kind: "SECRET REQUEST",
            who: terminal.to_owned(),
            what: String::new(),
        }
    }

    /// A terminal's later request joins the stack while its others are
    /// minimized, or soon after; another terminal's opens.
    #[test]
    fn a_later_request_of_a_minimized_terminal_joins_the_stack() {
        let a = Key::Secret { host: None, id: 1 };
        let b = Key::Secret { host: None, id: 2 };
        let c = Key::Job(3);
        let mut p = Pending::default();
        let mut now = vec![waiting(a.clone(), "claude-1")];
        assert_eq!(p.sync(&now, 0.0), Some(a.clone()));
        p.minimize(&a, &Terminal::new(None, Some("quill"), "claude-1"), 0.0);
        assert_eq!(p.sync(&now, 1.0), None);
        now.push(waiting(b.clone(), "claude-1"));
        assert_eq!(p.sync(&now, 2.0), None, "same terminal: minimized");
        assert!(p.fresh.contains(&b));
        now.push(waiting(c.clone(), "codex-2"));
        assert_eq!(
            p.sync(&now, 3.0),
            Some(c.clone()),
            "another terminal: opens"
        );
        // Minimized long ago and none minimized now: a new one opens.
        let mut q = Pending::default();
        let one = vec![waiting(a.clone(), "claude-1")];
        q.sync(&one, 0.0);
        q.minimize(&a, &Terminal::new(None, Some("quill"), "claude-1"), 0.0);
        q.sync(&[], 1.0);
        let later = vec![waiting(b.clone(), "claude-1")];
        assert_eq!(q.sync(&later, RECENT + 10.0), Some(b.clone()));
    }

    /// Another project's terminal of the same id is another terminal: its
    /// request opens.
    #[test]
    fn the_same_terminal_id_in_another_project_is_another_terminal() {
        let a = Key::Secret { host: None, id: 1 };
        let b = Key::Job(2);
        let mut p = Pending::default();
        let mut now = vec![in_project(a.clone(), "quill", "claude")];
        p.sync(&now, 0.0);
        p.minimize(&a, &Terminal::new(None, Some("quill"), "claude"), 0.0);
        now.push(in_project(b.clone(), "connecto", "claude"));
        assert_eq!(p.sync(&now, 1.0), Some(b));
    }
}
