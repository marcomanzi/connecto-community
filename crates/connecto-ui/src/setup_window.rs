//! The first-launch window: a welcome, a short living checklist, a way in. It
//! draws what [`connecto_core::SetupView`] says and passes the few answers back.
//! Every word on the default path is one a newcomer knows.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Instant;

use connecto_core::{ProblemAction, Screen, SetupControl, SetupView, StepState};
use egui::text::{LayoutJob, TextFormat};
use egui::{Align2, Color32, FontId, Label, Layout, RichText, Sense, Stroke, Ui, pos2, vec2};

use crate::{icons, platform, theme};

const ICON_PNG: &[u8] = include_bytes!("../../../assets/brand/icon-256.png");
const COLUMN: f32 = 460.0;
/// Where an agent's text starts in a choice card, and how far it may run.
const CARD_TEXT_X: f32 = 58.0;
const CARD_TEXT_RIGHT: f32 = 18.0;
const FOUND_AT_SIZE: f32 = 12.0;
/// The most the details of a problem may take before they scroll, so the
/// buttons above them stay on a 640-point window.
const DETAILS_MAX_HEIGHT: f32 = 110.0;
/// The width of every button on the problem screen.
const ACTION_WIDTH: f32 = 260.0;

/// What the user did on a frame.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Act {
    Start,
    Retry,
    ContinueWithout,
    OpenPage(String),
    PickFolder,
    Sample,
    Skip,
    Toggle(String),
    Confirm,
    SignIn,
}

pub(crate) struct SetupApp {
    control: Arc<dyn SetupControl>,
    details: bool,
    started: Instant,
    reduce_motion: bool,
    logo: Option<egui::TextureHandle>,
    picker: fn() -> Option<std::path::PathBuf>,
    /// The home folder, shown as `~` in paths.
    home: Option<PathBuf>,
    #[cfg(feature = "frame-capture")]
    shots: Option<crate::shots::ScreenshotSink>,
}

impl SetupApp {
    pub(crate) fn new(control: Arc<dyn SetupControl>) -> Self {
        Self {
            control,
            details: false,
            started: Instant::now(),
            reduce_motion: platform::reduce_motion(),
            logo: None,
            picker: crate::picker::pick_folder,
            home: platform::home_dir(),
            #[cfg(feature = "frame-capture")]
            shots: std::env::var_os("CONNECTO_SHOT_DIR")
                .and_then(|dir| crate::shots::ScreenshotSink::to_dir(dir.into()).ok()),
        }
    }

    fn logo(&mut self, ctx: &egui::Context) -> Option<egui::TextureId> {
        if self.logo.is_none()
            && let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG)
        {
            let image = egui::ColorImage::from_rgba_unmultiplied(
                [icon.width as usize, icon.height as usize],
                &icon.rgba,
            );
            self.logo = Some(ctx.load_texture("setup-logo", image, egui::TextureOptions::LINEAR));
        }
        self.logo.as_ref().map(egui::TextureHandle::id)
    }

    /// Draw the whole window and say what was clicked.
    pub(crate) fn draw(&mut self, ui: &mut Ui, view: &SetupView) -> Vec<Act> {
        let time = self.started.elapsed().as_secs_f64();
        let motion = !self.reduce_motion;
        let mut acts = Vec::new();
        let rect = ui.max_rect();
        ui.painter().rect_filled(rect, 0.0, theme::WINDOW_BG);
        let logo = self.logo(ui.ctx());
        let left = rect.center().x - COLUMN / 2.0;
        let mut column = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(egui::Rect::from_min_size(
                    pos2(left, rect.min.y + 52.0),
                    vec2(COLUMN, rect.height() - 72.0),
                ))
                .layout(egui::Layout::top_down(egui::Align::Center)),
        );
        let ui = &mut column;
        if let Some(id) = logo {
            // A slow breathing glow while something is going on.
            let breathe = if motion && view.screen == Screen::Working {
                0.82 + 0.18 * ((time * 1.6).sin() as f32 * 0.5 + 0.5)
            } else {
                1.0
            };
            ui.add(
                egui::Image::new((id, vec2(72.0, 72.0)))
                    .tint(Color32::from_white_alpha((255.0 * breathe) as u8)),
            );
        }
        ui.add_space(14.0);
        match view.screen {
            Screen::Welcome => welcome(ui, &mut acts),
            Screen::Choose => choose(ui, view, self.home.as_deref(), &mut acts),
            Screen::Working => working(ui, view, time, motion),
            Screen::Problem => problem(ui, view, &mut self.details, &mut acts),
            Screen::Ready => ready(ui, view, &mut acts),
        }
        acts
    }
}

