//! The pairing dialog. Both machines click Pair a device: each shows its own
//! digits and types the other's, then each names the other. It is drawn
//! above Settings, and closing it at any phase stops the pairing.

use std::time::Duration;

use connecto_core::{PAIRING_CODE_DIGITS, PairingPhase, RemoteView};
use egui::{Context, Id, Key, RichText, TextEdit, Ui};

use crate::dialog;
use crate::forms::{Out, heading, muted, problem};
use crate::theme;

/// How long the confirmation stays before the dialog closes itself.
const DONE_FOR: f64 = 2.0;

#[derive(Default)]
pub(crate) struct PairingSheet {
    /// Why the last call was refused. Cleared by the next attempt.
    pub(crate) error: Option<String>,
    /// The other machine's code, digits only.
    pub(crate) digits: String,
    /// The name for the other machine, once it is asked for. Prefilled with
    /// the suggested one.
    pub(crate) name: Option<String>,
    /// Whether this attempt has shown a phase other than Idle. Only then does
    /// Idle mean the pairing ended, rather than not having begun.
    started: bool,
    /// A command that was refused while the dialog is open. Transient: the
    /// next action clears it. Kept apart from `error`, which is the backend's.
    pub(crate) notice: Option<String>,
    /// The field that last asked for the keyboard, by phase, so focus moves
    /// once when that phase first shows.
    focus_for: Option<&'static str>,
    /// When the confirmation was first drawn.
    done_at: Option<f64>,
}

