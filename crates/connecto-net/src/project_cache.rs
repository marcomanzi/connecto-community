//! What this machine last heard of a host project's history, settings and
//! token optimization (see [`crate::project_panels`]), so the window never
//! waits for the host. A view that is missing or stale is fetched on a thread
//! of its own, which wakes the window when it ends; a command runs the same
//! way, and the view it changes is fetched again after it.

use std::collections::{HashMap, VecDeque};
use std::sync::{Arc, Mutex, PoisonError};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::{HistoryEntry, Transcript};

use crate::client::ClientError;
use crate::project_panels::{
    ProjectAsk, SettingsAnswer, TokensAnswer, history_from, ops_page_from, settings_answer_from,
    tokens_answer_from, transcript_from,
};
use crate::reconnect::HostHandle;
use crate::wire::DenyReason;

/// Which view of a project.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum PanelKind {
    History,
    Settings,
    Tokens,
    Ops,
    Secrets,
    /// The project's functional memory (ADR 0033): its status.
    FunctionalMemory,
}

impl PanelKind {
    const fn ask(self) -> ProjectAsk {
        match self {
            Self::History => ProjectAsk::History,
            Self::Settings => ProjectAsk::Settings,
            Self::Tokens => ProjectAsk::Tokens,
            Self::Ops => ProjectAsk::Ops { after: None },
            Self::Secrets => ProjectAsk::Secrets { after: None },
            Self::FunctionalMemory => ProjectAsk::FunctionalMemory,
        }
    }

    /// The view a command changes.
    const fn of(ask: &ProjectAsk) -> Self {
        match ask {
            ProjectAsk::History | ProjectAsk::Transcript { .. } | ProjectAsk::Continue { .. } => {
                Self::History
            }
            ProjectAsk::Settings
            | ProjectAsk::SetSettings(_)
            | ProjectAsk::SetSettingsBeforeOpenCode(_)
            | ProjectAsk::SetSettingsBeforeToolSets(_)
            | ProjectAsk::Refresh
            | ProjectAsk::CheckRequirements { .. }
            | ProjectAsk::InstallRequirement { .. } => Self::Settings,
            ProjectAsk::FunctionalMemory
            | ProjectAsk::FunctionalMemoryUpdate
            | ProjectAsk::FunctionalMemorySet { .. } => Self::FunctionalMemory,
            ProjectAsk::Tokens
            | ProjectAsk::TokensAnalyze
            | ProjectAsk::TokensApply { .. }
            | ProjectAsk::TokensUndo
            | ProjectAsk::TokensAutocompact(_) => Self::Tokens,
            ProjectAsk::Ops { .. } | ProjectAsk::OpsAct(_) => Self::Ops,
            ProjectAsk::Secrets { .. } | ProjectAsk::SecretsAct(_) => Self::Secrets,
            // Only the phone asks these, never a window panel; they change
            // no view the window keeps.
            ProjectAsk::SiriAsk { .. }
            | ProjectAsk::SiriResult { .. }
            | ProjectAsk::SiriDelivered { .. }
            | ProjectAsk::PushRegister { .. }
            | ProjectAsk::ActivityToken { .. }
            | ProjectAsk::SiriStatus { .. }
            | ProjectAsk::Questions
            | ProjectAsk::QuestionAnswer { .. }
            | ProjectAsk::SendPrompt { .. } => Self::History,
        }
    }
}

/// One view as last fetched.
#[derive(Debug, Clone)]
pub enum PanelValue {
    History(Arc<Vec<HistoryEntry>>),
    Settings(Arc<SettingsAnswer>),
    Tokens(Arc<TokensAnswer>),
    Ops(Arc<connecto_core::ops::OpsView>),
    Secrets(Arc<crate::remote_secrets::SecretsAnswer>),
    /// Where its updates stand (the files are not kept: the window shows
    /// only the status).
    FunctionalMemory(Arc<connecto_core::functional_memory::FmStatus>),
}

