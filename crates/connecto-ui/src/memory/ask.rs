//! Ask: a question to the project's memory, answered by one small model
//! session from the stored memories only. The answers are listed newest
//! first, as plain text, each with the memories it came from as links. Only
//! one question waits at a time, and nothing is sent twice.

use connecto_core::memory::{
    Address, AskEntry, AskRequest, AskState, Lookup, MAX_ASK_BYTES, MAX_PENDING_ASKS, ReadRequest,
    ask_choices,
};
use egui::{RichText, Ui};

use super::{Cx, MemoryAction, MemoryPanel, Slot};
use crate::forms::{Out, muted, problem, warning};
use crate::theme;

/// The Ask row's own state: what is typed, which model answers, and why the
/// last question was not sent.
#[derive(Debug, Clone, Default)]
pub(crate) struct AskDraft {
    pub(crate) text: String,
    /// Index into [`ask_choices`]; the first (Claude) by default.
    pub(crate) model: usize,
    pub(crate) error: Option<String>,
}

/// The Ask row and the answers, above the search.
pub(super) fn show(ui: &mut Ui, panel: &mut MemoryPanel, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    let choices = ask_choices();
    if choices.is_empty() {
        return;
    }
    // The engine takes at most MAX_PENDING_ASKS at once: the button is off at that many.
    let pending = cx
        .project
        .asks
        .iter()
        .filter(|a| a.state.is_pending())
        .count();
    let busy = pending >= MAX_PENDING_ASKS;
    let model = panel.ask.model.min(choices.len() - 1);
    ui.horizontal_wrapped(|ui| {
        out.say(ui, muted("Ask with"));
        for (i, choice) in choices.iter().enumerate() {
            let cap = cx.view.capability(choice.provider);
            let on = i == model;
            let key = format!("mem-ask-model:{i}");
            let label = model_label(choice);
            match cap.filter(|c| c.capability.is_available()) {
                Some(_) => {
                    if out.choice(ui, &key, on, &label) && !on {
                        panel.ask.model = i;
                    }
                }
                None => {
                    let why = cap
                        .and_then(|c| c.capability.reason())
                        .unwrap_or("not available");
                    let response = ui
                        .add_enabled_ui(false, |ui| ui.selectable_label(on, label.clone()))
                        .inner
                        .on_disabled_hover_text(why);
                    out.mark(&key, response.rect);
                }
            }
        }
    });
    // Bound to the live draft: what is typed is what the button checks, and
    // a click on Ask sends it at once, with no blur first.
    let mut go = false;
    ui.horizontal_wrapped(|ui| {
        let response = ui.add(
            egui::TextEdit::singleline(&mut panel.ask.text)
                .id(egui::Id::unique(("memory-field", "mem-ask-field")))
                .desired_width(360.0)
                .margin(theme::FIELD_MARGIN)
                .hint_text("Ask about what this project remembers…"),
        );
        out.mark("mem-ask-field", response.rect);
        // Enter in the field sends, as the button does.
        if response.lost_focus() && ui.input(|i| i.key_pressed(egui::Key::Enter)) {
            go = true;
        }
        let empty = panel.ask.text.trim().is_empty();
        let why = if busy {
            "Two questions are already waiting for an answer."
        } else if empty {
            "Type a question first."
        } else {
            ""
        };
        if out.button_hint(ui, "mem-ask", "Ask", why.is_empty(), why) {
            go = true;
        }
    });
    out.say(
        ui,
        muted("Answers come from the stored memories only. Each question runs one small model session."),
    );
    if go {
        let question = panel.ask.text.trim().to_owned();
        panel.ask.error = if question.is_empty() {
            Some("Type a question first.".to_owned())
        } else if question.len() > MAX_ASK_BYTES {
            Some(format!("A question is at most {MAX_ASK_BYTES} bytes."))
        } else if busy {
            Some("Two questions are already waiting for an answer.".to_owned())
        } else {
            out.actions.push(MemoryAction::Ask(AskRequest {
                question,
                choice: choices[model].clone(),
            }));
            None
        };
    }
    if let Some(error) = &panel.ask.error {
        out.say(ui, warning(error.clone()));
    }
    answers(ui, cx, out);
    ui.add_space(6.0);
}

/// The picker's words for a model: Codex's model is its own default, so it
/// is named as such rather than by a model name.
fn model_label(choice: &connecto_core::memory::ModelChoice) -> String {
    let effort = format!("{} effort", choice.effort);
    if choice.model == "default" {
        format!("{}, its default model, {effort}", choice.provider.label())
    } else {
        format!("{} {}, {effort}", choice.provider.label(), choice.model)
    }
}

/// The questions asked, newest first.
fn answers(ui: &mut Ui, cx: &Cx<'_>, out: &mut Out<MemoryAction>) {
    for ask in cx.project.asks.iter().rev() {
        ui.push_id(("ask", ask.id), |ui| answer(ui, ask, out));
    }
}

fn answer(ui: &mut Ui, ask: &AskEntry, out: &mut Out<MemoryAction>) {
    out.say(ui, muted(format!("“{}”", ask.question)));
    match &ask.state {
        AskState::Pending => out.say(ui, muted("Asking…")),
        AskState::Answered {
            text,
            used,
            version,
        } => {
            // The model's words: plain text only, never markup.
            out.say(
                ui,
                RichText::new(text.trim_end()).size(13.0).color(theme::TEXT),
            );
            if !used.is_empty() {
                ui.horizontal_wrapped(|ui| {
                    out.say(ui, muted("From:"));
                    for addr in used {
                        link(ui, addr, out);
                    }
                });
            }
            out.say(
                ui,
                muted(format!("{} · memory version {version}", ask.choice)),
            );
        }
        AskState::Failed(why) => {
            out.say(ui, problem(format!("No answer: {why}")));
            out.say(ui, muted(format!("{}", ask.choice)));
        }
    }
    ui.add_space(8.0);
}

/// A memory's address, as a link that opens it in the reading page. An
/// address that does not parse is shown as plain text.
fn link(ui: &mut Ui, addr: &str, out: &mut Out<MemoryAction>) {
    match Address::parse(addr) {
        Ok(address) => {
            let response = ui.add(
                egui::Button::new(RichText::new(addr).size(12.0).color(theme::CYAN)).frame(false),
            );
            out.mark(&format!("mem-ask-from:{addr}"), response.rect);
            if response.clicked() {
                out.actions.push(MemoryAction::Read(
                    ReadRequest::Open(Lookup::Address(address)),
                    Slot::Detail,
                ));
            }
        }
        Err(_) => out.say(ui, muted(addr)),
    }
}
