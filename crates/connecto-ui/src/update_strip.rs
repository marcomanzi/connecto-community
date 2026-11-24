//! The strip at the top of the window that offers an update: "Connecto 0.1.9
//! is ready. Update and restart / Later". It also says what a check the
//! person asked for found, and waits for running agent turns before the
//! restart.

use connecto_core::{UpdateStatus, UpdateView};
use egui::{Rect, RichText, Stroke, Ui, vec2};

use crate::forms::Out;
use crate::theme;

/// The strip's height.
pub(crate) const HEIGHT: f32 = 34.0;

/// What the strip asks for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum StripAction {
    /// Update and restart (after the running turns).
    Install,
    /// Not now: the strip goes until the next version, or a check asked for.
    Later,
    /// Restart now, without waiting for the running turns.
    RestartNow,
    /// Stop waiting to restart.
    CancelWait,
    /// Close a message.
    Dismiss,
    /// Open the download page (a copy that cannot update itself).
    OpenSite,
}

/// What the window remembers about the strip.
#[derive(Debug, Default)]
pub(crate) struct StripState {
    /// The version the person said "Later" to.
    pub(crate) later: Option<String>,
    /// Waiting for running turns before the restart.
    pub(crate) waiting: bool,
    /// The result of a check the person asked for was closed.
    pub(crate) closed_asked: bool,
    /// Why the restart could not start.
    pub(crate) error: Option<String>,
}

/// One line and its buttons.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Line {
    pub(crate) text: String,
    pub(crate) buttons: Vec<(&'static str, &'static str, StripAction)>,
}

fn line(text: String, buttons: Vec<(&'static str, &'static str, StripAction)>) -> Option<Line> {
    Some(Line { text, buttons })
}

/// "1 agent turn finishes", "2 agent turns finish".
fn turns_finish(n: usize) -> String {
    if n == 1 {
        "1 agent turn finishes".to_owned()
    } else {
        format!("{n} agent turns finish")
    }
}

/// What the strip says, if anything, given the updater's view and how many
/// agents are in a turn.
pub(crate) fn line_for(view: &UpdateView, state: &StripState, working: usize) -> Option<Line> {
    let ok = ("update-ok", "OK", StripAction::Dismiss);
    if let Some(notice) = &view.notice {
        return line(notice.clone(), vec![ok]);
    }
    if let Some(error) = &state.error {
        return line(format!("The update could not start: {error}"), vec![ok]);
    }
    if state.waiting {
        return line(
            format!(
                "Connecto restarts to update when {}.",
                turns_finish(working)
            ),
            vec![
                ("update-restart-now", "Restart now", StripAction::RestartNow),
                ("update-cancel", "Cancel", StripAction::CancelWait),
            ],
        );
    }
    let later = |version: &str| state.later.as_deref() == Some(version);
    match &view.status {
        // It installs itself: the strip says so, and what holds it back.
        UpdateStatus::Ready { version } if view.auto_install => line(
            match &view.held {
                Some(held) => {
                    format!("Connecto {version} installs itself when nothing holds it back. {held}")
                }
                None => format!(
                    "Connecto {version} installs itself and restarts in a moment. Agents carry on afterwards."
                ),
            },
            vec![("update-install", "Update and restart", StripAction::Install)],
        ),
        UpdateStatus::Ready { version } if !later(version) => line(
            format!("Connecto {version} is ready. Your terminals are restored after the restart."),
            vec![
                ("update-install", "Update and restart", StripAction::Install),
                ("update-later", "Later", StripAction::Later),
            ],
        ),
        UpdateStatus::Manual { version, why } if !later(version) => line(
            format!("Connecto {version} is available. {why}"),
            vec![
                ("update-download", "Download", StripAction::OpenSite),
                ("update-later", "Later", StripAction::Later),
            ],
        ),
        _ if !view.asked || state.closed_asked => None,
        UpdateStatus::Checking => line("Checking for updates…".to_owned(), vec![]),
        UpdateStatus::Downloading { version } => {
            line(format!("Downloading Connecto {version}…"), vec![])
        }
        UpdateStatus::UpToDate => line(
            format!("Connecto {} is the newest version.", view.current),
            vec![ok],
        ),
        UpdateStatus::Failed(why) => line(format!("Could not check for updates: {why}"), vec![ok]),
        UpdateStatus::Unavailable(why) => line(why.clone(), vec![ok]),
        _ => None,
    }
}