#[derive(Default)]
struct Panel {
    value: Option<PanelValue>,
    /// When the fetch that gave `value` began; `None` before the first.
    fetched_from: Option<Instant>,
    /// When the last fetch ended; `None` before the first, or once a command
    /// changed what it shows.
    at: Option<Instant>,
    fetching: bool,
    /// Why the last fetch failed, until one succeeds.
    error: Option<String>,
    /// How many commands were given for this view: the number of the last.
    submitted: u64,
    /// The command refused last, by its number, and why. A later command
    /// that goes through clears it.
    refused: Option<(u64, String)>,
    /// The same for the saves alone (a whole new settings, which the window
    /// shows until the host has it): a Refresh between them changes neither.
    saves: u64,
    /// The save refused last, by its save number, and why: said until a
    /// later save goes through, whatever a Refresh does meanwhile.
    save_refused: Option<(u64, String)>,
    /// What the latest commands came to, by number: `None` went through.
    outcomes: VecDeque<(u64, Option<String>)>,
}

/// How many commands' outcomes a view keeps.
const OUTCOMES: usize = 64;

impl Panel {
    fn record(&mut self, number: u64, outcome: Option<String>) {
        self.outcomes.push_back((number, outcome));
        while self.outcomes.len() > OUTCOMES {
            self.outcomes.pop_front();
        }
    }
}

impl Panel {
    fn shown_error(&self) -> Option<String> {
        self.save_refused
            .as_ref()
            .map(|(_, why)| why.clone())
            .or_else(|| self.refused.as_ref().map(|(_, why)| why.clone()))
            .or_else(|| self.error.clone())
    }
}

/// The transcript of one project's history sheet.
#[derive(Default)]
struct TranscriptSlot {
    shown: Option<Transcript>,
    /// The one asked for last: its number and history id. An answer for an
    /// older ask is dropped.
    wanted: Option<(u64, String)>,
    /// Why the transcript asked for last could not be read.
    error: Option<String>,
}

#[derive(Default)]
struct Inner {
    panels: HashMap<(String, PanelKind), Panel>,
    transcripts: HashMap<String, TranscriptSlot>,
    asked: u64,
    /// Commands not sent yet, in the order given: one runs at a time, so the
    /// host gets them in that order.
    /// Each with its number, and its save number when it is a save.
    queue: VecDeque<(String, ProjectAsk, u64, Option<u64>)>,
    /// A thread is sending the queue.
    sending: bool,
    /// The command that answered each request for a secret, by (project,
    /// request).
    answers: HashMap<(String, u64), u64>,
    /// Where each project's secrets are shown from: after this name, or
    /// from the first.
    secrets_from: HashMap<String, Option<String>>,
    /// The most bytes of one project's secrets read at once; `None`: the
    /// default ([`crate::remote_secrets::MAX_TOTAL_BYTES`]).
    secrets_limit: Option<usize>,
}

/// The cache of one host's projects.
#[derive(Default)]
pub struct Panels(Mutex<Inner>);

impl Panels {
    fn lock(&self) -> std::sync::MutexGuard<'_, Inner> {
        self.0.lock().unwrap_or_else(PoisonError::into_inner)
    }
}

/// A failed ask, as the window says it.
/// What a host from before an ask is said to need.
pub const UPDATE_TO_SEE: &str = "Update Connecto on that machine to see this from here.";

pub(crate) fn wording(error: &ClientError) -> String {
    match error {
        ClientError::Denied(DenyReason::Protocol) => UPDATE_TO_SEE.to_owned(),
        ClientError::Denied(DenyReason::Paused) => {
            "That machine paused remote actions. Try again when it resumes.".to_owned()
        }
        ClientError::Denied(DenyReason::NoSuchTerminal) => {
            "That project is no longer shared by that machine.".to_owned()
        }
        ClientError::Remote(why) => why.clone(),
        ClientError::Net(_) | ClientError::Protocol => {
            "That machine did not answer in time. Try again.".to_owned()
        }
        other => other.to_string(),
    }
}

