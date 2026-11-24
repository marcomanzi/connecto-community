//! The utility runner: every model call Connecto makes for itself (terminal
//! titles, the project-memory cycles, later uses) is one hidden agent session
//! of any CLI that has a utility launch ([`connecto_adapter::UtilityTool`]).
//!
//! One run is one fresh interactive session in a PTY of its own, in the
//! project folder, on the project's own profile and login. It is not a slot of
//! the registry: it never shows as a terminal, has no bridge, and its hook
//! events go to the run alone, never to the registry or to memory capture.
//! Connecto pastes one prompt, waits for the turn to end, takes the reply and
//! shuts the session down. Blocking: call it from a worker thread (ADR 0002).
//!
//! What the run proves before it returns a reply:
//! - the whole prompt arrived: from the prompt hook or Codex's `notify` input
//!   when they hold it whole, otherwise from the tool's session log (the hook
//!   carries at most 64 KiB);
//! - the reply is the model's: from the turn-end hook, or from the session log
//!   when the hook dropped it, read with a short retry; a reply that is the
//!   tool reporting an error (`API Error: …`) is a failure.
//!
//! Nothing here names a CLI: a provider is usable once its adapter returns a
//! [`connecto_adapter::UtilityTool`].

use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use connecto_adapter::{
    ComposerState, HookCtx, LoggedTurn, UtilityAccess, UtilityLaunch, UtilityTool,
    prompt_arrived_whole,
};
use connecto_core::memory::{Capability, ModelChoice, Provider};
use connecto_core::{
    AuthState, PaneControl, PaneStage, Reply, SessionStatus, TerminalSession, ToolEvent,
};
use connecto_pty::Wake;
use connecto_store::{Installed, Store, StoreError, Tool};

use crate::launch::{LaunchExtras, Layout, adapter, check_auth};
use crate::{INITIAL_SIZE, LiveSession, ManagedPane, PaneSetup};

/// Memory (and other non-title) runs in flight at once, per runner. Titles
/// have their own budget in [`crate::utility`], so neither starves the other.
pub const MAX_MEMORY_IN_FLIGHT: usize = 2;

/// What a run is for. It sets what the session may do and how strictly the
/// prompt's arrival is proven.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UtilityPurpose {
    /// A terminal title: the launch titles always had.
    Title,
    /// A project-memory cycle: reads the project, proves the prompt arrived.
    Memory,
    /// `connecto op llm`: one answer for an operation's script, no tools,
    /// the prompt proven to have arrived.
    Operation,
    /// The biggest-session study of Agent Token Optimization: the summary is
    /// in the prompt, so no tool at all; proves the prompt arrived.
    TokenStudy,
    /// A functional-memory update (ADR 0033): reads the project like a
    /// memory cycle; the host writes the files from its reply.
    FunctionalMemory,
}

impl UtilityPurpose {
    const fn access(self) -> UtilityAccess {
        match self {
            Self::Title | Self::Operation | Self::TokenStudy => UtilityAccess::Minimal,
            Self::Memory | Self::FunctionalMemory => UtilityAccess::ReadOnly,
        }
    }

    /// A strict run fails unless the whole prompt is proven to have arrived.
    /// A title, as always, trusts a turn whose prompt hook did not come.
    const fn strict(self) -> bool {
        matches!(
            self,
            Self::Memory | Self::Operation | Self::TokenStudy | Self::FunctionalMemory
        )
    }
}

/// One model call.
#[derive(Debug, Clone)]
pub struct UtilityJob {
    /// The project's id; checked against `project`.
    pub project_id: String,
    /// The project folder: the session's working folder, and what picks its
    /// profile and login.
    pub project: PathBuf,
    /// Provider, model and effort. The model word
    /// [`connecto_core::utility::DEFAULT_MODEL`] leaves the CLI's default.
    pub choice: ModelChoice,
    pub prompt: String,
    pub purpose: UtilityPurpose,
    /// For logs and the run list (a cycle name, "title").
    pub label: String,
}

/// Where the reply text was read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReplySource {
    /// The turn-end hook (Claude `Stop`, Codex `notify`).
    Hook,
    /// The tool's session log (Claude transcript, Codex rollout).
    SessionLog,
}

/// A run's answer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UtilityReply {
    pub text: String,
    pub source: ReplySource,
    pub chars_in: usize,
    pub chars_out: usize,
    pub duration: Duration,
}

