//! The first-launch setup, as the setup window sees it. The app does the work
//! (`connecto-app`); the window (`connecto-ui`) only draws a [`SetupView`] and
//! passes the user's few answers back through [`SetupControl`]. Nothing here
//! uses a word a newcomer would not: the labels are written for them.

use std::path::PathBuf;
use std::sync::Arc;

/// Which screen the window shows.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Screen {
    /// A first launch: what Connecto is, and one button.
    Welcome,
    /// Which agents the user wants.
    Choose,
    /// The checklist is running (also what shows before the first update).
    #[default]
    Working,
    /// Something needs the user; see [`SetupView::problem`].
    Problem,
    /// Done, with a clear next step.
    Ready,
}

/// Where one line of the checklist stands.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum StepState {
    Waiting,
    Working,
    Done,
    /// Left out, and why is in the note ("Codex isn't on this Mac").
    Skipped,
}

/// One line of the checklist, in a person's words.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SetupStep {
    pub label: String,
    pub state: StepState,
    pub note: Option<String>,
}

/// What the user can do about a problem, besides trying again.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ProblemAction {
    /// Open a web page (where to get the tool).
    OpenPage { label: String, url: String },
    /// Go on without the step that failed.
    ContinueWithout { label: String },
}

/// What went wrong, and what to do.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SetupProblem {
    pub what: String,
    pub next: String,
    /// The raw error and paths, behind "Show details".
    pub details: String,
    pub actions: Vec<ProblemAction>,
}

/// One card of the question "which agents do you want?".
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentChoice {
    /// What the app calls it internally (`claude`, `codex`).
    pub key: String,
    pub name: String,
    /// One line for a newcomer.
    pub about: String,
    pub selected: bool,
    /// Whether it is already on this Mac.
    pub found: bool,
    /// Where the copy that would be used is on this Mac, when it is found.
    pub found_at: Option<PathBuf>,
    /// For one that is not on this Mac and will be installed: what, from
    /// where, in plain words.
    pub install_note: Option<String>,
}

/// One agent on the ready screen.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReadyAgent {
    pub name: String,
    /// "Ready", or why not.
    pub status: String,
    pub ready: bool,
    /// Whether the user is signed in on this Mac, if it could be told.
    pub signed_in: Option<bool>,
}

/// Everything the setup window draws.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct SetupView {
    pub screen: Screen,
    pub steps: Vec<SetupStep>,
    /// 0.0 to 1.0 over the checklist .
    pub fraction: f32,
    pub problem: Option<SetupProblem>,
    pub agents: Vec<ReadyAgent>,
    pub choices: Vec<AgentChoice>,
    /// A chosen agent is ready and the user is not signed in to it yet: the
    /// way in goes through its sign-in.
    pub signin_wanted: bool,
    /// A sample project can be made.
    pub sample_available: bool,
}

/// What the setup window can ask the app to do.
pub trait SetupControl: Send + Sync {
    fn view(&self) -> Arc<SetupView>;
    /// "Set up Connecto": go on to the question of which agents.
    fn start(&self);
    /// Tap an agent card: choose it, or not.
    fn toggle_agent(&self, key: &str);
    /// The choice is made: begin the checklist.
    fn confirm_choice(&self);
    /// Open Connecto Home with the chosen agents, so each starts its own
    /// sign-in there.
    fn sign_in(&self);
    /// Try the failed step again.
    fn retry(&self);
    /// Go on without the step that failed.
    fn continue_without(&self);
    /// The user picked a folder to work in: open the app on it.
    fn open_folder(&self, path: PathBuf);
    /// Make the sample project and open the app on it.
    fn open_sample(&self);
    /// Go into the app with nothing opened.
    fn skip(&self);
    /// The window was closed without finishing.
    fn closed(&self);
    /// The setup has ended: the window closes and the app goes on.
    fn done(&self) -> bool;
}
