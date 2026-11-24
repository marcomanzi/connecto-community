//! Test seam: replays raw egui events at the normal input entry, so the gate
//! and PTY paths run unchanged. Compiled only with `ui-test-driver`.
//!
//! A script line is `<ms from start> click <x> <y>`, `<ms> press <name>`,
//! `<ms> text <text>`, `<ms> key <Enter|Down|Escape>`, `<ms> cmdclick <x> <y>`
//! (a click with Cmd held, Ctrl off macOS; the key stays held for the rest of
//! the run).
//!
//! `press` clicks a widget that named itself with [`note`], wherever this
//! frame's layout put it. It waits until the widget exists, so a later step
//! cannot overtake it. Prefer it to `click`: runs with fixed coordinates had
//! missed Send clicks, all at 2x, for a cause not measured.

use std::collections::{HashMap, VecDeque};
use std::path::Path;
use std::time::{Duration, Instant};

use crossbeam_channel::Sender;
use egui::{Context, Event, Id, Key, Modifiers, PointerButton, Pos2, RawInput, Rect};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Press {
    Hover,
    Down,
    Up,
}

#[derive(Debug, Clone)]
enum Step {
    Events(Vec<Event>),
    Point(Pos2, Press),
    /// A step of a click made with the command key held.
    CommandPoint(Pos2, Press),
    Target(String, Press),
    /// A command by name, for sheets no click reaches (`settings`).
    Command(String),
}

/// Where the named widgets were last drawn.
#[derive(Clone, Default)]
struct Targets(HashMap<String, Rect>);

fn targets_id() -> Id {
    Id::NULL.with("connecto-driver-targets")
}

/// Give a widget a name scripts can `press`.
pub(crate) fn note(ctx: &Context, name: impl Into<String>, rect: Rect) {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Targets>(targets_id())
            .0
            .insert(name.into(), rect);
    });
}

fn target(ctx: &Context, name: &str) -> Option<Rect> {
    ctx.data_mut(|data| {
        data.get_temp_mut_or_default::<Targets>(targets_id())
            .0
            .get(name)
            .copied()
    })
}

/// The named widget under a point, if any.
fn hit(ctx: &Context, pos: Pos2) -> Option<(String, Rect)> {
    ctx.data_mut(|data| {
        let targets = data.get_temp_mut_or_default::<Targets>(targets_id());
        let mut found: Vec<_> = targets
            .0
            .iter()
            .filter(|(_, rect)| rect.contains(pos))
            .map(|(name, rect)| (name.clone(), *rect))
            .collect();
        found.sort_by(|a, b| a.0.cmp(&b.0));
        found.into_iter().next()
    })
}

/// What a press hit, so a run shows the display scale and where the widget was.
fn press_line(name: &str, rect: Rect, scale: f32, pass: u64) -> String {
    format!(
        r#"{{"press":"{name}","scale":{scale},"x":{},"y":{},"w":{},"h":{},"pass":{pass}}}"#,
        rect.min.x,
        rect.min.y,
        rect.width(),
        rect.height()
    )
}

/// The end of a press, with how many real window events arrived during it.
fn release_line(pass: u64, foreign: usize) -> String {
    format!(r#"{{"release":true,"pass":{pass},"foreign":{foreign}}}"#)
}

/// Pointer and focus events from the real window. The driver adds its own
/// after this count, so these can only come from outside the script.
fn foreign_events(input: &RawInput) -> usize {
    input
        .events
        .iter()
        .filter(|event| {
            matches!(
                event,
                Event::PointerMoved(_)
                    | Event::PointerGone
                    | Event::PointerButton { .. }
                    | Event::WindowFocused(_)
            )
        })
        .count()
}

fn press_events(press: Press, pos: Pos2) -> Vec<Event> {
    let button = |pressed| Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    };
    match press {
        Press::Hover => vec![Event::PointerMoved(pos)],
        Press::Down => vec![button(true)],
        Press::Up => vec![button(false)],
    }
}

/// A script being played.
pub(crate) struct Driver {
    start: Instant,
    steps: VecDeque<(Duration, Step)>,
    log: Option<Sender<String>>,
    foreign: usize,
    commands: Vec<String>,
}

fn key_event(key: Key, pressed: bool) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}

