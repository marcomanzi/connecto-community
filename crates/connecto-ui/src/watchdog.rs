//! A stall watchdog, on when metrics are.
//!
//! It runs on its own thread and writes each line as it happens, so a stall
//! that lasts until the process is killed is still on disk. Frame reports
//! cannot show that: they are written by frames. Only timings and window state
//! are recorded, never session content.

use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicU64, Ordering};
use std::thread;
use std::time::Duration;

use crossbeam_channel::Sender;

use crate::app::{WakeSlot, clock_us};

const POLL: Duration = Duration::from_millis(50);
const LIMIT_US: u64 = 200_000;
const REPEAT_US: u64 = 1_000_000;

const UNKNOWN: u8 = 0;

fn encode(value: Option<bool>) -> u8 {
    value.map_or(UNKNOWN, |value| 1 + u8::from(value))
}

fn decode(value: u8) -> &'static str {
    match value {
        1 => "false",
        2 => "true",
        _ => "null",
    }
}

/// What the app last saw. The watchdog cannot ask the window itself. `App::logic`
/// reports the state and still runs while the window is hidden, unlike a frame,
/// but only when something asks for one, so each line says how old the state is.
#[derive(Default)]
pub(crate) struct WindowWatch {
    last_frame_us: AtomicU64,
    state_us: AtomicU64,
    visible: AtomicU8,
    focused: AtomicU8,
    occluded: AtomicU8,
    driver_steps: AtomicU64,
}

impl WindowWatch {
    pub(crate) fn frame_started(&self) {
        self.last_frame_us.store(clock_us(), Ordering::Relaxed);
    }

    pub(crate) fn window_state(
        &self,
        visible: Option<bool>,
        focused: Option<bool>,
        occluded: Option<bool>,
    ) {
        self.state_us.store(clock_us(), Ordering::Relaxed);
        self.visible.store(encode(visible), Ordering::Relaxed);
        self.focused.store(encode(focused), Ordering::Relaxed);
        self.occluded.store(encode(occluded), Ordering::Relaxed);
    }

    #[cfg(feature = "ui-test-driver")]
    pub(crate) fn set_driver_steps(&self, steps: usize) {
        self.driver_steps.store(steps as u64, Ordering::Relaxed);
    }
}