fn decode(kind: PanelKind, bytes: &[u8]) -> Result<PanelValue, String> {
    // The project id is filled in by whoever shows it.
    Ok(match kind {
        PanelKind::History => PanelValue::History(Arc::new(history_from(bytes)?)),
        PanelKind::Settings => PanelValue::Settings(Arc::new(settings_answer_from(bytes, "")?)),
        PanelKind::Tokens => PanelValue::Tokens(Arc::new(tokens_answer_from(bytes, "")?)),
        PanelKind::Ops => PanelValue::Ops(Arc::new(ops_page_from(bytes, "")?.0)),
        PanelKind::Secrets => {
            PanelValue::Secrets(Arc::new(crate::remote_secrets::secrets_from(bytes)?))
        }
        PanelKind::FunctionalMemory => PanelValue::FunctionalMemory(Arc::new(
            crate::project_panels::functional_memory_from(bytes)?.1,
        )),
    })
}

impl HostHandle {
    /// The host's answer to `ask` about `project`, waiting for it. Never call
    /// it from the window's thread.
    ///
    /// # Errors
    /// Not connected, or the host's refusal, as the window says it.
    pub fn project_ask(&self, project: &str, ask: &ProjectAsk) -> Result<Vec<u8>, String> {
        let client = self
            .client()
            .ok_or_else(|| "Not connected to that machine.".to_owned())?;
        client.project_ask(project, ask).map_err(|e| wording(&e))
    }

    /// `kind` of `project` as last fetched, and why the last command was
    /// refused or the last fetch failed. Never blocks: when it is missing or
    /// older than `fresh`, it is fetched again in the background.
    pub fn panel(
        self: &Arc<Self>,
        project: &str,
        kind: PanelKind,
        fresh: Duration,
    ) -> (Option<PanelValue>, Option<String>) {
        let key = (project.to_owned(), kind);
        let mut inner = self.panels().lock();
        let panel = inner.panels.entry(key.clone()).or_default();
        let stale = panel.at.is_none_or(|at| at.elapsed() >= fresh);
        if stale && !panel.fetching && self.is_connected() {
            panel.fetching = true;
            let this = Arc::clone(self);
            let spawned = thread::Builder::new()
                .name("connecto-remote-project".into())
                .spawn(move || {
                    let from = Instant::now();
                    let got = if kind == PanelKind::Secrets {
                        // Read in pages from where the person is, up to a
                        // bound on memory: a project may keep many.
                        let (from, limit) = {
                            let inner = this.panels().lock();
                            (
                                inner.secrets_from.get(&key.0).cloned().flatten(),
                                inner
                                    .secrets_limit
                                    .unwrap_or(crate::remote_secrets::MAX_TOTAL_BYTES),
                            )
                        };
                        crate::remote_secrets::fetch_secrets_within(
                            |ask| this.project_ask(&key.0, ask),
                            limit,
                            from,
                        )
                        .map(|answer| PanelValue::Secrets(Arc::new(answer)))
                    } else if kind == PanelKind::Ops {
                        // Read in pages: every operation comes with its files.
                        crate::project_panels::fetch_ops_view(
                            |ask| this.project_ask(&key.0, ask),
                            "",
                        )
                        .map(|view| PanelValue::Ops(Arc::new(view)))
                    } else {
                        this.project_ask(&key.0, &kind.ask())
                            .and_then(|bytes| decode(kind, &bytes))
                    };
                    {
                        let mut inner = this.panels().lock();
                        let panel = inner.panels.entry(key).or_default();
                        panel.fetching = false;
                        panel.at = Some(Instant::now());
                        match got {
                            Ok(value) => {
                                panel.value = Some(value);
                                panel.fetched_from = Some(from);
                                panel.error = None;
                            }
                            // What was shown stays; asked again once stale.
                            Err(why) => panel.error = Some(why),
                        }
                    }
                    this.wake_window();
                });
            if spawned.is_err() {
                panel.fetching = false;
            }
        }
        (panel.value.clone(), panel.shown_error())
    }

    /// `kind` of `project` as last fetched, without fetching it.
    #[must_use]
    pub fn peek_panel(
        &self,
        project: &str,
        kind: PanelKind,
    ) -> (Option<PanelValue>, Option<String>) {
        self.panels()
            .lock()
            .panels
            .get(&(project.to_owned(), kind))
            .map_or((None, None), |panel| {
                (panel.value.clone(), panel.shown_error())
            })
    }

