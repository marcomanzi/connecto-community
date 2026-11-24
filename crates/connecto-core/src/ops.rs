//! Project operations (ADR 0031, `docs/specs/project-operations.md`): routine
//! requests written down once as an approved script or recipe in
//! `.connecto/ops/<slug>/`, run from the project's Operations dialog, by an
//! agent with `connecto op run`, or by the prompt hook.
//!
//! The UI holds an [`OpsControl`]; the bridge (`connecto-session`) holds an
//! [`OpsBridge`]. `connecto-app` implements both.

use std::path::PathBuf;

use crate::ToolKind;

/// Longest `connecto op llm` prompt, in bytes. A longer one is refused,
/// never cut (the `connecto` command checks it too).
pub const LLM_PROMPT_MAX: usize = 32 * 1024;

/// What an operation is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OpKind {
    /// A shell script: no model at all.
    Script,
    /// A shell script that asks a small model for one step (`connecto op llm`).
    ScriptLlm,
    /// Steps an agent follows: too variable to script.
    Recipe,
}

impl OpKind {
    /// The word in `op.json`.
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Script => "script",
            Self::ScriptLlm => "script+llm",
            Self::Recipe => "recipe",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [Self::Script, Self::ScriptLlm, Self::Recipe]
            .into_iter()
            .find(|k| k.word() == word)
    }

    /// What the dialog shows.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Script => "Script",
            Self::ScriptLlm => "Script + small model",
            Self::Recipe => "Recipe for an agent",
        }
    }
}

/// What an operation may do, as its author labelled it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Danger {
    /// Reads only.
    None,
    /// Changes the working tree or local state.
    Changes,
    /// Reaches anything outside this machine.
    Deploys,
    /// Removes data that is not easily recovered.
    Deletes,
}

impl Danger {
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::None => "none",
            Self::Changes => "changes",
            Self::Deploys => "deploys",
            Self::Deletes => "deletes",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [Self::None, Self::Changes, Self::Deploys, Self::Deletes]
            .into_iter()
            .find(|d| d.word() == word)
    }

    /// Whether this danger always asks the person first.
    #[must_use]
    pub fn always_asks(self) -> bool {
        matches!(self, Self::Deploys | Self::Deletes)
    }

    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::None => "Reads only",
            Self::Changes => "Changes local files",
            Self::Deploys => "Reaches outside this machine",
            Self::Deletes => "Deletes data",
        }
    }
}

/// Where an operation stands for running.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpStanding {
    /// Approved, and its files are the ones approved.
    Approved,
    /// Never approved.
    Proposed,
    /// Approved once; the files changed since. It does not run.
    Changed,
    /// The files do not pass the checks; the reason.
    Invalid(String),
}

impl OpStanding {
    #[must_use]
    pub fn runs(&self) -> bool {
        matches!(self, Self::Approved)
    }

    #[must_use]
    pub fn word(&self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Proposed => "proposed",
            Self::Changed => "changed since approved",
            Self::Invalid(_) => "invalid",
        }
    }
}

/// One named argument.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpArg {
    pub name: String,
    pub description: String,
    pub required: bool,
}

/// What the runs of one operation add up to.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpStats {
    pub runs: u32,
    pub failures: u32,
    pub average_ms: u64,
    /// The last run: when it started (ms since the epoch) and how it ended.
    pub last: Option<(u64, String)>,
    /// From an agent's `connecto op run`.
    pub by_agent: u32,
    /// Prompts the hook answered with a suggestion of it.
    pub suggested: u32,
    /// Prompts it ran in place of (the agent never saw them).
    pub intercepted: u32,
}

/// One operation as the UI shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpRow {
    pub slug: String,
    /// The name, or the slug when the files cannot be read.
    pub name: String,
    pub description: String,
    pub kind: Option<OpKind>,
    pub danger: Option<Danger>,
    /// It asks the person before it runs (`confirm`, or a danger that
    /// always asks).
    pub asks_first: bool,
    pub standing: OpStanding,
    /// The person let the hook run it in place of the prompt.
    pub intercept: bool,
    pub phrases: Vec<String>,
    pub args: Vec<OpArg>,
    /// The whole script or recipe, for review.
    pub body: String,
    /// `op.json` exactly as read, empty when it could not be: with `body`,
    /// the bytes `hash` covers, so a peer shown the operation checks the
    /// hash it approves against what it shows.
    pub spec: String,
    /// The approval hash of the files shown: an approval or a run from the
    /// UI names it, so only what the person saw is approved or run.
    pub hash: String,
    pub timeout_s: u64,
    pub stats: OpStats,
}

