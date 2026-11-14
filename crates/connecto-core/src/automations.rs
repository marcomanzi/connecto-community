//! The contract between the UI and the automations engine.
//!
//! The UI reads [`AutomationsControl::view`] each frame (a cheap `Arc` clone)
//! and calls the commands below. Every method returns promptly and touches no
//! disk, no Keychain and no network: work happens on the engine's own threads
//! and its result shows in the next view (`status.last_error` for failures).
//! The `Wake` the engine was built with is called whenever the view changes.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::SystemTime;

use crate::ToolKind;

/// Whether a Jev API key is stored.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum KeyStatus {
    /// Not yet read from the Keychain (first moments of the run).
    #[default]
    Unknown,
    Missing,
    Present,
    /// The Keychain could not be read or written.
    Unavailable(String),
}

/// One automation, as the screen lists it.
#[derive(Debug, Clone, PartialEq)]
pub struct AutomationItem {
    /// Folder name under `<data>/automations/`; the id for every command.
    pub slug: String,
    pub name: String,
    pub enabled: bool,
    pub agent: ToolKind,
    /// `choice` or `noul`.
    pub question_type: String,
    pub instructions: String,
    /// The choice name that fires (`true` for noul).
    pub matches: String,
    pub keys: Vec<String>,
    pub threshold: f64,
    pub model: String,
    /// The file is still the template with TODO text: it will not be enabled.
    pub unfinished: bool,
    /// `automation.json` does not validate: the reasons. Such an automation is
    /// listed (so it can be reopened and fixed) and never runs.
    pub errors: Vec<String>,
    /// How the automation's latest check ended, if it has been checked.
    pub last: Option<LastCheck>,
    /// The phrases that must all be on a screen before Jev is asked about it
    /// (empty: not set).
    pub trigger: Vec<String>,
    /// The trigger is missing or invalid: the automation is never sent to Jev
    /// until its session adds one.
    pub needs_trigger: bool,
}

impl Default for AutomationItem {
    fn default() -> Self {
        Self {
            slug: String::new(),
            name: String::new(),
            enabled: false,
            agent: ToolKind::Claude,
            question_type: String::new(),
            instructions: String::new(),
            matches: String::new(),
            keys: Vec::new(),
            threshold: 0.8,
            model: String::new(),
            unfinished: false,
            errors: Vec::new(),
            last: None,
            trigger: Vec::new(),
            needs_trigger: false,
        }
    }
}

/// The latest result of checking one automation against a screen. Kept across
/// restarts (it is rebuilt from the history file).
#[derive(Debug, Clone, PartialEq)]
pub struct LastCheck {
    pub at: SystemTime,
    /// The terminal that was checked (its id).
    pub terminal: String,
    pub outcome: AutomationOutcome,
    /// What Jev answered, if it did. For an error, a short reason instead
    /// ("401 unauthorized", "429 rate limited", "timeout").
    pub answer: Option<String>,
    pub confidence: Option<f32>,
    /// How many checks in a row ended the same way (same outcome and answer),
    /// this one included: "no match x4".
    pub repeat: u32,
}

/// What the engine is doing.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EngineState {
    #[default]
    Starting,
    /// No API key: nothing is evaluated.
    NoKey,
    /// No enabled automation is valid: nothing to ask.
    NoAutomations,
    /// Connecto is paused: nothing is evaluated or typed.
    Paused,
    Watching,
}

#[derive(Debug, Clone, PartialEq, Default)]
pub struct EngineStatus {
    pub state: EngineState,
    /// The latest failure (Jev, Keychain, disk), cleared by the next success.
    pub last_error: Option<String>,
    /// Jev calls running right now.
    pub in_flight: usize,
    /// Jev calls made since the app started.
    pub calls: u64,
}

/// How a Jev call ended for one automation.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum AutomationOutcome {
    /// Matched at or above the threshold: the keys were typed.
    Fired,
    /// Matched below the threshold: a confirmation was raised.
    Asked,
    Approved,
    Dismissed,
    /// A confirmation that was dropped because the screen changed.
    Expired,
    #[default]
    NoMatch,
    /// Jev failed, or the keys were refused. The record carries the message.
    Error,
}