    /// Whether the last settings save asked of `project` was refused: what
    /// it set is not coming, whatever an earlier save, or a Refresh before
    /// or after it, did.
    #[must_use]
    pub fn latest_save_refused(&self, project: &str) -> bool {
        self.panels()
            .lock()
            .panels
            .get(&(project.to_owned(), PanelKind::Settings))
            .is_some_and(|panel| {
                panel.saves > 0
                    && panel
                        .save_refused
                        .as_ref()
                        .is_some_and(|(n, _)| *n == panel.saves)
            })
    }

    /// Run a command on `project` off the calling thread, after the commands
    /// given before it, so the host gets them in order. The view it changes
    /// is fetched again after it; a refusal shows as that view's error until
    /// the next command (a refused Continue, as the terminal list's error).
    pub fn project_command(self: &Arc<Self>, project: &str, ask: ProjectAsk) -> u64 {
        let kind = PanelKind::of(&ask);
        let continuing = matches!(ask, ProjectAsk::Continue { .. });
        if continuing {
            self.set_last_error(None);
        }
        let mut inner = self.panels().lock();
        let panel = inner.panels.entry((project.to_owned(), kind)).or_default();
        panel.submitted += 1;
        let number = panel.submitted;
        let save = matches!(ask, ProjectAsk::SetSettings(_)).then(|| {
            panel.saves += 1;
            panel.saves
        });
        if let ProjectAsk::SecretsAct(crate::remote_secrets::SecretsAction::Answer {
            ask: id,
            ..
        }) = &ask
        {
            inner.answers.insert((project.to_owned(), *id), number);
        }
        inner
            .queue
            .push_back((project.to_owned(), ask, number, save));
        if inner.sending {
            return number;
        }
        inner.sending = true;
        drop(inner);
        let this = Arc::clone(self);
        let spawned = thread::Builder::new()
            .name("connecto-remote-project".into())
            .spawn(move || this.send_commands());
        if let Err(err) = spawned {
            let mut inner = self.panels().lock();
            inner.sending = false;
            let failed: Vec<(String, ProjectAsk, u64, Option<u64>)> =
                inner.queue.drain(..).collect();
            for (project, ask, number, save) in failed {
                let panel = inner
                    .panels
                    .entry((project, PanelKind::of(&ask)))
                    .or_default();
                panel.refused = Some((number, err.to_string()));
                panel.record(number, Some(err.to_string()));
                if let Some(save) = save {
                    panel.save_refused = Some((save, err.to_string()));
                }
            }
        }
        number
    }

    /// Show `project`'s secrets after the name `from` (`None`: from the
    /// first); they are asked for again at once.
    pub fn show_secrets_from(&self, project: &str, from: Option<String>) {
        let mut inner = self.panels().lock();
        inner.secrets_from.insert(project.to_owned(), from);
        if let Some(panel) = inner
            .panels
            .get_mut(&(project.to_owned(), PanelKind::Secrets))
        {
            panel.at = None;
        }
        drop(inner);
        self.wake_window();
    }

    /// Read at most `bytes` of one project's secrets at once (instead of
    /// [`crate::remote_secrets::MAX_TOTAL_BYTES`]).
    pub fn limit_secrets_read(&self, bytes: usize) {
        self.panels().lock().secrets_limit = Some(bytes);
    }

    /// What the last answer given here to request `ask` of `project` came
    /// to: `None` when none was given, `Some(None)` while it is on its way,
    /// then `Some(Some(Ok))` or the refusal.
    #[must_use]
    pub fn answer_outcome(&self, project: &str, ask: u64) -> Option<Option<Result<(), String>>> {
        let inner = self.panels().lock();
        let number = *inner.answers.get(&(project.to_owned(), ask))?;
        let outcome = inner
            .panels
            .get(&(project.to_owned(), PanelKind::Secrets))
            .and_then(|panel| panel.outcomes.iter().find(|(n, _)| *n == number))
            .map(|(_, refused)| refused.clone().map_or(Ok(()), Err));
        Some(outcome)
    }