/// Draw the strip at the top of `area`; the room left below it.
pub(crate) fn draw(ui: &mut Ui, area: Rect, line: &Line, out: &mut Out<StripAction>) -> Rect {
    let strip = Rect::from_min_size(area.min, vec2(area.width(), HEIGHT));
    let painter = ui.painter_at(strip);
    painter.rect_filled(strip, 0.0, theme::RAISED_BG);
    painter.hline(
        strip.x_range(),
        strip.max.y - 0.5,
        Stroke::new(1.0, theme::BORDER),
    );
    let inner = strip.shrink2(vec2(14.0, 4.0));
    ui.scope_builder(egui::UiBuilder::new().max_rect(inner), |ui| {
        ui.horizontal_centered(|ui| {
            out.say(ui, RichText::new(&line.text).size(12.5).color(theme::TEXT));
            for (key, label, action) in &line.buttons {
                if out.button(ui, key, label, true) {
                    out.actions.push(*action);
                }
            }
        });
    });
    Rect::from_min_max(
        egui::pos2(area.min.x, strip.max.y.min(area.max.y)),
        area.max,
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn view(status: UpdateStatus, asked: bool) -> UpdateView {
        UpdateView {
            current: "0.1.8".into(),
            status,
            auto_check: true,
            asked,
            notice: None,
            ..UpdateView::default()
        }
    }

    #[test]
    fn with_automatic_installs_a_ready_update_says_it_installs_itself_and_what_holds_it() {
        let state = StripState::default();
        let mut ready = view(
            UpdateStatus::Ready {
                version: "0.1.9".into(),
            },
            false,
        );
        ready.auto_install = true;
        let line = line_for(&ready, &state, 0).unwrap();
        assert!(line.text.contains("installs itself"), "{}", line.text);
        assert_eq!(actions(&line), [StripAction::Install], "no Later");
        ready.held = Some("A file is being transferred.".into());
        let line = line_for(&ready, &state, 0).unwrap();
        assert!(line.text.contains("A file is being transferred."));
    }

    fn actions(line: &Line) -> Vec<StripAction> {
        line.buttons.iter().map(|b| b.2).collect()
    }

    #[test]
    fn a_ready_update_asks_update_and_restart_or_later() {
        let state = StripState::default();
        let ready = view(
            UpdateStatus::Ready {
                version: "0.1.9".into(),
            },
            false,
        );
        let line = line_for(&ready, &state, 0).unwrap();
        assert!(line.text.contains("0.1.9"));
        assert_eq!(actions(&line), [StripAction::Install, StripAction::Later]);
        // Later: gone until a newer version.
        let later = StripState {
            later: Some("0.1.9".into()),
            ..StripState::default()
        };
        assert_eq!(line_for(&ready, &later, 0), None);
        let newer = view(
            UpdateStatus::Ready {
                version: "0.1.10".into(),
            },
            false,
        );
        assert!(line_for(&newer, &later, 0).is_some());
    }

    #[test]
    fn while_waiting_it_counts_the_running_turns() {
        let state = StripState {
            waiting: true,
            ..StripState::default()
        };
        let ready = view(
            UpdateStatus::Ready {
                version: "0.1.9".into(),
            },
            false,
        );
        let line = line_for(&ready, &state, 2).unwrap();
        assert_eq!(
            line.text,
            "Connecto restarts to update when 2 agent turns finish."
        );
        assert_eq!(
            actions(&line),
            [StripAction::RestartNow, StripAction::CancelWait]
        );
        assert_eq!(
            line_for(&ready, &state, 1).unwrap().text,
            "Connecto restarts to update when 1 agent turn finishes."
        );
    }

    #[test]
    fn quiet_results_show_only_for_a_check_that_was_asked_for() {
        let state = StripState::default();
        for status in [
            UpdateStatus::UpToDate,
            UpdateStatus::Checking,
            UpdateStatus::Failed("offline".into()),
        ] {
            assert_eq!(
                line_for(&view(status.clone(), false), &state, 0),
                None,
                "{status:?}"
            );
            assert!(
                line_for(&view(status.clone(), true), &state, 0).is_some(),
                "{status:?}"
            );
        }
        let closed = StripState {
            closed_asked: true,
            ..StripState::default()
        };
        assert_eq!(
            line_for(&view(UpdateStatus::UpToDate, true), &closed, 0),
            None
        );
        let up = line_for(&view(UpdateStatus::UpToDate, true), &state, 0).unwrap();
        assert!(up.text.contains("0.1.8"));
    }

    #[test]
    fn a_copy_that_cannot_update_itself_offers_the_download_page() {
        let manual = view(
            UpdateStatus::Manual {
                version: "0.1.9".into(),
                why: "Move Connecto to Applications.".into(),
            },
            false,
        );
        let line = line_for(&manual, &StripState::default(), 0).unwrap();
        assert!(line.text.contains("Move Connecto"));
        assert_eq!(actions(&line), [StripAction::OpenSite, StripAction::Later]);
    }

    #[test]
    fn a_notice_comes_first() {
        let mut v = view(
            UpdateStatus::Ready {
                version: "0.1.9".into(),
            },
            false,
        );
        v.notice = Some("Connecto 0.1.9 did not start".into());
        let line = line_for(&v, &StripState::default(), 0).unwrap();
        assert!(line.text.contains("did not start"));
        assert_eq!(actions(&line), [StripAction::Dismiss]);
    }
}