fn title(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(28.0).strong().color(theme::TEXT));
}

fn body(ui: &mut Ui, text: &str) {
    ui.label(RichText::new(text).size(15.0).color(theme::TEXT_MUTED));
}

fn welcome(ui: &mut Ui, acts: &mut Vec<Act>) {
    title(ui, "Welcome to Connecto");
    ui.add_space(8.0);
    body(
        ui,
        "Run Claude Code and Codex side by side, in real terminals, and let them work together as a team.",
    );
    ui.add_space(30.0);
    if ui
        .add_sized([240.0, 42.0], theme::primary("Set up Connecto"))
        .clicked()
    {
        acts.push(Act::Start);
    }
    ui.add_space(14.0);
    body(
        ui,
        "It takes about a minute, and you don't have to do anything.",
    );
}

fn choose(ui: &mut Ui, view: &SetupView, home: Option<&Path>, acts: &mut Vec<Act>) {
    title(ui, "Which agents do you want?");
    ui.add_space(8.0);
    body(ui, "Tap to choose. You can change this later.");
    ui.add_space(18.0);
    for choice in &view.choices {
        // A found copy gets a fourth line, so the card is taller and its lines move up.
        let (height, lift) = if choice.found_at.is_some() {
            (100.0, -8.0)
        } else {
            (84.0, 0.0)
        };
        let (row, response) = ui.allocate_exact_size(vec2(COLUMN, height), Sense::click());
        let line = |offset: f32| row.center().y + lift + offset;
        let on = choice.selected;
        ui.painter().rect_filled(
            row,
            12.0,
            if on {
                theme::TEAL_DEEP
            } else {
                theme::PANEL_BG
            },
        );
        ui.painter().rect_stroke(
            row,
            12.0,
            Stroke::new(1.5, if on { theme::CYAN } else { theme::BORDER }),
            egui::StrokeKind::Inside,
        );
        let box_center = pos2(row.min.x + 28.0, row.center().y);
        ui.painter().circle_stroke(
            box_center,
            11.0,
            Stroke::new(1.5, if on { theme::CYAN } else { theme::FAINT }),
        );
        if on {
            icons::check(ui.painter(), box_center, theme::CYAN);
        }
        ui.painter().text(
            pos2(row.min.x + CARD_TEXT_X, line(-14.0)),
            Align2::LEFT_CENTER,
            &choice.name,
            FontId::proportional(18.0),
            theme::TEXT,
        );
        ui.painter().text(
            pos2(row.min.x + CARD_TEXT_X, line(8.0)),
            Align2::LEFT_CENTER,
            &choice.about,
            FontId::proportional(13.0),
            theme::TEXT_MUTED,
        );
        ui.painter().text(
            pos2(row.min.x + CARD_TEXT_X, line(26.0)),
            Align2::LEFT_CENTER,
            if choice.found {
                format!("Found on {}", platform::THIS_COMPUTER)
            } else {
                format!("Not on {} yet", platform::THIS_COMPUTER)
            },
            FontId::proportional(12.0),
            if choice.found {
                theme::GREEN
            } else {
                theme::FAINT
            },
        );
        // Where the copy that would be used lives. The full path is on hover.
        if let Some(path) = &choice.found_at {
            let max_width = COLUMN - CARD_TEXT_X - CARD_TEXT_RIGHT;
            let text = found_at_text(path, home, max_width, &|s| {
                ui.painter()
                    .layout_no_wrap(
                        s.to_owned(),
                        FontId::proportional(FOUND_AT_SIZE),
                        theme::TEXT_MUTED,
                    )
                    .size()
                    .x
            });
            let rect = ui.painter().text(
                pos2(row.min.x + CARD_TEXT_X, line(44.0)),
                Align2::LEFT_CENTER,
                text,
                FontId::proportional(FOUND_AT_SIZE),
                theme::TEXT_MUTED,
            );
            let id = ui.make_persistent_id(("found-at", &choice.key));
            ui.interact(rect, id, Sense::hover())
                .on_hover_text(without_control(&path.display().to_string()));
        }
        if response.clicked() {
            acts.push(Act::Toggle(choice.key.clone()));
        }
        // A chosen agent that is not here will be installed: say what and from where.
        if let (true, false, Some(note)) = (on, choice.found, &choice.install_note) {
            ui.add_space(4.0);
            ui.allocate_ui(vec2(COLUMN, 0.0), |ui| {
                ui.set_width(COLUMN);
                ui.add(
                    egui::Label::new(
                        RichText::new(note.as_str())
                            .size(12.5)
                            .color(theme::TEXT_MUTED),
                    )
                    .wrap(),
                );
            });
        }
        ui.add_space(10.0);
    }
    ui.add_space(14.0);
    let any = view.choices.iter().any(|c| c.selected);
    let label = if any {
        "Continue"
    } else {
        "Continue with just a terminal"
    };
    if ui.add_sized([280.0, 44.0], theme::primary(label)).clicked() {
        acts.push(Act::Confirm);
    }
}