/// Why a run gave no reply. The text is short and shown to the user as is.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum RunError {
    /// The deadline passed; the text says at which step.
    #[error("{0}")]
    Timeout(String),
    #[error("the run was cancelled")]
    Cancelled,
    #[error("the prompt reached {tool} cut off ({arrived} of {sent} characters)")]
    PromptCut {
        tool: &'static str,
        arrived: usize,
        sent: usize,
    },
    /// The CLI cannot serve here: not installed, not logged in, or no utility
    /// launch. Feeds the provider capability shown to the user.
    #[error("{0}")]
    Unavailable(String),
    /// The session was refused or did not start (folder, store, model words).
    #[error("{0}")]
    LaunchRefused(String),
    /// The CLI has not been trusted for the project folder yet: its trust
    /// screen showed. Not answered for the user; the job should be held until
    /// the person accepts it once in a terminal, not counted as a failure.
    #[error("{0}")]
    Untrusted(String),
    /// A screen waits for a person (first-run setup, a warning).
    #[error("{0}")]
    Blocked(String),
    /// The tool reported an error as its reply (`API Error: …`, a failed turn).
    #[error("the model call failed: {0}")]
    Provider(String),
    /// The turn ended but its text could not be read.
    #[error("{0}")]
    ReplyUnavailable(String),
    /// The prompt was typed and Enter sent, but whether the tool took it is
    /// unknown: no hook, no sign on the screen, and a session log that cannot
    /// tell. Not typed again (a second submission is worse than a failed run).
    #[error("{0}")]
    SubmitUncertain(String),
    /// The session ended, asked for input, or never took the prompt.
    #[error("{0}")]
    Session(String),
}

/// The CLI a provider runs.
#[must_use]
pub const fn tool_of(provider: Provider) -> Tool {
    match provider {
        Provider::Claude => Tool::Claude,
        Provider::Codex => Tool::Codex,
    }
}

/// Whether `provider` can run utility sessions in `project` now, or why not
/// (shown to the user). Blocking: verifies the private copy (or, the first
/// time, sets it up from the user's install with the same signature checks a
/// terminal makes) and asks the CLI's status command in the project's profile,
/// so call it off the UI thread.
#[must_use]
pub fn capability(layout: &Layout, project: &Path, provider: Provider) -> Capability {
    let tool = tool_of(provider);
    let name = provider.label();
    if adapter(tool).utility().is_none() {
        return Capability::Unavailable {
            reason: format!("{name} cannot run utility sessions"),
        };
    }
    let store = Store::new(&layout.data_root);
    let installed = match store
        .newest(tool)
        .and_then(|version| store.verify(tool, &version))
    {
        Ok(installed) => Ok(installed),
        Err(StoreError::NoVersion(_)) if Store::find_installed(tool, &layout.home).is_some() => {
            store
                .provision_found(tool, &layout.home, &connecto_store::Codesign)
                .map(|provisioned| provisioned.installed)
                .map_err(|err| format!("{name} could not be set up: {err}"))
        }
        Err(StoreError::NoVersion(_)) => Err(format!("{name} is not installed")),
        Err(err) => Err(format!("{name}: {err}")),
    };
    availability(name, installed, |installed| {
        check_auth(layout, project, tool, installed).map_err(|err| err.to_string())
    })
}

/// The capability from what the store gave and what the login check says.
/// A CLI is available only once its login is known good, also the first time,
/// when its private copy was only just set up.
fn availability(
    name: &str,
    installed: Result<Installed, String>,
    auth: impl FnOnce(&Installed) -> Result<AuthState, String>,
) -> Capability {
    let reason = match installed.map(|installed| auth(&installed)) {
        Err(reason) => reason,
        Ok(Ok(AuthState::LoggedIn { .. })) => return Capability::Available,
        Ok(Ok(AuthState::LoggedOut)) => format!("{name} is not logged in for this project"),
        Ok(Ok(_)) => format!("{name}'s login could not be confirmed"),
        Ok(Err(err)) => format!("{name}: {err}"),
    };
    Capability::Unavailable { reason }
}

// ---------------------------------------------------------------------------
// The conversation with one hidden session.

/// What a run sees of its session. The real one is a [`LiveSession`]; tests
/// script one.
pub(crate) trait Conversation {
    fn screen(&self) -> String;
    fn running(&self) -> bool;
    fn bracketed_paste(&self) -> bool;
    /// False when the input could not be written.
    fn send(&self, bytes: Vec<u8>) -> bool;
    /// The next hook event, without waiting.
    fn next_event(&self) -> Option<ToolEvent>;
    /// Where the session's profile is, to find and check its log.
    fn hook_ctx(&self) -> Option<HookCtx>;
}

