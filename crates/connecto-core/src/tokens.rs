//! Agent Token Optimization (docs/specs/token-optimization.md): what the
//! project's past Claude sessions spent, what enters every session without
//! being used, and the cuts the user applies to new Claude terminals.
//!
//! The UI reads [`TokenControl::view`] (an `Arc`, never blocking) and calls
//! its commands, which return at once: the work runs on a thread of the host.
//! Nothing here is money: only tokens and shares.

use std::sync::Arc;

/// The choices of "Compact earlier" (§7), in tokens. `None` is Claude's own
/// default.
pub const AUTOCOMPACT_CHOICES: [u32; 3] = [150_000, 200_000, 300_000];

/// The project setting `token_optimization` (§5.1).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TokenSettings {
    /// Every id of each plugin turned off (`<name>@synced`,
    /// `<name>@<marketplace>`), for `enabledPlugins`.
    pub plugins_off: Vec<String>,
    /// Skill names for `skillOverrides: "off"`.
    pub skills_off: Vec<String>,
    /// `--autocompact <N>`; `None` leaves Claude's default.
    pub autocompact: Option<u32>,
    /// When the cuts were applied (RFC 3339, UTC).
    pub applied_at: Option<String>,
    /// The start context measured when they were applied.
    pub before: Option<Before>,
}

impl TokenSettings {
    /// Whether any cut is set (the lists, not the compaction).
    #[must_use]
    pub fn has_cuts(&self) -> bool {
        !self.plugins_off.is_empty() || !self.skills_off.is_empty()
    }
}

/// The start context before the cuts (§6).
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct Before {
    pub start_context_median: u64,
    pub sessions: u32,
}

/// Where the tokens went over the sessions read (§3.2). Shares are 0–100.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Totals {
    pub sessions: usize,
    pub calls: usize,
    /// Of all input (fresh, cache writes, cache reads), the share read from
    /// the cache.
    pub cache_read_share: f64,
    pub median_context: u64,
    pub median_start_context: u64,
    /// Of the cache reads, the share made by calls above 200k of context.
    pub reads_above_200k: f64,
    pub cold_rebuilds: usize,
    pub compactions: usize,
    /// Relative cost (0.1× read, 2× write 1 h, 1.25× write 5 min, 5× output,
    /// 1× fresh input), as shares of the whole.
    pub cost_reads: f64,
    pub cost_writes: f64,
    pub cost_output: f64,
    pub cost_input: f64,
    /// Set when the 200 MB reading limit left older sessions out.
    pub limited: Option<String>,
}

/// What kind of thing a proposal is about.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProposalKind {
    /// Turned off with `enabledPlugins`.
    Plugin,
    /// Turned off with `skillOverrides`.
    Skill,
    /// Advice only: Connecto does not write `~/.claude.json`.
    McpServer,
    /// Advice only: a large memory or instructions file.
    MemoryFile,
}

impl ProposalKind {
    /// Whether Connecto can apply it (plugins and skills).
    #[must_use]
    pub const fn applicable(self) -> bool {
        matches!(self, Self::Plugin | Self::Skill)
    }
}

/// One thing loaded into every session and never used (§3.3).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub kind: ProposalKind,
    /// The plugin, skill or server name, or the file's path.
    pub name: String,
    /// What goes into the settings: every id of a plugin, the skill's name.
    /// Empty for advice.
    pub ids: Vec<String>,
    /// Characters / 4: an estimate.
    pub tokens: u64,
    /// "used 0 times in 23 sessions".
    pub evidence: String,
    /// Ticked when the list first shows.
    pub preselected: bool,
    /// How to do it by hand, for advice.
    pub hint: Option<String>,
}

/// Whether the cuts show in the first session after them (§6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Confirmation {
    /// No session after the cuts yet.
    NotYet,
    /// The transcript lists none of what was cut.
    Confirmed,
    /// What the transcript still lists.
    StillLoaded(Vec<String>),
    /// The transcript has no record of what was loaded.
    NotRecorded,
}

