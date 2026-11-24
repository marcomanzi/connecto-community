//! The first launch, and a launch that finds Connecto's own copies of the agents
//! missing: the work that used to be a manual `connecto-app provision`.
//!
//! The work runs before the app's workspace exists, in a small window of its own
//! (`connecto_ui::run_setup`), so nothing reads a half-made data folder and no
//! terminal can start before its agent is set up. When it is done the app
//! starts itself again, and finds nothing left to do.
//!
//! What it does, in order, and what it is careful about:
//! 1. sets up each agent from the user's own install, through the very code
//!    `connecto-app provision` runs (`Store::provision`: every checksum and
//!    signature check stays), and only what is missing; an agent the user
//!    chooses to leave out is remembered ([`SKIPPED_FILE`]) and not offered again;
//! 2. makes the default utility folder (Connecto Home) if none is set;
//! 3. shows where the user stands: which agents are ready, who is signed in.
//!
//! It never asks for, reads or stores an account credential: signed in or not is
//! the tool's own answer, and signing in is the tool's own flow, started in the
//! first terminal.

use std::collections::VecDeque;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::Duration;

use connecto_core::{
    AgentChoice, ProblemAction, ReadyAgent, Screen, SetupControl, SetupProblem, SetupStep,
    SetupView, StepState,
};
use connecto_session::launch::{Layout, system_signin};
use connecto_store::{
    Signature, Store, StoreError, Tool, data_dir_override, describe_list, shown_path,
};

use crate::platform::{OFFERS_POWERSHELL_7, THE_COMPUTER, THIS_COMPUTER};

/// The PowerShell 7 card's key on the choice screen (Windows only).
pub const POWERSHELL_KEY: &str = "powershell";

/// The agents, in the order the first launch and Settings list them. Each
/// is optional; Pi, Antigravity and OpenCode are not chosen to begin with
/// (Pi needs Node.js and a model provider of the user's choosing).
pub const AGENTS: [Tool; 5] = [
    Tool::Claude,
    Tool::Codex,
    Tool::Pi,
    Tool::Antigravity,
    Tool::OpenCode,
];

/// The agents Settings lists: the same five.
pub const SETTINGS_AGENTS: [Tool; 5] = AGENTS;

/// What the first launch chooses to begin with, in the order of [`AGENTS`].
const FIRST_CHOICE: [bool; 5] = [true, true, false, false, false];

/// The agents a later launch sets up by itself when it finds them on this
/// computer and not in Connecto: the two Connecto keeps a copy of. Pi and
/// Antigravity run where they are installed, and are set up when chosen.
pub const COPIED_AGENTS: [Tool; 2] = [Tool::Claude, Tool::Codex];

/// Written in the data root when the first-launch setup has been completed.
pub const ONBOARDED: &str = ".onboarded";

/// The agents the user chose at the first launch, comma separated
/// (`claude,codex,pi,antigravity`; empty for none). Absent: Claude and Codex.
pub const AGENTS_FILE: &str = ".agents";

/// The agents chosen at the first launch. A new project no longer starts
/// with them (it opens its Tools settings instead); the tests read the choice
/// back through this.
#[cfg(test)]
#[must_use]
pub fn chosen_agents(data_root: &Path) -> Vec<Tool> {
    let Ok(text) = std::fs::read_to_string(data_root.join(AGENTS_FILE)) else {
        return COPIED_AGENTS.to_vec();
    };
    let off = crate::agents_host::inactive_agents(data_root);
    AGENTS
        .into_iter()
        .filter(|tool| {
            text.split(',').any(|word| word.trim() == tool.name()) && !off.contains(tool)
        })
        .collect()
}

/// The agents the user chose to leave out of setup ("Continue without ...",
/// "Skip for now"), comma separated, in the data folder. Setup does not offer
/// such an agent again by itself; it is cleared when the agent is set up, from
/// the setup or from Settings > Agents.
pub const SKIPPED_FILE: &str = ".setup-skipped";

/// The agents left out of setup by the user.
#[must_use]
pub fn skipped_tools(data_root: &Path) -> Vec<Tool> {
    let Ok(text) = std::fs::read_to_string(data_root.join(SKIPPED_FILE)) else {
        return Vec::new();
    };
    AGENTS
        .into_iter()
        .filter(|tool| text.split(',').any(|word| word.trim() == tool.name()))
        .collect()
}

fn write_skipped(data_root: &Path, list: &[Tool]) {
    let path = data_root.join(SKIPPED_FILE);
    if list.is_empty() {
        let _ = std::fs::remove_file(path);
        return;
    }
    let names: Vec<&str> = list.iter().map(|t| t.name()).collect();
    let _ = std::fs::create_dir_all(data_root);
    let _ = std::fs::write(path, names.join(","));
}

/// Remember that the user left `tool` out of setup.
pub fn remember_skipped(data_root: &Path, tool: Tool) {
    let mut list = skipped_tools(data_root);
    if !list.contains(&tool) {
        list.push(tool);
        write_skipped(data_root, &list);
    }
}

/// `tool` is set up: setup may look at it again if it ever goes missing.
pub fn forget_skipped(data_root: &Path, tool: Tool) {
    let mut list = skipped_tools(data_root);
    if list.contains(&tool) {
        list.retain(|t| *t != tool);
        write_skipped(data_root, &list);
    }
}

/// Where to get an agent that is not on this Mac.
fn page_of(tool: Tool) -> (&'static str, &'static str) {
    match tool {
        Tool::Claude => (
            "Claude Code",
            "https://docs.claude.com/en/docs/claude-code/setup",
        ),
        Tool::Codex => ("Codex", "https://github.com/openai/codex"),
        Tool::Pi => ("Pi", "https://pi.dev"),
        Tool::Antigravity => ("Antigravity CLI", "https://antigravity.google/docs/cli"),
        // The 1.x line Connecto's plugin is for: opencode.ai's own pages
        // describe OpenCode 2 now.
        Tool::OpenCode => ("OpenCode", "https://github.com/anomalyco/opencode"),
        Tool::Shell => ("Shell", "https://connecto.studio"),
    }
}

fn nice_name(tool: Tool) -> &'static str {
    page_of(tool).0
}

/// What kind of launch this is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// The first launch: welcome, checklist, a way in.
    FirstRun,
    /// Only the checklist: something the app needs is missing.
    Repair,
}

/// What this launch has to do before the app starts.
#[derive(Debug, Clone)]
pub struct Plan {
    pub mode: Mode,
    pub home: PathBuf,
    pub data_root: PathBuf,
}

fn has_existing_data(data_root: &Path) -> bool {
    ["workspace.json", "settings.json", "projects"]
        .iter()
        .any(|name| data_root.join(name).exists())
}

/// The agents that are on this computer and not yet set up, leaving out those
/// the user chose to skip.
fn to_set_up(store: &Store, home: &Path, skipped: &[Tool]) -> Vec<Tool> {
    COPIED_AGENTS
        .into_iter()
        .filter(|tool| {
            !skipped.contains(tool)
                && Store::find_installed(*tool, home).is_some()
                && store.newest(*tool).is_err()
        })
        .collect()
}

/// Decide, from a few cheap checks, whether this launch needs the setup.
#[must_use]
pub fn plan(home: &Path) -> Option<Plan> {
    plan_with(
        home,
        data_dir_override(),
        std::env::var_os("CONNECTO_SKIP_SETUP").is_some(),
    )
}

/// [`plan`], with the environment given.
#[must_use]
pub fn plan_with(home: &Path, data_dir: Option<std::ffi::OsString>, skip: bool) -> Option<Plan> {
    if skip {
        return None;
    }
    let data_root = connecto_store::data_root(data_dir, home);
    let marked = data_root.join(ONBOARDED).exists();
    let existing = has_existing_data(&data_root);
    if !marked && existing {
        // An install that predates the setup: nothing to welcome it to.
        let _ = std::fs::write(
            data_root.join(ONBOARDED),
            "set up before the first-launch setup\n",
        );
    }
    let marked = marked || existing;
    let store = Store::new(&data_root);
    let mode = if !marked {
        Mode::FirstRun
    } else if !to_set_up(&store, home, &skipped_tools(&data_root)).is_empty()
        || store.repair_wanted()
    {
        Mode::Repair
    } else {
        return None;
    };
    Some(Plan {
        mode,
        home: home.to_owned(),
        data_root,
    })
}

// ---------- the words ----------

/// An agent found on this Mac in a form Connecto cannot copy: where each copy
/// is and why it was not used, and a way on without it.
#[must_use]
pub fn explain_unusable(tool: Tool, found: &[(PathBuf, String)]) -> SetupProblem {
    let name = nice_name(tool);
    let details = describe_list(found, "\n");
    let what = match found.first() {
        Some((path, why)) => format!(
            "Connecto found {name} at {}, but cannot use this copy of it: {why}.",
            shown_path(path)
        ),
        None => {
            format!("Connecto found {name} on {THIS_COMPUTER} but cannot use this copy of it.")
        }
    };
    SetupProblem {
        what,
        next: format!(
            "Install {name} from its official page, or fix the install named below, then try again. You can also go on without {name} and set it up later in Settings."
        ),
        details,
        actions: vec![
            ProblemAction::OpenPage {
                label: format!("Get {name} yourself"),
                url: page_of(tool).1.to_owned(),
            },
            ProblemAction::ContinueWithout {
                label: format!("Continue without {name}"),
            },
        ],
    }
}

/// A store error, said for a person, with what to do.
fn explain(tool: Tool, err: &StoreError) -> SetupProblem {
    let name = nice_name(tool);
    let (what, next) = match err {
        StoreError::NoUsableCopy {
            tried,
            refused: false,
            ..
        } => return explain_unusable(tool, tried),
        StoreError::NoUsableCopy { refused: true, .. } => (
            format!(
                "Connecto could not confirm that the copies of {name} it found are the genuine ones, so none was used."
            ),
            format!("Reinstall {name} from its official page, then try again."),
        ),
        StoreError::Signature(_) => (
            format!(
                "Connecto could not confirm that this copy of {name} is the genuine one, so it was not used."
            ),
            format!("Reinstall {name} from its official page, then try again."),
        ),
        StoreError::Unchecked(_) => (
            format!(
                "Connecto could not finish checking this copy of {name}, so it was not used. The check was stopped or could not run, which can happen while {THE_COMPUTER} is busy."
            ),
            format!(
                "Wait a moment, then press Try again. If it keeps happening, close other heavy apps or restart {THE_COMPUTER}."
            ),
        ),
        StoreError::SourceChanged(_) | StoreError::CopyMismatch(_) => (
            format!(
                "{name} changed while Connecto was getting it ready (an update may be running)."
            ),
            "Wait a moment, then try again.".to_owned(),
        ),
        StoreError::Io { source, .. } if source.raw_os_error() == Some(28) => (
            format!("There is not enough free space on {THIS_COMPUTER}."),
            "Free up some space, then try again.".to_owned(),
        ),
        StoreError::UnknownVersion(_) | StoreError::BadSource(_) => (
            format!("Connecto could not read how {name} is installed."),
            format!("Reinstall {name}, then try again."),
        ),
        // A version Connecto cannot work with (OpenCode 2): it says which,
        // and how to install one that works.
        StoreError::Unsupported(why) => (why.clone(), "Then press Try again.".to_owned()),
        _ => (
            format!("Something went wrong while getting {name} ready."),
            "Try again. If it keeps happening, quit and reopen Connecto.".to_owned(),
        ),
    };
    SetupProblem {
        what,
        next,
        details: err.to_string(),
        actions: vec![ProblemAction::ContinueWithout {
            label: format!("Continue without {name}"),
        }],
    }
}