struct Live<'a>(&'a LiveSession);

impl Conversation for Live<'_> {
    fn screen(&self) -> String {
        self.0.text_all().0
    }

    fn running(&self) -> bool {
        self.0.status() == SessionStatus::Running
    }

    fn bracketed_paste(&self) -> bool {
        let mut snapshot = connecto_core::TermSnapshot::default();
        self.0.poll_snapshot(&mut snapshot);
        snapshot.modes.bracketed_paste
    }

    fn send(&self, bytes: Vec<u8>) -> bool {
        self.0.send_input(bytes).is_ok()
    }

    fn next_event(&self) -> Option<ToolEvent> {
        self.0.hook()?.events().recv_timeout(Duration::ZERO)
    }

    fn hook_ctx(&self) -> Option<HookCtx> {
        self.0.hook_ctx().cloned()
    }
}

/// The timings of a run. Tests use short ones.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Pace {
    pub poll: Duration,
    /// Pause between the pasted prompt and the still-screen wait.
    pub submit_delay: Duration,
    /// How long the screen must hold still after a paste before Enter.
    pub settle_still: Duration,
    /// The longest that wait may take.
    pub settle_limit: Duration,
    /// The composer must stay up this long before the prompt is typed.
    pub ready_stable: Duration,
    /// How long a blocking screen may show before the run gives up on it.
    pub block_grace: Duration,
    /// How long the tool may take to show its composer.
    pub boot_limit: Duration,
    /// How long a typed prompt may go unacknowledged before Enter again.
    pub submit_limit: Duration,
    /// After the start hook, a screen with no known blocker is taken as ready.
    pub fallback_ready: Duration,
    /// How long the session log may lag the turn-end hook (S1: 1 to 122 ms).
    pub log_wait: Duration,
    pub log_poll: Duration,
}

impl Default for Pace {
    fn default() -> Self {
        Self {
            poll: Duration::from_millis(100),
            submit_delay: Duration::from_millis(400),
            settle_still: Duration::from_millis(500),
            settle_limit: Duration::from_secs(5),
            ready_stable: Duration::from_millis(300),
            block_grace: Duration::from_secs(3),
            boot_limit: Duration::from_secs(25),
            submit_limit: Duration::from_secs(6),
            fallback_ready: Duration::from_secs(4),
            log_wait: Duration::from_secs(2),
            log_poll: Duration::from_millis(25),
        }
    }
}

/// The reply and where it came from.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Answer {
    pub text: String,
    pub source: ReplySource,
}

/// Lock `m`, recovering the data from a poisoned lock.
fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// The largest session log read whole.
const MAX_LOG_BYTES: u64 = 64 * 1024 * 1024;

fn read_log(path: &Path) -> Option<String> {
    let meta = std::fs::metadata(path).ok()?;
    if meta.len() > MAX_LOG_BYTES {
        return None;
    }
    let bytes = std::fs::read(path).ok()?;
    Some(String::from_utf8_lossy(&bytes).into_owned())
}

/// The last few lines of a screen, on one line, for a failure reason.
pub(crate) fn screen_tail(screen: &str) -> String {
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(3)..].join(" / ");
    tail.chars().take(160).collect()
}

/// Seconds, rounded up, from now to `deadline`: the job's timeout as the
/// person set it (60 for a title), for the messages.
fn secs_until(deadline: Instant) -> u64 {
    let left = deadline.saturating_duration_since(Instant::now());
    u64::try_from(left.as_millis().div_ceil(1000)).unwrap_or(u64::MAX)
}

/// One run's talk with its session.
pub(crate) struct Talk<'a> {
    pub conv: &'a dyn Conversation,
    pub tool: &'a dyn UtilityTool,
    /// "Claude", "Codex": for messages.
    pub name: &'static str,
    pub prompt: &'a str,
    pub strict: bool,
    pub deadline: Instant,
    pub cancelled: &'a dyn Fn() -> bool,
    pub pace: Pace,
}

/// What a turn end left to settle.
struct Ending {
    reply: Reply,
    thread: Option<String>,
    log: Option<PathBuf>,
    /// The arrival must be read from the log.
    prove: bool,
}