impl OpRow {
    /// Whether the hook could ever run it in place of a prompt.
    #[must_use]
    pub fn can_intercept(&self) -> bool {
        !self.asks_first
            && self.standing.runs()
            && matches!(self.kind, Some(OpKind::Script | OpKind::ScriptLlm))
    }
}

/// What the prompt hook does with a project's operations.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum HookMode {
    Off,
    #[default]
    Suggest,
}

impl HookMode {
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Off => "off",
            Self::Suggest => "suggest",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [Self::Off, Self::Suggest]
            .into_iter()
            .find(|m| m.word() == word)
    }
}

/// A project's operations, for the dialog and Project Settings.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct OpsView {
    pub project: String,
    /// The project folder could be read.
    pub available: bool,
    /// Why the operations cannot be shown, in place of the usual line
    /// (a host that cannot be asked, or one too old to share them).
    pub unavailable: Option<String>,
    pub ops: Vec<OpRow>,
    pub hook: HookMode,
    pub weekly: bool,
    /// Time for the weekly suggestion to look for operations.
    pub suggest_discovery: bool,
    /// The discovery terminal, while it is open.
    pub discovery_terminal: Option<String>,
    /// When the last discovery started (ms since the epoch).
    pub last_discovery_ms: Option<u64>,
    /// Prompts recorded since the last discovery.
    pub prompts_since_discovery: u32,
    /// The Jev key is set: the hook can confirm a match and intercept.
    pub jev: bool,
    /// The last thing an action said: (text, is an error).
    pub notice: Option<(String, bool)>,
}

impl OpsView {
    /// Proposed or changed operations waiting for the person.
    #[must_use]
    pub fn waiting(&self) -> usize {
        self.ops
            .iter()
            .filter(|op| matches!(op.standing, OpStanding::Proposed | OpStanding::Changed))
            .count()
    }

    #[must_use]
    pub fn approved(&self) -> usize {
        self.ops.iter().filter(|op| op.standing.runs()).count()
    }
}

/// What the person asks of a project's operations.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpsAction {
    /// Approve the files with this hash, the ones the person was shown.
    Approve {
        slug: String,
        hash: String,
    },
    /// Take the approval back: it no longer runs.
    Withdraw(String),
    /// Remove the operation's folder.
    Delete(String),
    SetIntercept(String, bool),
    /// Run it in a new shell terminal (a recipe: in a new agent terminal),
    /// with these arguments, if its files still have this hash. The dialog
    /// has already asked when it asks first.
    Run {
        slug: String,
        hash: String,
        args: Vec<String>,
    },
    /// Open an agent terminal to fix an operation whose last run failed.
    Fix(String),
    /// Open the discovery terminal.
    Discover,
    /// Hide the weekly suggestion for a few days.
    Later,
    SetHook(HookMode),
    SetWeekly(bool),
}

pub trait OpsControl: Send + Sync {
    fn view(&self, project: &str) -> OpsView;
    fn submit(&self, project: String, action: OpsAction);

    /// `action`, done before returning, and what it came to (also kept as
    /// the view's notice): how a remote peer's action reaches this machine.
    /// Never call it from the window's thread.
    ///
    /// # Errors
    /// Why it was not done.
    fn act(&self, project: &str, action: OpsAction) -> Result<String, String> {
        let _ = (project, action);
        Err("Project operations are not available in this Connecto.".to_owned())
    }
}

/// The UI's stand-in when operations did not start.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOps;

impl OpsControl for NoOps {
    fn view(&self, project: &str) -> OpsView {
        OpsView {
            project: project.to_owned(),
            ..OpsView::default()
        }
    }

    fn submit(&self, _project: String, _action: OpsAction) {}
}

// ---- the bridge side ------------------------------------------------------

/// Who runs `connecto op`, as the bridge found it from the process tree.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsCaller {
    pub terminal: String,
    pub tool: ToolKind,
    /// The relay's project id.
    pub project_id: String,
    /// The terminal's project folder.
    pub folder: PathBuf,
    /// For a prompt: the hook can hold an intercept and confirm it
    /// (prepare, then go). An older hook cannot: it gets a suggestion.
    pub holds: bool,
}