fn nothing_installed() -> SetupProblem {
    SetupProblem {
        what: format!(
            "Connecto works with Claude Code, Codex, Pi, Antigravity and OpenCode, and none of them is on {THIS_COMPUTER} yet."
        ),
        next: "Install one of them, then press Try again. You can also continue now and use a plain terminal."
            .to_owned(),
        details: format!("Looked in {}.", crate::platform::AGENT_PLACES),
        actions: vec![
            ProblemAction::OpenPage {
                label: "Get Claude Code".to_owned(),
                url: page_of(Tool::Claude).1.to_owned(),
            },
            ProblemAction::OpenPage {
                label: "Get Codex".to_owned(),
                url: page_of(Tool::Codex).1.to_owned(),
            },
            ProblemAction::OpenPage {
                label: "Get Pi".to_owned(),
                url: page_of(Tool::Pi).1.to_owned(),
            },
            ProblemAction::OpenPage {
                label: "Get Antigravity CLI".to_owned(),
                url: page_of(Tool::Antigravity).1.to_owned(),
            },
            ProblemAction::OpenPage {
                label: "Get OpenCode".to_owned(),
                url: page_of(Tool::OpenCode).1.to_owned(),
            },
            ProblemAction::ContinueWithout {
                label: "Continue with a plain terminal".to_owned(),
            },
        ],
    }
}

// ---------- the runner ----------

#[derive(Debug, Clone, PartialEq, Eq)]
enum Cmd {
    Start,
    Toggle(String),
    Confirm,
    SignIn,
    Retry,
    ContinueWithout,
    OpenFolder(PathBuf),
    OpenSample,
    Skip,
    Closed,
}

/// Where the app goes after the setup.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcome2 {
    /// A folder to open as a project.
    pub open: Option<PathBuf>,
    /// Open Connecto Home, where the chosen agents sign in.
    pub open_home: bool,
    /// The agents the user chose (`claude`, `codex`, `pi`, `antigravity`).
    pub agents: Vec<String>,
    /// A first message to send in it (the sample project's).
    pub first_prompt: Option<String>,
    /// The window was closed before the end: quit.
    pub quit: bool,
    /// Something was left out: do not set it up again by itself.
    pub skip_setup_next_start: bool,
}

struct Inner {
    view: Mutex<Arc<SetupView>>,
    cmds: Mutex<VecDeque<Cmd>>,
    wake: Condvar,
    outcome: Mutex<Option<Outcome2>>,
}

/// The setup, as the window and the worker thread share it.
#[derive(Clone)]
pub struct Runner {
    inner: Arc<Inner>,
}

impl Runner {
    #[must_use]
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                view: Mutex::new(Arc::new(SetupView::default())),
                cmds: Mutex::new(VecDeque::new()),
                wake: Condvar::new(),
                outcome: Mutex::new(None),
            }),
        }
    }

    fn push(&self, cmd: Cmd) {
        self.inner
            .cmds
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .push_back(cmd);
        self.inner.wake.notify_all();
    }

    /// Wait for a command the worker can use.
    fn next(&self, accept: impl Fn(&Cmd) -> bool) -> Cmd {
        let mut queue = self
            .inner
            .cmds
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        loop {
            while let Some(cmd) = queue.pop_front() {
                if cmd == Cmd::Closed || accept(&cmd) {
                    return cmd;
                }
            }
            queue = self
                .inner
                .wake
                .wait_timeout(queue, Duration::from_millis(200))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    fn update(&self, change: impl FnOnce(&mut SetupView)) {
        let mut slot = self
            .inner
            .view
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        let mut view = (**slot).clone();
        change(&mut view);
        *slot = Arc::new(view);
    }

    fn finish(&self, outcome: Outcome2) {
        *self
            .inner
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner) = Some(outcome);
    }

    /// What the worker decided, once it is done.
    #[must_use]
    pub fn outcome(&self) -> Option<Outcome2> {
        self.inner
            .outcome
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone()
    }
}

impl Default for Runner {
    fn default() -> Self {
        Self::new()
    }
}

impl SetupControl for Runner {
    fn view(&self) -> Arc<SetupView> {
        Arc::clone(
            &self
                .inner
                .view
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        )
    }
    fn start(&self) {
        self.push(Cmd::Start);
    }
    fn toggle_agent(&self, key: &str) {
        self.push(Cmd::Toggle(key.to_owned()));
    }
    fn confirm_choice(&self) {
        self.push(Cmd::Confirm);
    }
    fn sign_in(&self) {
        self.push(Cmd::SignIn);
    }
    fn retry(&self) {
        self.push(Cmd::Retry);
    }
    fn continue_without(&self) {
        self.push(Cmd::ContinueWithout);
    }
    fn open_folder(&self, path: PathBuf) {
        self.push(Cmd::OpenFolder(path));
    }
    fn open_sample(&self) {
        self.push(Cmd::OpenSample);
    }
    fn skip(&self) {
        self.push(Cmd::Skip);
    }
    fn closed(&self) {
        self.push(Cmd::Closed);
    }
    fn done(&self) -> bool {
        self.outcome().is_some()
    }
}

// ---------- the work ----------

fn step(label: &str) -> SetupStep {
    SetupStep {
        label: label.to_owned(),
        state: StepState::Waiting,
        note: None,
    }
}

fn set_step(runner: &Runner, index: usize, state: StepState, note: Option<&str>) {
    runner.update(|view| {
        if let Some(step) = view.steps.get_mut(index) {
            step.state = state;
            step.note = note.map(str::to_owned);
        }
    });
}

/// Where the default utility folder goes.
#[must_use]
pub fn default_home(user_home: &Path) -> PathBuf {
    user_home.join("Connecto")
}

/// Make Connecto Home at its default and save it, if none is saved. Through the
/// code Settings uses: the same check, the same layout, the same settings file.
fn prepare_workspace(plan: &Plan) -> Result<bool, String> {
    if connecto_workspace::saved_connecto_home(&plan.data_root).is_some() {
        return Ok(false);
    }
    let target = default_home(&plan.home);
    std::fs::create_dir_all(&target).map_err(|err| format!("{}: {err}", target.display()))?;
    let real = connecto_packages::home::validate(&target, &plan.data_root, &plan.home)?;
    connecto_packages::home::ensure_layout(&real)?;
    connecto_workspace::save_connecto_home(&plan.data_root, &real)?;
    Ok(true)
}

/// The entry of the newest stored copy of `tool`, if it verifies. A copy that
/// was changed or does not match its record is marked for repair at the next
/// start, as the launch path does, and is not used.
///
/// # Errors
/// When there is no stored copy, or it does not verify.
pub fn checked_stored_entry(store: &Store, tool: Tool) -> Result<PathBuf, StoreError> {
    let version = store.newest(tool)?;
    match store.verify(tool, &version) {
        Ok(installed) => Ok(installed.entry),
        Err(err @ (StoreError::Tampered(_) | StoreError::CopyMismatch(_))) => {
            store.mark_repair();
            Err(err)
        }
        Err(err) => Err(err),
    }
}

/// The entry of the verified stored copy of `tool`, if there is one.
pub fn stored_entry(store: &Store, tool: Tool) -> Option<PathBuf> {
    checked_stored_entry(store, tool).ok()
}

/// The program Connecto checks and runs for `tool`: the verified stored copy
/// when there is one, else the user's own install that discovery finds. Settings
/// and setup both use this, so they agree on what is checked. The second value
/// says what is wrong with the stored copy, when it was changed or does not match.
#[must_use]
/// Why the `tool` at `entry` cannot be used now, asked of the program itself
/// and not of the store's record: OpenCode upgrades itself in place, and an
/// OpenCode 2 cannot load Connecto's plugin. `None` for the other tools, and
/// when the version cannot be read.
pub fn live_unsupported(tool: Tool, entry: &Path, home: &Path) -> Option<String> {
    if tool != Tool::OpenCode {
        return None;
    }
    let version = crate::agent_install::version_of_install(tool, entry, home)?;
    connecto_store::opencode_unsupported(&version)
}

pub fn runnable_entry(
    data_root: &Path,
    home: &Path,
    tool: Tool,
) -> (Option<PathBuf>, Option<String>) {
    let store = Store::new(data_root);
    match checked_stored_entry(&store, tool) {
        Ok(entry) => (Some(entry), None),
        Err(err @ (StoreError::Tampered(_) | StoreError::CopyMismatch(_))) => (
            Store::find_installed(tool, home),
            Some(format!(
                "Connecto's private copy of {} did not pass its check ({err}). It will be set up again at the next start.",
                nice_name(tool)
            )),
        ),
        Err(_) => (Store::find_installed(tool, home), None),
    }
}

/// The agents' state for the last screen.
fn ready_agents(plan: &Plan, layout: &Layout) -> Vec<ReadyAgent> {
    let store = Store::new(&plan.data_root);
    AGENTS
        .into_iter()
        .map(|tool| {
            let name = nice_name(tool).to_owned();
            let entry = stored_entry(&store, tool);
            if let Some(why) = entry
                .as_deref()
                .and_then(|entry| live_unsupported(tool, entry, &plan.home))
            {
                return ReadyAgent {
                    name,
                    status: why,
                    ready: false,
                    signed_in: None,
                };
            }
            if let Some(entry) = entry {
                let signed_in = match system_signin(layout, tool, &entry) {
                    // Antigravity's and OpenCode's answers only say that it
                    // runs: the sign-in is not asked (the adapters'
                    // `parse_status`).
                    _ if matches!(tool, Tool::Antigravity | Tool::OpenCode) => None,
                    connecto_core::AuthState::LoggedIn { .. } => Some(true),
                    connecto_core::AuthState::LoggedOut => Some(false),
                    connecto_core::AuthState::Unknown => None,
                };
                ReadyAgent {
                    name,
                    status: "Ready".to_owned(),
                    ready: true,
                    signed_in,
                }
            } else {
                ReadyAgent {
                    name,
                    status: format!("Not on {THIS_COMPUTER} yet"),
                    ready: false,
                    signed_in: None,
                }
            }
        })
        .collect()
}

const SAMPLE_README: &str =
    "# Connecto sample\n\nA tiny folder to try Connecto with. `hello.py` prints a greeting.\n";
const SAMPLE_PY: &str = "def greet(name):\n    return f\"Hello, {name}!\"\n\n\nif __name__ == \"__main__\":\n    print(greet(\"world\"))\n";
/// The first message of the sample: harmless for any agent, changes nothing.
pub const SAMPLE_PROMPT: &str =
    "Say hello, and tell me in two sentences what is in this folder. Do not change any files.";

/// Make the small sample project (once) and give its path.
///
/// # Errors
/// When the folder cannot be made.
pub fn make_sample(user_home: &Path) -> Result<PathBuf, String> {
    let documents = user_home.join("Documents");
    let base = if documents.is_dir() {
        documents
    } else {
        user_home.to_owned()
    };
    let dir = base.join("Connecto Sample");
    std::fs::create_dir_all(&dir).map_err(|err| format!("{}: {err}", dir.display()))?;
    for (name, text) in [("README.md", SAMPLE_README), ("hello.py", SAMPLE_PY)] {
        let path = dir.join(name);
        if !path.exists() {
            std::fs::write(&path, text).map_err(|err| format!("{}: {err}", path.display()))?;
        }
    }
    Ok(dir)
}