impl Talk<'_> {
    fn expired(&self) -> bool {
        (self.cancelled)() || Instant::now() >= self.deadline
    }

    /// Cancelled or past the deadline: the error to stop with, if so.
    fn stop(&self, at: &str) -> Result<(), RunError> {
        if (self.cancelled)() {
            Err(RunError::Cancelled)
        } else if Instant::now() >= self.deadline {
            Err(RunError::Timeout(format!("the deadline passed {at}")))
        } else {
            Ok(())
        }
    }

    /// Whether `got` (a hook's or the log's prompt) is the prompt sent. A
    /// strict run needs exactly it; a title keeps the looser check it had.
    fn arrived(&self, got: &str) -> bool {
        if self.strict {
            self.tool.prompt_matches(self.prompt, got)
        } else {
            prompt_arrived_whole(self.prompt, got)
        }
    }

    /// Sleep `wait`, but not past the deadline.
    fn nap(&self, wait: Duration) {
        thread::sleep(wait.min(self.deadline.saturating_duration_since(Instant::now())));
    }

    /// Whether the session log holds a user message: `Some(true)` it does,
    /// `Some(false)` it was read and holds none, `None` it cannot be read (or
    /// found), which proves nothing either way.
    fn log_has_prompt(&self, reported: Option<&Path>) -> Option<bool> {
        let ctx = self.conv.hook_ctx()?;
        let path = self.tool.log_path(reported, None, &ctx)?;
        let text = read_log(&path)?;
        Some(self.tool.read_turn(&text, self.prompt).prompt_chars > 0)
    }

    /// Paste the whole prompt, wait until the screen holds still, then Enter.
    /// Neither wait goes past the deadline, and nothing is sent after it.
    fn type_prompt(&self) -> Result<(), RunError> {
        self.stop("before the prompt was typed")?;
        let bytes = if self.conv.bracketed_paste() {
            connecto_core::encode_paste(
                self.prompt,
                connecto_core::InputModes {
                    bracketed_paste: true,
                    ..connecto_core::InputModes::default()
                },
            )
        } else {
            // No bracketed paste: one line, so no newline can submit early.
            self.prompt.replace(['\r', '\n'], " ").into_bytes()
        };
        if !self.conv.send(bytes) {
            return Err(RunError::Session("could not type the prompt".to_owned()));
        }
        self.nap(self.pace.submit_delay);
        let limit = (Instant::now() + self.pace.settle_limit).min(self.deadline);
        let mut last = self.conv.screen();
        let mut still_since = Instant::now();
        while Instant::now() < limit && still_since.elapsed() < self.pace.settle_still {
            if (self.cancelled)() {
                return Err(RunError::Cancelled);
            }
            self.nap(self.pace.poll);
            let now = self.conv.screen();
            if now != last {
                last = now;
                still_since = Instant::now();
            }
        }
        self.enter()
    }

    fn enter(&self) -> Result<(), RunError> {
        self.stop("before the prompt was submitted")?;
        if self.conv.send(b"\r".to_vec()) {
            Ok(())
        } else {
            Err(RunError::Session("could not submit the prompt".to_owned()))
        }
    }

    /// Run the conversation to its answer.
    #[allow(clippy::too_many_lines)]
    pub(crate) fn run(&self) -> Result<Answer, RunError> {
        let secs = secs_until(self.deadline);
        let booted = Instant::now();
        let mut ready_since: Option<Instant> = None;
        let mut sent_at: Option<Instant> = None;
        let mut resent = false;
        let mut retyped = false;
        let mut lost_check: Option<Instant> = None;
        let mut submitted = false;
        let mut hook_seen = false;
        let mut started_at: Option<Instant> = None;
        let mut reported_log: Option<PathBuf> = None;
        let mut arrived = false;
        let mut prove = false;
        loop {
            let screen = self.conv.screen();
            if (self.cancelled)() {
                return Err(RunError::Cancelled);
            }
            if !self.conv.running() {
                return Err(RunError::Session(format!(
                    "the session ended: {}",
                    screen_tail(&screen)
                )));
            }
            if self.expired() {
                let tail = screen_tail(&screen);
                let name = self.name;
                return Err(if let Some(why) = self.tool.untrusted(&screen) {
                    RunError::Untrusted(why)
                } else if sent_at.is_none() {
                    match self.tool.blocked(&screen) {
                        Some(why) => RunError::Blocked(why),
                        None => RunError::Timeout(format!(
                            "{name} never became ready within {secs} s ({tail})"
                        )),
                    }
                } else if !submitted && lost_check.is_some() {
                    // Enter went, the composer let go, and nothing (no hook,
                    // no readable log) said whether the tool took it.
                    RunError::SubmitUncertain(format!(
                        "{name} showed no sign of taking the prompt within {secs} s, and its session log cannot tell whether it did ({tail})"
                    ))
                } else if !submitted {
                    RunError::Timeout(format!("the prompt was typed but never submitted ({tail})"))
                } else if !hook_seen {
                    RunError::Timeout(format!(
                        "no reply: the Stop hook never fired within {secs} s ({tail})"
                    ))
                } else {
                    RunError::Timeout(format!("no reply within {secs} s ({tail})"))
                });
            }
            while let Some(event) = self.conv.next_event() {
                hook_seen = true;
                match event {
                    ToolEvent::Started { turn_log, .. } => {
                        started_at.get_or_insert_with(Instant::now);
                        reported_log = turn_log.map(|log| PathBuf::from(log.expose()));
                    }
                    ToolEvent::PromptSubmitted { prompt, .. } if sent_at.is_some() => {
                        submitted = true;
                        if prompt.truncated() {
                            // The hook holds only the start: the log decides.
                            prove = true;
                        } else if self.arrived(prompt.expose()) {
                            arrived = true;
                        } else {
                            return Err(RunError::PromptCut {
                                tool: self.name,
                                arrived: prompt.expose().chars().count(),
                                sent: self.prompt.chars().count(),
                            });
                        }
                    }
                    ToolEvent::NeedsUser if sent_at.is_some() => {
                        return Err(RunError::Session(format!(
                            "{} stopped to ask for input ({})",
                            self.name,
                            screen_tail(&screen)
                        )));
                    }
                    ToolEvent::TurnEnded {
                        input,
                        input_total,
                        reply,
                        thread,
                        ..
                    } if sent_at.is_some() => {
                        if !input.is_empty() {
                            // Whether a turn is this job's at all is the loose
                            // question; whether its prompt arrived is not.
                            let ours = input.iter().any(|t| {
                                t.truncated() || prompt_arrived_whole(self.prompt, t.expose())
                            });
                            // A turn the tool ran for itself (Codex titles its
                            // thread first): not the end of this job.
                            if !ours && input_total == input.len() {
                                continue;
                            }
                            if input
                                .iter()
                                .any(|t| !t.truncated() && self.arrived(t.expose()))
                            {
                                arrived = true;
                            } else {
                                prove = true;
                            }
                        }
                        return self.finish(&Ending {
                            reply,
                            thread: thread.map(|t| t.expose().clone()),
                            log: reported_log,
                            prove: !arrived && (prove || self.strict),
                        });
                    }
                    _ => {}
                }
            }
            // The trust screen never clears by itself: fail at once, before
            // anything is typed into it.
            if sent_at.is_none()
                && let Some(why) = self.tool.untrusted(&screen)
            {
                return Err(RunError::Untrusted(why));
            }
            match sent_at {
                None => {
                    // The start hook is the other signal: if the composer's
                    // look changes in a later version, it still gets there.
                    let started_long_ago = started_at
                        .is_some_and(|at| at.elapsed() >= self.pace.fallback_ready)
                        && self.tool.blocked(&screen).is_none();
                    if self.tool.ready(&screen) || started_long_ago {
                        ready_since.get_or_insert_with(Instant::now);
                    } else {
                        ready_since = None;
                    }
                    if ready_since.is_some_and(|at| at.elapsed() >= self.pace.ready_stable) {
                        self.type_prompt()?;
                        sent_at = Some(Instant::now());
                    } else if booted.elapsed() >= self.pace.block_grace
                        && let Some(why) = self.tool.blocked(&screen)
                    {
                        return Err(RunError::Blocked(why));
                    } else if booted.elapsed() >= self.pace.boot_limit {
                        return Err(RunError::Session(format!(
                            "{} never became ready ({})",
                            self.name,
                            screen_tail(&screen)
                        )));
                    }
                }
                Some(at) if !submitted => {
                    let composer = self.tool.composer(&screen);
                    if composer == ComposerState::Busy {
                        if !self.tool.prompt_hook() {
                            // No prompt hook: the tool is working on it.
                            submitted = true;
                        }
                    } else if at.elapsed() >= self.pace.submit_limit {
                        if composer == ComposerState::Occupied {
                            // The text is still in the composer, so it was not
                            // taken. A title presses Enter again, once, as it
                            // always has. A memory run sends no keystroke it
                            // cannot prove safe: an "occupied" line can also be
                            // a menu (a permission prompt whose hook is late),
                            // and Enter would answer it.
                            if resent || self.strict {
                                return Err(RunError::Session(format!(
                                    "the prompt was typed but never submitted ({})",
                                    screen_tail(&screen)
                                )));
                            }
                            resent = true;
                            self.enter()?;
                            sent_at = Some(Instant::now());
                        } else {
                            // An empty composer proves nothing: the prompt may
                            // be taken with its hook late (under load, many
                            // seconds). Only the session log can tell, after a
                            // bounded wait. With no readable log the run waits
                            // on, types nothing again, and at the deadline
                            // says the submission is uncertain.
                            let since = *lost_check.get_or_insert_with(Instant::now);
                            match self.log_has_prompt(reported_log.as_deref()) {
                                Some(true) => submitted = true,
                                Some(false) if since.elapsed() >= self.pace.log_wait => {
                                    if retyped {
                                        return Err(RunError::Session(format!(
                                            "the prompt was typed twice and never submitted ({})",
                                            screen_tail(&screen)
                                        )));
                                    }
                                    // Readable, with no user message, and no
                                    // hook: the paste was lost. Type it
                                    // again, once.
                                    retyped = true;
                                    lost_check = None;
                                    self.type_prompt()?;
                                    sent_at = Some(Instant::now());
                                }
                                _ => {}
                            }
                        }
                    }
                }
                Some(_) => {}
            }
            self.nap(self.pace.poll);
        }
    }

    /// The reply of a turn that ended: from the hook when it holds it whole and
    /// nothing is left to prove, otherwise from the session log. Never returned
    /// after the deadline or a cancel.
    fn finish(&self, end: &Ending) -> Result<Answer, RunError> {
        let from_hook = match &end.reply {
            Reply::Text(text) if !text.truncated() => Some(text.expose().clone()),
            _ => None,
        };
        // The tool reporting an error is never output, whatever the log says.
        if let Some(why) = from_hook.as_deref().and_then(|t| self.tool.reply_error(t)) {
            return Err(RunError::Provider(why));
        }
        // A memory reply must be the turn's finished response, which only the
        // log marks (a reply cut at the token limit fits a hook too). A title
        // takes the hook's text as it always has.
        let answer = match (from_hook, end.prove || self.strict) {
            (Some(text), false) => Answer {
                text,
                source: ReplySource::Hook,
            },
            (hook, _) => self.read_from_log(end, hook, end.prove)?,
        };
        if let Some(why) = self.tool.reply_error(&answer.text) {
            return Err(RunError::Provider(why));
        }
        self.stop("before the reply was taken")?;
        Ok(answer)
    }

    fn read_from_log(
        &self,
        end: &Ending,
        hook: Option<String>,
        prove: bool,
    ) -> Result<Answer, RunError> {
        let path = self.conv.hook_ctx().and_then(|ctx| {
            self.tool
                .log_path(end.log.as_deref(), end.thread.as_deref(), &ctx)
        });
        let need_reply = hook.is_none();
        let until = (Instant::now() + self.pace.log_wait).min(self.deadline);
        let mut seen: Option<LoggedTurn> = None;
        // Retry for a short while, within the deadline: the log is written
        // just after the hook.
        loop {
            self.stop("while the session log was read")?;
            if let Some(text) = path.as_deref().and_then(read_log) {
                let turn = self.tool.read_turn(&text, self.prompt);
                let proven = !prove || turn.prompt_whole;
                let finished = !self.strict || turn.complete;
                let replied = finished && (!need_reply || turn.reply.is_some() && turn.complete);
                let done = (proven && replied) || turn.error.is_some();
                seen = Some(turn);
                if done {
                    break;
                }
            }
            if Instant::now() >= until {
                self.stop("while the session log was read")?;
                break;
            }
            self.nap(self.pace.log_poll);
        }
        let Some(turn) = seen else {
            return Err(RunError::ReplyUnavailable(if need_reply {
                "the reply was not in the turn-end hook and the session log could not be read"
                    .to_owned()
            } else {
                "the session log could not be read to prove the whole prompt arrived".to_owned()
            }));
        };
        if let Some(error) = turn.error {
            return Err(RunError::Provider(error.chars().take(200).collect()));
        }
        if prove && !turn.prompt_whole {
            return Err(RunError::PromptCut {
                tool: self.name,
                arrived: turn.prompt_chars,
                sent: self.prompt.chars().count(),
            });
        }
        if self.strict && !turn.complete {
            return Err(RunError::ReplyUnavailable(
                "the session log does not mark the reply as finished (cut at the token limit, or still running)"
                    .to_owned(),
            ));
        }
        match (hook, turn.reply) {
            (Some(text), _) => Ok(Answer {
                text,
                source: ReplySource::Hook,
            }),
            // Only a response the log marks as the end of the turn.
            (None, Some(text)) if turn.complete => Ok(Answer {
                text,
                source: ReplySource::SessionLog,
            }),
            (None, Some(_)) => Err(RunError::ReplyUnavailable(
                "the session log has no finished reply".to_owned(),
            )),
            (None, None) => Err(RunError::ReplyUnavailable(match end.reply {
                Reply::Dropped => {
                    "the reply was too large for the hook and is not in the session log".to_owned()
                }
                _ => "the turn ended with no reply text".to_owned(),
            })),
        }
    }
}