/// A script for the `connecto` command to run in the caller's terminal.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpsExec {
    /// Handed back with `connecto op done`.
    pub run: String,
    pub program: String,
    pub args: Vec<String>,
    pub cwd: PathBuf,
    pub env: Vec<(String, String)>,
    pub timeout_s: u64,
}

/// The answer to one `connecto op` command.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum OpsAnswer {
    /// Text to print.
    Text(String),
    Refused {
        code: String,
        reason: String,
    },
    Exec(OpsExec),
}

/// What the prompt hook does with one prompt.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub enum PromptAdvice {
    #[default]
    Nothing,
    /// One line for the agent's context.
    Suggest(String),
    /// Stop the prompt, once the hook confirms with `connecto op intercept
    /// <ticket>`: only then is the operation started. Until that, nothing
    /// runs, and a hold the hook never confirms expires. The reason is shown
    /// to the person.
    Intercept { reason: String, ticket: String },
}

pub trait OpsBridge: Send + Sync {
    /// One `connecto op <verb>` command. `text` is stdin for `llm`.
    fn command(
        &self,
        caller: &OpsCaller,
        verb: &str,
        args: &[String],
        text: Option<&str>,
    ) -> OpsAnswer;

    /// Whether the project has an approved operation (for the launch text).
    fn has_ops(&self, project_id: &str, folder: &std::path::Path) -> bool;

    /// A person's prompt in one of the project's terminals, before the
    /// agent sees it. Must answer within a few seconds.
    fn on_prompt(&self, caller: &OpsCaller, prompt: &str) -> PromptAdvice;
}

/// The bridge used when operations are not wired in.
#[derive(Debug, Default, Clone, Copy)]
pub struct NoOpsBridge;

impl OpsBridge for NoOpsBridge {
    fn command(
        &self,
        _caller: &OpsCaller,
        _verb: &str,
        _args: &[String],
        _text: Option<&str>,
    ) -> OpsAnswer {
        OpsAnswer::Refused {
            code: "not_available".to_owned(),
            reason: "Project operations are not available in this Connecto.".to_owned(),
        }
    }

    fn has_ops(&self, _project_id: &str, _folder: &std::path::Path) -> bool {
        false
    }

    fn on_prompt(&self, _caller: &OpsCaller, _prompt: &str) -> PromptAdvice {
        PromptAdvice::Nothing
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn words_round_trip() {
        for kind in [OpKind::Script, OpKind::ScriptLlm, OpKind::Recipe] {
            assert_eq!(OpKind::from_word(kind.word()), Some(kind));
        }
        for danger in [
            Danger::None,
            Danger::Changes,
            Danger::Deploys,
            Danger::Deletes,
        ] {
            assert_eq!(Danger::from_word(danger.word()), Some(danger));
        }
        for mode in [HookMode::Off, HookMode::Suggest] {
            assert_eq!(HookMode::from_word(mode.word()), Some(mode));
        }
        assert_eq!(OpKind::from_word("Script"), None);
    }

    #[test]
    fn only_deploys_and_deletes_always_ask() {
        assert!(!Danger::None.always_asks());
        assert!(!Danger::Changes.always_asks());
        assert!(Danger::Deploys.always_asks());
        assert!(Danger::Deletes.always_asks());
    }

    #[test]
    fn a_recipe_or_an_asking_op_is_never_intercepted() {
        let mut row = OpRow {
            slug: "tests".into(),
            name: "Tests".into(),
            description: String::new(),
            kind: Some(OpKind::Script),
            danger: Some(Danger::None),
            asks_first: false,
            standing: OpStanding::Approved,
            intercept: true,
            phrases: Vec::new(),
            args: Vec::new(),
            body: String::new(),
            spec: String::new(),
            hash: String::new(),
            timeout_s: 600,
            stats: OpStats::default(),
        };
        assert!(row.can_intercept());
        row.asks_first = true;
        assert!(!row.can_intercept());
        row.asks_first = false;
        row.kind = Some(OpKind::Recipe);
        assert!(!row.can_intercept());
        row.kind = Some(OpKind::Script);
        row.standing = OpStanding::Changed;
        assert!(!row.can_intercept());
    }
}