fn working(ui: &mut Ui, view: &SetupView, time: f64, motion: bool) {
    title(ui, "Getting everything ready");
    ui.add_space(8.0);
    body(
        ui,
        "This takes about a minute. You don't need to do anything.",
    );
    ui.add_space(20.0);
    let (bar, _) = ui.allocate_exact_size(vec2(COLUMN, 8.0), Sense::hover());
    ui.painter().rect_filled(bar, 4.0, theme::RAISED_BG);
    let mut filled = bar;
    filled.set_width((bar.width() * view.fraction.clamp(0.02, 1.0)).max(8.0));
    ui.painter().rect_filled(filled, 4.0, theme::CYAN);
    ui.add_space(22.0);
    for step in &view.steps {
        let (row, _) = ui.allocate_exact_size(vec2(COLUMN, 30.0), Sense::hover());
        let center = pos2(row.min.x + 14.0, row.center().y);
        match step.state {
            StepState::Done => icons::check(ui.painter(), center, theme::GREEN),
            StepState::Working => {
                let angle = if motion { time as f32 * 5.0 } else { 0.0 };
                let arc: Vec<_> = (0..=14)
                    .map(|i| {
                        let a = angle + i as f32 * 0.28;
                        center + vec2(a.cos(), a.sin()) * 7.0
                    })
                    .collect();
                ui.painter()
                    .add(egui::Shape::line(arc, Stroke::new(2.0, theme::CYAN)));
            }
            StepState::Waiting => {
                ui.painter()
                    .circle_stroke(center, 6.0, Stroke::new(1.5, theme::FAINT));
            }
            StepState::Skipped => {
                ui.painter().line_segment(
                    [center - vec2(5.0, 0.0), center + vec2(5.0, 0.0)],
                    Stroke::new(1.5, theme::FAINT),
                );
            }
        }
        let color = match step.state {
            StepState::Waiting | StepState::Skipped => theme::FAINT,
            _ => theme::TEXT,
        };
        let text = match &step.note {
            Some(note) => format!("{}  ({note})", step.label),
            None => step.label.clone(),
        };
        ui.painter().text(
            pos2(row.min.x + 36.0, row.center().y),
            Align2::LEFT_CENTER,
            text,
            FontId::proportional(15.0),
            color,
        );
    }
}