// ---------------------------------------------------------------------------
// Starting the hidden session.

/// Runs one job to its answer. The real one drives a hidden PTY; tests fake it.
pub(crate) trait Engine: Send + Sync {
    /// Blocks until the answer, `deadline`, or `cancelled()`. A pane is handed
    /// to `adopt` as soon as it exists; the caller closes it.
    fn drive(
        &self,
        job: &UtilityJob,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
        adopt: &dyn Fn(Arc<ManagedPane>),
    ) -> Result<Answer, RunError>;
}

/// Hidden sessions in PTYs.
#[derive(Clone)]
pub(crate) struct PtyEngine {
    pub layout: Layout,
    pub wake: Wake,
    pub extras: LaunchExtras,
}

impl Engine for PtyEngine {
    fn drive(
        &self,
        job: &UtilityJob,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
        adopt: &dyn Fn(Arc<ManagedPane>),
    ) -> Result<Answer, RunError> {
        let provider = job.choice.provider;
        let name = provider.label();
        let tool = tool_of(provider);
        let surface = adapter(tool)
            .utility()
            .ok_or_else(|| RunError::Unavailable(format!("{name} cannot run utility sessions")))?;
        if !job.choice.is_valid() {
            return Err(RunError::LaunchRefused(format!(
                "not a model and effort {name} can be given: {}",
                job.choice
            )));
        }
        let store = Store::new(&self.layout.data_root);
        if store.newest(tool).is_err() && Store::find_installed(tool, &self.layout.home).is_none() {
            return Err(RunError::Unavailable(format!("{name} is not installed")));
        }
        let launch = UtilityLaunch {
            model: connecto_core::utility::cli_model(&job.choice).map(str::to_owned),
            effort: job.choice.effort.clone(),
            access: job.purpose.access(),
        };
        let pane = Arc::new(
            ManagedPane::spawn(PaneSetup {
                layout: self.layout.clone(),
                project: Some(job.project.clone()),
                tool,
                size: INITIAL_SIZE,
                wake: Arc::clone(&self.wake),
                extras: LaunchExtras {
                    utility: Some(launch),
                    ..self.extras.clone()
                },
                bridge: None,
            })
            .map_err(|err| RunError::LaunchRefused(format!("could not start: {err}")))?,
        );
        adopt(Arc::clone(&pane));
        let secs = secs_until(deadline);
        let session = loop {
            if cancelled() {
                return Err(RunError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(RunError::Timeout(format!("no session after {secs} s")));
            }
            match pane.stage() {
                PaneStage::Failed { reason } => return Err(RunError::LaunchRefused(reason)),
                PaneStage::NeedsLogin { .. } | PaneStage::LoggingIn => {
                    return Err(RunError::Unavailable(format!(
                        "{name} is not logged in for this project"
                    )));
                }
                PaneStage::Stopped | PaneStage::Idle => {
                    return Err(RunError::LaunchRefused(
                        "the session did not start".to_owned(),
                    ));
                }
                PaneStage::Preparing { .. } | PaneStage::Running => {}
            }
            if let Some(session) = pane.session() {
                break session;
            }
            thread::sleep(Duration::from_millis(100));
        };
        if session.hook().is_none() {
            return Err(RunError::Session(
                "the session has no hook channel".to_owned(),
            ));
        }
        Talk {
            conv: &Live(&session),
            tool: surface,
            name,
            prompt: &job.prompt,
            strict: job.purpose.strict(),
            deadline,
            cancelled,
            pace: Pace::default(),
        }
        .run()
    }
}

// ---------------------------------------------------------------------------
// The runner memory (and any later use) calls.

/// Utility runs with a budget of their own and a way to cancel them all.
pub struct UtilityRunner {
    engine: Arc<dyn Engine>,
    max: usize,
    busy: Mutex<usize>,
    freed: Condvar,
    /// Bumped by [`UtilityRunner::cancel_all`]: a run started before it stops.
    epoch: AtomicU64,
    panes: Mutex<Vec<(u64, Arc<ManagedPane>)>>,
    next: AtomicU64,
}

/// Holds one in-flight place until dropped.
struct Slot<'a>(&'a UtilityRunner);

