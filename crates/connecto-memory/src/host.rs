//! What the memory engine needs from the running app (spec §5.1), behind a
//! trait so the engine is tested with [`crate::testing::FakeHost`]. The real
//! one is `connecto-app/src/memory_host.rs`.
//!
//! A model run is an ordinary CLI session of the chosen provider, run in the
//! project's own folder (the host knows it from `project_id`): there is no
//! separate work folder in the job.

use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

use connecto_core::memory::{
    Capability, Cycle, LocalDate, LocalTime, MemorySettings, ModelChoice, Provider, Weekday,
};

/// One call to a model.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelJob {
    pub project_id: String,
    /// The cycle the run belongs to; `None` for a user's question to the
    /// memory ("Ask the memory", [`crate::ask`]), which is no cycle.
    pub cycle: Option<Cycle>,
    pub choice: ModelChoice,
    /// The whole prompt, evidence inside per-run delimiters (spec §5.8).
    pub prompt: String,
}

/// Where the reply text was read from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplySource {
    Hook,
    SessionLog,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelReply {
    pub text: String,
    pub source: ReplySource,
    pub chars_in: u64,
    pub chars_out: u64,
    pub duration: Duration,
}

/// Why a model call gave no reply.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    #[error("the run passed its deadline")]
    Timeout,
    #[error("the run was cancelled")]
    Cancelled,
    #[error("the provider cannot run here: {0}")]
    Unavailable(String),
    /// Held: the CLI does not trust the project folder yet. Not a failure:
    /// it uses no attempt and is not retried in a loop; the work waits
    /// ([`crate::Store::hold_batch`], [`crate::Store::hold_run`]), `reason`
    /// is shown to the user in `status`, and it resumes once the folder is
    /// trusted.
    #[error("held: {reason}")]
    HeldUntrusted { reason: String },
    /// The CLI answered with an error (`API Error: …`) instead of an output.
    #[error("the model answered with an error")]
    ModelError,
    #[error("the run failed: {0}")]
    Failed(String),
}

/// A git fact about one file.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ChangedFile {
    /// Relative to the project root, `/`-separated.
    pub path: String,
    /// Blob hash of the working-tree content; `None` when it is deleted or
    /// when its hash is not known (over a cap, a failed hash): see `deleted`.
    pub blob: Option<String>,
    /// Git or the file system confirmed the file is gone. `blob: None` with
    /// `deleted: false` is *unknown*, never a deletion.
    pub deleted: bool,
}

/// The git state of the project folder (spec D7).
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitSnapshot {
    /// `None` on a detached HEAD.
    pub branch: Option<String>,
    /// `None` before the first commit.
    pub head: Option<String>,
    /// Paths changed in the working tree or index, untracked included.
    pub changed: Vec<ChangedFile>,
}

/// Now, as UTC and as the user's local calendar (for the scheduler).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct WallClock {
    pub utc_ms: u64,
    pub local_date: LocalDate,
    pub local_time: LocalTime,
    pub weekday: Weekday,
    /// Local minus UTC, in seconds.
    pub utc_offset_secs: i32,
}

/// A project open in the app: its id and its real folder.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpenProject {
    pub id: String,
    pub path: PathBuf,
}

/// A reply read back from a tool's own session log (a turn whose reply was
/// too large for the hook, `Reply::Dropped`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct LoggedReply {
    pub text: String,
    /// Byte offset in the log where the piece that held it started.
    pub offset: u64,
}

