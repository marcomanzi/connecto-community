//! The status dot: one small shape that says what a terminal or a project is
//! doing (project-tree-ui R19).

use std::time::Duration;

use connecto_core::TermActivity;
use egui::{Painter, Pos2, Stroke};

use crate::icons;
use crate::theme;

/// How often the window repaints for the pulse while something works.
pub(crate) const PULSE_FRAME: Duration = Duration::from_millis(50);
/// One pulse, in seconds.
const PULSE_PERIOD: f64 = 1.6;
const RADIUS: f32 = 4.5;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dot {
    NeedsUser,
    NeedsLogin,
    Failed,
    Working,
    /// Cut short by the app closing, and waiting to be told to go on.
    Interrupted,
    /// A turn ended since the user last looked.
    Unseen,
    Done,
    Idle,
    Starting,
    /// A plain shell that is running; nothing is known about what it is doing.
    Plain,
    /// Exited, stopped, or never started.
    Hollow,
}

impl Dot {
    /// The terminal's dot. `unseen` only shows on a terminal at rest: a
    /// working one is already louder.
    pub(crate) const fn of(state: TermActivity, unseen: bool) -> Self {
        match state {
            TermActivity::NeedsUser => Self::NeedsUser,
            TermActivity::NeedsLogin => Self::NeedsLogin,
            TermActivity::Failed => Self::Failed,
            TermActivity::Working => Self::Working,
            TermActivity::Interrupted => Self::Interrupted,
            TermActivity::Idle | TermActivity::Done if unseen => Self::Unseen,
            TermActivity::Done => Self::Done,
            TermActivity::Idle => Self::Idle,
            TermActivity::Starting => Self::Starting,
            TermActivity::Plain => Self::Plain,
            TermActivity::Exited | TermActivity::NotStarted => Self::Hollow,
        }
    }

    /// How urgent: a project shows its terminals' highest.
    pub(crate) const fn rank(self) -> u8 {
        match self {
            Self::NeedsUser => 9,
            Self::NeedsLogin => 8,
            Self::Failed => 7,
            Self::Interrupted => 6,
            Self::Working => 5,
            Self::Unseen => 4,
            Self::Done | Self::Idle => 3,
            Self::Starting | Self::Plain => 2,
            Self::Hollow => 1,
        }
    }

    /// The most urgent of several. The first wins a tie.
    pub(crate) fn most_urgent(dots: impl IntoIterator<Item = Self>) -> Option<Self> {
        dots.into_iter()
            .reduce(|best, dot| if dot.rank() > best.rank() { dot } else { best })
    }

    pub(crate) const fn is_working(self) -> bool {
        matches!(self, Self::Working)
    }

    /// Whether the user is wanted.
    pub(crate) const fn needs_me(self) -> bool {
        matches!(self, Self::NeedsUser | Self::NeedsLogin)
    }

    /// Words for the state, for the chip and for screen readers.
    pub(crate) const fn word(self) -> &'static str {
        match self {
            Self::NeedsUser => "waiting for you",
            Self::NeedsLogin => "needs login",
            Self::Failed => "failed",
            Self::Working => "working",
            Self::Interrupted => "interrupted",
            Self::Unseen => "finished",
            Self::Done => "done",
            Self::Idle => "idle",
            Self::Starting => "starting",
            Self::Plain => "shell",
            Self::Hollow => "stopped",
        }
    }

    /// Paint at `center`. `time` is egui's clock, for the pulse.
    pub(crate) fn paint(self, painter: &Painter, center: Pos2, time: f64) {
        match self {
            Self::Working => {
                let phase = (time % PULSE_PERIOD / PULSE_PERIOD) as f32;
                let fade = (1.0 - phase).clamp(0.0, 1.0);
                painter.circle_stroke(
                    center,
                    RADIUS + 6.0 * phase,
                    Stroke::new(1.5, theme::WORKING.gamma_multiply(0.55 * fade)),
                );
                painter.circle_filled(center, RADIUS, theme::WORKING);
            }
            Self::NeedsUser | Self::NeedsLogin => {
                painter.circle_filled(center, RADIUS + 2.0, theme::AMBER.gamma_multiply(0.22));
                painter.circle_filled(center, RADIUS, theme::AMBER);
            }
            // Amber like the states that want the user, but a ring with a
            // core and no halo: it waits, it does not ask.
            Self::Interrupted => {
                painter.circle_stroke(center, RADIUS - 0.5, Stroke::new(1.5, theme::AMBER));
                painter.circle_filled(center, RADIUS - 2.5, theme::AMBER);
            }
            Self::Idle => {
                painter.circle_filled(center, RADIUS, theme::IDLE);
            }
            Self::Unseen => {
                painter.circle_filled(center, RADIUS + 2.0, theme::GREEN.gamma_multiply(0.25));
                painter.circle_filled(center, RADIUS, theme::GREEN);
                painter.circle_stroke(
                    center,
                    RADIUS + 3.5,
                    Stroke::new(1.0, theme::GREEN.gamma_multiply(0.5)),
                );
            }
            Self::Done => icons::check(painter, center, theme::GREEN),
            Self::Failed => {
                painter.circle_filled(center, RADIUS, theme::CORAL);
            }
            Self::Starting => {
                painter.circle_stroke(
                    center,
                    RADIUS - 0.5,
                    Stroke::new(1.5, theme::CYAN.gamma_multiply(0.8)),
                );
            }
            // A small solid square: running, but not an agent with a state.
            Self::Plain => {
                let half = RADIUS - 1.0;
                painter.rect_filled(
                    egui::Rect::from_center_size(center, egui::vec2(half * 2.0, half * 2.0)),
                    1.5,
                    theme::IDLE,
                );
            }
            Self::Hollow => {
                painter.circle_stroke(center, RADIUS - 0.5, Stroke::new(1.5, theme::FAINT));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_running_shell_has_its_own_quiet_dot_that_is_never_unseen() {
        assert_eq!(Dot::of(TermActivity::Plain, false), Dot::Plain);
        assert_eq!(Dot::of(TermActivity::Plain, true), Dot::Plain);
        assert_eq!(Dot::Plain.word(), "shell");
        assert!(Dot::Plain.rank() < Dot::Idle.rank());
        assert!(!Dot::Plain.needs_me() && !Dot::Plain.is_working());
    }

    #[test]
    fn a_project_shows_its_most_urgent_terminal() {
        use TermActivity as T;
        let order = [
            T::NeedsUser,
            T::NeedsLogin,
            T::Failed,
            T::Interrupted,
            T::Working,
            T::Idle,
            T::Starting,
            T::Exited,
        ];
        // Every state beats all the ones after it, in the order of R15.
        for (index, state) in order.iter().enumerate() {
            for later in &order[index + 1..] {
                let best = Dot::most_urgent([Dot::of(*later, false), Dot::of(*state, false)]);
                assert_eq!(
                    best,
                    Some(Dot::of(*state, false)),
                    "{state:?} over {later:?}"
                );
            }
        }
        // A finished-unseen terminal beats idle and below, and loses to working.
        let unseen = Dot::of(T::Idle, true);
        assert_eq!(unseen, Dot::Unseen);
        assert!(unseen.rank() > Dot::of(T::Idle, false).rank());
        assert!(unseen.rank() > Dot::of(T::Done, false).rank());
        assert!(unseen.rank() < Dot::Working.rank());
        assert_eq!(Dot::of(T::Working, true), Dot::Working);
        assert_eq!(Dot::most_urgent([]), None);
    }
}