/// Run the setup to its end. Blocks; the window runs beside it.
pub fn run<S: Signature>(
    plan: &Plan,
    runner: &Runner,
    signature: &S,
    installer: &dyn crate::agent_install::AgentInstaller,
) {
    let layout = Layout {
        data_root: plan.data_root.clone(),
        home: plan.home.clone(),
    };
    let store = Store::new(&plan.data_root);
    let first = plan.mode == Mode::FirstRun;
    // Which agents are wanted; a launch that is not the first wants the
    // copied ones, and Pi, Antigravity and OpenCode when they are set up
    // already.
    let mut selected = [true; AGENTS.len()];
    if !first {
        // Not again what the user left out, unless it is set up already (and
        // so may need repairing).
        let skipped = skipped_tools(&plan.data_root);
        for (tool, on) in AGENTS.into_iter().zip(&mut selected) {
            *on = if COPIED_AGENTS.contains(&tool) {
                !skipped.contains(&tool) || store.newest(tool).is_ok()
            } else {
                store.newest(tool).is_ok()
            };
        }
    }
    // Windows: PowerShell 7 for the terminals, offered at the first launch when
    // it is missing, chosen to begin with. Never required: Windows PowerShell
    // stays the fallback, and a failed install only says so.
    let offer_pwsh = first && OFFERS_POWERSHELL_7 && !crate::platform::has_powershell_7();
    let mut want_pwsh = offer_pwsh;
    if first {
        runner.update(|view| {
            view.screen = Screen::Welcome;
            view.sample_available = true;
        });
        if runner.next(|cmd| *cmd == Cmd::Start) == Cmd::Closed {
            runner.finish(Outcome2 {
                quit: true,
                ..Outcome2::default()
            });
            return;
        }
        // Which agents: Claude Code and Codex chosen to begin with; a tap
        // changes one. What is on this computer is looked up once.
        let found: Vec<Option<PathBuf>> = AGENTS
            .into_iter()
            .map(|tool| Store::find_installed(tool, &plan.home))
            .collect();
        let pi_note = if found[2].is_none() {
            first_launch_note(Tool::Pi, &installer.node_for_pi(&plan.home))
        } else {
            None
        };
        let cards = |chosen: &[bool; AGENTS.len()], pwsh: bool| -> Vec<AgentChoice> {
            let mut cards: Vec<AgentChoice> = AGENTS
                .into_iter()
                .zip(chosen)
                .zip(&found)
                .map(|((tool, selected), found_at)| {
                    let found = found_at.is_some();
                    AgentChoice {
                        key: tool.name().to_owned(),
                        name: nice_name(tool).to_owned(),
                        about: about(tool).to_owned(),
                        selected: *selected,
                        found,
                        found_at: found_at.clone(),
                        install_note: if found {
                            None
                        } else if tool == Tool::Pi {
                            pi_note.clone()
                        } else {
                            crate::agent_install::announcement(tool)
                        },
                    }
                })
                .collect();
            if offer_pwsh {
                cards.push(powershell_card(pwsh));
            }
            cards
        };
        let mut chosen = FIRST_CHOICE;
        runner.update(|view| {
            view.screen = Screen::Choose;
            view.choices = cards(&chosen, want_pwsh);
        });
        loop {
            match runner.next(|cmd| matches!(cmd, Cmd::Toggle(_) | Cmd::Confirm)) {
                Cmd::Toggle(key) => {
                    if offer_pwsh && key == POWERSHELL_KEY {
                        want_pwsh = !want_pwsh;
                        runner.update(|view| view.choices = cards(&chosen, want_pwsh));
                    } else if let Some(at) = AGENTS.iter().position(|t| t.name() == key) {
                        chosen[at] = !chosen[at];
                        runner.update(|view| view.choices = cards(&chosen, want_pwsh));
                    }
                }
                Cmd::Closed => {
                    runner.finish(Outcome2 {
                        quit: true,
                        ..Outcome2::default()
                    });
                    return;
                }
                _ => break,
            }
        }

        selected = chosen;
    }
    // The checklist: what is going to happen, so it can be watched.
    let mut labels: Vec<String> = Vec::new();
    for (tool, wanted) in AGENTS.into_iter().zip(selected) {
        if wanted {
            labels.push(format!("Getting {} ready", nice_name(tool)));
        }
    }
    if want_pwsh {
        labels.push("Getting PowerShell 7".to_owned());
    }
    labels.push("Preparing your workspace".to_owned());
    runner.update(|view| {
        view.screen = Screen::Working;
        view.steps = labels.iter().map(|label| step(label)).collect();
        view.problem = None;
        view.fraction = 0.0;
    });
    let total = labels.len() as f32;
    let progress = |runner: &Runner, done: usize, part: f32| {
        runner.update(|view| view.fraction = ((done as f32 + part) / total).min(1.0));
    };
    let mut skipped = false;

    // Room for what is about to be copied.
    let need = 500_u64 * 1024 * 1024;
    loop {
        match crate::platform::free_bytes(&plan.data_root) {
            Some(free) if free < need => {
                let problem = SetupProblem {
                    what: format!(
                        "There is not enough free space on {THIS_COMPUTER}: about {} are needed.",
                        human(need)
                    ),
                    next: "Free up some space, then try again.".to_owned(),
                    details: format!("{} free, {} needed.", human(free), human(need)),
                    actions: Vec::new(),
                };
                runner.update(|view| {
                    view.screen = Screen::Problem;
                    view.problem = Some(problem);
                });
                match runner.next(|cmd| *cmd == Cmd::Retry) {
                    Cmd::Closed => {
                        runner.finish(Outcome2 {
                            quit: true,
                            ..Outcome2::default()
                        });
                        return;
                    }
                    _ => runner.update(|view| {
                        view.screen = Screen::Working;
                        view.problem = None;
                    }),
                }
            }
            _ => break,
        }
    }

    // 1. The agents.
    let mut any_ready = false;
    // The agents Connecto installed in this run, with the version each reported.
    let mut installed_now: Vec<(Tool, String)> = Vec::new();
    let wanted: Vec<Tool> = AGENTS
        .into_iter()
        .zip(selected)
        .filter_map(|(tool, on)| on.then_some(tool))
        .collect();
    for (index, tool) in wanted.iter().copied().enumerate() {
        let at = index;
        loop {
            // A record of a 1.x OpenCode that has since upgraded itself to 2
            // is not "set up": it goes on below, where recording it again
            // refuses it with what to do.
            if let Ok(version) = store.newest(tool)
                && let Ok(installed) = store.verify(tool, &version)
                && !store.repair_wanted()
                && live_unsupported(tool, &installed.entry, &plan.home).is_none()
            {
                set_step(runner, at, StepState::Done, Some("Already set up"));
                forget_skipped(&plan.data_root, tool);
                any_ready = true;
                break;
            }
            if Store::find_installed(tool, &plan.home).is_none() {
                // On this computer, but not in a form Connecto can copy: say where, and why.
                let unusable = Store::unusable_installs(tool, &plan.home);
                if !unusable.is_empty() {
                    set_step(runner, at, StepState::Waiting, None);
                    let problem = explain_unusable(tool, &unusable);
                    runner.update(|view| {
                        view.screen = Screen::Problem;
                        view.problem = Some(problem);
                    });
                    match runner.next(|cmd| matches!(cmd, Cmd::Retry | Cmd::ContinueWithout)) {
                        Cmd::Retry => {
                            runner.update(|view| {
                                view.screen = Screen::Working;
                                view.problem = None;
                            });
                            continue;
                        }
                        Cmd::Closed => {
                            runner.finish(Outcome2 {
                                quit: true,
                                ..Outcome2::default()
                            });
                            return;
                        }
                        _ => {
                            set_step(runner, at, StepState::Skipped, Some("Left out for now"));
                            skipped = true;
                            remember_skipped(&plan.data_root, tool);
                            runner.update(|view| {
                                view.screen = Screen::Working;
                                view.problem = None;
                            });
                            break;
                        }
                    }
                }
                // Chosen at the first launch and not on this Mac: install it,
                // from the vendor's official source, and say so.
                if first && crate::agent_install::source_of(tool).is_some() {
                    set_step(
                        runner,
                        at,
                        StepState::Working,
                        Some("Starting the installer…"),
                    );
                    progress(runner, at, 0.1);
                    let say = |line: &str| set_step(runner, at, StepState::Working, Some(line));
                    // Pi's installer needs Node.js: chosen on the card, which
                    // said so, it is installed first, as Settings does.
                    let node = if tool == Tool::Pi
                        && installer.node_for_pi(&plan.home) != crate::node_setup::NodeCheck::Ready
                    {
                        installer.install_node(&plan.home, &say)
                    } else {
                        Ok(())
                    };
                    match node.and_then(|()| installer.install(tool, &plan.home, &say)) {
                        Ok(done) => {
                            installed_now.push((tool, done.version));
                            // Around again: it is found now, and gets set up.
                            continue;
                        }
                        Err(failure) => {
                            set_step(runner, at, StepState::Waiting, None);
                            let name = nice_name(tool);
                            let problem = SetupProblem {
                                what: failure.what,
                                next: failure.next,
                                details: failure.details,
                                actions: vec![
                                    ProblemAction::OpenPage {
                                        label: format!("Get {name} yourself"),
                                        url: page_of(tool).1.to_owned(),
                                    },
                                    ProblemAction::ContinueWithout {
                                        label: "Skip for now".to_owned(),
                                    },
                                ],
                            };
                            runner.update(|view| {
                                view.screen = Screen::Problem;
                                view.problem = Some(problem);
                            });
                            match runner
                                .next(|cmd| matches!(cmd, Cmd::Retry | Cmd::ContinueWithout))
                            {
                                Cmd::Retry => {
                                    runner.update(|view| {
                                        view.screen = Screen::Working;
                                        view.problem = None;
                                    });
                                    continue;
                                }
                                Cmd::Closed => {
                                    runner.finish(Outcome2 {
                                        quit: true,
                                        ..Outcome2::default()
                                    });
                                    return;
                                }
                                _ => {
                                    set_step(
                                        runner,
                                        at,
                                        StepState::Skipped,
                                        Some("Skipped for now"),
                                    );
                                    skipped = true;
                                    remember_skipped(&plan.data_root, tool);
                                    runner.update(|view| {
                                        view.screen = Screen::Working;
                                        view.problem = None;
                                    });
                                    break;
                                }
                            }
                        }
                    }
                }
                let note = format!("Not on {THIS_COMPUTER} yet");
                set_step(runner, at, StepState::Skipped, Some(&note));
                skipped = true;
                break;
            }
            set_step(runner, at, StepState::Working, None);
            progress(runner, at, 0.3);
            match store.provision_found(tool, &plan.home, signature) {
                Ok(_) => {
                    let note = installed_now
                        .iter()
                        .find(|(t, _)| *t == tool)
                        .map(|(_, v)| format!("Installed: {v}"));
                    set_step(runner, at, StepState::Done, note.as_deref());
                    forget_skipped(&plan.data_root, tool);
                    any_ready = true;
                    break;
                }
                // Another start finished it first: that copy is as good. Not
                // for a version Connecto cannot use: the record is of the one
                // that was there before (an OpenCode that upgraded itself).
                Err(ref err)
                    if !matches!(err, StoreError::Unsupported(_))
                        && store
                            .newest(tool)
                            .ok()
                            .and_then(|v| store.verify(tool, &v).ok())
                            .is_some() =>
                {
                    set_step(runner, at, StepState::Done, None);
                    forget_skipped(&plan.data_root, tool);
                    any_ready = true;
                    break;
                }
                Err(err) => {
                    set_step(runner, at, StepState::Waiting, None);
                    let problem = explain(tool, &err);
                    runner.update(|view| {
                        view.screen = Screen::Problem;
                        view.problem = Some(problem);
                    });
                    match runner.next(|cmd| matches!(cmd, Cmd::Retry | Cmd::ContinueWithout)) {
                        Cmd::Retry => runner.update(|view| {
                            view.screen = Screen::Working;
                            view.problem = None;
                        }),
                        Cmd::Closed => {
                            runner.finish(Outcome2 {
                                quit: true,
                                ..Outcome2::default()
                            });
                            return;
                        }
                        _ => {
                            set_step(runner, at, StepState::Skipped, Some("Left out for now"));
                            skipped = true;
                            remember_skipped(&plan.data_root, tool);
                            runner.update(|view| {
                                view.screen = Screen::Working;
                                view.problem = None;
                            });
                            break;
                        }
                    }
                }
            }
        }
        progress(runner, at + 1, 0.0);
    }
    if !wanted.is_empty()
        && !any_ready
        && AGENTS.into_iter().all(|tool| store.newest(tool).is_err())
    {
        // No agent is here: say so, and what to do.
        loop {
            runner.update(|view| {
                view.screen = Screen::Problem;
                view.problem = Some(nothing_installed());
            });
            match runner.next(|cmd| matches!(cmd, Cmd::Retry | Cmd::ContinueWithout)) {
                Cmd::Closed => {
                    runner.finish(Outcome2 {
                        quit: true,
                        ..Outcome2::default()
                    });
                    return;
                }
                Cmd::Retry => {
                    runner.update(|view| {
                        view.screen = Screen::Working;
                        view.problem = None;
                    });
                    let mut found = false;
                    for (index, tool) in wanted.iter().copied().enumerate() {
                        let at = index;
                        if store.provision_found(tool, &plan.home, signature).is_ok() {
                            set_step(runner, at, StepState::Done, None);
                            forget_skipped(&plan.data_root, tool);
                            found = true;
                        }
                    }
                    if found {
                        break;
                    }
                }
                _ => {
                    skipped = true;
                    for tool in wanted.iter().copied() {
                        if Store::find_installed(tool, &plan.home).is_some() {
                            remember_skipped(&plan.data_root, tool);
                        }
                    }
                    break;
                }
            }
        }
    }

    // 2. PowerShell 7 (Windows, when chosen). A failure is said and passed by.
    let mut at = wanted.len();
    if want_pwsh {
        set_step(runner, at, StepState::Working, None);
        progress(runner, at, 0.1);
        let say = |line: &str| set_step(runner, at, StepState::Working, Some(line));
        match installer.install_powershell(&say) {
            Ok(()) => set_step(runner, at, StepState::Done, None),
            Err(failure) => set_step(runner, at, StepState::Skipped, Some(&failure.what)),
        }
        progress(runner, at + 1, 0.0);
        at += 1;
    }

    // 3. The workspace.
    set_step(runner, at, StepState::Working, None);
    match prepare_workspace(plan) {
        Ok(_) => set_step(runner, at, StepState::Done, None),
        Err(why) => set_step(runner, at, StepState::Skipped, Some(&why)),
    }
    progress(runner, labels.len(), 0.0);
    let _ = store.take_repair();

    if !first {
        runner.finish(Outcome2 {
            skip_setup_next_start: skipped && !any_ready,
            ..Outcome2::default()
        });
        return;
    }

    // 4. Where the user stands, and a way in.
    let mut agents = ready_agents(plan, &layout);
    agents.retain(|agent| wanted.iter().any(|tool| nice_name(*tool) == agent.name));
    let chosen_keys: Vec<String> = wanted.iter().map(|t| t.name().to_owned()).collect();
    let signin_wanted = agents.iter().any(|a| a.ready && a.signed_in != Some(true));
    runner.update(|view| {
        view.screen = Screen::Ready;
        view.agents = agents;
        view.signin_wanted = signin_wanted;
    });
    let outcome = match runner.next(|cmd| {
        matches!(
            cmd,
            Cmd::OpenFolder(_) | Cmd::OpenSample | Cmd::Skip | Cmd::SignIn
        )
    }) {
        Cmd::SignIn => Outcome2 {
            open: connecto_workspace::saved_connecto_home(&plan.data_root),
            open_home: true,
            agents: chosen_keys,
            ..Outcome2::default()
        },
        Cmd::OpenFolder(path) => Outcome2 {
            open: Some(path),
            agents: chosen_keys,
            ..Outcome2::default()
        },
        Cmd::OpenSample => match make_sample(&plan.home) {
            Ok(dir) => Outcome2 {
                open: Some(dir),
                first_prompt: Some(SAMPLE_PROMPT.to_owned()),
                agents: chosen_keys,
                ..Outcome2::default()
            },
            Err(_) => Outcome2::default(),
        },
        Cmd::Closed => {
            runner.finish(Outcome2 {
                quit: true,
                ..Outcome2::default()
            });
            return;
        }
        _ => Outcome2 {
            agents: chosen_keys,
            ..Outcome2::default()
        },
    };
    let _ = std::fs::create_dir_all(&plan.data_root);
    let _ = std::fs::write(plan.data_root.join(AGENTS_FILE), outcome.agents.join(","));
    let _ = std::fs::write(plan.data_root.join(ONBOARDED), "first-launch setup done\n");
    runner.finish(outcome);
}

