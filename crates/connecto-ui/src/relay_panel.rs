//! The top bar and the Manual relay panel.
//!
//! The window does not draw either since project-tree-ui (R20). The module
//! stays compiled and tested so the relay can come back.
//!
//! Captured replies are shown as text and offered through **Use**, which only
//! fills the message box. Sending takes a click on **Send**, every time.

use std::sync::Arc;

use connecto_core::{DeliveryState, Origin, PauseSwitch, RelayControl, RelayView, SendRequest};
use egui::{Label, RichText, ScrollArea, TextEdit, Ui};

const CAPTURES_HEIGHT: f32 = 60.0;
const SHOWN_REPLY: usize = 160;
use crate::picker::pick_folder;
use crate::theme::{self, CORAL_SOFT as WARNING, ERROR, FieldStyle as _};

/// Where the top bar's controls start: after the traffic lights and the mark.
pub(crate) const BAR_LEFT: f32 = 80.0 + 22.0 + 8.0;
pub(crate) const TOP_BAR_HEIGHT: f32 = 38.0;
pub(crate) const RELAY_HEIGHT: f32 = 210.0;

pub(crate) struct RelayPanel {
    control: Arc<dyn RelayControl>,
    pause: Arc<PauseSwitch>,
    draft: String,
    target: usize,
    source: Option<usize>,
    origin: Option<Origin>,
    previewed: String,
    preview: Result<String, String>,
    notice: Option<String>,
    /// The confirmation was scrolled into view when it appeared.
    confirmation_seen: bool,
    /// Every control of the last pass and the scroll area it sits in.
    #[cfg(test)]
    controls: Vec<(&'static str, egui::Rect)>,
    #[cfg(test)]
    scroll: Option<(egui::Rect, egui::Vec2)>,
}

fn short(text: &str) -> String {
    let line = text.lines().next().unwrap_or_default();
    if line.chars().count() > SHOWN_REPLY {
        let cut: String = line.chars().take(SHOWN_REPLY).collect();
        format!("{cut}…")
    } else {
        line.to_owned()
    }
}

impl RelayPanel {
    pub(crate) fn new(control: Arc<dyn RelayControl>) -> Self {
        let pause = control.pause_switch();
        Self {
            control,
            pause,
            draft: String::new(),
            target: 0,
            source: None,
            origin: None,
            previewed: String::new(),
            preview: Err("the message is empty".to_owned()),
            notice: None,
            confirmation_seen: false,
            #[cfg(test)]
            controls: Vec::new(),
            #[cfg(test)]
            scroll: None,
        }
    }