/// The sessions started after `applied_at` (§6).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct After {
    pub sessions: usize,
    pub start_context_median: Option<u64>,
    pub confirmation: Confirmation,
}

/// "Compact earlier" (§7).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct CompactEstimate {
    /// The share of cache reads made by calls above 200k.
    pub reads_above_200k: f64,
    /// For each of [`AUTOCOMPACT_CHOICES`], the estimated share of reads
    /// saved (0–100).
    pub savings: Vec<(u32, f64)>,
}

/// One finding of the biggest-session study (§4).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StudyFinding {
    pub title: String,
    pub evidence: String,
    pub advice: String,
}

/// The biggest-session study.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub enum StudyState {
    #[default]
    NotRun,
    Running,
    Done(Vec<StudyFinding>),
    /// "study unavailable" and why.
    Unavailable(String),
}

/// What an analysis found.
#[derive(Debug, Clone, PartialEq)]
pub struct TokenReport {
    /// When it ran (ms since the epoch).
    pub analyzed_ms: u64,
    pub totals: Totals,
    /// Skills, MCP tools, agents and memory files loaded in the latest
    /// session, as counts; `None` when the transcript does not record them.
    pub loaded: Option<Loaded>,
    pub proposals: Vec<Proposal>,
    pub compact: CompactEstimate,
    pub after: Option<After>,
    /// The biggest session: its start, calls and cache reads.
    pub biggest: Option<String>,
}

/// What entered the latest session, as counts and estimated tokens.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Loaded {
    pub skills: usize,
    pub skill_tokens: u64,
    pub mcp_tools: usize,
    pub mcp_tokens: u64,
    pub agents: usize,
    pub agent_tokens: u64,
    pub memory_files: usize,
    pub memory_tokens: u64,
}

/// One project's state.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ProjectTokenView {
    pub project_id: String,
    pub settings: TokenSettings,
    /// What runs now ("reading 23 sessions…"); `None` when idle.
    pub busy: Option<String>,
    pub report: Option<TokenReport>,
    pub study: StudyState,
    /// Why the last command failed.
    pub error: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct TokenView {
    pub projects: Vec<ProjectTokenView>,
    /// Whether the feature is available in this build.
    pub available: bool,
}

impl TokenView {
    #[must_use]
    pub fn project(&self, id: &str) -> Option<&ProjectTokenView> {
        self.projects.iter().find(|p| p.project_id == id)
    }
}

const UNAVAILABLE: &str = "token optimization is not available in this build";

/// Behind the Tokens tab. Every command returns at once.
pub trait TokenControl: Send + Sync {
    /// Never blocks.
    fn view(&self) -> Arc<TokenView>;

    /// Load the project's settings into the view, if not there yet. Cheap.
    fn open(&self, project_id: &str) {
        let _ = project_id;
    }

    /// Read the past sessions, then study the biggest one.
    fn analyze(&self, project_id: &str) -> Result<(), String> {
        let _ = project_id;
        Err(UNAVAILABLE.to_owned())
    }

    /// Turn these off for Claude terminals started from now on.
    fn apply(
        &self,
        project_id: &str,
        plugins_off: Vec<String>,
        skills_off: Vec<String>,
    ) -> Result<(), String> {
        let _ = (project_id, plugins_off, skills_off);
        Err(UNAVAILABLE.to_owned())
    }

    /// Empty the lists (from new terminals on).
    fn undo(&self, project_id: &str) -> Result<(), String> {
        let _ = project_id;
        Err(UNAVAILABLE.to_owned())
    }

    /// "Compact earlier": `None` is Claude's default.
    fn set_autocompact(&self, project_id: &str, tokens: Option<u32>) -> Result<(), String> {
        let _ = (project_id, tokens);
        Err(UNAVAILABLE.to_owned())
    }
}

/// No token optimization (tests, builds without the host).
pub struct NoTokens;

impl TokenControl for NoTokens {
    fn view(&self) -> Arc<TokenView> {
        Arc::new(TokenView::default())
    }
}