/// What an agent's card says it is.
const fn about(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "Anthropic's coding agent. A good all-rounder.",
        Tool::Codex => "OpenAI's coding agent. A good second opinion.",
        Tool::Pi => {
            "An open coding agent on the models you pick: OpenRouter, Together AI or LM Studio."
        }
        Tool::Antigravity => "Google's coding agent, on Gemini. Signs in with your Google account.",
        Tool::OpenCode => {
            "An open coding agent on any provider's models. Signs in with /connect, and has free models too."
        }
        Tool::Shell => "Your own shell.",
    }
}

/// What the card of an agent that is not here says will happen when it is
/// chosen. For Pi, also what happens about Node.js, by what `node` is here:
/// the first launch installs it when Pi is chosen, as Settings does when
/// asked.
fn first_launch_note(tool: Tool, node: &crate::node_setup::NodeCheck) -> Option<String> {
    let announced = crate::agent_install::announcement(tool)?;
    if tool != Tool::Pi {
        return Some(announced);
    }
    let source = crate::agent_install::source_of(tool)?;
    let (major, minor, _) = crate::node_setup::NEEDED;
    let base = format!(
        "Not on {THIS_COMPUTER} yet. Connecto will install {} for you from {}'s official installer at {}. It goes in your own folder and needs no password.",
        source.name, source.vendor, source.host
    );
    Some(match node {
        crate::node_setup::NodeCheck::Ready => base,
        _ => format!(
            "{base} Pi runs on Node.js {major}.{minor} or newer, which {THIS_COMPUTER} does not have: Connecto installs the official Node.js {major} from nodejs.org first, in your own folder. Any Node.js you already have stays as it is. You then set up Pi's model providers in Settings > Agents."
        ),
    })
}

/// The choice screen's PowerShell 7 card (Windows, when it is missing).
fn powershell_card(selected: bool) -> AgentChoice {
    AgentChoice {
        key: POWERSHELL_KEY.to_owned(),
        name: "PowerShell 7".to_owned(),
        about: "The current PowerShell, for your terminals. Without it they use Windows PowerShell 5.1."
            .to_owned(),
        selected,
        found: false,
        found_at: None,
        install_note: Some(
            "Connecto will install it with winget, from Microsoft's package. It can take a few minutes; Windows may ask for your permission."
                .to_owned(),
        ),
    }
}

fn human(bytes: u64) -> String {
    let gib = bytes as f64 / (1024.0 * 1024.0 * 1024.0);
    if gib >= 1.0 {
        format!("{gib:.1} GB")
    } else {
        format!("{} MB", bytes / (1024 * 1024))
    }
}