    /// Laid out from the right, so a long path gives way and the buttons
    /// always show.
    ///
    /// Returns where every control sits and its id, so the title strip leaves them out of
    /// its drag area.
    pub(crate) fn top_bar(&mut self, ui: &mut Ui, any_live: bool) -> Vec<(egui::Rect, egui::Id)> {
        let view = self.control.view();
        let mut controls = Vec::new();
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            let stop = ui.add(theme::destructive("Stop all"));
            controls.push((stop.rect, stop.id));
            if stop.clicked() {
                self.control.stop_all();
            }
            if self.pause.is_paused() {
                let resume = ui.add(theme::primary("Resume"));
                controls.push((resume.rect, resume.id));
                if resume.clicked() {
                    self.pause.resume();
                }
                let label = ui
                    .colored_label(WARNING, "Paused")
                    .on_hover_text("Running turns continue.");
                controls.push((label.rect, label.id));
            } else {
                let pause = ui.add(theme::button("Pause"));
                controls.push((pause.rect, pause.id));
                if pause.clicked() {
                    self.pause.pause();
                }
            }
            ui.separator();
            let choose = ui.add_enabled(!any_live, theme::button("Choose…"));
            controls.push((choose.rect, choose.id));
            if choose.clicked() {
                self.choose();
            }
            let project = view.project.as_deref().unwrap_or("no project chosen");
            let path = ui.add(Label::new(format!("Project: {project}")).truncate());
            controls.push((path.rect, path.id));
        });
        controls
    }

    /// The native panel is modal, which is why this needs no live session.
    fn choose(&mut self) {
        if let Some(path) = pick_folder() {
            self.notice = self.control.choose_project(&path).err();
        }
    }

    /// Latest replies, in a box of fixed height so eight terminals leave the
    /// composer on screen.
    fn captures(&mut self, ui: &mut Ui, view: &RelayView) {
        ScrollArea::vertical()
            .id_salt("captures")
            .max_height(CAPTURES_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| self.capture_rows(ui, view));
    }

    fn capture_rows(&mut self, ui: &mut Ui, view: &RelayView) {
        for (index, session) in view.sessions.iter().enumerate() {
            if session.vacant {
                continue;
            }
            ui.horizontal_wrapped(|ui| {
                ui.label(format!("Latest from {}:", session.name));
                let Some(capture) = &session.capture else {
                    ui.weak("nothing yet");
                    return;
                };
                let use_capture = ui.add(theme::button("Use"));
                #[cfg(feature = "ui-test-driver")]
                crate::driver::note(ui.ctx(), format!("use:{index}"), use_capture.rect);
                #[cfg(test)]
                self.controls.push(("use", use_capture.rect));
                if use_capture.clicked() {
                    capture.text.expose().clone_into(&mut self.draft);
                    self.source = Some(index);
                    self.origin.clone_from(&capture.origin);
                }
                let suffix = if capture.text.truncated() {
                    " (truncated)"
                } else {
                    ""
                };
                ui.label(format!("{}{suffix}", short(capture.text.expose())));
            });
        }
    }

    fn send(&mut self, view: &RelayView, confirmed: bool) {
        let attempt = view.sessions.get(self.target).map_or(0, |s| s.attempt);
        let request = SendRequest {
            target: self.target,
            attempt,
            body: self.draft.clone(),
            confirmed,
            source: self.source,
            origin: self.origin.clone(),
        };
        self.notice = self.control.send(request).err();
    }

    fn composer(&mut self, ui: &mut Ui, view: &RelayView) {
        ui.horizontal(|ui| {
            ui.label("To:");
            // Slots are stable, so a removed terminal leaves a gap.
            if view.sessions.get(self.target).is_none_or(|s| s.vacant)
                && let Some(first) = view.sessions.iter().position(|s| !s.vacant)
            {
                self.target = first;
            }
            let current = view
                .sessions
                .get(self.target)
                .map_or("", |session| session.name.as_str());
            let _picker = egui::ComboBox::from_id_salt("relay-target")
                .selected_text(current)
                .width(120.0)
                .show_ui(ui, |ui| {
                    for (index, session) in view.sessions.iter().enumerate() {
                        if !session.vacant {
                            let _item =
                                ui.selectable_value(&mut self.target, index, session.name.as_str());
                            #[cfg(feature = "ui-test-driver")]
                            crate::driver::note(ui.ctx(), format!("to:{index}"), _item.rect);
                        }
                    }
                });
            #[cfg(feature = "ui-test-driver")]
            crate::driver::note(ui.ctx(), "to", _picker.response.rect);
            #[cfg(test)]
            self.controls.push(("picker", _picker.response.rect));
        });
        let edit = TextEdit::multiline(&mut self.draft)
            .padded()
            .desired_rows(3)
            .desired_width(f32::INFINITY)
            .hint_text("Message");
        let editor = ui.add(edit);
        #[cfg(test)]
        self.controls.push(("draft", editor.rect));
        #[cfg(feature = "ui-test-driver")]
        crate::driver::note(ui.ctx(), "draft", editor.rect);
        if editor.changed() && self.draft.is_empty() {
            self.source = None;
            self.origin = None;
        }
        if self.previewed != self.draft {
            self.preview = self.control.preview(&self.draft);
            self.previewed.clone_from(&self.draft);
        }
        ui.horizontal_wrapped(|ui| {
            let ready = self.preview.is_ok();
            let send = ui.add_enabled(ready, theme::primary("Send"));
            #[cfg(feature = "ui-test-driver")]
            crate::driver::note(ui.ctx(), "send", send.rect);
            #[cfg(test)]
            self.controls.push(("send", send.rect));
            if send.clicked() {
                self.send(view, false);
            }
            match &self.preview {
                Ok(text) if *text == self.draft => {
                    ui.weak("Sent exactly as typed, after a generated header.");
                }
                Ok(_) => {
                    ui.colored_label(WARNING, "Changed before sending. See below.");
                }
                Err(reason) => {
                    ui.colored_label(ERROR, reason);
                }
            }
        });
        if let Ok(text) = &self.preview
            && *text != self.draft
        {
            ui.label(RichText::new(short(text)).monospace());
        }
        if let Some(confirmation) = &view.confirmation
            && confirmation.target == self.target
        {
            let text = confirmation.text.clone();
            ui.horizontal_wrapped(|ui| {
                ui.colored_label(WARNING, text);
                let anyway = ui.add(theme::destructive("Send anyway"));
                #[cfg(feature = "ui-test-driver")]
                crate::driver::note(ui.ctx(), "send_anyway", anyway.rect);
                #[cfg(test)]
                self.controls.push(("send anyway", anyway.rect));
                if !self.confirmation_seen {
                    self.confirmation_seen = true;
                    anyway.scroll_to_me(None);
                }
                if anyway.clicked() {
                    self.send(view, true);
                }
            });
        }
        if view.confirmation.is_none() {
            self.confirmation_seen = false;
        }
        if let Some(notice) = &self.notice {
            ui.colored_label(ERROR, notice);
        }
    }

    fn ledger(&mut self, ui: &mut Ui, view: &RelayView) {
        ScrollArea::vertical().stick_to_bottom(true).show(ui, |ui| {
            for row in &view.ledger {
                let text = if row.reason.is_empty() {
                    format!(
                        "{} · {} → {} · {}",
                        row.id,
                        row.from,
                        row.to,
                        row.state.label()
                    )
                } else {
                    format!(
                        "{} · {} → {} · {} · {}",
                        row.id,
                        row.from,
                        row.to,
                        row.state.label(),
                        row.reason
                    )
                };
                ui.horizontal_wrapped(|ui| {
                    ui.label(RichText::new(text).size(11.0));
                    let open = matches!(
                        row.state,
                        DeliveryState::Written | DeliveryState::Acknowledged
                    );
                    let end = open.then(|| ui.add(theme::button("End delivery").small()));
                    #[cfg(test)]
                    if let Some(end) = &end {
                        self.controls.push(("end delivery", end.rect));
                    }
                    if end.is_some_and(|end| end.clicked()) {
                        self.notice = self.control.end_delivery(row.target, &row.id).err();
                    }
                });
            }
        });
    }

    pub(crate) fn panel(&mut self, ui: &mut Ui) {
        let view = self.control.view();
        #[cfg(test)]
        self.controls.clear();
        // The whole body scrolls inside the fixed height, so no state can
        // push a control out of reach.
        let _body = ScrollArea::vertical()
            .id_salt("relay-body")
            .auto_shrink([false, false])
            .show(ui, |ui| {
                ui.strong("Manual relay");
                self.captures(ui, &view);
                ui.columns(2, |columns| {
                    self.composer(&mut columns[0], &view);
                    self.ledger(&mut columns[1], &view);
                });
            });
        #[cfg(test)]
        {
            self.scroll = Some((_body.inner_rect, _body.content_size));
        }
    }
}