fn parse_line(line: &str, steps: &mut VecDeque<(Duration, Step)>) -> Option<()> {
    let (time, rest) = line.split_once(' ')?;
    let at: u64 = time.parse().ok()?;
    let (verb, argument) = rest.split_once(' ').unwrap_or((rest, ""));
    let when = |offset: u64| Duration::from_millis(at + offset);
    match verb {
        "click" => {
            let (x, y) = argument.split_once(' ')?;
            let pos = Pos2::new(x.parse().ok()?, y.parse().ok()?);
            // One step per frame: a press and release in one frame is no click.
            for (offset, press) in [(0, Press::Hover), (40, Press::Down), (80, Press::Up)] {
                steps.push_back((when(offset), Step::Point(pos, press)));
            }
        }
        "cmdclick" => {
            let (x, y) = argument.split_once(' ')?;
            let pos = Pos2::new(x.parse().ok()?, y.parse().ok()?);
            for (offset, press) in [(0, Press::Hover), (40, Press::Down), (80, Press::Up)] {
                steps.push_back((when(offset), Step::CommandPoint(pos, press)));
            }
        }
        "press" if !argument.is_empty() => {
            for (offset, press) in [(0, Press::Hover), (40, Press::Down), (80, Press::Up)] {
                steps.push_back((when(offset), Step::Target(argument.to_owned(), press)));
            }
        }
        "scroll" => {
            let delta: f32 = argument.parse().ok()?;
            let event = Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: egui::vec2(0.0, delta),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Wheel,
                modifiers: Modifiers::NONE,
            };
            steps.push_back((when(0), Step::Events(vec![event])));
        }
        "cmd" if !argument.is_empty() => {
            steps.push_back((when(0), Step::Command(argument.to_owned())));
        }
        "text" => {
            let event = Event::Text(argument.to_owned());
            steps.push_back((when(0), Step::Events(vec![event])));
        }
        "key" => {
            let key = match argument {
                "Enter" => Key::Enter,
                "Down" => Key::ArrowDown,
                "Escape" => Key::Escape,
                // The Settings shortcut, for looking at that sheet by script.
                "CmdComma" => {
                    let down = Event::Key {
                        key: Key::Comma,
                        physical_key: None,
                        pressed: true,
                        repeat: false,
                        modifiers: Modifiers {
                            mac_cmd: true,
                            command: true,
                            ..Modifiers::NONE
                        },
                    };
                    let up = Event::Key {
                        key: Key::Comma,
                        physical_key: None,
                        pressed: false,
                        repeat: false,
                        modifiers: Modifiers {
                            mac_cmd: true,
                            command: true,
                            ..Modifiers::NONE
                        },
                    };
                    steps.push_back((when(0), Step::Events(vec![down, up])));
                    return Some(());
                }
                _ => return None,
            };
            let events = vec![key_event(key, true), key_event(key, false)];
            steps.push_back((when(0), Step::Events(events)));
        }
        _ => return None,
    }
    Some(())
}

impl Driver {
    /// Load a script, or `None` when it cannot be read or parsed.
    pub(crate) fn load(path: &Path) -> Option<Self> {
        let text = std::fs::read_to_string(path).ok()?;
        let mut steps = VecDeque::new();
        for line in text.lines().filter(|line| !line.trim().is_empty()) {
            parse_line(line, &mut steps)?;
        }
        Some(Self {
            start: Instant::now(),
            steps,
            log: None,
            foreign: 0,
            commands: Vec::new(),
        })
    }

    /// Send a line per press to the metrics file.
    pub(crate) fn log_to(&mut self, lines: Sender<String>) {
        self.log = Some(lines);
    }

    /// The commands scripts asked for since the last call.
    pub(crate) fn take_commands(&mut self) -> Vec<String> {
        std::mem::take(&mut self.commands)
    }

    /// Steps not played yet.
    pub(crate) fn remaining(&self) -> usize {
        self.steps.len()
    }

    /// Record one step of a click.
    fn note_press(&mut self, ctx: &Context, press: Press, name: &str, rect: Rect) {
        let pass = ctx.cumulative_pass_nr();
        let line = match press {
            Press::Hover => {
                self.foreign = 0;
                return;
            }
            Press::Down => press_line(name, rect, ctx.pixels_per_point(), pass),
            Press::Up => release_line(pass, self.foreign),
        };
        if let Some(log) = &self.log {
            // A full queue loses one line and never blocks a frame.
            let _ = log.try_send(line);
        }
    }

