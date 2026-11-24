//! A scripted [`Host`] for tests of this crate and of the crates that build
//! on it: model replies queued in advance, a clock that moves only when told,
//! a fixed git snapshot, a pause switch. No CLI, no git, no clock.

use std::collections::{BTreeMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use connecto_core::memory::{
    Capability, DreamSettings, LocalDate, LocalTime, MemorySettings, ModelChoice, Provider, Weekday,
};

use crate::host::{
    ChangedFile, GitSnapshot, Host, LoggedReply, ModelJob, ModelReply, OpenProject, ReplySource,
    RunError, WallClock,
};
use crate::lock;

/// 2026-11-14 00:00 UTC, a Saturday: the fake clock's default start.
pub const START_MS: u64 = 1_794_614_400_000;

/// The fake host. All state is behind locks, so tests can change it while
/// an engine thread uses it.
pub struct FakeHost {
    replies: Mutex<VecDeque<Result<ModelReply, RunError>>>,
    jobs: Mutex<Vec<ModelJob>>,
    capabilities: Mutex<BTreeMap<Provider, Capability>>,
    git: Mutex<Option<GitSnapshot>>,
    clock: Mutex<WallClock>,
    paused: AtomicBool,
    epoch: AtomicU64,
    secrets: Mutex<Vec<String>>,
    wakes: AtomicU64,
    projects: Mutex<Vec<OpenProject>>,
    settings: Mutex<BTreeMap<String, MemorySettings>>,
    logs: Mutex<BTreeMap<PathBuf, String>>,
    blobs: Mutex<BTreeMap<String, Option<String>>>,
    branches: Mutex<Option<Vec<String>>>,
    cancels: AtomicU64,
    /// Held by `run_model` until released: lets a test act while a run is
    /// in flight.
    gate: Mutex<Option<crossbeam_channel::Receiver<()>>>,
    entered: Mutex<Option<crossbeam_channel::Sender<()>>>,
    git_refusal: Mutex<Option<String>>,
    git_limits: Mutex<Vec<String>>,
    git_asks: AtomicU64,
    /// The next `known_secrets` call signals and waits here.
    secrets_hold: Mutex<
        Option<(
            crossbeam_channel::Sender<()>,
            crossbeam_channel::Receiver<()>,
        )>,
    >,
}

impl Default for FakeHost {
    fn default() -> Self {
        Self::new()
    }
}

impl FakeHost {
    /// Claude available, Codex not; no replies queued; the clock at
    /// [`START_MS`], UTC; not a git repository; not paused.
    #[must_use]
    pub fn new() -> Self {
        let mut capabilities = BTreeMap::new();
        capabilities.insert(Provider::Claude, Capability::Available);
        capabilities.insert(
            Provider::Codex,
            Capability::Unavailable {
                reason: "no proven tool-less mode".to_owned(),
            },
        );
        Self {
            replies: Mutex::new(VecDeque::new()),
            jobs: Mutex::new(Vec::new()),
            capabilities: Mutex::new(capabilities),
            git: Mutex::new(None),
            clock: Mutex::new(WallClock {
                utc_ms: START_MS,
                local_date: LocalDate {
                    year: 2026,
                    month: 11,
                    day: 14,
                },
                local_time: LocalTime::new(0, 0).unwrap_or(DreamSettings::default().time),
                weekday: Weekday::Sat,
                utc_offset_secs: 0,
            }),
            paused: AtomicBool::new(false),
            epoch: AtomicU64::new(0),
            secrets: Mutex::new(Vec::new()),
            wakes: AtomicU64::new(0),
            projects: Mutex::new(Vec::new()),
            settings: Mutex::new(BTreeMap::new()),
            logs: Mutex::new(BTreeMap::new()),
            blobs: Mutex::new(BTreeMap::new()),
            branches: Mutex::new(None),
            cancels: AtomicU64::new(0),
            gate: Mutex::new(None),
            entered: Mutex::new(None),
            git_refusal: Mutex::new(None),
            git_limits: Mutex::new(Vec::new()),
            git_asks: AtomicU64::new(0),
            secrets_hold: Mutex::new(None),
        }
    }

    /// Queue a reply with this text.
    pub fn reply(&self, text: &str) {
        let chars = u64::try_from(text.chars().count()).unwrap_or(u64::MAX);
        lock(&self.replies).push_back(Ok(ModelReply {
            text: text.to_owned(),
            source: ReplySource::Hook,
            chars_in: 0,
            chars_out: chars,
            duration: Duration::from_millis(10),
        }));
    }

    /// Queue a failure.
    pub fn fail(&self, error: RunError) {
        lock(&self.replies).push_back(Err(error));
    }

    /// What `capability` answers for `provider`.
    pub fn set_capability(&self, provider: Provider, capability: Capability) {
        lock(&self.capabilities).insert(provider, capability);
    }

    /// Every job run so far.
    pub fn jobs(&self) -> Vec<ModelJob> {
        lock(&self.jobs).clone()
    }

    pub fn set_git(&self, snapshot: Option<GitSnapshot>) {
        *lock(&self.git) = snapshot;
    }

    pub fn set_clock(&self, clock: WallClock) {
        *lock(&self.clock) = clock;
    }

    /// Move UTC time forward (the local fields stay: set them with
    /// [`FakeHost::set_clock`] when a test needs them).
    pub fn advance(&self, by: Duration) {
        let mut clock = lock(&self.clock);
        clock.utc_ms = clock
            .utc_ms
            .saturating_add(u64::try_from(by.as_millis()).unwrap_or(u64::MAX));
    }

    /// Pause (the epoch grows) or resume.
    pub fn set_paused(&self, paused: bool) {
        if paused && !self.paused.swap(true, Ordering::SeqCst) {
            self.epoch.fetch_add(1, Ordering::SeqCst);
        } else if !paused {
            self.paused.store(false, Ordering::SeqCst);
        }
    }

    pub fn set_secrets(&self, secrets: Vec<String>) {
        *lock(&self.secrets) = secrets;
    }

    /// How many times `wake` was called.
    pub fn wakes(&self) -> u64 {
        self.wakes.load(Ordering::SeqCst)
    }

    pub fn set_projects(&self, projects: Vec<OpenProject>) {
        *lock(&self.projects) = projects;
    }

    /// The settings `load_settings` returns (and `save_settings` replaces).
    pub fn put_settings(&self, project_id: &str, settings: MemorySettings) {
        lock(&self.settings).insert(project_id.to_owned(), settings);
    }

    pub fn saved_settings(&self, project_id: &str) -> Option<MemorySettings> {
        lock(&self.settings).get(project_id).cloned()
    }

    /// The reply text `read_logged_reply` finds in `log`.
    pub fn set_log(&self, log: &Path, reply: &str) {
        lock(&self.logs).insert(log.to_owned(), reply.to_owned());
    }

    /// The blob `file_blobs` reports for `path`. A path never set is
    /// reported as unknown (no hash, not deleted).
    pub fn set_blob(&self, path: &str, blob: &str) {
        lock(&self.blobs).insert(path.to_owned(), Some(blob.to_owned()));
    }

    /// `file_blobs` reports `path` as deleted.
    pub fn set_missing(&self, path: &str) {
        lock(&self.blobs).insert(path.to_owned(), None);
    }

    pub fn set_branches(&self, branches: Option<Vec<String>>) {
        *lock(&self.branches) = branches;
    }

    /// How many times `cancel_runs` was called.
    pub fn cancels(&self) -> u64 {
        self.cancels.load(Ordering::SeqCst)
    }

    /// What `git_refusal` and `git_limits` answer.
    pub fn set_git_use(&self, refusal: Option<&str>, limits: &[&str]) {
        *lock(&self.git_refusal) = refusal.map(str::to_owned);
        *lock(&self.git_limits) = limits.iter().map(|l| (*l).to_owned()).collect();
    }

    /// The next `known_secrets` call signals `entered`, then waits for a
    /// message (or a disconnect) on `release`. Once.
    pub fn hold_secrets(
        &self,
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    ) {
        *lock(&self.secrets_hold) = Some((entered, release));
    }

    /// How many times `git_refusal` was asked.
    pub fn git_asks(&self) -> u64 {
        self.git_asks.load(Ordering::SeqCst)
    }

    /// From now on `run_model` signals `entered` when called, then waits for
    /// a message (or a disconnect) on `release` before answering, while still
    /// honouring `cancel`.
    pub fn hold_runs(
        &self,
        entered: crossbeam_channel::Sender<()>,
        release: crossbeam_channel::Receiver<()>,
    ) {
        *lock(&self.entered) = Some(entered);
        *lock(&self.gate) = Some(release);
    }
}

impl Host for FakeHost {
    fn run_model(
        &self,
        job: &ModelJob,
        _deadline: Instant,
        cancel: &AtomicBool,
    ) -> Result<ModelReply, RunError> {
        lock(&self.jobs).push(job.clone());
        if let Some(entered) = lock(&self.entered).clone() {
            let _ = entered.send(());
        }
        let gate = lock(&self.gate).clone();
        if let Some(release) = gate {
            loop {
                if cancel.load(Ordering::SeqCst) {
                    return Err(RunError::Cancelled);
                }
                match release.recv_timeout(Duration::from_millis(5)) {
                    Err(crossbeam_channel::RecvTimeoutError::Timeout) => {}
                    _ => break,
                }
            }
        }
        if cancel.load(Ordering::SeqCst) {
            return Err(RunError::Cancelled);
        }
        let mut reply = lock(&self.replies)
            .pop_front()
            .unwrap_or_else(|| Err(RunError::Failed("no scripted reply".to_owned())))?;
        reply.chars_in = u64::try_from(job.prompt.chars().count()).unwrap_or(u64::MAX);
        Ok(reply)
    }

    fn capability(&self, choice: &ModelChoice) -> Capability {
        lock(&self.capabilities)
            .get(&choice.provider)
            .cloned()
            .unwrap_or(Capability::Unavailable {
                reason: "unknown provider".to_owned(),
            })
    }

    fn git_snapshot(&self, _project: &Path) -> Option<GitSnapshot> {
        lock(&self.git).clone()
    }

    fn paused(&self) -> bool {
        self.paused.load(Ordering::SeqCst)
    }

    fn pause_epoch(&self) -> u64 {
        self.epoch.load(Ordering::SeqCst)
    }

    fn known_secrets(&self) -> Vec<String> {
        let hold = lock(&self.secrets_hold).take();
        if let Some((entered, release)) = hold {
            let _ = entered.send(());
            let _ = release.recv();
        }
        lock(&self.secrets).clone()
    }

    fn now(&self) -> WallClock {
        *lock(&self.clock)
    }

    fn wake(&self) {
        self.wakes.fetch_add(1, Ordering::SeqCst);
    }

    fn projects(&self) -> Vec<OpenProject> {
        lock(&self.projects).clone()
    }

    fn load_settings(&self, project_id: &str) -> MemorySettings {
        lock(&self.settings)
            .get(project_id)
            .cloned()
            .unwrap_or_default()
    }

    fn save_settings(&self, project_id: &str, settings: &MemorySettings) -> Result<(), String> {
        self.put_settings(project_id, settings.clone());
        Ok(())
    }

    fn read_logged_reply(
        &self,
        _provider: Provider,
        log: &Path,
        _sent: Option<&str>,
    ) -> Option<LoggedReply> {
        lock(&self.logs).get(log).map(|text| LoggedReply {
            text: text.clone(),
            offset: 0,
        })
    }

    fn file_blobs(&self, _project: &Path, paths: &[String]) -> Option<Vec<ChangedFile>> {
        let blobs = lock(&self.blobs);
        Some(
            paths
                .iter()
                .map(|p| ChangedFile {
                    path: p.clone(),
                    blob: blobs.get(p).cloned().flatten(),
                    deleted: matches!(blobs.get(p), Some(None)),
                })
                .collect(),
        )
    }

    fn branches(&self, _project: &Path) -> Option<Vec<String>> {
        lock(&self.branches).clone()
    }

    fn cancel_runs(&self) {
        self.cancels.fetch_add(1, Ordering::SeqCst);
    }

    fn git_refusal(&self, _project: &Path) -> Option<String> {
        self.git_asks.fetch_add(1, Ordering::SeqCst);
        lock(&self.git_refusal).clone()
    }

    fn git_limits(&self, _project: &Path) -> Vec<String> {
        lock(&self.git_limits).clone()
    }
}