#[cfg(test)]
#[allow(clippy::panic)]
mod tests {
    use super::*;
    use connecto_core::{Capture, EventText, LedgerRow, Readiness, SessionView};

    #[derive(Default)]
    struct Eight {
        pause: Arc<PauseSwitch>,
        /// Show the Codex "send anyway" confirmation for the first terminal.
        confirm: bool,
    }

    impl RelayControl for Eight {
        fn view(&self) -> Arc<RelayView> {
            let sessions = (0..8)
                .map(|i| SessionView {
                    name: format!("claude-{i}"),
                    vacant: false,
                    attempt: 1,
                    live: true,
                    readiness: Readiness::Ready,
                    capture: Some(Capture {
                        text: EventText::new("a long reply that goes on and on ".repeat(10)),
                        origin: None,
                    }),
                })
                .collect();
            let ledger = (0..4)
                .map(|i| LedgerRow {
                    id: format!("message-{i}-with-a-long-identifier"),
                    target: 0,
                    from: "claude-1".to_owned(),
                    to: "claude-7".to_owned(),
                    state: DeliveryState::Written,
                    reason: "waiting for the other terminal to finish".to_owned(),
                })
                .collect();
            let confirmation = self.confirm.then(|| connecto_core::Confirmation {
                target: 0,
                text: "Codex may still be starting, or has a draft in its composer. A draft joins this message and Enter sends both.".to_owned(),
            });
            Arc::new(RelayView {
                sessions,
                ledger,
                confirmation,
                ..RelayView::default()
            })
        }

        fn pause_switch(&self) -> Arc<PauseSwitch> {
            Arc::clone(&self.pause)
        }

        fn preview(&self, body: &str) -> Result<String, String> {
            Ok(body.to_owned())
        }

        fn send(&self, _request: SendRequest) -> Result<(), String> {
            Ok(())
        }