impl Drop for Slot<'_> {
    fn drop(&mut self) {
        let mut busy = lock(&self.0.busy);
        *busy = busy.saturating_sub(1);
        self.0.freed.notify_all();
    }
}

impl UtilityRunner {
    /// Hidden sessions in PTYs, at most [`MAX_MEMORY_IN_FLIGHT`] at once.
    #[must_use]
    pub fn new(layout: Layout, wake: Wake) -> Self {
        Self::with_engine(
            Arc::new(PtyEngine {
                layout,
                wake,
                extras: LaunchExtras::default(),
            }),
            MAX_MEMORY_IN_FLIGHT,
        )
    }

    pub(crate) fn with_engine(engine: Arc<dyn Engine>, max: usize) -> Self {
        Self {
            engine,
            max: max.max(1),
            busy: Mutex::new(0),
            freed: Condvar::new(),
            epoch: AtomicU64::new(0),
            panes: Mutex::new(Vec::new()),
            next: AtomicU64::new(1),
        }
    }

    /// Runs in flight now.
    #[must_use]
    pub fn in_flight(&self) -> usize {
        *lock(&self.busy)
    }

    /// Cancel every run in flight now and close its session (Stop Everything,
    /// closing). Runs started afterwards are not affected.
    pub fn cancel_all(&self) {
        self.epoch.fetch_add(1, Ordering::SeqCst);
        let panes = std::mem::take(&mut *lock(&self.panes));
        for (_, pane) in panes {
            pane.close();
        }
        self.freed.notify_all();
    }