pub trait Host: Send + Sync {
    /// One model call. Blocks until a reply, the deadline, or `cancel`.
    fn run_model(
        &self,
        job: &ModelJob,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<ModelReply, RunError>;
    /// What this provider/model can do here, or why not.
    fn capability(&self, choice: &ModelChoice) -> Capability;
    /// The git state of the project folder; `None` if it is not a
    /// repository.
    fn git_snapshot(&self, project: &Path) -> Option<GitSnapshot>;
    fn paused(&self) -> bool;
    /// Grows at every pause; a run publishes only if it is unchanged.
    fn pause_epoch(&self) -> u64;
    /// Known secret values, removed first by redaction.
    fn known_secrets(&self) -> Vec<String>;
    fn now(&self) -> WallClock;
    /// The memory view changed.
    fn wake(&self);

    // ---- Added for the engine (T4-T6). Defaults keep a minimal host valid.

    /// The projects open now. The engine runs memory for those among them
    /// whose memory is enabled.
    fn projects(&self) -> Vec<OpenProject> {
        Vec::new()
    }
    /// A project's memory settings (`settings.json`, key `memory`).
    fn load_settings(&self, project_id: &str) -> MemorySettings {
        let _ = project_id;
        MemorySettings::default()
    }
    /// Persist a project's memory settings.
    fn save_settings(&self, project_id: &str, settings: &MemorySettings) -> Result<(), String> {
        let _ = (project_id, settings);
        Ok(())
    }
    /// The reply of the latest turn answering `sent` (or of the latest turn
    /// when `sent` is unknown), read from the tool's session log in pieces.
    /// `None` when the log cannot be read or holds no reply: nothing is made
    /// up.
    fn read_logged_reply(
        &self,
        provider: Provider,
        log: &Path,
        sent: Option<&str>,
    ) -> Option<LoggedReply> {
        let _ = (provider, log, sent);
        None
    }
    /// Blob hashes of the working-tree contents of `paths` (`None` blob:
    /// missing). `None` when git cannot tell. Never writes the repository.
    fn file_blobs(&self, project: &Path, paths: &[String]) -> Option<Vec<ChangedFile>> {
        let _ = (project, paths);
        None
    }
    /// The local branch names; `None` when git cannot tell.
    fn branches(&self, project: &Path) -> Option<Vec<String>> {
        let _ = project;
        None
    }
    /// Cancel every model run in flight (Stop Everything, closing).
    fn cancel_runs(&self) {}

    // ---- Added for the initial build (T9b). Read-only; the defaults say
    // "not available", which the build declares in its coverage.

    /// The project's files, read-only, at most `max`: with git, the tracked
    /// files and the untracked ones git does not ignore; without, a walk that
    /// follows no link and skips dependency and build folders. `None` when
    /// the folder cannot be listed.
    fn project_files(&self, project: &Path, max: usize) -> Option<ProjectFiles> {
        let _ = (project, max);
        None
    }
    /// A project's memory starts (opened with memory on, or turned on).
    fn project_started(&self, project: &Path) {
        let _ = project;
    }
    /// A project's memory stopped (closed, turned off) and none of its
    /// workers runs any more: what the host keeps for it can go, and must
    /// not come back until [`Host::project_started`].
    fn project_stopped(&self, project: &Path) {
        let _ = project;
    }
    /// Memory shuts down: what the host keeps for projects can go.
    fn shutdown(&self) {}
    /// Why git is not used for `project` when a repository was found but
    /// refused (its configuration or layout could make git run a program or
    /// read elsewhere); `None` when git is used or there is no repository.
    fn git_refusal(&self, project: &Path) -> Option<String> {
        let _ = project;
        None
    }
    /// What of the user's own git configuration was not taken for
    /// `project` (empty when everything was, or git is not used).
    fn git_limits(&self, project: &Path) -> Vec<String> {
        let _ = project;
        Vec::new()
    }
    /// Whether this host reads project file contents at all. A host that
    /// cannot open a file without following a link (or a swapped one) says
    /// no, and the build declares it ([`CoverageGap::FilesNotRead`]).
    ///
    /// [`CoverageGap::FilesNotRead`]: connecto_core::memory::CoverageGap::FilesNotRead
    fn reads_files(&self) -> bool {
        true
    }
    /// The text of one project file (`path` relative, `/`-separated), read
    /// only. `None` when it is missing, not a regular file inside the
    /// project (no link is followed out), larger than `max_bytes`, or not
    /// text (binary, not UTF-8).
    fn read_project_file(&self, project: &Path, path: &str, max_bytes: u64) -> Option<String> {
        let _ = (project, path, max_bytes);
        None
    }
    /// Commits made since `since_ms`, newest first, at most `max`. `None`
    /// when the folder is not a repository or git cannot tell.
    fn git_history(&self, project: &Path, since_ms: u64, max: usize) -> Option<GitHistory> {
        let _ = (project, since_ms, max);
        None
    }
    /// The uncommitted changes of `paths` (and only those) against HEAD as a
    /// unified diff, read only. `None` when not a repository, before the
    /// first commit, or when the diff is larger than `max_bytes` (never cut).
    /// A path whose committed version is larger than `max_file_bytes` (or
    /// of unknown size) is left out of the diff: its old content would
    /// otherwise reach a packet that the file itself may not.
    fn git_diff(
        &self,
        project: &Path,
        paths: &[String],
        max_file_bytes: u64,
        max_bytes: u64,
    ) -> Option<String> {
        let _ = (project, paths, max_file_bytes, max_bytes);
        None
    }
    /// The project's past Connecto conversations that ended after
    /// `since_ms`, newest first. `None` when the history cannot be read.
    fn past_conversations(&self, project_id: &str, since_ms: u64) -> Option<PastConversations> {
        let _ = (project_id, since_ms);
        None
    }
}

/// One file of the project, as listed for the initial build.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProjectFile {
    /// Relative to the project root, `/`-separated.
    pub path: String,
    pub bytes: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct ProjectFiles {
    pub files: Vec<ProjectFile>,
    /// More files exist than were listed.
    pub truncated: bool,
    /// Folders not entered, relative to the project: nested repositories
    /// and submodules (a folder holding its own `.git`, or named in
    /// `.gitmodules`). Nothing in them is listed or read.
    pub nested: Vec<String>,
}

/// One commit, as `git log` shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Commit {
    pub hash: String,
    /// Unix milliseconds of the commit.
    pub time_ms: u64,
    pub subject: String,
    pub body: String,
    /// Paths it changed, relative to the project root.
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct GitHistory {
    /// Newest first.
    pub commits: Vec<Commit>,
    /// Older commits exist, before the window or past `max`.
    pub older: bool,
}

/// Who wrote a turn of a past conversation, as far as the host can tell.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TurnRole {
    /// A prompt typed into the tool (bridge and Connecto messages are told
    /// apart by their text).
    User,
    /// The agent's reply.
    Agent,
    /// A user-role message that Connecto's history shows was not typed by
    /// the user (the opening task of a terminal another agent started).
    Injected,
    /// Unknown: a screen transcript mixes both.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastTurn {
    pub role: TurnRole,
    pub text: String,
    pub at_ms: Option<u64>,
}