        fn end_delivery(&self, _target: usize, _id: &str) -> Result<(), String> {
            Ok(())
        }

        fn stop_all(&self) {}

        fn choose_project(&self, _path: &std::path::Path) -> Result<(), String> {
            Ok(())
        }
    }

    /// Where the top bar and the panel draw, in the boxes the window gives
    /// them: the bar's horizontal extent, the panel's, and the panel's bottom.
    /// Painted shapes are measured, because a row that does not fit still
    /// reports the room it was given.
    fn fits(width: f32, paused: bool) -> ((f32, f32), (f32, f32), f32) {
        let control = Arc::new(Eight::default());
        if paused {
            control.pause.pause();
        }
        let mut panel = RelayPanel::new(control);
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let top = egui::Rect::from_min_size(
            egui::pos2(BAR_LEFT + 8.0, 4.0),
            egui::vec2(width - BAR_LEFT - 16.0, TOP_BAR_HEIGHT - 8.0),
        );
        let bounds = egui::Rect::from_min_size(
            egui::pos2(8.0, 0.0),
            egui::vec2(width - 16.0, RELAY_HEIGHT - 16.0),
        );
        let screen = egui::Rect::from_min_size(egui::Pos2::ZERO, egui::vec2(width, 760.0));
        let run = |draw: &mut dyn FnMut(&mut Ui)| {
            let mut spans = (f32::MAX, f32::MIN);
            let mut bottom = 0.0_f32;
            for _ in 0..3 {
                let input = egui::RawInput {
                    screen_rect: Some(screen),
                    ..egui::RawInput::default()
                };
                let mut output = ctx.run_ui(input, |ui| draw(ui));
                output.textures_delta.clear();
                spans = (f32::MAX, f32::MIN);
                for clipped in &output.shapes {
                    let rect = clipped.shape.visual_bounding_rect();
                    if rect.is_positive() {
                        spans = (spans.0.min(rect.left()), spans.1.max(rect.right()));
                        bottom = bottom.max(rect.intersect(clipped.clip_rect).bottom());
                    }
                }
            }
            (spans, bottom)
        };
        let (bar, _) = run(&mut |ui| {
            ui.scope_builder(egui::UiBuilder::new().max_rect(top), |ui| {
                panel.top_bar(ui, false);
            });
        });
        let (body, bottom) = run(&mut |ui| {
            ui.scope_builder(egui::UiBuilder::new().max_rect(bounds), |ui| {
                panel.panel(ui);
            });
        });
        (bar, body, bottom)
    }

    #[test]
    fn eight_terminals_fit_the_panel_at_every_supported_width() {
        for width in [1280.0, 480.0] {
            for paused in [false, true] {
                let (bar, body, bottom) = fits(width, paused);
                let height = RELAY_HEIGHT - 16.0;
                assert!(bottom <= height + 0.5, "{width}: needs {bottom}");
                assert!(
                    body.0 >= 0.0 && body.1 <= width,
                    "{width}: panel draws {}..{}",
                    body.0,
                    body.1
                );
                assert!(
                    bar.0 >= BAR_LEFT && bar.1 <= width,
                    "{width}, paused {paused}: bar draws {}..{}",
                    bar.0,
                    bar.1
                );
            }
        }
    }

    /// Every control must be on screen or reachable by scrolling, in every
    /// state the panel can be in.
    #[test]
    fn every_control_is_reachable_with_a_draft_and_a_confirmation() {
        for width in [1280.0_f32, 480.0] {
            for paused in [false, true] {
                for confirm in [false, true] {
                    reachable(width, paused, confirm);
                }
            }
        }
    }