    /// Send the queued commands, one at a time, until none is left.
    fn send_commands(self: &Arc<Self>) {
        loop {
            let next = {
                let mut inner = self.panels().lock();
                let next = inner.queue.pop_front();
                if next.is_none() {
                    inner.sending = false;
                }
                next
            };
            let Some((project, ask, number, save)) = next else {
                return;
            };
            let got = self.project_ask(&project, &ask);
            let continuing = matches!(ask, ProjectAsk::Continue { .. });
            {
                let mut inner = self.panels().lock();
                let panel = inner
                    .panels
                    .entry((project, PanelKind::of(&ask)))
                    .or_default();
                // Whatever happened, what it shows is asked for again.
                panel.at = None;
                panel.record(number, got.as_ref().err().cloned());
                if let Some(save) = save {
                    match &got {
                        Err(why) => panel.save_refused = Some((save, why.clone())),
                        Ok(_) if panel.save_refused.as_ref().is_some_and(|(n, _)| *n < save) => {
                            panel.save_refused = None;
                        }
                        Ok(_) => {}
                    }
                }
                match &got {
                    Err(why) if continuing => self.set_last_error(Some(why.clone())),
                    Err(why) => panel.refused = Some((number, why.clone())),
                    // An older refusal no longer stands once a later
                    // command went through.
                    Ok(_) => {
                        if panel.refused.as_ref().is_some_and(|(n, _)| *n < number) {
                            panel.refused = None;
                        }
                    }
                }
            }
            if continuing && got.is_ok() {
                // The terminal it opened is listed at once.
                self.request_list();
            }
            if matches!(ask, ProjectAsk::SecretsAct(_))
                && let Some(client) = self.client()
            {
                // An answered request leaves the dialog at once, not at the
                // next heartbeat.
                self.poll_secret_asks(&client);
            }
            self.wake_window();
        }
    }

    /// The transcript of `project` last loaded with
    /// [`HostHandle::request_transcript`].
    #[must_use]
    pub fn transcript(&self, project: &str) -> Option<Transcript> {
        self.panels()
            .lock()
            .transcripts
            .get(project)
            .and_then(|slot| slot.shown.clone())
    }

    /// Why the transcript of `project` asked for last could not be read.
    #[must_use]
    pub fn transcript_error(&self, project: &str) -> Option<String> {
        self.panels()
            .lock()
            .transcripts
            .get(project)
            .and_then(|slot| slot.error.clone())
    }

    /// Load the transcript of the closed terminal `history` of `project` in
    /// the background. Only the one asked for last is kept: an answer for an
    /// earlier ask that comes after it is dropped.
    pub fn request_transcript(self: &Arc<Self>, project: &str, history: &str) {
        let number = {
            let mut inner = self.panels().lock();
            inner.asked += 1;
            let number = inner.asked;
            let slot = inner.transcripts.entry(project.to_owned()).or_default();
            if slot.shown.as_ref().is_some_and(|t| t.history_id == history) {
                slot.wanted = None;
                slot.error = None;
                return;
            }
            if slot
                .wanted
                .as_ref()
                .is_some_and(|(_, wanted)| wanted == history)
            {
                return;
            }
            slot.wanted = Some((number, history.to_owned()));
            slot.error = None;
            number
        };
        let this = Arc::clone(self);
        let (project, history) = (project.to_owned(), history.to_owned());
        let spawned = thread::Builder::new()
            .name("connecto-remote-transcript".into())
            .spawn(move || {
                let got = this
                    .project_ask(
                        &project,
                        &ProjectAsk::Transcript {
                            history: history.clone(),
                        },
                    )
                    .and_then(|bytes| transcript_from(&bytes));
                {
                    let mut inner = this.panels().lock();
                    let slot = inner.transcripts.entry(project).or_default();
                    if slot.wanted.as_ref().map(|(n, _)| *n) != Some(number) {
                        // Another was asked for since: this answer is old.
                        return;
                    }
                    slot.wanted = None;
                    match got {
                        Ok((_, text)) => {
                            slot.shown = Some(Transcript {
                                history_id: history,
                                text: Arc::new(text),
                            });
                        }
                        Err(why) => slot.error = Some(why),
                    }
                }
                this.wake_window();
            });
        if spawned.is_err()
            && let Some(slot) = self
                .panels()
                .lock()
                .transcripts
                .values_mut()
                .find(|slot| slot.wanted.as_ref().map(|(n, _)| *n) == Some(number))
        {
            slot.wanted = None;
            slot.error = Some("the transcript could not be asked for".to_owned());
        }
    }
}