/// What a stall was waiting for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A session asked for a frame: real work is pending.
    Wake,
    /// Only a test script is waiting for frames.
    Driver,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Phase {
    Start,
    Ongoing,
    End,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct Report {
    kind: Kind,
    phase: Phase,
    waited_us: u64,
}

#[derive(Debug, Clone, Copy)]
struct Stall {
    kind: Kind,
    since_us: u64,
    next_report_us: u64,
}

/// Decides what to report from what it is shown. Pure, so tests need no clock.
#[derive(Default)]
struct Detector {
    current: Option<Stall>,
}

impl Detector {
    fn check(
        &mut self,
        now_us: u64,
        wake_since_us: u64,
        last_frame_us: u64,
        driver_steps: u64,
    ) -> Option<Report> {
        let late = |since: u64| since != 0 && now_us.saturating_sub(since) >= LIMIT_US;
        let found = if late(wake_since_us) {
            Some((Kind::Wake, wake_since_us))
        } else if driver_steps > 0 && late(last_frame_us) {
            Some((Kind::Driver, last_frame_us))
        } else {
            None
        };
        let report = |kind, phase, since: u64| {
            Some(Report {
                kind,
                phase,
                waited_us: now_us.saturating_sub(since),
            })
        };
        match (self.current, found) {
            (None, None) => None,
            (Some(stall), None) => {
                self.current = None;
                report(stall.kind, Phase::End, stall.since_us)
            }
            (Some(stall), Some((kind, since))) if stall.kind == kind && stall.since_us == since => {
                if now_us < stall.next_report_us {
                    return None;
                }
                self.current = Some(Stall {
                    next_report_us: now_us + REPEAT_US,
                    ..stall
                });
                report(kind, Phase::Ongoing, since)
            }
            (_, Some((kind, since))) => {
                self.current = Some(Stall {
                    kind,
                    since_us: since,
                    next_report_us: now_us + REPEAT_US,
                });
                report(kind, Phase::Start, since)
            }
        }
    }
}

fn line(report: Report, now_us: u64, watch: &WindowWatch) -> String {
    let kind = match report.kind {
        Kind::Wake => "wake",
        Kind::Driver => "driver",
    };
    let phase = match report.phase {
        Phase::Start => "start",
        Phase::Ongoing => "ongoing",
        Phase::End => "end",
    };
    let last_frame = watch.last_frame_us.load(Ordering::Relaxed);
    format!(
        r#"{{"stall":"{kind}","phase":"{phase}","at_ms":{},"waited_ms":{},"frame_age_ms":{},"state_age_ms":{},"visible":{},"focused":{},"occluded":{}}}"#,
        now_us / 1000,
        report.waited_us / 1000,
        now_us.saturating_sub(last_frame) / 1000,
        now_us.saturating_sub(watch.state_us.load(Ordering::Relaxed)) / 1000,
        decode(watch.visible.load(Ordering::Relaxed)),
        decode(watch.focused.load(Ordering::Relaxed)),
        decode(watch.occluded.load(Ordering::Relaxed)),
    )
}

/// Watch for the life of the process. A failed spawn only loses the measurement.
pub(crate) fn spawn(slot: Arc<WakeSlot>, watch: Arc<WindowWatch>, lines: Sender<String>) {
    let _ = thread::Builder::new()
        .name("stall-watchdog".into())
        .spawn(move || {
            let mut detector = Detector::default();
            loop {
                thread::sleep(POLL);
                let now = clock_us();
                let found = detector.check(
                    now,
                    slot.pending_since_us(),
                    watch.last_frame_us.load(Ordering::Relaxed),
                    watch.driver_steps.load(Ordering::Relaxed),
                );
                if let Some(report) = found {
                    // A full queue loses one line, and never blocks this thread.
                    let _ = lines.try_send(line(report, now, &watch));
                }
            }
        });
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const MS: u64 = 1000;

    #[test]
    fn a_line_reports_hidden_state_from_after_the_last_frame() {
        let watch = WindowWatch::default();
        watch.frame_started();
        watch.window_state(Some(false), Some(false), Some(true));
        let report = Report {
            kind: Kind::Wake,
            phase: Phase::Ongoing,
            waited_us: 5000 * MS,
        };
        let now = clock_us() + 5000 * MS;
        let text = line(report, now, &watch);
        assert!(text.contains(r#""occluded":true"#), "{text}");
        assert!(text.contains(r#""visible":false"#), "{text}");
        assert!(text.contains(r#""state_age_ms":"#), "{text}");
    }

    #[test]
    fn a_wake_answered_in_time_reports_nothing() {
        let mut detector = Detector::default();
        assert_eq!(detector.check(150 * MS, 10 * MS, 5 * MS, 0), None);
        assert_eq!(detector.check(300 * MS, 0, 290 * MS, 0), None);
    }

    #[test]
    fn a_late_wake_reports_start_repeats_while_it_lasts_then_ends() {
        let mut detector = Detector::default();
        let since = 1000 * MS;
        let start = detector.check(1200 * MS, since, 900 * MS, 0).unwrap();
        assert_eq!((start.kind, start.phase), (Kind::Wake, Phase::Start));
        assert_eq!(start.waited_us, 200 * MS);
        assert_eq!(detector.check(1500 * MS, since, 900 * MS, 0), None);
        let ongoing = detector.check(2200 * MS, since, 900 * MS, 0).unwrap();
        assert_eq!(ongoing.phase, Phase::Ongoing);
        let end = detector.check(2300 * MS, 0, 2290 * MS, 0).unwrap();
        assert_eq!((end.phase, end.waited_us), (Phase::End, 1300 * MS));
        assert_eq!(detector.check(2400 * MS, 0, 2390 * MS, 0), None);
    }

    #[test]
    fn an_idle_window_is_no_stall_unless_a_script_is_waiting() {
        let mut detector = Detector::default();
        assert_eq!(detector.check(9000 * MS, 0, 1000 * MS, 0), None);
        let report = detector.check(9000 * MS, 0, 1000 * MS, 3).unwrap();
        assert_eq!((report.kind, report.phase), (Kind::Driver, Phase::Start));
    }

    #[test]
    fn work_pending_outranks_a_waiting_script() {
        let mut detector = Detector::default();
        let report = detector.check(9000 * MS, 8000 * MS, 1000 * MS, 3).unwrap();
        assert_eq!(report.kind, Kind::Wake);
    }

    #[test]
    fn a_line_holds_timings_and_window_state_only() {
        let watch = WindowWatch::default();
        watch.frame_started();
        watch.window_state(Some(true), Some(false), None);
        let report = Report {
            kind: Kind::Wake,
            phase: Phase::Start,
            waited_us: 250 * MS,
        };
        let text = line(report, clock_us(), &watch);
        assert!(text.contains(r#""stall":"wake""#), "{text}");
        assert!(text.contains(r#""waited_ms":250"#), "{text}");
        assert!(text.contains(r#""visible":true,"focused":false,"occluded":null"#));
    }
}