impl AutomationOutcome {
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Fired => "fired",
            Self::Asked => "asked",
            Self::Approved => "approved",
            Self::Dismissed => "dismissed",
            Self::Expired => "expired",
            Self::NoMatch => "no-match",
            Self::Error => "error",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [
            Self::Fired,
            Self::Asked,
            Self::Approved,
            Self::Dismissed,
            Self::Expired,
            Self::NoMatch,
            Self::Error,
        ]
        .into_iter()
        .find(|o| o.word() == word)
    }
}

/// One line of the history.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct HistoryRecord {
    /// Stable: the same entry has the same id after a restart.
    pub id: u64,
    pub time_ms: u64,
    pub project: String,
    pub terminal_id: String,
    pub terminal_name: Option<String>,
    /// Slug of the automation (empty for a call-level error).
    pub automation: String,
    /// What Jev answered (choice name, or `true`/`false`).
    pub answer: Option<String>,
    pub confidence: Option<f64>,
    pub outcome: AutomationOutcome,
    pub message: Option<String>,
    pub latency_ms: u64,
    pub input_tokens: u64,
    pub output_tokens: u64,
    /// The start of the screen text that was sent, truncated.
    pub screen: String,
    /// The exact JSON body sent to Jev (model, state, questions; never the API
    /// key or a header; the state is the redacted screen text), capped at
    /// about 64 KiB. One call serves every automation it asked about, so those
    /// entries share it. Only in [`AutomationsControl::history_for`], never in
    /// the view's own list.
    pub request: Option<String>,
    /// The raw JSON answer, or the status and body of a refusal, or the network
    /// error; capped like `request`. Same sharing and same place.
    pub response: Option<String>,
}

/// A match below the threshold, waiting for the user.
#[derive(Debug, Clone, PartialEq)]
pub struct PendingConfirmation {
    pub id: u64,
    pub created_ms: u64,
    pub project_id: String,
    pub project: String,
    pub terminal_id: String,
    pub terminal_name: Option<String>,
    pub slug: String,
    pub automation: String,
    pub keys: Vec<String>,
    pub confidence: f64,
    pub threshold: f64,
    /// The screen text the answer was for (truncated).
    pub screen: String,
}

/// Everything the Automations screen shows.
/// A named set of automations that is switched on and off as a whole, opened
/// to show its members, each of which can still be switched alone. Groups are
/// data that ships with the app (a later release can add more, for more agents);
/// a member is named by its folder (`slug`), so an automation already on disk
/// belongs to its group without being rewritten.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AutomationGroup {
    pub id: String,
    pub title: String,
    /// One line for a newcomer: what the group is for.
    pub about: String,
    pub agent: ToolKind,
    /// The slugs of its automations that exist, in the order they ship (for a
    /// custom group, the order they were put in).
    pub members: Vec<String>,
    /// Made by the user: it can be renamed and deleted, and automations can be
    /// moved in and out. False for the groups that ship, whose members are
    /// fixed. For a custom group `about` is empty and `agent` is a placeholder
    /// (`Claude`): show neither.
    pub custom: bool,
}

#[derive(Debug, Clone, Default)]
pub struct AutomationsView {
    /// Bumped on every change; compare to skip work.
    pub revision: u64,
    /// The Automations folder, once the user has chosen one.
    pub root: Option<PathBuf>,
    pub key: KeyStatus,
    pub automations: Vec<AutomationItem>,
    /// The shipped groups that have at least one member in the folder, then the
    /// user's groups in the order they were made (empty ones included).
    pub groups: Vec<AutomationGroup>,
    pub pending: Vec<PendingConfirmation>,
    /// Newest first, at most [`RECENT_HISTORY`].
    pub history: Arc<Vec<HistoryRecord>>,
    pub status: EngineStatus,
}

/// How many history records the view carries.
pub const RECENT_HISTORY: usize = 200;

pub trait AutomationsControl: Send + Sync {
    fn view(&self) -> Arc<AutomationsView>;

    /// Store the Jev API key in the macOS Keychain. The text is never kept
    /// in the view, logged or written to disk.
    fn set_api_key(&self, key: &str) -> Result<(), String>;

    fn clear_api_key(&self);