fn problem(ui: &mut Ui, view: &SetupView, details: &mut bool, acts: &mut Vec<Act>) {
    let Some(problem) = &view.problem else {
        return;
    };
    title(ui, "One thing needs you");
    ui.add_space(12.0);
    ui.label(RichText::new(&problem.what).size(16.0).color(theme::TEXT));
    ui.add_space(6.0);
    body(ui, &problem.next);
    ui.add_space(22.0);
    if ui
        .add_sized([ACTION_WIDTH, 40.0], theme::primary("Try again"))
        .clicked()
    {
        acts.push(Act::Retry);
    }
    for action in &problem.actions {
        ui.add_space(8.0);
        match action {
            ProblemAction::OpenPage { label, url } => {
                if ui
                    .add_sized([ACTION_WIDTH, 36.0], theme::button(label))
                    .clicked()
                {
                    acts.push(Act::OpenPage(url.clone()));
                }
            }
            ProblemAction::ContinueWithout { label } => {
                if ui
                    .add_sized([ACTION_WIDTH, 36.0], theme::button(label))
                    .clicked()
                {
                    acts.push(Act::ContinueWithout);
                }
            }
        }
    }
    ui.add_space(16.0);
    if ui
        .link(
            RichText::new(if *details {
                "Hide details"
            } else {
                "Show details"
            })
            .size(13.0),
        )
        .clicked()
    {
        *details = !*details;
    }
    if *details {
        ui.add_space(6.0);
        // The candidates tried, one per line. Long paths break anywhere so
        // they wrap inside the column, and a long list scrolls instead of
        // pushing the buttons off the window.
        egui::ScrollArea::vertical()
            .id_salt("setup-details")
            .max_height(DETAILS_MAX_HEIGHT)
            .auto_shrink([false, true])
            .show(ui, |ui| {
                ui.set_width(COLUMN);
                ui.with_layout(Layout::top_down(egui::Align::LEFT), |ui| {
                    let mut job = LayoutJob::single_section(
                        problem.details.clone(),
                        TextFormat {
                            font_id: FontId::monospace(12.0),
                            color: theme::TEXT_MUTED,
                            ..TextFormat::default()
                        },
                    );
                    job.wrap.break_anywhere = true;
                    ui.add(Label::new(job).wrap());
                });
            });
    }
}

/// Replace the middle of a path with `…` so it fits `max_width`. The start
/// and the end (the file name) are kept. `width_of` measures a string.
fn middle_fit(text: &str, max_width: f32, width_of: &dyn Fn(&str) -> f32) -> String {
    if width_of(text) <= max_width {
        return text.to_owned();
    }
    let chars: Vec<char> = text.chars().collect();
    for keep in (0..chars.len()).rev() {
        let head = keep.div_ceil(2);
        let tail = keep / 2;
        let candidate: String = chars[..head]
            .iter()
            .copied()
            .chain(std::iter::once('…'))
            .chain(chars[chars.len() - tail..].iter().copied())
            .collect();
        if width_of(&candidate) <= max_width {
            return candidate;
        }
    }
    "…".to_owned()
}

/// A path as a person reads it: the home folder is `~`.
fn tilde_path(path: &Path, home: Option<&Path>) -> String {
    match home.and_then(|home| path.strip_prefix(home).ok()) {
        Some(rest) if rest.as_os_str().is_empty() => "~".to_owned(),
        Some(rest) => format!("~{}{}", std::path::MAIN_SEPARATOR, rest.display()),
        None => path.display().to_string(),
    }
}

/// The line under a found agent: "Found at ~/.local/bin/claude", with the
/// path cut in the middle when it would not fit `max_width`.
fn found_at_text(
    path: &Path,
    home: Option<&Path>,
    max_width: f32,
    width_of: &dyn Fn(&str) -> f32,
) -> String {
    const PREFIX: &str = "Found at ";
    let room = (max_width - width_of(PREFIX)).max(0.0);
    let shown = without_control(&tilde_path(path, home));
    format!("{PREFIX}{}", middle_fit(&shown, room, width_of))
}