impl PairingSheet {
    /// A dialog for a pairing just attempted: nothing carried over from an
    /// earlier attempt, and the refusal (if any) shown.
    pub(crate) fn fresh(start: Result<(), String>) -> Self {
        Self {
            error: start.err(),
            ..Self::default()
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum PairingAction {
    /// Stop the pairing and close the dialog.
    Close,
    /// Start a pairing (or try again).
    Start,
    SubmitCode(String),
    FinishName(String),
    /// Close this, and add a machine by address and SSH key instead.
    ByAddress,
}

/// The code's digits from whatever was typed or pasted: `1234 5678` gives
/// `12345678`.
pub(crate) fn code_digits(text: &str) -> String {
    text.chars()
        .filter(char::is_ascii_digit)
        .take(PAIRING_CODE_DIGITS)
        .collect()
}

/// A code in two halves, `1234 5678`. Anything that is not a whole code is
/// shown as its digits.
fn grouped(code: &str) -> String {
    let digits = code_digits(code);
    let half = PAIRING_CODE_DIGITS / 2;
    if digits.len() == PAIRING_CODE_DIGITS {
        format!("{} {}", &digits[..half], &digits[half..])
    } else {
        digits
    }
}

/// Draw the dialog for the view's pairing. `esc` is set when Escape was
/// pressed: that closes it, as the close button does.
pub(crate) fn dialog(
    ctx: &Context,
    state: &mut PairingSheet,
    view: &RemoteView,
    esc: bool,
    out: &mut Out<PairingAction>,
) {
    let phase = &view.pairing;
    // Escape closes the dialog, but not while the backend finishes: it refuses
    // a cancel then, so the dialog keeps showing the live phase.
    if esc && !matches!(phase, PairingPhase::Keeping) {
        out.actions.push(PairingAction::Close);
        return;
    }
    if matches!(phase, PairingPhase::Idle) {
        // Idle after a phase was seen for this attempt: the pairing ended with
        // nothing to show, so the dialog has nothing left to say.
        if state.started && state.error.is_none() {
            out.actions.push(PairingAction::Close);
            return;
        }
    } else {
        state.started = true;
    }
    let now = ctx.input(|i| i.time);
    match phase {
        PairingPhase::Done { .. } => {
            ctx.request_repaint_after(Duration::from_millis(200));
        }
        PairingPhase::Idle
        | PairingPhase::Preparing
        | PairingPhase::ShowingCode { .. }
        | PairingPhase::WaitingForPeer { .. }
        | PairingPhase::Connecting
        | PairingPhase::Confirming
        | PairingPhase::Keeping => {
            // The countdown and the progress move on their own.
            ctx.request_repaint_after(Duration::from_millis(500));
        }
        PairingPhase::NameNeeded { .. } | PairingPhase::Failed { .. } => {}
    }
    if !matches!(phase, PairingPhase::Done { .. }) {
        state.done_at = None;
    }
    dialog::card(ctx, "pairing-dialog", |ui| {
        body(ui, state, phase, now, out);
    });
}

fn body(
    ui: &mut Ui,
    state: &mut PairingSheet,
    phase: &PairingPhase,
    now: f64,
    out: &mut Out<PairingAction>,
) {
    // The field of this phase takes the keyboard once, when it first shows.
    let wanted = match phase {
        PairingPhase::ShowingCode { .. } => Some("code"),
        PairingPhase::NameNeeded { .. } => Some("name"),
        _ => None,
    };
    let fresh = state.focus_for != wanted;
    state.focus_for = wanted;

    out.say(ui, heading("PAIR A DEVICE"));
    match phase {
        PairingPhase::Idle => out.say(
            ui,
            RichText::new("No pairing is under way.")
                .size(13.0)
                .color(theme::TEXT),
        ),
        PairingPhase::Preparing => progress(
            ui,
            out,
            "Preparing…",
            "The two codes are turned into a key. That is slow on purpose, and reaching the relay can take up to about 20 seconds. Keep this open.",
        ),
        PairingPhase::ShowingCode {
            my_code,
            expires_in_secs,
        } => {
            code_card(ui, my_code, *expires_in_secs, out);
            ui.add_space(12.0);
            enter_code(ui, state, fresh, out);
        }
        PairingPhase::WaitingForPeer {
            my_code,
            expires_in_secs,
        } => {
            code_card(ui, my_code, *expires_in_secs, out);
            ui.add_space(12.0);
            progress(
                ui,
                out,
                "Waiting for the other machine",
                "Both codes are entered here. Waiting for the other machine to enter this code.",
            );
        }
        PairingPhase::Connecting => progress(
            ui,
            out,
            "Connecting…",
            "The other machine is connected. The keys are being exchanged.",
        ),
        PairingPhase::NameNeeded { suggested_name } => {
            name_field(ui, state, suggested_name, fresh, out);
        }
        PairingPhase::Confirming => progress(
            ui,
            out,
            "Waiting for the other machine",
            "Your name for it is saved. Waiting for the other machine to name this one too. Cancelling now undoes the pairing on both machines.",
        ),
        PairingPhase::Keeping => progress(
            ui,
            out,
            "Finishing…",
            "Finishing… this takes a few seconds. The pairing cannot be stopped now.",
        ),
        PairingPhase::Done { .. } => {
            let started = *state.done_at.get_or_insert(now);
            out.say(
                ui,
                RichText::new("Paired. The machine is in the list now.")
                    .size(15.0)
                    .strong()
                    .color(theme::GREEN),
            );
            if now - started >= DONE_FOR {
                out.actions.push(PairingAction::Close);
            }
        }
        PairingPhase::Failed { message } => {
            out.say(ui, problem(message.clone()));
        }
    }
    if let Some(error) = &state.error {
        ui.add_space(8.0);
        out.say(ui, problem(error.clone()));
    }
    if let Some(notice) = &state.notice {
        ui.add_space(8.0);
        out.say(ui, muted(notice.clone()));
    }
    footer(ui, phase, state, out);
}

/// A step under way: a spinner, the step, and what it means.
fn progress(ui: &mut Ui, out: &mut Out<PairingAction>, title: &str, text: &str) {
    ui.horizontal(|ui| {
        ui.spinner();
        out.say(
            ui,
            RichText::new(title).size(15.0).strong().color(theme::TEXT),
        );
    });
    out.say(ui, RichText::new(text).size(13.0).color(theme::TEXT));
}

/// This machine's code, large, with its countdown.
fn code_card(ui: &mut Ui, my_code: &str, expires_in_secs: u64, out: &mut Out<PairingAction>) {
    out.say(ui, heading("THIS MACHINE'S CODE"));
    out.say(
        ui,
        RichText::new(grouped(my_code))
            .monospace()
            .size(36.0)
            .strong()
            .color(theme::CYAN),
    );
    out.say(
        ui,
        RichText::new(format!(
            "Valid for {}:{:02}. Enter its {} digits on the other machine.",
            expires_in_secs / 60,
            expires_in_secs % 60,
            PAIRING_CODE_DIGITS
        ))
        .size(13.0)
        .color(theme::TEXT),
    );
    let copy = ui.button("Copy code");
    out.mark("pairing-copy", copy.rect);
    if copy.clicked() {
        // The digits only, so a paste into the other machine's field is exact.
        ui.ctx().copy_text(code_digits(my_code));
    }
}

/// The other machine's code, typed or pasted. Submitted once each time it
/// becomes whole, or on Enter.
fn enter_code(ui: &mut Ui, state: &mut PairingSheet, fresh: bool, out: &mut Out<PairingAction>) {
    out.say(ui, heading("THE OTHER MACHINE'S CODE"));
    out.say(
        ui,
        RichText::new(format!("On the other machine, click Pair a device too. Type the {PAIRING_CODE_DIGITS} digits it shows. Enter submits them."))
            .size(13.0)
            .color(theme::TEXT),
    );
    let before = state.digits.len();
    let response = ui.add(
        TextEdit::singleline(&mut state.digits)
            .id(Id::unique("pairing-code"))
            .hint_text("digits from the other machine")
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    );
    if fresh {
        response.request_focus();
    }
    out.mark("pairing-code", response.rect);
    state.digits = code_digits(&state.digits);
    // Only the edit that completes the code submits it: typing on after that
    // does not resend the same code.
    let completed = response.changed()
        && before < PAIRING_CODE_DIGITS
        && state.digits.len() == PAIRING_CODE_DIGITS;
    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    let ready = state.digits.len() == PAIRING_CODE_DIGITS;
    if out.button_hint(
        ui,
        "pairing-submit",
        "Use this code",
        ready,
        &format!("Type all {PAIRING_CODE_DIGITS} digits first."),
    ) || ((completed || enter) && ready)
    {
        state.error = None;
        out.actions
            .push(PairingAction::SubmitCode(state.digits.clone()));
    }
}

/// The name for the other machine, prefilled with the suggested one.
fn name_field(
    ui: &mut Ui,
    state: &mut PairingSheet,
    suggested: &str,
    fresh: bool,
    out: &mut Out<PairingAction>,
) {
    out.say(ui, heading("NAME THE OTHER MACHINE"));
    out.say(
        ui,
        RichText::new("The two machines are connected. Choose the name this machine shows for the other one, then press Enter.")
            .size(13.0)
            .color(theme::TEXT),
    );
    let name = state.name.get_or_insert_with(|| suggested.to_owned());
    let response = ui.add(
        TextEdit::singleline(name)
            .id(Id::unique("pairing-name"))
            .desired_width(f32::INFINITY)
            .margin(theme::FIELD_MARGIN),
    );
    if fresh {
        response.request_focus();
    }
    out.mark("pairing-name", response.rect);
    let ready = !name.trim().is_empty();
    let enter = response.lost_focus() && ui.input(|i| i.key_pressed(Key::Enter));
    if out.button_hint(
        ui,
        "pairing-save-name",
        "Save name",
        ready,
        "Give it a name.",
    ) || (enter && ready)
    {
        state.error = None;
        out.actions
            .push(PairingAction::FinishName(name.trim().to_owned()));
    }
}

fn footer(ui: &mut Ui, phase: &PairingPhase, state: &PairingSheet, out: &mut Out<PairingAction>) {
    ui.add_space(14.0);
    let finishing = matches!(phase, PairingPhase::Keeping);
    let stopped = matches!(
        phase,
        PairingPhase::Idle | PairingPhase::Done { .. } | PairingPhase::Failed { .. }
    );
    ui.horizontal_wrapped(|ui| {
        let retry = matches!(phase, PairingPhase::Failed { .. })
            || (matches!(phase, PairingPhase::Idle) && state.error.is_some());
        if retry && out.button(ui, "pairing-retry", "Try again", true) {
            out.actions.push(PairingAction::Start);
        }
        if finishing {
            out.button_hint(
                ui,
                "pairing-close",
                "Finishing…",
                false,
                "The pairing is being finished. It cannot be stopped now.",
            );
        } else {
            let label = if stopped { "Close" } else { "Cancel pairing" };
            if out.button(ui, "pairing-close", label, true) {
                out.actions.push(PairingAction::Close);
            }
        }
    });
    ui.add_space(6.0);
    if out.button_hint(
        ui,
        "pairing-advanced",
        "Advanced: add by address and an SSH key instead",
        !finishing,
        "Not while the pairing is being finished.",
    ) {
        out.actions.push(PairingAction::ByAddress);
    }
}