    /// Add what is due. True while steps remain.
    pub(crate) fn feed(&mut self, ctx: &Context, input: &mut RawInput) -> bool {
        self.foreign += foreign_events(input);
        let elapsed = self.start.elapsed();
        let due = self
            .steps
            .front()
            .filter(|(at, _)| *at <= elapsed)
            .map(|(_, step)| step.clone());
        let events = match due {
            None => None,
            Some(Step::Events(events)) => Some(events),
            Some(Step::Point(pos, press)) => {
                let (name, rect) = hit(ctx, pos)
                    .unwrap_or_else(|| ("nothing".to_owned(), Rect::from_min_max(pos, pos)));
                self.note_press(ctx, press, &format!("click:{name}"), rect);
                Some(press_events(press, pos))
            }
            Some(Step::CommandPoint(pos, press)) => {
                let held = Modifiers {
                    command: true,
                    mac_cmd: cfg!(target_os = "macos"),
                    ctrl: !cfg!(target_os = "macos"),
                    ..Modifiers::NONE
                };
                let mut events = vec![Event::ModifiersChanged(held)];
                events.extend(press_events(press, pos));
                Some(events)
            }
            Some(Step::Command(name)) => {
                self.commands.push(name);
                Some(Vec::new())
            }
            Some(Step::Target(name, press)) => target(ctx, &name).map(|rect| {
                self.note_press(ctx, press, &name, rect);
                press_events(press, rect.center())
            }),
        };
        if let Some(events) = events {
            self.steps.pop_front();
            input.events.extend(events);
        }
        !self.steps.is_empty()
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_click_takes_three_frames_and_a_bad_line_is_refused() {
        let mut steps = VecDeque::new();
        assert!(parse_line("100 click 10 20", &mut steps).is_some());
        assert_eq!(steps.len(), 3);
        assert!(parse_line("100 jump", &mut steps).is_none());
        assert!(parse_line("x text hi", &mut steps).is_none());
    }

    fn driver(script: &str) -> Driver {
        let mut steps = VecDeque::new();
        for line in script.lines() {
            parse_line(line, &mut steps).unwrap();
        }
        Driver {
            start: Instant::now(),
            steps,
            log: None,
            foreign: 0,
            commands: Vec::new(),
        }
    }

    #[test]
    fn a_press_waits_for_its_target_then_clicks_where_it_is_now() {
        let ctx = Context::default();
        let mut driver = driver("0 press send\n0 text after");
        driver
            .steps
            .iter_mut()
            .for_each(|(at, _)| *at = Duration::ZERO);
        let mut input = RawInput::default();
        assert!(driver.feed(&ctx, &mut input));
        assert!(input.events.is_empty(), "nothing to press yet");
        assert_eq!(driver.remaining(), 4);

        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), egui::vec2(20.0, 10.0));
        note(&ctx, "send", rect);
        driver.feed(&ctx, &mut input);
        assert_eq!(
            input.events,
            vec![Event::PointerMoved(Pos2::new(20.0, 25.0))]
        );
        assert_eq!(driver.remaining(), 3);
    }

    #[test]
    fn a_press_logs_its_scale_and_rect_once() {
        let ctx = Context::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut driver = driver("0 press send");
        driver
            .steps
            .iter_mut()
            .for_each(|(at, _)| *at = Duration::ZERO);
        driver.log_to(tx);
        let rect = Rect::from_min_size(Pos2::new(10.0, 20.0), egui::vec2(20.0, 10.0));
        note(&ctx, "send", rect);
        for _ in 0..3 {
            driver.feed(&ctx, &mut RawInput::default());
        }
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![
                r#"{"press":"send","scale":1,"x":10,"y":20,"w":20,"h":10,"pass":0}"#.to_owned(),
                r#"{"release":true,"pass":0,"foreign":0}"#.to_owned(),
            ]
        );
    }

    #[test]
    fn a_fixed_click_names_what_it_hit_and_counts_real_events_during_it() {
        let ctx = Context::default();
        let (tx, rx) = crossbeam_channel::unbounded();
        let mut driver = driver("0 click 15 25");
        driver
            .steps
            .iter_mut()
            .for_each(|(at, _)| *at = Duration::ZERO);
        driver.log_to(tx);
        note(
            &ctx,
            "send",
            Rect::from_min_size(Pos2::new(10.0, 20.0), egui::vec2(20.0, 10.0)),
        );
        driver.feed(&ctx, &mut RawInput::default());
        let mut real = RawInput::default();
        real.events.push(Event::PointerMoved(Pos2::new(900.0, 9.0)));
        driver.feed(&ctx, &mut real);
        assert_eq!(real.events.len(), 2, "the real event stays in the input");
        driver.feed(&ctx, &mut RawInput::default());
        assert_eq!(
            rx.try_iter().collect::<Vec<_>>(),
            vec![
                r#"{"press":"click:send","scale":1,"x":10,"y":20,"w":20,"h":10,"pass":0}"#
                    .to_owned(),
                r#"{"release":true,"pass":0,"foreign":1}"#.to_owned(),
            ]
        );
    }

    #[test]
    fn a_press_needs_a_name() {
        assert!(parse_line("0 press ", &mut VecDeque::new()).is_none());
    }
}