/// `text` with control characters left out, so a file name cannot break a line.
fn without_control(text: &str) -> String {
    text.chars().filter(|c| !c.is_control()).collect()
}

fn ready(ui: &mut Ui, view: &SetupView, acts: &mut Vec<Act>) {
    title(ui, "You're all set");
    ui.add_space(8.0);
    body(ui, "Connecto is ready. Here is where your agents stand.");
    ui.add_space(18.0);
    for agent in &view.agents {
        let (row, _) = ui.allocate_exact_size(vec2(COLUMN, 44.0), Sense::hover());
        ui.painter().rect_filled(row, 8.0, theme::PANEL_BG);
        let dot = pos2(row.min.x + 18.0, row.center().y);
        if agent.ready {
            icons::check(ui.painter(), dot, theme::GREEN);
        } else {
            ui.painter()
                .circle_stroke(dot, 6.0, Stroke::new(1.5, theme::FAINT));
        }
        ui.painter().text(
            pos2(row.min.x + 40.0, row.center().y - 8.0),
            Align2::LEFT_CENTER,
            &agent.name,
            FontId::proportional(15.0),
            theme::TEXT,
        );
        let status = match (agent.ready, agent.signed_in) {
            (true, Some(true)) => "Ready · signed in".to_owned(),
            (true, Some(false)) => "Ready · you'll sign in the first time you open it".to_owned(),
            (true, None) => "Ready".to_owned(),
            (false, _) => format!(
                "{}. Install it any time and Connecto will pick it up.",
                agent.status
            ),
        };
        ui.painter().text(
            pos2(row.min.x + 40.0, row.center().y + 10.0),
            Align2::LEFT_CENTER,
            status,
            FontId::proportional(12.0),
            theme::TEXT_MUTED,
        );
        ui.add_space(6.0);
    }
    ui.add_space(20.0);
    if view.signin_wanted {
        if ui
            .add_sized([300.0, 44.0], theme::primary("Sign in"))
            .clicked()
        {
            acts.push(Act::SignIn);
        }
        ui.add_space(6.0);
        body(
            ui,
            "Connecto opens each agent and takes you to its sign-in.",
        );
        ui.add_space(10.0);
        if ui
            .add_sized([300.0, 38.0], theme::button("Pick a folder to work in"))
            .clicked()
        {
            acts.push(Act::PickFolder);
        }
    } else if ui
        .add_sized([280.0, 44.0], theme::primary("Pick a folder to work in"))
        .clicked()
    {
        acts.push(Act::PickFolder);
    }
    if view.sample_available {
        ui.add_space(10.0);
        if ui
            .add_sized([280.0, 38.0], theme::button("Try it with a sample project"))
            .clicked()
        {
            acts.push(Act::Sample);
        }
    }
    ui.add_space(14.0);
    if ui
        .link(RichText::new("I'll do it later").size(13.0))
        .clicked()
    {
        acts.push(Act::Skip);
    }
}