/// The agents a launch has to set up, in the order it does them, for the
/// `provision` command: the same work without the window.
///
/// # Errors
/// The first failure, in words for a terminal.
pub fn provision_all<S: Signature>(
    data_root: &Path,
    home: &Path,
    signature: &S,
    report: &mut dyn FnMut(String),
) -> Result<(), String> {
    let store = Store::new(data_root);
    let mut failed = false;
    for tool in COPIED_AGENTS {
        if Store::find_installed(tool, home).is_none() {
            report(format!("{}: not installed", tool.name()));
            failed = true;
            continue;
        }
        match store.provision_found(tool, home, signature) {
            Ok(provisioned) => report(format!(
                "{} {} -> {}",
                tool.name(),
                provisioned.installed.version,
                provisioned.installed.entry.display()
            )),
            Err(err) => {
                report(format!("{}: {err}", tool.name()));
                failed = true;
            }
        }
    }
    if failed {
        Err("some tools were not set up".to_owned())
    } else {
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    struct AnySignature;
    impl Signature for AnySignature {
        fn verify(&self, _file: &Path, _team: Option<&str>) -> Result<(), String> {
            Ok(())
        }
    }

    struct Rejecting;
    impl Signature for Rejecting {
        fn verify(&self, _file: &Path, _team: Option<&str>) -> Result<(), String> {
            Err("not signed by the expected team".to_owned())
        }
    }

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "connecto-setup-{name}-{}-{nanos}",
                std::process::id()
            ));
            std::fs::create_dir_all(&path).unwrap();
            Self(std::fs::canonicalize(path).unwrap())
        }
        fn home(&self) -> PathBuf {
            self.0.join("home")
        }
        fn data(&self) -> PathBuf {
            self.0.join("data")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            #[cfg(unix)]
            fn unlock(p: &Path) {
                if let Ok(meta) = std::fs::symlink_metadata(p)
                    && meta.is_dir()
                {
                    let _ = std::fs::set_permissions(p, std::fs::Permissions::from_mode(0o755));
                    if let Ok(entries) = std::fs::read_dir(p) {
                        for entry in entries.flatten() {
                            unlock(&entry.path());
                        }
                    }
                }
            }
            #[cfg(unix)]
            unlock(&self.0);
            let _ = std::fs::remove_dir_all(&self.0);
        }
    }

    /// A fake Claude Code in `~/.local/bin` that answers `--version`: a native
    /// program (`claude.exe` on Windows).
    fn fake_claude(home: &Path) {
        let path = home.join(format!(".local/bin/claude{}", std::env::consts::EXE_SUFFIX));
        crate::test_support::link_native("2.1.318 (Claude Code)", &path);
    }

    fn plan_for(dir: &Dir, mode: Mode) -> Plan {
        std::fs::create_dir_all(dir.home()).unwrap();
        Plan {
            mode,
            home: dir.home(),
            data_root: dir.data(),
        }
    }

    /// An installer that cannot reach the vendor: the Mac is offline.
    struct Offline;
    impl crate::agent_install::AgentInstaller for Offline {
        fn install(
            &self,
            tool: Tool,
            _home: &Path,
            say: &dyn Fn(&str),
        ) -> Result<crate::agent_install::Installed, crate::agent_install::Failure> {
            say("Downloading the installer…");
            Err(crate::agent_install::Failure {
                what: format!(
                    "Connecto could not reach the {} installer. Is this Mac online?",
                    tool.name()
                ),
                next: "Check your internet connection, then try again, or skip this for now."
                    .to_owned(),
                details: "curl exit Some(6)".to_owned(),
            })
        }
    }

    fn drive(
        plan: &Plan,
        runner: &Runner,
        signature: impl Signature + Send + 'static,
    ) -> std::thread::JoinHandle<()> {
        drive_with(plan, runner, signature, Arc::new(Offline))
    }

    fn drive_with(
        plan: &Plan,
        runner: &Runner,
        signature: impl Signature + Send + 'static,
        installer: Arc<dyn crate::agent_install::AgentInstaller>,
    ) -> std::thread::JoinHandle<()> {
        let plan = plan.clone();
        let runner = runner.clone();
        std::thread::spawn(move || run(&plan, &runner, &signature, &*installer))
    }

    fn wait_for_screen(runner: &Runner, screen: Screen) -> Arc<SetupView> {
        let deadline =
            std::time::Instant::now() + crate::test_support::scaled(Duration::from_secs(30));
        loop {
            let view = runner.view();
            if view.screen == screen {
                return view;
            }
            assert!(
                std::time::Instant::now() < deadline,
                "waiting for {screen:?}: {:?}",
                runner.view()
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_first_launch_starts_the_setup_and_launches_after_it_do_not() {
        let dir = Dir::new("plan");
        std::fs::create_dir_all(dir.home()).unwrap();
        let data = Some(dir.data().into_os_string());
        let plan = |skip| plan_with(&dir.home(), data.clone(), skip);
        // Nothing there: a first launch.
        assert_eq!(plan(false).unwrap().mode, Mode::FirstRun);
        assert!(plan(true).is_none(), "skipped on request");
        // Done once: marked, and nothing missing, so nothing to do.
        std::fs::create_dir_all(dir.data()).unwrap();
        std::fs::write(dir.data().join(ONBOARDED), "x").unwrap();
        assert!(plan(false).is_none());
        // An agent installed since, not yet set up: only the checklist, no welcome.
        fake_claude(&dir.home());
        assert_eq!(plan(false).unwrap().mode, Mode::Repair);
        // A stored copy that failed to verify asks for the same.
        let store = Store::new(&dir.data());
        store
            .provision(
                Tool::Claude,
                &dir.home().join(".local/bin/claude"),
                &AnySignature,
            )
            .unwrap();
        assert!(plan(false).is_none());
        store.mark_repair();
        assert_eq!(plan(false).unwrap().mode, Mode::Repair);
    }

    #[test]
    fn an_install_that_predates_the_setup_is_marked_done_without_a_welcome() {
        let dir = Dir::new("old-install");
        std::fs::create_dir_all(dir.home()).unwrap();
        std::fs::create_dir_all(dir.data()).unwrap();
        std::fs::write(dir.data().join("workspace.json"), "{}").unwrap();
        let data = Some(dir.data().into_os_string());
        assert!(plan_with(&dir.home(), data.clone(), false).is_none());
        assert!(dir.data().join(ONBOARDED).exists());
        assert!(plan_with(&dir.home(), data, false).is_none());
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn what_is_installed_and_not_yet_set_up_is_what_gets_set_up() {
        let dir = Dir::new("toset");
        std::fs::create_dir_all(dir.home()).unwrap();
        let store = Store::new(&dir.data());
        assert!(
            to_set_up(&store, &dir.home(), &[]).is_empty(),
            "nothing installed"
        );
        fake_claude(&dir.home());
        assert_eq!(to_set_up(&store, &dir.home(), &[]), [Tool::Claude]);
        store
            .provision(
                Tool::Claude,
                &dir.home().join(".local/bin/claude"),
                &AnySignature,
            )
            .unwrap();
        assert!(
            to_set_up(&store, &dir.home(), &[]).is_empty(),
            "already set up"
        );
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn the_checklist_sets_up_what_is_installed_and_says_what_is_not() {
        let dir = Dir::new("checklist");
        let plan = plan_for(&dir, Mode::Repair);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        worker.join().unwrap();
        let view = runner.view();
        let claude = view
            .steps
            .iter()
            .find(|s| s.label.contains("Claude"))
            .unwrap();
        let codex = view
            .steps
            .iter()
            .find(|s| s.label.contains("Codex"))
            .unwrap();
        assert_eq!(claude.state, StepState::Done);
        assert_eq!(codex.state, StepState::Skipped);
        assert_eq!(codex.note.as_deref(), Some("Not on this Mac yet"));
        // Only what is missing: the Claude copy is there, verified.
        let store = Store::new(&dir.data());
        let v = store.newest(Tool::Claude).unwrap();
        assert!(store.verify(Tool::Claude, &v).is_ok());
        assert!(store.newest(Tool::Codex).is_err());
        // The default utility folder is made and saved.
        let home = connecto_workspace::saved_connecto_home(&dir.data()).unwrap();
        assert!(home.join("automations").is_dir() && home.join("packages").is_dir());
        assert!(runner.outcome().is_some());
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_failed_check_shows_a_plain_problem_and_retry_works_once_it_is_fixed() {
        let dir = Dir::new("retry");
        let plan = plan_for(&dir, Mode::Repair);
        fake_claude(&dir.home());
        // The first try is refused: the signature does not check out.
        struct Flaky(std::sync::atomic::AtomicBool);
        impl Signature for Flaky {
            fn verify(&self, _f: &Path, _t: Option<&str>) -> Result<(), String> {
                if self.0.load(std::sync::atomic::Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err("bad".to_owned())
                }
            }
        }
        let good = Arc::new(Flaky(std::sync::atomic::AtomicBool::new(false)));
        struct Shared(Arc<Flaky>);
        impl Signature for Shared {
            fn verify(&self, f: &Path, t: Option<&str>) -> Result<(), String> {
                self.0.verify(f, t)
            }
        }
        let runner = Runner::new();
        let worker = drive(&plan, &runner, Shared(Arc::clone(&good)));
        let view = wait_for_screen(&runner, Screen::Problem);
        let problem = view.problem.clone().unwrap();
        assert!(problem.what.contains("could not confirm"), "{problem:?}");
        assert!(problem.next.contains("try again"), "{problem:?}");
        assert!(!problem.details.is_empty());
        // Nothing half-made is taken for done, and a failure is not a skip.
        let store = Store::new(&dir.data());
        assert!(store.newest(Tool::Claude).is_err());
        assert!(skipped_tools(&dir.data()).is_empty());
        good.0.store(true, std::sync::atomic::Ordering::SeqCst);
        runner.retry();
        worker.join().unwrap();
        let v = store.newest(Tool::Claude).unwrap();
        assert!(store.verify(Tool::Claude, &v).is_ok());
        assert!(skipped_tools(&dir.data()).is_empty());
    }

    /// A check that never gets an answer: codesign was stopped every time.
    struct Unfinished;

    impl Signature for Unfinished {
        fn verify(&self, _f: &Path, _t: Option<&str>) -> Result<(), String> {
            Err("stopped".to_owned())
        }
        fn check(&self, _f: &Path, _t: Option<&str>) -> Result<(), connecto_store::Failure> {
            Err(connecto_store::Failure::Incomplete(
                "codesign did not finish within 60 seconds".to_owned(),
            ))
        }
    }

    #[test]
    fn a_check_that_could_not_finish_is_not_a_skip_only_continue_without_is() {
        let dir = Dir::new("unfinished");
        let plan = plan_for(&dir, Mode::Repair);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive(&plan, &runner, Unfinished);
        let view = wait_for_screen(&runner, Screen::Problem);
        let problem = view.problem.clone().unwrap();
        assert!(problem.what.contains("could not finish"), "{problem:?}");
        assert!(problem.details.contains("did not finish"), "{problem:?}");
        // Failing is not skipping: the next start offers Claude again.
        assert!(skipped_tools(&dir.data()).is_empty());
        // Only the user's own choice leaves it out. The second command is for
        // the screen that follows, when nothing is left to set up.
        runner.continue_without();
        runner.continue_without();
        worker.join().unwrap();
        assert!(runner.outcome().is_some());
        assert_eq!(skipped_tools(&dir.data()), [Tool::Claude]);
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn an_interrupted_run_leaves_nothing_that_counts_and_the_next_one_finishes() {
        let dir = Dir::new("interrupted");
        std::fs::create_dir_all(dir.home()).unwrap();
        fake_claude(&dir.home());
        let store = Store::new(&dir.data());
        // What a run killed in the middle leaves: a staging folder of a dead process.
        let stale = dir.data().join("cli/.staging-claude-1073741823/copy");
        std::fs::create_dir_all(&stale).unwrap();
        std::fs::write(stale.join("claude"), "half").unwrap();
        assert!(
            store.newest(Tool::Claude).is_err(),
            "a half copy is not a copy"
        );
        let plan = plan_for(&dir, Mode::Repair);
        let runner = Runner::new();
        drive(&plan, &runner, AnySignature).join().unwrap();
        assert!(store.newest(Tool::Claude).is_ok());
        assert!(!dir.data().join("cli/.staging-claude-1073741823").exists());
    }

    /// A Codex on this Mac that Connecto cannot read (a layout it does not know).
    #[cfg(unix)]
    fn unreadable_codex(home: &Path) {
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        let path = bin.join("codex");
        crate::test_support::link_fake("#!/bin/sh\necho 'codex-cli 1.0.0'\n", &path);
    }

    /// A Codex release that Connecto can read; with a signature that rejects it,
    /// setting it up fails.
    #[cfg(unix)]
    fn release_codex(home: &Path) {
        let release = home.join("release");
        std::fs::create_dir_all(release.join("bin")).unwrap();
        std::fs::write(release.join("codex-package.json"), "{}").unwrap();
        for tree in ["codex-path", "codex-resources"] {
            std::fs::create_dir_all(release.join(tree)).unwrap();
            std::fs::write(release.join(tree).join("data"), "x").unwrap();
        }
        let entry = release.join("bin/codex");
        crate::test_support::link_fake("#!/bin/sh\necho 'codex-cli 1.0.0'\n", &entry);
        let bin = home.join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        std::os::unix::fs::symlink(&entry, bin.join("codex")).unwrap();
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_skipped_agent_is_remembered_so_the_next_start_goes_into_the_app() {
        let dir = Dir::new("skip-remembered");
        let plan = plan_for(&dir, Mode::Repair);
        fake_claude(&dir.home());
        release_codex(&dir.home());
        let store = Store::new(&dir.data());
        std::fs::create_dir_all(dir.data()).unwrap();
        std::fs::write(dir.data().join(ONBOARDED), "x").unwrap();
        // Claude is ready; Codex is on the Mac but cannot be set up.
        store
            .provision(
                Tool::Claude,
                &dir.home().join(".local/bin/claude"),
                &AnySignature,
            )
            .unwrap();
        let data = Some(dir.data().into_os_string());
        assert_eq!(
            plan_with(&dir.home(), data.clone(), false).unwrap().mode,
            Mode::Repair
        );
        let runner = Runner::new();
        let worker = drive(&plan, &runner, Rejecting);
        let view = wait_for_screen(&runner, Screen::Problem);
        assert!(view.problem.is_some());
        runner.continue_without();
        worker.join().unwrap();
        assert!(runner.outcome().is_some());
        assert_eq!(skipped_tools(&dir.data()), [Tool::Codex]);
        // The next start sees nothing to do, with no help from the environment.
        assert!(plan_with(&dir.home(), data.clone(), false).is_none());
        // A repair for another reason does not ask about Codex again.
        store.mark_repair();
        let again = Runner::new();
        drive(&plan, &again, Rejecting).join().unwrap();
        assert!(again.outcome().is_some());
        // Setting it up later (here: a readable layout) clears the skip.
        forget_skipped(&dir.data(), Tool::Codex);
        assert!(skipped_tools(&dir.data()).is_empty());
        assert_eq!(
            plan_with(&dir.home(), data, false).unwrap().mode,
            Mode::Repair
        );
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_skip_is_cleared_when_the_agent_gets_set_up() {
        let dir = Dir::new("skip-cleared");
        let plan = plan_for(&dir, Mode::Repair);
        fake_claude(&dir.home());
        std::fs::create_dir_all(dir.data()).unwrap();
        remember_skipped(&dir.data(), Tool::Claude);
        remember_skipped(&dir.data(), Tool::Claude);
        remember_skipped(&dir.data(), Tool::Codex);
        assert_eq!(skipped_tools(&dir.data()), [Tool::Claude, Tool::Codex]);
        // Claude was skipped, so the repair leaves it alone.
        let data = Some(dir.data().into_os_string());
        std::fs::write(dir.data().join(ONBOARDED), "x").unwrap();
        assert!(plan_with(&dir.home(), data, false).is_none());
        // Set up on request (as Settings does) and it is forgotten.
        forget_skipped(&dir.data(), Tool::Claude);
        let runner = Runner::new();
        drive(&plan, &runner, AnySignature).join().unwrap();
        assert!(Store::new(&dir.data()).newest(Tool::Claude).is_ok());
        assert_eq!(skipped_tools(&dir.data()), [Tool::Codex]);
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_codex_that_is_found_but_cannot_be_copied_is_named_and_can_be_left_out() {
        let dir = Dir::new("unusable");
        let plan = plan_for(&dir, Mode::Repair);
        fake_claude(&dir.home());
        unreadable_codex(&dir.home());
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        let view = wait_for_screen(&runner, Screen::Problem);
        let problem = view.problem.clone().unwrap();
        assert!(problem.what.contains("cannot use this copy"), "{problem:?}");
        assert!(
            problem.details.contains(".local/bin/codex")
                && problem.details.contains("launcher script"),
            "{problem:?}"
        );
        assert!(problem.actions.iter().any(
            |a| matches!(a, ProblemAction::ContinueWithout { label } if label == "Continue without Codex")
        ));
        runner.continue_without();
        worker.join().unwrap();
        assert!(runner.outcome().is_some());
        assert_eq!(skipped_tools(&dir.data()), [Tool::Codex]);
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_claude_wrapper_is_named_as_a_wrapper_and_is_not_sent_to_reinstall() {
        let dir = Dir::new("claude-wrapper");
        let plan = plan_for(&dir, Mode::Repair);
        // A terminal's shim named `claude`: a script, not Claude Code's program.
        let bin = dir.home().join(".local/bin");
        std::fs::create_dir_all(&bin).unwrap();
        crate::test_support::link_fake("#!/bin/sh\necho 'wrapper'\n", &bin.join("claude"));
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        let view = wait_for_screen(&runner, Screen::Problem);
        let problem = view.problem.clone().unwrap();
        assert!(problem.what.contains("script or wrapper"), "{problem:?}");
        assert!(problem.details.contains(".local/bin/claude"), "{problem:?}");
        assert!(
            !problem.what.contains("Reinstall") && !problem.next.contains("Reinstall"),
            "{problem:?}"
        );
        // Skipping it leaves no agent ready: the next screen says so.
        runner.continue_without();
        let _ = wait_for_screen(&runner, Screen::Problem);
        runner.continue_without();
        worker.join().unwrap();
        assert!(runner.outcome().is_some());
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn neither_agent_installed_says_so_with_where_to_get_them_and_a_way_on() {
        let dir = Dir::new("none");
        let plan = plan_for(&dir, Mode::Repair);
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        let view = wait_for_screen(&runner, Screen::Problem);
        let problem = view.problem.clone().unwrap();
        assert!(
            problem.what.contains("none of them is on this Mac"),
            "{problem:?}"
        );
        assert!(
            problem
                .actions
                .iter()
                .any(|a| matches!(a, ProblemAction::OpenPage { .. }))
        );
        runner.continue_without();
        worker.join().unwrap();
        assert!(runner.outcome().unwrap().skip_setup_next_start);
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn the_first_launch_welcomes_then_works_then_offers_a_way_in_and_is_not_shown_again() {
        let dir = Dir::new("first");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.toggle_agent("codex");
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        assert!(
            view.agents
                .iter()
                .any(|a| a.name == "Claude Code" && a.ready)
        );
        assert!(view.agents.iter().all(|a| a.name != "Codex"));
        let folder = dir.0.join("work");
        std::fs::create_dir_all(&folder).unwrap();
        runner.open_folder(folder.clone());
        worker.join().unwrap();
        assert_eq!(runner.outcome().unwrap().open, Some(folder));
        // Marked: the next launch does not show it again.
        assert!(dir.data().join(ONBOARDED).exists());
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn the_sample_project_is_small_and_made_once_and_opens_with_a_first_message() {
        let dir = Dir::new("sample");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.toggle_agent("codex");
        runner.confirm_choice();
        wait_for_screen(&runner, Screen::Ready);
        runner.open_sample();
        worker.join().unwrap();
        let outcome = runner.outcome().unwrap();
        let sample = outcome.open.unwrap();
        assert!(sample.join("README.md").is_file() && sample.join("hello.py").is_file());
        assert_eq!(outcome.first_prompt.as_deref(), Some(SAMPLE_PROMPT));
        std::fs::write(sample.join("hello.py"), "mine").unwrap();
        assert_eq!(make_sample(&dir.home()).unwrap(), sample);
        assert_eq!(
            std::fs::read_to_string(sample.join("hello.py")).unwrap(),
            "mine"
        );
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn only_the_chosen_agents_are_set_up_and_none_chosen_still_leaves_a_usable_app() {
        let dir = Dir::new("choice");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        let view = wait_for_screen(&runner, Screen::Choose);
        // All five are offered, described for a newcomer; Claude Code and
        // Codex are chosen to begin with.
        assert_eq!(view.choices.len(), 5);
        assert!(view.choices.iter().all(|c| !c.about.is_empty()));
        let chosen: Vec<bool> = view.choices.iter().map(|c| c.selected).collect();
        assert_eq!(chosen, [true, true, false, false, false]);
        assert!(view.choices[0].found && !view.choices[1].found);
        // Tap Claude off: nothing is chosen but Codex, which is not on this Mac.
        runner.toggle_agent("claude");
        for _ in 0..100 {
            if !runner.view().choices[0].selected {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        runner.toggle_agent("codex");
        for _ in 0..100 {
            if !runner.view().choices[1].selected {
                break;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        // No agent line at all, and Claude was not set up although it is installed.
        assert!(
            view.steps
                .iter()
                .all(|s| !s.label.contains("Claude") && !s.label.contains("Codex"))
        );
        assert!(Store::new(&dir.data()).newest(Tool::Claude).is_err());
        assert!(view.agents.is_empty() && !view.signin_wanted);
        runner.skip();
        worker.join().unwrap();
        assert_eq!(chosen_agents(&dir.data()), Vec::<Tool>::new());
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_chosen_agent_that_is_not_signed_in_sends_the_user_to_sign_in_through_connecto_home() {
        let dir = Dir::new("signin");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.toggle_agent("codex");
        std::thread::sleep(Duration::from_millis(200));
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        // Only Claude is chosen; it is ready, and the fake cannot say it is signed in.
        assert_eq!(view.agents.len(), 1);
        assert!(view.agents[0].ready && view.agents[0].signed_in != Some(true));
        assert!(view.signin_wanted);
        runner.sign_in();
        worker.join().unwrap();
        let outcome = runner.outcome().unwrap();
        let home = connecto_workspace::saved_connecto_home(&dir.data()).unwrap();
        assert!(outcome.open_home);
        assert_eq!(outcome.open, Some(home));
        assert_eq!(outcome.agents, ["claude"]);
        assert_eq!(chosen_agents(&dir.data()), [Tool::Claude]);
    }

    #[test]
    fn closing_the_window_before_the_end_quits_without_marking_anything_done() {
        let dir = Dir::new("closed");
        let plan = plan_for(&dir, Mode::FirstRun);
        let runner = Runner::new();
        let worker = drive(&plan, &runner, AnySignature);
        wait_for_screen(&runner, Screen::Welcome);
        runner.closed();
        worker.join().unwrap();
        assert!(runner.outcome().unwrap().quit);
        assert!(!dir.data().join(ONBOARDED).exists());
    }

    #[test]
    fn a_stored_copy_that_failed_to_verify_asks_for_a_repair_at_the_next_start() {
        let dir = Dir::new("repair");
        std::fs::create_dir_all(dir.home()).unwrap();
        let store = Store::new(&dir.data());
        assert!(!store.repair_wanted());
        store.mark_repair();
        assert!(store.repair_wanted());
        assert!(store.take_repair());
        assert!(!store.repair_wanted());
        let _ = Rejecting;
    }

    #[test]
    fn the_explanations_use_plain_words() {
        let signature = explain(Tool::Claude, &StoreError::Signature("x".into()));
        assert!(signature.what.contains("genuine"));
        let unchecked = explain(Tool::Claude, &StoreError::Unchecked("stopped".into()));
        assert!(unchecked.what.contains("could not finish"), "{unchecked:?}");
        assert!(!unchecked.what.contains("genuine"), "{unchecked:?}");
        let changed = explain(Tool::Codex, &StoreError::SourceChanged(PathBuf::from("/x")));
        assert!(changed.what.contains("update may be running"));
        for p in [signature, changed, nothing_installed()] {
            let text = format!("{} {}", p.what, p.next).to_lowercase();
            for jargon in ["provision", "cli", "path", "isolated", "hooks", "checksum"] {
                assert!(!text.contains(jargon), "{jargon} in {text}");
            }
        }
    }

    /// An installer that puts a fake program where the real one would, and can
    /// be told to fail a number of times first (the Mac was offline).
    #[cfg(unix)]
    struct Fixing {
        calls: std::sync::Mutex<Vec<Tool>>,
        failures: std::sync::atomic::AtomicUsize,
    }

    #[cfg(unix)]
    impl Fixing {
        fn new(failures: usize) -> Arc<Self> {
            Arc::new(Self {
                calls: std::sync::Mutex::default(),
                failures: std::sync::atomic::AtomicUsize::new(failures),
            })
        }
        fn calls(&self) -> Vec<Tool> {
            self.calls.lock().unwrap().clone()
        }
    }

    #[cfg(unix)]
    impl crate::agent_install::AgentInstaller for Fixing {
        fn install(
            &self,
            tool: Tool,
            home: &Path,
            say: &dyn Fn(&str),
        ) -> Result<crate::agent_install::Installed, crate::agent_install::Failure> {
            self.calls.lock().unwrap().push(tool);
            say("Downloading the installer…");
            let left = self.failures.load(std::sync::atomic::Ordering::SeqCst);
            if left > 0 {
                self.failures
                    .store(left - 1, std::sync::atomic::Ordering::SeqCst);
                return Err(crate::agent_install::Failure {
                    what: "Connecto could not reach the installer. Is this Mac online?".to_owned(),
                    next: "Check your internet connection, then try again, or skip this for now."
                        .to_owned(),
                    details: "curl exit Some(6)".to_owned(),
                });
            }
            // Where each installer puts its program: OpenCode's in its own
            // folder, the others in `~/.local/bin`.
            let bin = if tool == Tool::OpenCode {
                home.join(".opencode/bin")
            } else {
                home.join(".local/bin")
            };
            std::fs::create_dir_all(&bin).unwrap();
            let path = bin.join(tool.program());
            let program = if tool == Tool::Codex {
                // Codex's own installer: a release folder under ~/.codex and a link in ~/.local/bin.
                let release = home.join(".codex/packages/standalone/current");
                std::fs::create_dir_all(release.join("bin")).unwrap();
                std::fs::create_dir_all(release.join("codex-path")).unwrap();
                std::fs::create_dir_all(release.join("codex-resources")).unwrap();
                std::fs::write(release.join("codex-package.json"), "{}").unwrap();
                release.join("bin/codex")
            } else {
                path.clone()
            };
            // OpenCode's installer gives the 1.x line, the one Connecto takes.
            let version = if tool == Tool::OpenCode {
                "1.7.7 (Fake)"
            } else {
                "7.7.7 (Fake)"
            };
            crate::test_support::link_fake(&format!("#!/bin/sh\necho '{version}'\n"), &program);
            if program != path {
                std::os::unix::fs::symlink(&program, &path).unwrap();
            }
            Ok(crate::agent_install::Installed {
                path,
                version: version.to_owned(),
            })
        }
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn the_choice_says_what_will_be_installed_and_from_where_only_for_a_missing_agent() {
        let dir = Dir::new("note");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, Fixing::new(0));
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        let view = wait_for_screen(&runner, Screen::Choose);
        let claude = view.choices.iter().find(|c| c.key == "claude").unwrap();
        let codex = view.choices.iter().find(|c| c.key == "codex").unwrap();
        assert!(claude.found && claude.install_note.is_none());
        let note = codex.install_note.as_deref().unwrap();
        assert!(
            note.contains("chatgpt.com") && note.contains("OpenAI") && note.contains("no password")
        );
        runner.closed();
        worker.join().unwrap();
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_chosen_agent_that_is_missing_is_installed_then_set_up_and_shows_its_version() {
        let dir = Dir::new("install");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let installer = Fixing::new(0);
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, installer.clone());
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        assert_eq!(
            installer.calls(),
            [Tool::Codex],
            "only the missing one is installed"
        );
        assert!(view.agents.iter().all(|a| a.ready), "{:?}", view.agents);
        let codex = view
            .steps
            .iter()
            .find(|s| s.label.contains("Codex"))
            .unwrap();
        assert_eq!(codex.state, StepState::Done);
        assert_eq!(codex.note.as_deref(), Some("Installed: 7.7.7 (Fake)"));
        // Found where the isolated copy is made from, so Isolated and System both have it.
        assert!(Store::find_installed(Tool::Codex, &dir.home()).is_some());
        assert!(Store::new(&dir.data()).newest(Tool::Codex).is_ok());
        runner.skip();
        worker.join().unwrap();
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn the_first_launch_offers_all_five_agents_by_their_names_and_installers() {
        let dir = Dir::new("five");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, Fixing::new(0));
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        let view = wait_for_screen(&runner, Screen::Choose);
        let names: Vec<&str> = view.choices.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(
            names,
            ["Claude Code", "Codex", "Pi", "Antigravity CLI", "OpenCode"]
        );
        let keys: Vec<&str> = view.choices.iter().map(|c| c.key.as_str()).collect();
        assert_eq!(keys, ["claude", "codex", "pi", "antigravity", "opencode"]);
        let note = |key: &str| {
            view.choices
                .iter()
                .find(|c| c.key == key)
                .and_then(|c| c.install_note.clone())
                .unwrap()
        };
        assert!(note("pi").contains("pi.dev"), "{}", note("pi"));
        assert!(
            note("antigravity").contains("antigravity.google")
                && note("antigravity").contains("Google"),
            "{}",
            note("antigravity")
        );
        assert!(
            note("opencode").contains("opencode.ai"),
            "{}",
            note("opencode")
        );
        runner.closed();
        worker.join().unwrap();
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn choosing_antigravity_installs_it_and_runs_it_where_it_is() {
        let dir = Dir::new("agy");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let installer = Fixing::new(0);
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, installer.clone());
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.toggle_agent("codex");
        runner.toggle_agent("antigravity");
        wait_for_choices(&runner, |c| c.key != "antigravity" || c.selected);
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        assert_eq!(installer.calls(), [Tool::Antigravity]);
        let step = view
            .steps
            .iter()
            .find(|s| s.label == "Getting Antigravity CLI ready")
            .unwrap();
        assert_eq!(step.state, StepState::Done);
        let entry = Store::new(&dir.data())
            .verify(
                Tool::Antigravity,
                &Store::new(&dir.data()).newest(Tool::Antigravity).unwrap(),
            )
            .unwrap()
            .entry;
        assert_eq!(entry, dir.home().join(".local/bin/agy"), "not copied");
        let agy = view
            .agents
            .iter()
            .find(|a| a.name == "Antigravity CLI")
            .unwrap();
        // Ready, and its sign-in is not claimed.
        assert!(agy.ready && agy.signed_in.is_none(), "{agy:?}");
        runner.skip();
        worker.join().unwrap();
        assert_eq!(
            chosen_agents(&dir.data()),
            [Tool::Claude, Tool::Antigravity]
        );
    }

    /// OpenCode's installer puts it in `~/.opencode/bin`, a folder no shell
    /// search path has (the installer is told to leave the startup file
    /// alone): Connecto finds it there, and runs it where it is.
    #[cfg(unix)]
    #[test]
    fn choosing_opencode_installs_it_in_its_own_folder_and_runs_it_where_it_is() {
        let dir = Dir::new("opencode");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let installer = Fixing::new(0);
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, installer.clone());
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.toggle_agent("codex");
        runner.toggle_agent("opencode");
        wait_for_choices(&runner, |c| c.key != "opencode" || c.selected);
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        assert_eq!(installer.calls(), [Tool::OpenCode]);
        let step = view
            .steps
            .iter()
            .find(|s| s.label == "Getting OpenCode ready")
            .unwrap();
        assert_eq!(step.state, StepState::Done, "{step:?}");
        let store = Store::new(&dir.data());
        let entry = store
            .verify(Tool::OpenCode, &store.newest(Tool::OpenCode).unwrap())
            .unwrap()
            .entry;
        assert_eq!(
            entry,
            dir.home().join(".opencode/bin/opencode"),
            "not copied"
        );
        let opencode = view.agents.iter().find(|a| a.name == "OpenCode").unwrap();
        // Ready, and its sign-in is not claimed: `--version` says nothing of it.
        assert!(
            opencode.ready && opencode.signed_in.is_none(),
            "{opencode:?}"
        );
        runner.skip();
        worker.join().unwrap();
        assert_eq!(chosen_agents(&dir.data()), [Tool::Claude, Tool::OpenCode]);
    }

    /// An OpenCode 2 already on this Mac is not taken: the setup says it is
    /// not supported yet and how to install the 1.x line, and nothing is
    /// installed over it.
    #[cfg(unix)]
    #[test]
    fn an_opencode_2_on_this_mac_is_refused_with_what_to_do() {
        let dir = Dir::new("opencode2");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        crate::test_support::link_fake(
            "#!/bin/sh\necho '2.3.2'\n",
            &dir.home().join(".opencode/bin/opencode"),
        );
        let installer = Fixing::new(0);
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, installer.clone());
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.toggle_agent("codex");
        runner.toggle_agent("opencode");
        wait_for_choices(&runner, |c| c.key != "opencode" || c.selected);
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Problem);
        let problem = view.problem.clone().unwrap();
        assert!(
            problem.what.contains("OpenCode 2.3.2 is not supported yet")
                && problem.what.contains("--version 1."),
            "{problem:?}"
        );
        assert!(installer.calls().is_empty(), "installed over it");
        runner.closed();
        worker.join().unwrap();
    }

    /// Recorded as 1.x, upgraded in place to 2 since: the setup does not call
    /// it "Already set up" from the record, but says it is not supported.
    #[cfg(unix)]
    #[test]
    fn an_opencode_recorded_as_1_and_upgraded_to_2_is_not_already_set_up() {
        recorded_1_then_2_is_refused("opencode-upgraded", "#!/bin/sh\necho '2.3.2'\n");
    }

    /// The same when the 2 prints its version on stderr alone.
    #[cfg(unix)]
    #[test]
    fn an_opencode_upgraded_to_a_2_that_prints_on_stderr_is_not_already_set_up() {
        recorded_1_then_2_is_refused("opencode-upgraded-err", "#!/bin/sh\necho '2.3.2' >&2\n");
    }

    #[cfg(unix)]
    fn recorded_1_then_2_is_refused(name: &str, two: &str) {
        let dir = Dir::new(name);
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let entry = dir.home().join(".opencode/bin/opencode");
        crate::test_support::link_fake("#!/bin/sh\necho '1.19.6'\n", &entry);
        Store::new(&dir.data())
            .provision(Tool::OpenCode, &entry, &AnySignature)
            .unwrap();
        crate::test_support::link_fake(two, &entry);
        let installer = Fixing::new(0);
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, installer.clone());
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.toggle_agent("codex");
        runner.toggle_agent("opencode");
        wait_for_choices(&runner, |c| c.key != "opencode" || c.selected);
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Problem);
        let problem = view.problem.clone().unwrap();
        assert!(
            problem.what.contains("OpenCode 2.3.2 is not supported yet"),
            "{problem:?}"
        );
        assert!(
            !view.steps.iter().any(
                |s| s.note.as_deref() == Some("Already set up") && s.label.contains("OpenCode")
            ),
            "{view:?}"
        );
        runner.closed();
        worker.join().unwrap();
    }

    /// [`Fixing`], on a computer without Node.js: installing it is recorded.
    #[cfg(unix)]
    struct NoNode {
        inner: Arc<Fixing>,
        node_installed: std::sync::atomic::AtomicBool,
    }

    #[cfg(unix)]
    impl crate::agent_install::AgentInstaller for NoNode {
        fn install(
            &self,
            tool: Tool,
            home: &Path,
            say: &dyn Fn(&str),
        ) -> Result<crate::agent_install::Installed, crate::agent_install::Failure> {
            assert!(
                tool != Tool::Pi
                    || self
                        .node_installed
                        .load(std::sync::atomic::Ordering::SeqCst),
                "Pi's installer ran before Node.js was there"
            );
            self.inner.install(tool, home, say)
        }

        fn node_for_pi(&self, _home: &Path) -> crate::node_setup::NodeCheck {
            if self
                .node_installed
                .load(std::sync::atomic::Ordering::SeqCst)
            {
                crate::node_setup::NodeCheck::Ready
            } else {
                crate::node_setup::NodeCheck::Missing
            }
        }

        fn install_node(
            &self,
            _home: &Path,
            say: &dyn Fn(&str),
        ) -> Result<(), crate::agent_install::Failure> {
            say("Installing Node.js…");
            self.node_installed
                .store(true, std::sync::atomic::Ordering::SeqCst);
            Ok(())
        }
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn choosing_pi_without_node_says_so_and_installs_node_first() {
        let dir = Dir::new("pi");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let installer = Arc::new(NoNode {
            inner: Fixing::new(0),
            node_installed: std::sync::atomic::AtomicBool::new(false),
        });
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, installer.clone());
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        let view = wait_for_screen(&runner, Screen::Choose);
        let pi = view.choices.iter().find(|c| c.key == "pi").unwrap();
        let note = pi.install_note.as_deref().unwrap();
        assert!(
            note.contains("nodejs.org") && note.contains("Node.js 22"),
            "{note}"
        );
        runner.toggle_agent("codex");
        runner.toggle_agent("pi");
        wait_for_choices(&runner, |c| c.key != "pi" || c.selected);
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        assert!(
            installer
                .node_installed
                .load(std::sync::atomic::Ordering::SeqCst)
        );
        assert_eq!(installer.inner.calls(), [Tool::Pi]);
        assert!(view.agents.iter().any(|a| a.name == "Pi" && a.ready));
        runner.skip();
        worker.join().unwrap();
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn an_install_that_fails_leaves_setup_usable_with_a_retry_and_a_skip() {
        let dir = Dir::new("offline");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let installer = Fixing::new(1);
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, installer.clone());
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Problem);
        let problem = view.problem.clone().unwrap();
        assert!(problem.what.contains("Is this Mac online?"), "{problem:?}");
        assert!(problem.details.contains("curl"));
        assert!(problem.actions.iter().any(|a| matches!(
            a, ProblemAction::ContinueWithout { label } if label == "Skip for now"
        )));
        assert!(
            problem
                .actions
                .iter()
                .any(|a| matches!(a, ProblemAction::OpenPage { .. }))
        );
        // Try again, and this time it works.
        runner.retry();
        let view = wait_for_screen(&runner, Screen::Ready);
        assert_eq!(installer.calls(), [Tool::Codex, Tool::Codex]);
        assert!(view.agents.iter().all(|a| a.ready));
        runner.skip();
        worker.join().unwrap();
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn skipping_a_failed_install_goes_on_without_that_agent() {
        let dir = Dir::new("skip-install");
        let plan = plan_for(&dir, Mode::FirstRun);
        fake_claude(&dir.home());
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, AnySignature, Fixing::new(5));
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        runner.confirm_choice();
        wait_for_screen(&runner, Screen::Problem);
        runner.continue_without();
        let view = wait_for_screen(&runner, Screen::Ready);
        let codex = view
            .steps
            .iter()
            .find(|s| s.label.contains("Codex"))
            .unwrap();
        assert_eq!(codex.state, StepState::Skipped);
        assert_eq!(codex.note.as_deref(), Some("Skipped for now"));
        assert!(
            view.agents
                .iter()
                .any(|a| a.name == "Claude Code" && a.ready)
        );
        assert!(Store::find_installed(Tool::Codex, &dir.home()).is_none());
        runner.skip();
        worker.join().unwrap();
    }

    // The fake agents are shell scripts: Unix only.
    #[cfg(unix)]
    #[test]
    fn a_repair_launch_never_installs_anything() {
        let dir = Dir::new("no-repair-install");
        let plan = plan_for(&dir, Mode::Repair);
        fake_claude(&dir.home());
        let installer = Fixing::new(0);
        let runner = Runner::new();
        drive_with(&plan, &runner, AnySignature, installer.clone())
            .join()
            .unwrap();
        assert!(installer.calls().is_empty());
        assert!(Store::find_installed(Tool::Codex, &dir.home()).is_none());
    }

    /// A fresh first run in an empty throwaway home, with the vendors' real
    /// installers (network, downloads both programs). Run by hand:
    /// `cargo test -p connecto-app --bin connecto-app a_fresh_first_run_installs -- --ignored --nocapture`.
    #[cfg(unix)]
    #[test]
    #[ignore = "downloads from the vendors; run by hand"]
    fn a_fresh_first_run_installs_both_agents_for_real_into_an_isolated_home() {
        let dir = Dir::new("fresh-real");
        let plan = plan_for(&dir, Mode::FirstRun);
        assert!(Store::find_installed(Tool::Claude, &dir.home()).is_none());
        let installer = Arc::new(crate::agent_install::Official::new(&dir.data()));
        let runner = Runner::new();
        let worker = drive_with(&plan, &runner, connecto_store::Codesign, installer);
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        let choose = wait_for_screen(&runner, Screen::Choose);
        for choice in &choose.choices {
            println!(
                "{}: {}",
                choice.name,
                choice.install_note.clone().unwrap_or_default()
            );
        }
        runner.confirm_choice();
        let mut last = String::new();
        let mut view;
        loop {
            view = runner.view();
            let line = format!(
                "{:?} {:?}",
                view.screen,
                view.steps
                    .iter()
                    .map(|s| format!("{}={:?}/{:?}", s.label, s.state, s.note))
                    .collect::<Vec<_>>()
            );
            if line != last {
                println!("{line}");
                last = line;
            }
            if matches!(view.screen, Screen::Ready | Screen::Problem) {
                break;
            }
            std::thread::sleep(Duration::from_millis(500));
        }
        assert_eq!(view.screen, Screen::Ready, "{:?}", view.problem);
        for agent in &view.agents {
            println!(
                "ready: {} {} signed_in={:?}",
                agent.name, agent.status, agent.signed_in
            );
        }
        assert!(view.agents.iter().all(|a| a.ready));
        assert!(Store::new(&dir.data()).newest(Tool::Claude).is_ok());
        assert!(Store::new(&dir.data()).newest(Tool::Codex).is_ok());
        runner.skip();
        worker.join().unwrap();
        assert!(dir.data().join(ONBOARDED).exists());
    }

    /// A Claude Code where the Windows install puts it: a native program
    /// (discovery takes Claude only as one).
    #[cfg(windows)]
    fn windows_claude(home: &Path) {
        fake_claude(home);
    }

    /// Records every install it is asked for, and installs nothing.
    #[cfg(windows)]
    #[derive(Default)]
    struct Counting(std::sync::Mutex<Vec<Tool>>, std::sync::atomic::AtomicUsize);
    #[cfg(windows)]
    impl crate::agent_install::AgentInstaller for Counting {
        fn install_powershell(
            &self,
            say: &dyn Fn(&str),
        ) -> Result<(), crate::agent_install::Failure> {
            self.1.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
            say("Installing with winget…");
            Err(crate::agent_install::Failure {
                what: "Not installed; the terminals use Windows PowerShell".to_owned(),
                next: String::new(),
                details: "winget ended with exit code Some(1)".to_owned(),
            })
        }

        fn install(
            &self,
            tool: Tool,
            _home: &Path,
            _say: &dyn Fn(&str),
        ) -> Result<crate::agent_install::Installed, crate::agent_install::Failure> {
            self.0.lock().unwrap().push(tool);
            Err(crate::agent_install::Failure {
                what: String::new(),
                next: String::new(),
                details: String::new(),
            })
        }
    }

    /// Wait until every choice satisfies `done`.
    fn wait_for_choices(runner: &Runner, done: impl Fn(&AgentChoice) -> bool) {
        for _ in 0..200 {
            if runner.view().choices.iter().all(&done) {
                return;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        panic!("the choices did not change: {:?}", runner.view().choices);
    }

    // Windows (M3): the first launch offers both agents as on macOS, with the
    // vendors' installers when they are missing. Left out, nothing is set up
    // or installed, and a new project starts with no agent terminal.
    #[cfg(windows)]
    #[test]
    fn on_windows_the_first_launch_offers_the_agents_and_installs_nothing_left_out() {
        let dir = Dir::new("windows-first");
        let plan = plan_for(&dir, Mode::FirstRun);
        let installer = Arc::new(Counting::default());
        let runner = Runner::new();
        let worker = drive_with(
            &plan,
            &runner,
            AnySignature,
            Arc::clone(&installer) as Arc<dyn crate::agent_install::AgentInstaller>,
        );
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        let view = wait_for_screen(&runner, Screen::Choose);
        // PowerShell 7 is offered, chosen, only where it is missing.
        let pwsh_missing = !crate::platform::has_powershell_7();
        let pwsh_card = view.choices.iter().find(|c| c.key == POWERSHELL_KEY);
        assert_eq!(pwsh_card.is_some(), pwsh_missing, "{:?}", view.choices);
        if let Some(card) = pwsh_card {
            assert!(card.selected && card.install_note.is_some(), "{card:?}");
        }
        let agents: Vec<_> = view
            .choices
            .iter()
            .filter(|c| c.key != POWERSHELL_KEY)
            .collect();
        assert_eq!(agents.len(), 4);
        for choice in &agents {
            // Claude Code and Codex are chosen to begin with.
            assert_eq!(
                choice.selected,
                matches!(choice.key.as_str(), "claude" | "codex"),
                "{choice:?}"
            );
            assert!(choice.about.contains("coding agent"), "{choice:?}");
            // Not in this test's home: the official installer is announced.
            assert!(!choice.found, "{choice:?}");
            let note = choice.install_note.as_deref().unwrap_or_default();
            assert!(note.contains("official installer"), "{choice:?}");
        }
        runner.toggle_agent("claude");
        wait_for_choices(&runner, |c| c.key != "claude" || !c.selected);
        runner.toggle_agent("codex");
        wait_for_choices(&runner, |c| c.key != "codex" || !c.selected);
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        assert!(
            view.steps
                .iter()
                .all(|s| !s.label.contains("Claude") && !s.label.contains("Codex")),
            "{:?}",
            view.steps
        );
        // A failed PowerShell 7 install is said, and does not stop the setup.
        let pwsh_step = view.steps.iter().find(|s| s.label.contains("PowerShell"));
        assert_eq!(pwsh_step.is_some(), pwsh_missing, "{:?}", view.steps);
        if let Some(step) = pwsh_step {
            assert_eq!(step.state, StepState::Skipped);
            assert!(
                step.note
                    .as_deref()
                    .unwrap_or_default()
                    .contains("Windows PowerShell")
            );
        }
        assert_eq!(
            installer.1.load(std::sync::atomic::Ordering::SeqCst),
            usize::from(pwsh_missing)
        );
        assert!(view.agents.is_empty() && !view.signin_wanted);
        assert!(connecto_workspace::saved_connecto_home(&dir.data()).is_some());
        let folder = dir.0.join("work");
        std::fs::create_dir_all(&folder).unwrap();
        runner.open_folder(folder.clone());
        worker.join().unwrap();
        assert_eq!(runner.outcome().unwrap().open, Some(folder));
        assert!(installer.0.lock().unwrap().is_empty(), "nothing installed");
        let store = Store::new(&dir.data());
        assert!(store.newest(Tool::Claude).is_err() && store.newest(Tool::Codex).is_err());
        assert!(dir.data().join(ONBOARDED).exists());
        assert!(chosen_agents(&dir.data()).is_empty());
    }

    // Windows (M3): an agent found on this PC and not yet in the store is set
    // up, as on macOS, unless the user skipped it.
    #[cfg(windows)]
    #[test]
    fn on_windows_a_found_agent_is_set_up_unless_skipped() {
        let dir = Dir::new("windows-repair");
        std::fs::create_dir_all(dir.home()).unwrap();
        std::fs::create_dir_all(dir.data()).unwrap();
        windows_claude(&dir.home());
        let store = Store::new(&dir.data());
        assert_eq!(to_set_up(&store, &dir.home(), &[]), [Tool::Claude]);
        assert!(to_set_up(&store, &dir.home(), &[Tool::Claude]).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_powershell_7_can_be_left_out_and_is_then_not_installed() {
        if crate::platform::has_powershell_7() {
            return; // Not offered where it is installed.
        }
        let dir = Dir::new("windows-no-pwsh");
        let plan = plan_for(&dir, Mode::FirstRun);
        let installer = Arc::new(Counting::default());
        let runner = Runner::new();
        let worker = drive_with(
            &plan,
            &runner,
            AnySignature,
            Arc::clone(&installer) as Arc<dyn crate::agent_install::AgentInstaller>,
        );
        wait_for_screen(&runner, Screen::Welcome);
        runner.start();
        wait_for_screen(&runner, Screen::Choose);
        for key in [POWERSHELL_KEY, "claude", "codex"] {
            runner.toggle_agent(key);
            wait_for_choices(&runner, |c| c.key != key || !c.selected);
        }
        runner.confirm_choice();
        let view = wait_for_screen(&runner, Screen::Ready);
        assert!(view.steps.iter().all(|s| !s.label.contains("PowerShell")));
        runner.skip();
        worker.join().unwrap();
        assert_eq!(installer.1.load(std::sync::atomic::Ordering::SeqCst), 0);
    }
}
