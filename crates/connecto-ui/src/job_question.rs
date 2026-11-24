//! A running Agentic Works job asks the user a question (ADR 0040): which
//! project, work type, database and job, the agent's words, and a field for
//! the answer. Send gives the job the answer; No answer tells it none will
//! come, and it ends with its JSON.

use connecto_core::agentic_works::JobQuestion;
use egui::{Id, RichText, TextEdit};

use crate::forms::{self, Out, heading, muted, problem};
use crate::theme;

/// The answer being typed to one question (the window keeps one per
/// question, by its id).
#[derive(Debug, Default)]
pub(crate) struct Form {
    pub(crate) answer: String,
    /// Why the last answer was not taken.
    pub(crate) error: Option<String>,
}

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Action {
    /// The answer, or None: no answer will come.
    Answer { id: u64, answer: Option<String> },
    /// Shrink it to a card in the corner; the answer typed is kept.
    Minimize,
}

pub(crate) fn dialog(
    ctx: &egui::Context,
    form: &mut Form,
    question: &JobQuestion,
    waiting: usize,
    out: &mut Out<Action>,
) {
    crate::dialog::card(ctx, "job-question", |ui| {
        egui::ScrollArea::vertical().show(ui, |ui| {
            if crate::dialog::heading_with_minimize(
                ui,
                out,
                "AN AGENTIC WORKS JOB ASKS YOU",
                "job-question-minimize",
            ) {
                out.actions.push(Action::Minimize);
            }
            let project = if question.project_name.is_empty() {
                question.project.as_str()
            } else {
                question.project_name.as_str()
            };
            out.say(
                ui,
                RichText::new(format!(
                    "Project {project}: job {} of the work type {}, from the database {}.",
                    question.job, question.kind, question.database
                ))
                .size(14.0)
                .strong()
                .color(theme::TEXT),
            );
            out.say(
                ui,
                muted(format!(
                    "Terminal {}. Asked {}; answer by {}, or the job is told no answer came.",
                    question.terminal, question.asked_at, question.answer_by
                )),
            );
            forms::separator(ui);
            out.say(ui, heading("THE QUESTION"));
            out.say(
                ui,
                RichText::new(format!("\u{201c}{}\u{201d}", question.text))
                    .size(14.0)
                    .color(theme::TEXT),
            );
            out.say(
                ui,
                muted("The agent's own words: Connecto cannot check them. Answer only what the job needs."),
            );
            if waiting > 1 {
                out.say(
                    ui,
                    muted(format!("{} more questions are waiting.", waiting - 1)),
                );
            }
            if let Some(why) = &form.error {
                out.say(ui, problem(why));
            }
            forms::separator(ui);
            out.say(ui, RichText::new("Your answer").color(theme::TEXT));
            let typed = ui.add(
                TextEdit::multiline(&mut form.answer)
                    .id(Id::unique(("job-question-answer", question.id)))
                    .desired_rows(4)
                    .desired_width(f32::INFINITY)
                    .margin(theme::FIELD_MARGIN),
            );
            out.mark("job-question-answer", typed.rect);
            forms::separator(ui);
            ui.horizontal(|ui| {
                if out.button(
                    ui,
                    "job-question-send",
                    "Send answer",
                    !form.answer.trim().is_empty(),
                ) {
                    out.actions.push(Action::Answer {
                        id: question.id,
                        answer: Some(form.answer.trim().to_owned()),
                    });
                }
                if out.button(ui, "job-question-none", "No answer", true) {
                    out.actions.push(Action::Answer {
                        id: question.id,
                        answer: None,
                    });
                }
            });
        });
    });
}