impl eframe::App for SetupApp {
    fn ui(&mut self, ui: &mut Ui, _frame: &mut eframe::Frame) {
        let view = self.control.view();
        let acts = self.draw(ui, &view);
        for act in acts {
            match act {
                Act::Start => self.control.start(),
                Act::Toggle(key) => self.control.toggle_agent(&key),
                Act::Confirm => self.control.confirm_choice(),
                Act::SignIn => self.control.sign_in(),
                Act::Retry => self.control.retry(),
                Act::ContinueWithout => self.control.continue_without(),
                Act::OpenPage(url) => platform::open_web_page(ui.ctx(), &url),
                Act::PickFolder => {
                    if let Some(path) = (self.picker)() {
                        self.control.open_folder(path);
                    }
                }
                Act::Sample => self.control.open_sample(),
                Act::Skip => self.control.skip(),
            }
        }
        // A scripted run (screenshots): the two buttons are pressed on a timer.
        #[cfg(feature = "ui-test-driver")]
        if std::env::var_os("CONNECTO_SETUP_SCRIPT").is_some() {
            let at = self.started.elapsed().as_secs_f32();
            match view.screen {
                Screen::Welcome if at > 3.0 => self.control.start(),
                Screen::Choose if at > 6.0 => self.control.confirm_choice(),
                Screen::Ready
                    if at > 12.0
                        && std::env::var("CONNECTO_SETUP_SCRIPT").as_deref() == Ok("skip") =>
                {
                    self.control.skip();
                }
                _ => {}
            }
        }
        #[cfg(feature = "frame-capture")]
        if let Some(shots) = &mut self.shots {
            shots.request_if_due(ui.ctx());
        }
        if self.control.done() {
            ui.ctx().send_viewport_cmd(egui::ViewportCommand::Close);
        }
        // Moving things need frames; a still screen needs none.
        let moving = view.screen == Screen::Working && !self.reduce_motion;
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(if moving {
                33
            } else {
                200
            }));
    }

    fn on_exit(&mut self) {
        if !self.control.done() {
            self.control.closed();
        }
    }
}