    /// Choose the Automations folder. It must be an existing, writable folder,
    /// not the data folder or the home folder. The choice is saved, the store
    /// reloads from it, and when the folder is new or different a visible
    /// Claude terminal opens in it, so the user can log in (one login for all
    /// automations; Codex logs in on its first Codex terminal there). The
    /// folder is checked at once; the rest happens on the engine's thread.
    fn set_root(&self, path: PathBuf) -> Result<(), String> {
        let _ = path;
        Err("automations are not available".to_owned())
    }

    /// Open a Claude or Codex terminal in the Automations folder, to log in
    /// again. `Err("Set the automations folder first")` without a folder.
    fn open_login(&self, tool: ToolKind) -> Result<(), String> {
        let _ = tool;
        Err("automations are not available".to_owned())
    }

    /// Make `<root>/<slug>/`, write its instructions file and template, and
    /// open a visible authoring terminal of `agent` in the Automations folder
    /// (which is one project in the sidebar, with one profile for every
    /// automation). `Err("Set the automations folder first")` without a folder. The name is checked at once;
    /// the new automation then appears in the view (its slug is the folder
    /// name, which may differ from the name) and a failure shows in
    /// `status.last_error`.
    fn create(&self, name: &str, agent: ToolKind) -> Result<(), String>;

    /// As [`AutomationsControl::create`], from a description instead of a
    /// screenshot: what the terminal shows (`screen`) and what should happen
    /// (`action`), both required and each cut at about 4 KiB. The new session's
    /// first message carries them as quoted user data and asks the agent to
    /// write the automation and show it for confirmation.
    fn create_described(
        &self,
        name: &str,
        tool: ToolKind,
        screen: &str,
        action: &str,
    ) -> Result<(), String> {
        let _ = (name, tool, screen, action);
        Err("automations are not available".to_owned())
    }

    /// Show an authoring terminal of the automation's agent, starting one if
    /// none is open. `Err("Set the automations folder first")` without a folder.
    fn open_session(&self, slug: &str) -> Result<(), String>;

    fn set_enabled(&self, slug: &str, enabled: bool);

    /// Close its session, remove the folder.
    fn delete(&self, slug: &str);

    /// Clamped to 0.5 (exclusive) .. 1.0.
    fn set_threshold(&self, slug: &str, threshold: f64);

    /// Type the keys now (refused while paused) and drop the confirmation.
    fn approve(&self, pending: u64);

    fn dismiss(&self, pending: u64);

    /// Forget the history.
    fn clear_history(&self);

    /// Forget one automation's history: its records (request and response
    /// included) leave the history file, and its latest result is reset to
    /// "not checked yet". Other automations keep theirs, even those that shared
    /// a Jev call with it. An unknown name changes nothing.
    fn clear_history_for(&self, slug: &str) -> Result<(), String> {
        let _ = slug;
        Err("automations are not available".to_owned())
    }

    /// The history of one automation, newest first, with what was sent to Jev
    /// and what came back. Reads the history file: call it when a detail is
    /// opened, not every frame.
    fn history_for(&self, slug: &str) -> Vec<HistoryRecord> {
        let _ = slug;
        Vec::new()
    }

    /// Make a group of the user's own and return its id. The title is trimmed;
    /// `Err` for an empty one, one over 80 characters, or one that equals (ignoring
    /// case) the title of any existing group. The group is shown, empty.
    fn create_group(&self, title: &str) -> Result<String, String> {
        let _ = title;
        Err("automations are not available".to_owned())
    }

    /// Rename a custom group; the same checks as `create_group` (its own title
    /// is not a clash). `Err` for an unknown or built-in group.
    fn rename_group(&self, id: &str, title: &str) -> Result<(), String> {
        let _ = (id, title);
        Err("automations are not available".to_owned())
    }

    /// Delete a custom group. Its automations are kept, loose. `Err` for an
    /// unknown or built-in group.
    fn delete_group(&self, id: &str) -> Result<(), String> {
        let _ = id;
        Err("automations are not available".to_owned())
    }

    /// Put an automation in a custom group (leaving the one it was in), or with
    /// `None` leave its group. `Err` for an unknown automation or group, for a
    /// built-in group as the target, and for an automation that belongs to a
    /// built-in group (those cannot be moved or removed).
    fn set_group(&self, slug: &str, group: Option<&str>) -> Result<(), String> {
        let _ = (slug, group);
        Err("automations are not available".to_owned())
    }
}