/// A conversation of a terminal that has closed.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PastConversation {
    /// Unique within the project (the history id).
    pub id: String,
    pub provider: Option<Provider>,
    pub terminal: Option<String>,
    pub tool_session: Option<String>,
    pub ended_ms: u64,
    pub log: Option<PathBuf>,
    pub turns: Vec<PastTurn>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PastConversations {
    /// Newest first.
    pub conversations: Vec<PastConversation>,
    /// Older conversations exist, before the window.
    pub older: bool,
    /// Some conversations of the window were left out, or read only in
    /// part, for the read budget or the entry limit.
    pub truncated: bool,
}

/// How [`Host::git_refusal`] starts when git was used but a command failed
/// recently (the rest names the command): git is then partly used, not
/// refused.
pub const GIT_FAILED: &str = "git failed: ";

/// Why git is not used, or only partly used, for `project`, as the view
/// shows it next to coverage; `None` when git is fully used (or the folder
/// is not a repository, which coverage already says). Asks the host, which
/// may run git: call it off the UI thread, where a snapshot was taken anyway.
#[must_use]
pub fn git_note(host: &dyn Host, project: &Path) -> Option<String> {
    let clean = |s: &str| crate::render::clean(s).trim().to_owned();
    if let Some(why) = host.git_refusal(project) {
        return Some(match why.strip_prefix(GIT_FAILED) {
            Some(command) => format!(
                "Git is partly used for this project: a command failed recently ({})",
                clean(command)
            ),
            None => format!("Git is not used for this project: {}", clean(&why)),
        });
    }
    let limits: Vec<String> = host
        .git_limits(project)
        .iter()
        .map(|l| clean(l))
        .filter(|l| !l.is_empty())
        .collect();
    (!limits.is_empty()).then(|| format!("Git is used with limits: {}", limits.join("; ")))
}