/// Open the first-launch window on `control`. Returns when it closes.
pub fn run_setup(control: Arc<dyn SetupControl>) -> eframe::Result {
    let mut viewport = egui::ViewportBuilder::default()
        .with_title("Connecto")
        .with_inner_size([720.0, 640.0])
        .with_resizable(false);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        viewport = viewport.with_icon(icon);
    }
    eframe::run_native(
        "Connecto",
        eframe::NativeOptions {
            viewport,
            ..eframe::NativeOptions::default()
        },
        Box::new(move |creation| {
            crate::fonts::install(&creation.egui_ctx);
            theme::apply(&creation.egui_ctx);
            Ok(Box::new(SetupApp::new(control)))
        }),
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use connecto_core::{ReadyAgent, SetupProblem, SetupStep};
    use egui::Rect;

    struct Nothing;
    impl SetupControl for Nothing {
        fn view(&self) -> Arc<SetupView> {
            Arc::default()
        }
        fn start(&self) {}
        fn toggle_agent(&self, _key: &str) {}
        fn confirm_choice(&self) {}
        fn sign_in(&self) {}
        fn retry(&self) {}
        fn continue_without(&self) {}
        fn open_folder(&self, _path: std::path::PathBuf) {}
        fn open_sample(&self) {}
        fn skip(&self) {}
        fn closed(&self) {}
        fn done(&self) -> bool {
            false
        }
    }

    fn frame(app: &mut SetupApp, ctx: &egui::Context, view: &SetupView) -> Vec<Act> {
        let mut acts = Vec::new();
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    pos2(0.0, 0.0),
                    vec2(720.0, 640.0),
                )),
                ..egui::RawInput::default()
            },
            |ui| {
                acts = app.draw(ui, view);
            },
        );
        output.textures_delta.clear();
        acts
    }

    #[test]
    fn every_screen_draws_and_asks_for_nothing_by_itself() {
        let mut app = SetupApp::new(Arc::new(Nothing));
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let step = |label: &str, state| SetupStep {
            label: label.to_owned(),
            state,
            note: None,
        };
        let views = [
            SetupView {
                screen: Screen::Welcome,
                ..SetupView::default()
            },
            SetupView {
                screen: Screen::Choose,
                choices: vec![connecto_core::AgentChoice {
                    key: "claude".to_owned(),
                    name: "Claude Code".to_owned(),
                    about: "Anthropic's coding agent.".to_owned(),
                    selected: true,
                    found: true,
                    found_at: None,
                    install_note: None,
                }],
                ..SetupView::default()
            },
            SetupView {
                screen: Screen::Working,
                steps: vec![
                    step("Getting Claude Code ready", StepState::Done),
                    step("Getting Codex ready", StepState::Working),
                    step("Preparing your workspace", StepState::Waiting),
                ],
                fraction: 0.5,
                ..SetupView::default()
            },
            SetupView {
                screen: Screen::Problem,
                problem: Some(SetupProblem {
                    what: "There is not enough free space on this Mac.".to_owned(),
                    next: "Free up some space, then try again.".to_owned(),
                    details: "1 GB free, 2 GB needed.".to_owned(),
                    actions: vec![ProblemAction::ContinueWithout {
                        label: "Continue without Codex".to_owned(),
                    }],
                }),
                ..SetupView::default()
            },
            SetupView {
                screen: Screen::Ready,
                sample_available: true,
                agents: vec![ReadyAgent {
                    name: "Claude Code".to_owned(),
                    status: "Ready".to_owned(),
                    ready: true,
                    signed_in: Some(false),
                }],
                ..SetupView::default()
            },
        ];
        for view in &views {
            assert!(frame(&mut app, &ctx, view).is_empty(), "{:?}", view.screen);
        }
    }

    /// Every text drawn in one frame, with where it lands.
    fn drawn_text(
        app: &mut SetupApp,
        ctx: &egui::Context,
        view: &SetupView,
    ) -> Vec<(String, Rect)> {
        let mut output = ctx.run_ui(
            egui::RawInput {
                screen_rect: Some(egui::Rect::from_min_size(
                    pos2(0.0, 0.0),
                    vec2(720.0, 640.0),
                )),
                ..egui::RawInput::default()
            },
            |ui| {
                app.draw(ui, view);
            },
        );
        output.textures_delta.clear();
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect_text(&clipped.shape, &mut texts);
        }
        texts
    }

    fn collect_text(shape: &egui::Shape, texts: &mut Vec<(String, Rect)>) {
        match shape {
            egui::Shape::Text(text) => {
                texts.push((text.galley.text().to_owned(), text.visual_bounding_rect()));
            }
            egui::Shape::Vec(shapes) => {
                for shape in shapes {
                    collect_text(shape, texts);
                }
            }
            _ => {}
        }
    }

    fn choice(found_at: Option<&str>) -> connecto_core::AgentChoice {
        connecto_core::AgentChoice {
            key: "claude".to_owned(),
            name: "Claude Code".to_owned(),
            about: "Anthropic's coding agent.".to_owned(),
            selected: true,
            found: true,
            found_at: found_at.map(PathBuf::from),
            install_note: None,
        }
    }

    fn choose_view(found_at: Option<&str>) -> SetupView {
        SetupView {
            screen: Screen::Choose,
            choices: vec![choice(found_at)],
            ..SetupView::default()
        }
    }

    fn app_at_home(home: &str) -> SetupApp {
        let mut app = SetupApp::new(Arc::new(Nothing));
        app.home = Some(PathBuf::from(home));
        app
    }

    fn found_lines(texts: &[(String, Rect)]) -> Vec<&(String, Rect)> {
        texts
            .iter()
            .filter(|(text, _)| text.starts_with("Found at"))
            .collect()
    }

    #[test]
    fn a_found_copy_shows_its_path_with_home_as_tilde() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = app_at_home("/Users/ana");
        let sep = std::path::MAIN_SEPARATOR;
        let view = choose_view(Some(&format!("/Users/ana{sep}.local{sep}bin{sep}claude")));
        let texts = drawn_text(&mut app, &ctx, &view);
        let lines = found_lines(&texts);
        assert_eq!(lines.len(), 1, "{texts:?}");
        assert_eq!(
            lines[0].0,
            format!("Found at ~{sep}.local{sep}bin{sep}claude")
        );
    }

    #[test]
    fn a_path_outside_home_is_shown_whole_and_no_line_without_a_path() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = app_at_home("/Users/ana");
        let texts = drawn_text(&mut app, &ctx, &choose_view(Some("/opt/tools/claude")));
        assert_eq!(found_lines(&texts)[0].0, "Found at /opt/tools/claude");

        let texts = drawn_text(&mut app, &ctx, &choose_view(None));
        assert!(found_lines(&texts).is_empty(), "{texts:?}");
        let found_here = format!("Found on {}", platform::THIS_COMPUTER);
        assert!(texts.iter().any(|(text, _)| *text == found_here));
    }

    #[test]
    fn a_long_path_is_cut_in_the_middle_to_fit_the_card() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = app_at_home("/Users/ana");
        let long = format!("/opt/{}/claude", "deeply-nested-folder/".repeat(12));
        let texts = drawn_text(&mut app, &ctx, &choose_view(Some(&long)));
        let lines = found_lines(&texts);
        assert_eq!(lines.len(), 1, "{texts:?}");
        let (text, rect) = lines[0];
        assert!(text.contains('…'), "{text}");
        assert!(text.ends_with("/claude"), "{text}");
        // The card's text room: the column less the name's indent and a margin.
        assert!(
            rect.width() <= COLUMN - CARD_TEXT_X - CARD_TEXT_RIGHT + 1.0,
            "{rect:?}"
        );
    }

    #[test]
    fn path_helpers_abbreviate_home_and_cut_the_middle() {
        let sep = std::path::MAIN_SEPARATOR;
        let home = Path::new("/Users/ana");
        assert_eq!(tilde_path(home, Some(home)), "~");
        assert_eq!(
            tilde_path(Path::new("/Users/anabel/bin"), Some(home)),
            "/Users/anabel/bin",
            "a sibling folder that only shares the start is not home"
        );
        assert_eq!(tilde_path(Path::new("/x/y"), None), "/x/y");

        let width = |s: &str| s.chars().count() as f32 * 7.0;
        assert_eq!(middle_fit("short", 100.0, &width), "short");
        let cut = middle_fit(&format!("/start{sep}middle{sep}end-claude"), 105.0, &width);
        assert!(cut.chars().count() <= 15, "{cut}");
        assert!(
            cut.contains('…') && cut.starts_with("/s") && cut.ends_with("claude"),
            "{cut}"
        );
        assert!(width(&cut) <= 105.0);
    }

    #[test]
    fn the_candidates_scroll_and_the_buttons_stay_on_a_640_window() {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut app = SetupApp::new(Arc::new(Nothing));
        app.details = true;
        let candidates: String = (0..12)
            .map(|i| format!("/Users/ana/.nvm/versions/node/v{i}/bin/claude: it is a launcher script (an npm install), not Claude Code's own program\n"))
            .collect();
        let view = SetupView {
            screen: Screen::Problem,
            problem: Some(SetupProblem {
                what: "Connecto found Claude Code at /Users/ana/bin/claude, but cannot use this copy of it: it is a script or wrapper, not the Claude Code program.".to_owned(),
                next: "Install Claude Code from its official page, or fix the install named below, then try again.".to_owned(),
                details: candidates,
                actions: vec![
                    ProblemAction::OpenPage {
                        label: "Get Claude Code yourself".to_owned(),
                        url: "https://claude.ai".to_owned(),
                    },
                    ProblemAction::ContinueWithout {
                        label: "Continue without Claude Code".to_owned(),
                    },
                ],
            }),
            ..SetupView::default()
        };
        let texts = drawn_text(&mut app, &ctx, &view);
        for label in [
            "Try again",
            "Get Claude Code yourself",
            "Continue without Claude Code",
        ] {
            let rect = texts
                .iter()
                .find(|(text, _)| text == label)
                .map(|(_, rect)| *rect);
            assert!(
                rect.is_some_and(|rect| rect.max.y <= 640.0),
                "{label} is not on the window: {rect:?}"
            );
        }
        assert!(
            texts
                .iter()
                .any(|(text, _)| text.contains("launcher script"))
        );
    }
}