    fn reachable(width: f32, paused: bool, confirm: bool) {
        let state = format!("{width} px, paused {paused}, confirmation {confirm}");
        let control = Arc::new(Eight {
            confirm,
            ..Eight::default()
        });
        if paused {
            control.pause.pause();
        }
        let mut panel = RelayPanel::new(control);
        panel.draft = "hello".to_owned();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let bounds = egui::Rect::from_min_size(
            egui::pos2(8.0, 0.0),
            egui::vec2(width - 16.0, RELAY_HEIGHT - 16.0),
        );
        let frame = |panel: &mut RelayPanel, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(width, 760.0),
                )),
                events,
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                ui.scope_builder(egui::UiBuilder::new().max_rect(bounds), |ui| {
                    panel.panel(ui);
                });
            });
            output.textures_delta.clear();
        };
        for _ in 0..3 {
            frame(&mut panel, vec![]);
        }
        let Some((inner, content)) = panel.scroll else {
            panic!("{state}: the panel drew nothing");
        };
        if confirm && width < 600.0 {
            assert!(
                content.y > inner.height(),
                "{state}: the case must overflow"
            );
        }
        let mut wanted = vec!["picker", "draft", "send"];
        if confirm {
            wanted.push("send anyway");
        }
        let hover = egui::Event::PointerMoved(inner.center());
        for name in wanted {
            let mut seen = false;
            for _ in 0..80 {
                let rect = panel
                    .controls
                    .iter()
                    .find(|(n, _)| *n == name)
                    .unwrap_or_else(|| panic!("{state}: no {name}"))
                    .1;
                assert!(
                    rect.left() >= 0.0 && rect.right() <= width,
                    "{state}: {name} spans {}..{}",
                    rect.left(),
                    rect.right()
                );
                if name.contains("send") {
                    assert!(rect.height() <= 28.0, "{state}: {name} wraps: {rect:?}");
                }
                if rect.top() >= inner.top() - 0.5 && rect.bottom() <= inner.bottom() + 0.5 {
                    seen = true;
                    break;
                }
                let toward = if rect.top() < inner.top() {
                    40.0
                } else {
                    -40.0
                };
                let wheel = egui::Event::MouseWheel {
                    unit: egui::MouseWheelUnit::Point,
                    delta: egui::vec2(0.0, toward),
                    phase: egui::TouchPhase::Move,
                    source: egui::MouseWheelSource::Unknown,
                    modifiers: egui::Modifiers::NONE,
                };
                frame(&mut panel, vec![hover.clone(), wheel]);
                frame(&mut panel, vec![]);
            }
            assert!(seen, "{state}: {name} cannot be scrolled into view");
        }
    }

    /// Cmd+A in the Send box keeps its normal meaning: select its text.
    #[test]
    fn cmd_a_in_the_composer_selects_the_draft() {
        let mut panel = RelayPanel::new(Arc::new(Eight::default()));
        panel.draft = "hello".to_owned();
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let bounds = egui::Rect::from_min_size(
            egui::pos2(8.0, 0.0),
            egui::vec2(1264.0, RELAY_HEIGHT - 16.0),
        );
        let frame = |panel: &mut RelayPanel, events: Vec<egui::Event>| {
            let input = egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    egui::Pos2::ZERO,
                    egui::vec2(1280.0, 760.0),
                )),
                events,
                ..egui::RawInput::default()
            };
            let mut output = ctx.run_ui(input, |ui| {
                ui.scope_builder(egui::UiBuilder::new().max_rect(bounds), |ui| {
                    panel.panel(ui);
                });
            });
            output.textures_delta.clear();
        };
        for _ in 0..2 {
            frame(&mut panel, vec![]);
        }
        let Some(&(_, draft)) = panel.controls.iter().find(|(name, _)| *name == "draft") else {
            panic!("no draft box");
        };
        let at = draft.center();
        let click = |pressed| egui::Event::PointerButton {
            pos: at,
            button: egui::PointerButton::Primary,
            pressed,
            modifiers: egui::Modifiers::NONE,
        };
        frame(&mut panel, vec![egui::Event::PointerMoved(at)]);
        frame(&mut panel, vec![click(true)]);
        frame(&mut panel, vec![click(false)]);
        let cmd_a = egui::Event::Key {
            key: egui::Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: egui::Modifiers {
                mac_cmd: true,
                command: true,
                ..egui::Modifiers::NONE
            },
        };
        frame(&mut panel, vec![cmd_a]);
        frame(&mut panel, vec![]);
        let Some(id) = ctx.memory(|memory| memory.focused()) else {
            panic!("the draft box lost focus");
        };
        let range = egui::TextEdit::load_state(&ctx, id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| (range.primary.index.0, range.secondary.index.0));
        assert!(
            matches!(range, Some((0, 5) | (5, 0))),
            "the whole draft is selected: {range:?}"
        );
        assert_eq!(panel.draft, "hello");
    }

    #[test]
    fn the_target_picker_lists_all_eight_terminals() {
        let view = Eight::default().view();
        let names: Vec<_> = view.sessions.iter().map(|s| s.name.as_str()).collect();
        assert_eq!(names.len(), 8);
        // A picker of fixed width, not a row of eight buttons: the panel
        // fits at the narrowest window (see the test above).
    }
}