    fn take_slot(
        &self,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
    ) -> Result<Slot<'_>, RunError> {
        let mut busy = lock(&self.busy);
        loop {
            if cancelled() {
                return Err(RunError::Cancelled);
            }
            if *busy < self.max {
                *busy += 1;
                return Ok(Slot(self));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() {
                return Err(RunError::Timeout(
                    "other utility runs were still busy at the deadline".to_owned(),
                ));
            }
            busy = self
                .freed
                .wait_timeout(busy, left.min(Duration::from_millis(50)))
                .unwrap_or_else(PoisonError::into_inner)
                .0;
        }
    }

    /// One model call: blocks until the reply, `deadline`, or `cancel` (or
    /// [`UtilityRunner::cancel_all`]). The session is closed before it returns.
    ///
    /// # Errors
    ///
    /// Why there is no reply; see [`RunError`].
    pub fn run(
        &self,
        job: &UtilityJob,
        deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<UtilityReply, RunError> {
        let started = Instant::now();
        let epoch = self.epoch.load(Ordering::SeqCst);
        let cancelled =
            || cancel.load(Ordering::SeqCst) || self.epoch.load(Ordering::SeqCst) != epoch;
        let real = std::fs::canonicalize(&job.project).map_err(|_| {
            RunError::LaunchRefused("the project folder cannot be found".to_owned())
        })?;
        if crate::launch::project_id(&real) != job.project_id {
            return Err(RunError::LaunchRefused(
                "the job's project id does not match its folder".to_owned(),
            ));
        }
        if job.prompt.trim().is_empty() {
            return Err(RunError::LaunchRefused("the prompt is empty".to_owned()));
        }
        let _slot = self.take_slot(deadline, &cancelled)?;
        let run = self.next.fetch_add(1, Ordering::SeqCst);
        let result = self.engine.drive(job, deadline, &cancelled, &|pane| {
            let mut panes = lock(&self.panes);
            if self.epoch.load(Ordering::SeqCst) == epoch {
                panes.push((run, pane));
            } else {
                pane.close();
            }
        });
        let mine: Vec<Arc<ManagedPane>> = {
            let mut panes = lock(&self.panes);
            let (mine, rest) = std::mem::take(&mut *panes)
                .into_iter()
                .partition(|(id, _)| *id == run);
            *panes = rest;
            mine.into_iter().map(|(_, pane)| pane).collect()
        };
        for pane in mine {
            pane.close();
            pane.release();
        }
        let answer = result?;
        Ok(UtilityReply {
            chars_in: job.prompt.chars().count(),
            chars_out: answer.text.chars().count(),
            text: answer.text,
            source: answer.source,
            duration: started.elapsed(),
        })
    }
}

#[cfg(test)]
#[path = "utility_run_tests.rs"]
mod tests;
