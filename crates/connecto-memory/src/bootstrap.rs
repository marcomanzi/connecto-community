//! The initial build of the memory palace from an existing project (spec
//! §5.9, §5.10): inventory and coverage plan, bounded evidence packets,
//! extraction by a model, reconciliation, validation, and one atomic
//! publication of the first version and its brief.
//!
//! # Steps
//!
//! 1. **Plan** ([`plan`]): the inventory ([`crate::bootstrap_inventory`]) with
//!    coverage per kind of source, the gaps, the characters and the runs it
//!    takes. Never a figure in money.
//! 2. **Prepare** (once per build): the selected sources are read through the
//!    [`Host`] (read-only), redacted, split into parts and stored as evidence
//!    ([`crate::bootstrap_packet`]); the manifest (every source with its hash
//!    and state) and one checkpoint per packet are written. The cutoff (the
//!    highest annotation `seq`) is fixed here.
//! 3. **Extract**: one fresh model session per packet, its material between
//!    delimiters of its own. The reply is parsed and judged by
//!    [`crate::operations`] against that packet; what passes is staged in the
//!    packet's checkpoint (redacted), and the checkpoint is marked done.
//! 4. **Reconcile and publish** ([`crate::bootstrap_reconcile`]): the staged
//!    operations of every packet, joined, reconciled with the memory as it is
//!    then and held to the truth rules, go to [`crate::Store::publish`] as
//!    one version, with the base version checked. Annotations are never
//!    absorbed: those written meanwhile (or before) stay open.
//!
//! # Persistence and recovery
//!
//! The build has a persistent id, a manifest and a checkpoint per packet.
//! A restart resumes from the first packet not done (the evidence is not
//! read again; nothing is staged twice); a crash during publication is
//! recognised from the version history, so nothing is published twice; a
//! failure publishes nothing. States: queued (waiting: a pause, a held run,
//! another publication), running, interrupted (Stop Everything or the user:
//! resumed by the user), failed (after [`MAX_ATTEMPTS`] failed runs of one
//! packet: resumable), completed. Builds left queued or running by a closed
//! app resume on their own at the next start.
//!
//! A run held because the CLI does not trust the project folder uses no
//! attempt: the build waits ([`HOLD_RETRY_MS`], or until resumed) and goes
//! on. A pause during a run discards its reply; nothing runs or publishes
//! while paused. At the time limit the build stops extracting and publishes
//! what it analysed, with [`CoverageGap::TimeLimit`]; the rest is listed as
//! remaining.
//!
//! # Concurrency
//!
//! The build holds the project's [`PublicationGate`] while it runs (the
//! dream does the same), so a dream and a build never publish at the same
//! time. Event capture and the development cycle go on: events stay queued,
//! annotations written meanwhile stay open. Sources that changed since they
//! were read are marked to check again ([`CoverageGap::ChangedSinceStart`]).
//!
//! Every model session is a run row (cycle `bootstrap`): characters in and
//! out, duration, outcome. A session whose material was staged stays
//! `running` until the publication, which gives it its final outcome.

use std::collections::{BTreeMap, BTreeSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::{Duration, Instant};

use connecto_core::memory::{
    BootstrapOptions, BootstrapPlan, BootstrapState, BootstrapView, CoverageGap, Cycle, EvidenceId,
    Memory, MemoryId, MemorySettings, ModelChoice, Room, RunEntry, RunOutcome, SourceClass,
};
use serde_json::{Value, json};

use crate::bootstrap_inventory::{self as inventory, Bucket};
use crate::bootstrap_packet::{self as packet, Block};
use crate::bootstrap_reconcile::{self as reconcile, Fact};
use crate::develop::retry_delay_ms;
use crate::dream::{DreamReport, PublicationGate};
use crate::error::{Result, StoreError};
use crate::gate::RunGate;
use crate::host::{Host, ModelJob, RunError};
use crate::lock;
use crate::operations::{self, Packet, Staged};
use crate::publish::{MemoryOp, PublishStep};
use crate::queue::{BootstrapSource, Checkpoint};
use crate::store::Store;
use crate::writer::{WriterHandle, call};

/// How long one model session of the build may take, at most.
pub const RUN_DEADLINE: Duration = Duration::from_secs(15 * 60);
/// Failed sessions of one packet before the build fails.
pub const MAX_ATTEMPTS: u32 = 3;
/// A held build tries again after this long (or when resumed).
pub const HOLD_RETRY_MS: u64 = crate::develop::HOLD_RETRY_MS;
/// The checkpoint that holds the build's own state.
pub const META: u32 = u32::MAX;
const POLL: Duration = Duration::from_millis(20);
const MINUTE_MS: u64 = 60_000;

const PENDING: &str = "pending";
const DONE: &str = "done";
/// A packet of one piece of evidence that alone does not fit the input
/// limit: left out, its evidence and sources kept, never sent. The
/// input-limit gap and the sources' `partly_read` state derive from it.
const SKIPPED: &str = "skipped";
const VOID: &str = "void";
const PREPARED: &str = "prepared";
const PUBLISHING: &str = "publishing";
const PUBLISHED: &str = "published";
/// Manifest words.
const ANALYSED: &str = "analysed";
const CHANGED: &str = "changed";
const OVER_LIMIT: &str = "over_limit";
/// A source some of whose pieces were left out of the build (each alone
/// over the input limit once redacted): read in part, never "analysed".
const PARTLY_READ: &str = "partly_read";
const TIME_LEFT: &str = "remaining:time_limit";

/// The per-run input cap the build uses: the dream's.
fn run_cap(settings: &MemorySettings) -> usize {
    settings.dream.max_input_chars as usize
}

/// Why the per-run input limit cannot hold one packet's prompt (the fixed
/// instructions, room for context and the smallest material), if it cannot:
/// a misconfiguration shown as is, never an over-limit prompt.
#[must_use]
pub fn limit_problem(settings: &MemorySettings) -> Option<String> {
    let min = packet::min_run_chars();
    (run_cap(settings) < min).then(|| {
        format!(
            "the dream's input limit ({} characters) is too small for the initial build: \
             each session needs at least {min} (its fixed instructions alone take {})",
            run_cap(settings),
            packet::fixed_chars()
        )
    })
}

/// The model the build uses.
#[must_use]
pub fn choice_for(options: &BootstrapOptions, settings: &MemorySettings) -> ModelChoice {
    options
        .choice
        .clone()
        .unwrap_or_else(|| settings.dream.choice.clone())
}

// ---- the plan

/// What a build with these options would read (spec §5.9): sources by kind,
/// gaps, characters and runs. Reads listings, git and the conversation
/// index through the host; no file content, no store.
#[must_use]
pub fn plan(
    host: &dyn Host,
    project_id: &str,
    project: &Path,
    options: &BootstrapOptions,
    settings: &MemorySettings,
) -> BootstrapPlan {
    let options = options.clone().clamped();
    let now = host.now().utc_ms;
    let inv = inventory::collect(host, project_id, project, &options, now);
    let chars = inv.selected_chars();
    let budget = packet::material_budget(run_cap(settings)) as u64;
    let runs = if chars == 0 {
        0
    } else {
        u32::try_from(chars.div_ceil(budget.max(1))).unwrap_or(u32::MAX)
    };
    BootstrapPlan {
        coverage: inv.plan_coverage(),
        gaps: inv.gaps.clone(),
        chars,
        runs,
        options,
    }
}

// ---- the engine's slot

/// A project's build state in the engine: the latest plan, the build thread,
/// and how to stop it.
#[derive(Debug, Default)]
pub struct BootstrapSlot {
    inner: Mutex<SlotInner>,
    plan_gen: AtomicU64,
    /// Set by a resume while the build waits (held, retry): try now.
    nudge: Arc<AtomicBool>,
}

#[derive(Debug, Default)]
struct SlotInner {
    plan: Option<BootstrapPlan>,
    planning: Option<u64>,
    thread: Option<JoinHandle<()>>,
    stops: Option<Stops>,
}

/// What a build thread is given to stop with.
#[derive(Debug, Clone, Default)]
pub struct Stops {
    /// Set by any stop; given to the model run in flight.
    pub stop: Arc<AtomicBool>,
    /// With `stop`: memory turned off or the app closing, so the build stays
    /// resumable on its own. Without: the user or Stop Everything, so the
    /// build becomes interrupted.
    pub closing: Arc<AtomicBool>,
    pub nudge: Arc<AtomicBool>,
}

impl Stops {
    /// The user or Stop Everything.
    pub fn cancel(&self) {
        self.stop.store(true, Ordering::SeqCst);
    }

    /// Memory turned off or the app closing.
    pub fn close(&self) {
        self.closing.store(true, Ordering::SeqCst);
        self.stop.store(true, Ordering::SeqCst);
    }
}

impl BootstrapSlot {
    /// A build thread of this project is alive.
    #[must_use]
    pub fn running(&self) -> bool {
        lock(&self.inner)
            .thread
            .as_ref()
            .is_some_and(|t| !t.is_finished())
    }

    /// The latest plan and whether one is being computed.
    #[must_use]
    pub fn plan(&self) -> (Option<BootstrapPlan>, bool) {
        let inner = lock(&self.inner);
        (inner.plan.clone(), inner.planning.is_some())
    }

    /// A plan is being computed; returns its number.
    pub fn begin_plan(&self) -> u64 {
        let n = self.plan_gen.fetch_add(1, Ordering::SeqCst) + 1;
        lock(&self.inner).planning = Some(n);
        n
    }

    /// The plan numbered `n` is ready (an older one than the latest asked
    /// for is dropped).
    pub fn end_plan(&self, n: u64, plan: BootstrapPlan) {
        let mut inner = lock(&self.inner);
        if inner.planning == Some(n) {
            inner.plan = Some(plan);
            inner.planning = None;
        }
    }

    /// The plan numbered `n` will not come.
    pub fn drop_plan(&self, n: u64) {
        let mut inner = lock(&self.inner);
        if inner.planning == Some(n) {
            inner.planning = None;
        }
    }

    /// Start `body` on a thread of its own unless a build runs.
    pub fn launch(
        &self,
        name: String,
        body: impl FnOnce(Stops) + Send + 'static,
    ) -> std::result::Result<(), String> {
        let mut inner = lock(&self.inner);
        if inner.thread.as_ref().is_some_and(|t| !t.is_finished()) {
            return Err("an initial build is already running".to_owned());
        }
        if let Some(old) = inner.thread.take() {
            let _ = old.join();
        }
        let stops = Stops {
            nudge: Arc::clone(&self.nudge),
            ..Stops::default()
        };
        self.nudge.store(false, Ordering::SeqCst);
        inner.stops = Some(stops.clone());
        let thread = std::thread::Builder::new()
            .name(name)
            .spawn(move || body(stops))
            .map_err(|e| format!("the initial build did not start: {e}"))?;
        inner.thread = Some(thread);
        Ok(())
    }

    /// Stop the build in flight; it becomes interrupted.
    pub fn cancel(&self) {
        if let Some(s) = lock(&self.inner).stops.as_ref() {
            s.cancel();
        }
    }

    /// Wake a build that waits (held, retry delay).
    pub fn nudge(&self) {
        self.nudge.store(true, Ordering::SeqCst);
    }

    /// Stop the build in flight without waiting; it stays resumable on its
    /// own (the app is closing).
    pub fn halt(&self) {
        if let Some(s) = lock(&self.inner).stops.as_ref() {
            s.close();
        }
    }

    /// Stop the build thread and wait for it; the build stays resumable.
    pub fn close(&self) {
        let thread = {
            let mut inner = lock(&self.inner);
            if let Some(s) = inner.stops.as_ref() {
                s.close();
            }
            inner.thread.take()
        };
        if let Some(t) = thread {
            let _ = t.join();
        }
    }
}

// ---- the build

/// What a build needs from its thread.
pub struct Context<'a> {
    pub host: Arc<dyn Host>,
    pub writer: &'a WriterHandle,
    pub project_id: &'a str,
    pub project: &'a Path,
    pub settings: MemorySettings,
    pub stops: Stops,
    pub gate: Arc<PublicationGate>,
    /// Called when the view should be refreshed.
    pub changed: &'a (dyn Fn() + Sync),
}

/// How a build starts.
#[derive(Debug, Clone)]
pub enum Start {
    /// A new build with this id.
    New {
        id: String,
        options: BootstrapOptions,
    },
    /// The latest unfinished build.
    Resume,
}

/// A new build id: `b-` and 16 hex characters.
#[must_use]
pub fn new_id() -> String {
    format!("b-{}", &crate::prompt::nonce()[..16])
}

/// How a wait ended.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Wake {
    Ready,
    Cancelled,
    Closing,
}

/// The build's own state, in the [`META`] checkpoint. Once published, it
/// holds what the publication did with each staged proposal (written in
/// the publication's own transaction, so it survives any crash with it).
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Meta {
    state: String,
    gaps: BTreeSet<CoverageGap>,
    cutoff: u64,
    base: u64,
    version: Option<u64>,
    git: Option<String>,
    /// The publication's [`Outcomes`].
    done: Outcomes,
}

fn tags_json(list: &BTreeSet<Tag>) -> Value {
    Value::Array(list.iter().map(|(p, o)| json!([p, o])).collect())
}

fn tag_of(t: &Value) -> Option<Tag> {
    let n = |i: usize| t.get(i)?.as_u64().and_then(|x| u32::try_from(x).ok());
    Some((n(0)?, n(1)?))
}

fn tagged_json(list: &[(Tag, String)]) -> Value {
    Value::Array(
        list.iter()
            .map(|((p, o), why)| json!([p, o, why]))
            .collect(),
    )
}

fn tagged_from(v: &Value) -> Vec<(Tag, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|t| Some((tag_of(t)?, t.get(2)?.as_str()?.to_owned())))
        .collect()
}

impl Meta {
    fn to_checkpoint(&self, now_ms: u64) -> Checkpoint {
        Checkpoint {
            packet: META,
            state: self.state.clone(),
            run_id: None,
            payload: json!({
                "gaps": self.gaps.iter().map(|g| g.word()).collect::<Vec<_>>(),
                "cutoff": self.cutoff,
                "base": self.base,
                "version": self.version,
                "git": self.git,
                "in_version": tags_json(&self.done.in_version),
                "adjusted": tagged_json(&self.done.adjusted),
                "refused": tagged_json(&self.done.refused),
            })
            .to_string(),
            updated_ms: now_ms,
        }
    }

    fn from_checkpoint(c: &Checkpoint) -> Self {
        let v: Value = serde_json::from_str(&c.payload).unwrap_or(Value::Null);
        Self {
            state: c.state.clone(),
            gaps: v["gaps"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|g| g.as_str().and_then(CoverageGap::from_word))
                .collect(),
            cutoff: v["cutoff"].as_u64().unwrap_or(0),
            base: v["base"].as_u64().unwrap_or(0),
            version: v["version"].as_u64(),
            git: v["git"].as_str().map(str::to_owned),
            done: Outcomes {
                in_version: v["in_version"]
                    .as_array()
                    .into_iter()
                    .flatten()
                    .filter_map(tag_of)
                    .collect(),
                adjusted: tagged_from(&v["adjusted"]),
                refused: tagged_from(&v["refused"]),
            },
        }
    }
}

/// One packet's checkpoint payload.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
struct Work {
    evidence: Vec<u64>,
    labels: Vec<String>,
    sources: Vec<String>,
    memories: Vec<String>,
    staged: Vec<String>,
    attempts: u32,
    rejected: u32,
    /// Operations the model proposed in this packet's reply.
    proposed: u32,
    /// Why each refused one was refused: (its index in the reply, reason).
    /// The validator's words, never the model's text.
    reasons: Vec<(u32, String)>,
    /// What was kept in another form, and why: (index, reason).
    adjusted: Vec<(u32, String)>,
    /// For each of `staged`, the index in the reply of the proposal it
    /// came from.
    origin: Vec<u32>,
    /// For each of `evidence`, the manifest key of the source it is a piece
    /// of: what a split packet's halves, or a skipped piece, are about.
    piece_sources: Vec<String>,
}

/// `(index, reason)` pairs as JSON, and back.
fn pairs_json(list: &[(u32, String)]) -> Value {
    Value::Array(list.iter().map(|(i, r)| json!([i, r])).collect())
}

fn pairs_from(v: &Value) -> Vec<(u32, String)> {
    v.as_array()
        .into_iter()
        .flatten()
        .filter_map(|p| {
            let i = u32::try_from(p.get(0)?.as_u64()?).ok()?;
            Some((i, p.get(1)?.as_str()?.to_owned()))
        })
        .collect()
}

impl Work {
    fn encode(&self) -> String {
        json!({
            "evidence": self.evidence,
            "labels": self.labels,
            "sources": self.sources,
            "memories": self.memories,
            "staged": self.staged,
            "attempts": self.attempts,
            "rejected": self.rejected,
            "proposed": self.proposed,
            "reasons": pairs_json(&self.reasons),
            "adjusted": pairs_json(&self.adjusted),
            "origin": self.origin,
            "piece_sources": self.piece_sources,
        })
        .to_string()
    }

    fn decode(text: &str) -> Self {
        let v: Value = serde_json::from_str(text).unwrap_or(Value::Null);
        let strings = |k: &str| -> Vec<String> {
            v[k].as_array()
                .into_iter()
                .flatten()
                .filter_map(|s| s.as_str().map(str::to_owned))
                .collect()
        };
        Self {
            evidence: v["evidence"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(Value::as_u64)
                .collect(),
            labels: strings("labels"),
            sources: strings("sources"),
            memories: strings("memories"),
            staged: strings("staged"),
            attempts: u32::try_from(v["attempts"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX),
            rejected: u32::try_from(v["rejected"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX),
            proposed: u32::try_from(v["proposed"].as_u64().unwrap_or(0)).unwrap_or(u32::MAX),
            reasons: pairs_from(&v["reasons"]),
            adjusted: pairs_from(&v["adjusted"]),
            origin: v["origin"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|x| x.as_u64().and_then(|x| u32::try_from(x).ok()))
                .collect(),
            piece_sources: strings("piece_sources"),
        }
    }

    /// The part of this packet holding the evidence in `range`, with only
    /// the sources of those pieces. A checkpoint written before pieces
    /// carried their source keeps every source (the safe direction: none
    /// of them counts as analysed before all are done).
    fn part(&self, range: std::ops::Range<usize>) -> Self {
        let known = self.piece_sources.len() == self.evidence.len();
        let piece_sources: Vec<String> = if known {
            self.piece_sources[range.clone()].to_vec()
        } else {
            Vec::new()
        };
        let mut sources: Vec<String> = Vec::new();
        if known {
            for src in &piece_sources {
                if !sources.contains(src) {
                    sources.push(src.clone());
                }
            }
        } else {
            sources.clone_from(&self.sources);
        }
        Self {
            evidence: self
                .evidence
                .get(range.clone())
                .map(<[u64]>::to_vec)
                .unwrap_or_default(),
            labels: self
                .labels
                .get(range)
                .map(<[String]>::to_vec)
                .unwrap_or_default(),
            sources,
            piece_sources,
            ..Self::default()
        }
    }

    /// How many the model proposed: counted at the reply, or (a checkpoint
    /// written before that count was kept) what was staged and refused.
    fn proposed(&self) -> u32 {
        if self.proposed > 0 {
            return self.proposed;
        }
        let staged = u32::try_from(self.staged.len()).unwrap_or(u32::MAX);
        self.rejected.saturating_add(staged)
    }

    /// The staged operations of packet `packet`, each tagged with its
    /// provenance (a checkpoint from before provenance was kept tags each
    /// by its own position).
    fn tagged(&self, packet: u32) -> Vec<(MemoryOp, Tag)> {
        let whole = self.origin.len() == self.staged.len();
        self.staged
            .iter()
            .enumerate()
            .filter_map(|(i, s)| match operations::decode(s) {
                Some(Staged::Op(op)) => {
                    let at = if whole {
                        self.origin[i]
                    } else {
                        u32::try_from(i).unwrap_or(u32::MAX)
                    };
                    Some((op, (packet, at)))
                }
                _ => None,
            })
            .collect()
    }
}

fn set_state(cx: &Context<'_>, id: &str, state: BootstrapState) -> Result<()> {
    let now = cx.host.now().utc_ms;
    let id = id.to_owned();
    call(cx.writer, move |s| s.set_bootstrap_state(&id, &state, now))?;
    (cx.changed)();
    Ok(())
}

impl Context<'_> {
    fn stopped(&self) -> Option<Wake> {
        if !self.stops.stop.load(Ordering::SeqCst) {
            None
        } else if self.stops.closing.load(Ordering::SeqCst) {
            Some(Wake::Closing)
        } else {
            Some(Wake::Cancelled)
        }
    }

    /// Wait until `until_ms` (fake or real clock) and no pause, or a stop.
    /// A nudge ends a timed wait at once.
    fn wait(&self, until_ms: Option<u64>) -> Wake {
        loop {
            if let Some(w) = self.stopped() {
                return w;
            }
            let nudged = self.stops.nudge.swap(false, Ordering::SeqCst);
            let due = nudged || until_ms.is_none_or(|t| self.host.now().utc_ms >= t);
            if due && !self.host.paused() {
                return Wake::Ready;
            }
            std::thread::sleep(POLL);
        }
    }
}

/// Run a build to its end, on the caller's thread: a new one, or the latest
/// unfinished one. Returns its final state (`Running` when the thread was
/// asked to close: it resumes at the next start).
pub fn run(cx: &Context<'_>, start: Start) -> Result<BootstrapState> {
    let now = cx.host.now().utc_ms;
    let id = match start {
        Start::New { id, options } => {
            let new_id = id.clone();
            call(cx.writer, move |s| {
                if let Some(old) = s.bootstrap(None)?
                    && !matches!(old.state, BootstrapState::Completed)
                {
                    s.set_bootstrap_state(
                        &old.id,
                        &BootstrapState::Failed {
                            reason: "replaced by a newer initial build".to_owned(),
                        },
                        now,
                    )?;
                }
                s.create_bootstrap(&new_id, &options, now)
            })?;
            (cx.changed)();
            id
        }
        Start::Resume => match call(cx.writer, |s| s.bootstrap(None))? {
            Some(row) if row.state != BootstrapState::Completed => row.id,
            Some(row) => return Ok(row.state),
            None => return Ok(BootstrapState::NotStarted),
        },
    };
    let result = drive(cx, &id);
    match result {
        Ok(state) => Ok(state),
        Err(e) => {
            if cx.stopped() == Some(Wake::Closing) {
                return Ok(BootstrapState::Running);
            }
            let state = BootstrapState::Failed {
                reason: e.to_string(),
            };
            let _ = settle_runs(cx, &id, &RunOutcome::Interrupted);
            let _ = set_state(cx, &id, state.clone());
            Ok(state)
        }
    }
}

/// Every session of the build still marked running (or cut by a restart)
/// gets `outcome`, keeping its measures.
fn settle_runs(cx: &Context<'_>, id: &str, outcome: &RunOutcome) -> Result<()> {
    let id = id.to_owned();
    let outcome = outcome.clone();
    call(cx.writer, move |s| {
        for c in s.checkpoints(&id)? {
            if c.packet == META || c.state != DONE {
                continue;
            }
            let Some(run) = c.run_id else { continue };
            let Some(entry) = s.run(run)? else { continue };
            if matches!(entry.outcome, RunOutcome::Running | RunOutcome::Interrupted) {
                finish_keeping(s, &entry, &outcome)?;
            }
        }
        Ok(())
    })
}

fn finish_keeping(s: &mut Store, entry: &RunEntry, outcome: &RunOutcome) -> Result<()> {
    let finished = entry
        .started_ms
        .saturating_add(entry.duration_ms.unwrap_or(0));
    s.finish_run(entry.id, outcome, finished, entry.chars_in, entry.chars_out)
}

fn stop_with(cx: &Context<'_>, id: &str, wake: Wake) -> Result<BootstrapState> {
    if wake == Wake::Closing {
        return Ok(BootstrapState::Running);
    }
    settle_runs(cx, id, &RunOutcome::Interrupted)?;
    set_state(cx, id, BootstrapState::Interrupted)?;
    Ok(BootstrapState::Interrupted)
}

#[allow(clippy::too_many_lines)]
fn drive(cx: &Context<'_>, id: &str) -> Result<BootstrapState> {
    let started = cx.host.now().utc_ms;
    let row = {
        let key = id.to_owned();
        call(cx.writer, move |s| s.bootstrap(Some(&key)))?
            .ok_or_else(|| StoreError::UnknownBootstrap(id.to_owned()))?
    };
    let options = row.options.clone();
    let choice = choice_for(&options, &cx.settings);
    // Wait for the gate: a dream in flight finishes first.
    set_state(cx, id, BootstrapState::Queued)?;
    let Some(_gate) = cx
        .gate
        .acquire(Cycle::Bootstrap, &cx.stops.stop)
        .filter(|_| cx.stopped().is_none())
    else {
        return stop_with(cx, id, cx.stopped().unwrap_or(Wake::Cancelled));
    };
    if let Some(reason) = cx.host.capability(&choice).reason() {
        let state = BootstrapState::Failed {
            reason: format!("{}: {reason}", choice.provider.label()),
        };
        set_state(cx, id, state.clone())?;
        return Ok(state);
    }
    if let Some(reason) = limit_problem(&cx.settings) {
        let state = BootstrapState::Failed { reason };
        set_state(cx, id, state.clone())?;
        return Ok(state);
    }
    set_state(cx, id, BootstrapState::Running)?;

    let mut meta = load_meta(cx, id)?;
    if meta.as_ref().is_none_or(|m| m.state.is_empty()) {
        meta = Some(prepare(cx, id, &options)?);
        (cx.changed)();
    }
    let mut meta = meta.unwrap_or_default();
    if meta.state == PUBLISHED {
        // Published: settle any run row still unfinished (idempotent).
        let (key, m, now) = (id.to_owned(), meta.clone(), cx.host.now().utc_ms);
        call(cx.writer, move |s| finalise(s, &key, &m, now))?;
        set_state(cx, id, BootstrapState::Completed)?;
        return Ok(BootstrapState::Completed);
    }
    if meta.state == PUBLISHING && (meta.version.is_some() || already_published(cx, &meta)?) {
        return finish_published(cx, id, &mut meta, None);
    }
    // A resumed build gets its attempts back.
    {
        let id = id.to_owned();
        call(cx.writer, move |s| {
            for mut c in s.checkpoints(&id)? {
                let mut work = Work::decode(&c.payload);
                if c.packet != META && c.state == PENDING && work.attempts > 0 {
                    work.attempts = 0;
                    c.payload = work.encode();
                    s.put_checkpoint(&id, &c)?;
                }
            }
            Ok(())
        })?;
    }

    // Extraction, packet by packet.
    let deadline_ms = started.saturating_add(u64::from(options.max_minutes) * MINUTE_MS);
    let secrets = cx.host.known_secrets();
    let mut timed_out = false;
    loop {
        let checkpoints = {
            let id = id.to_owned();
            call(cx.writer, move |s| s.checkpoints(&id))?
        };
        let Some(next) = checkpoints
            .iter()
            .find(|c| c.packet != META && c.state == PENDING)
            .cloned()
        else {
            break;
        };
        if let Some(w) = cx.stopped() {
            return stop_with(cx, id, w);
        }
        if cx.host.paused() {
            set_state(cx, id, BootstrapState::Queued)?;
            match cx.wait(None) {
                Wake::Ready => set_state(cx, id, BootstrapState::Running)?,
                w => return stop_with(cx, id, w),
            }
        }
        let now = cx.host.now().utc_ms;
        if now >= deadline_ms {
            timed_out = true;
            break;
        }
        let mut work = Work::decode(&next.payload);
        let candidates = candidates(&checkpoints);
        let (prompt, shown) = build_prompt(cx, &work, &candidates, &secrets)?;
        if prompt.chars().count() > run_cap(&cx.settings) {
            // Packets are cut to the limit when prepared, on text redacted
            // then: a secret known since that grows on redaction, or a limit
            // lowered since, gets here. Never failed, never retried as it
            // was: the packet splits in two, or a single piece that alone
            // does not fit is left out, as an input-limit gap.
            let (key, cp, w, now) = (
                id.to_owned(),
                next.clone(),
                work.clone(),
                cx.host.now().utc_ms,
            );
            // One transaction; the input-limit gap derives from a skipped
            // packet ([`view`], [`mark_sources`]): nothing else to write.
            call(cx.writer, move |s| split_packet(s, &key, &cp, &w, now))?;
            (cx.changed)();
            continue;
        }
        // This session's right to stage: its pause epoch and the build's
        // stop flag, checked again right before its result is written.
        let gate = RunGate::new(Arc::clone(&cx.host), Arc::clone(&cx.stops.stop));
        let epoch = gate.epoch();
        let run_id = {
            let (c, id) = (choice.clone(), id.to_owned());
            let mut cp = next.clone();
            // The run and its link from the packet, together.
            call(cx.writer, move |s| {
                crate::scheduler::in_tx(s, |s| {
                    let run = s.start_run(Cycle::Bootstrap, &c, now, epoch)?;
                    cp.run_id = Some(run);
                    cp.updated_ms = now;
                    s.put_checkpoint(&id, &cp)?;
                    Ok(run)
                })
            })?
        };
        (cx.changed)();
        let left = Duration::from_millis(deadline_ms.saturating_sub(now));
        let job = ModelJob {
            project_id: cx.project_id.to_owned(),
            cycle: Some(Cycle::Bootstrap),
            choice: choice.clone(),
            prompt: prompt.clone(),
        };
        let chars_in = prompt.chars().count() as u64;
        let reply = cx.host.run_model(
            &job,
            Instant::now() + RUN_DEADLINE.min(left.max(Duration::from_secs(1))),
            gate.cancel_flag(),
        );
        let finished = cx.host.now().utc_ms;
        let finish = |outcome: RunOutcome, chars_out: u64, chars_in: u64| {
            call(cx.writer, move |s| {
                s.finish_run(run_id, &outcome, finished, chars_in, chars_out)
            })
        };
        let reply = match reply {
            Ok(r) => r,
            Err(RunError::Cancelled) => {
                let w = cx.stopped().unwrap_or(Wake::Cancelled);
                finish(
                    if w == Wake::Closing {
                        RunOutcome::Interrupted
                    } else {
                        RunOutcome::Cancelled
                    },
                    0,
                    chars_in,
                )?;
                return stop_with(cx, id, w);
            }
            Err(RunError::HeldUntrusted { reason }) => {
                let r = reason.clone();
                call(cx.writer, move |s| s.hold_run(run_id, &r, finished))?;
                set_state(cx, id, BootstrapState::Queued)?;
                match cx.wait(Some(finished.saturating_add(HOLD_RETRY_MS))) {
                    Wake::Ready => {
                        set_state(cx, id, BootstrapState::Running)?;
                        continue;
                    }
                    w => return stop_with(cx, id, w),
                }
            }
            Err(e) => {
                finish(
                    RunOutcome::Failed {
                        reason: e.to_string(),
                    },
                    0,
                    chars_in,
                )?;
                match failed_attempt(cx, id, &next, &mut work, &e.to_string())? {
                    Some(state) => return Ok(state),
                    None => continue,
                }
            }
        };
        let chars_out = reply.text.chars().count() as u64;
        let chars_in = if reply.chars_in > 0 {
            reply.chars_in
        } else {
            chars_in
        };
        if gate.closed().is_some() {
            finish(RunOutcome::HeldByPause, chars_out, chars_in)?;
            continue;
        }
        let proposed = match operations::parse(&reply.text) {
            Ok(p) => p,
            Err(e) => {
                let reason = format!("the reply was not usable: {e}");
                finish(
                    RunOutcome::Failed {
                        reason: reason.clone(),
                    },
                    chars_out,
                    chars_in,
                )?;
                match failed_attempt(cx, id, &next, &mut work, &reason)? {
                    Some(state) => return Ok(state),
                    None => continue,
                }
            }
        };
        let proposed_count = u32::try_from(proposed.len()).unwrap_or(u32::MAX);
        // Judge against this packet, and stage what passes.
        let packet = Packet::new(
            work.evidence.iter().map(|e| EvidenceId::new(*e)).collect(),
            shown.clone(),
            Vec::new(),
            meta.cutoff,
        );
        let secrets = secrets.clone();
        let verdict = call(cx.writer, move |s| {
            Ok(operations::validate(&s.read(), &packet, proposed, &secrets))
        })?;
        work.memories = shown.iter().map(ToString::to_string).collect();
        work.staged = verdict
            .ops
            .iter()
            .map(|op| operations::encode(&Staged::Op(op.clone())))
            .collect();
        work.origin = verdict
            .origin
            .iter()
            .map(|i| u32::try_from(*i).unwrap_or(u32::MAX))
            .collect();
        work.rejected = u32::try_from(verdict.rejected.len()).unwrap_or(u32::MAX);
        work.proposed = proposed_count;
        let indexed = |list: &[operations::Rejected]| -> Vec<(u32, String)> {
            list.iter()
                .map(|r| (u32::try_from(r.index).unwrap_or(u32::MAX), r.reason.clone()))
                .collect()
        };
        work.reasons = indexed(&verdict.rejected);
        work.adjusted = indexed(&verdict.adjusted);
        // Accepted but with nothing to stage (an absorb or a keep): not
        // part of an initial build, and said so rather than lost.
        for i in 0..proposed_count {
            if !work.origin.contains(&i) && !work.reasons.iter().any(|(r, _)| *r == i) {
                work.reasons.push((
                    i,
                    "absorbing or keeping notes is left to the dream, not the initial build"
                        .to_owned(),
                ));
            }
        }
        work.reasons.sort_by_key(|(i, _)| *i);
        work.rejected = u32::try_from(work.reasons.len()).unwrap_or(u32::MAX);
        {
            let (key, mut cp) = (id.to_owned(), next.clone());
            cp.state = DONE.to_owned();
            cp.run_id = Some(run_id);
            cp.payload = work.encode();
            cp.updated_ms = finished;
            let gate = gate.clone();
            let staged = call(cx.writer, move |s| {
                stage_gated(
                    s,
                    &key,
                    &cp,
                    &Measures {
                        run: run_id,
                        finished_ms: finished,
                        chars_in,
                        chars_out,
                    },
                    &gate,
                )
            });
            match staged {
                Ok(()) => {}
                Err(StoreError::Aborted) => {
                    let w = cx.stopped();
                    finish(
                        if w.is_some() {
                            RunOutcome::Cancelled
                        } else {
                            RunOutcome::HeldByPause
                        },
                        chars_out,
                        chars_in,
                    )?;
                    match w {
                        Some(w) => return stop_with(cx, id, w),
                        None => continue,
                    }
                }
                Err(e) => return Err(e),
            }
        }
        (cx.changed)();
    }

    // The analysed sources; what changed since it was read.
    if timed_out {
        meta.gaps.insert(CoverageGap::TimeLimit);
    }
    mark_sources(cx, id, &mut meta, timed_out)?;

    // Publication: never while paused.
    if cx.host.paused() {
        set_state(cx, id, BootstrapState::Queued)?;
        match cx.wait(None) {
            Wake::Ready => set_state(cx, id, BootstrapState::Running)?,
            w => return stop_with(cx, id, w),
        }
    }
    loop {
        if let Some(w) = cx.stopped() {
            return stop_with(cx, id, w);
        }
        match publish(cx, id, &mut meta) {
            // A pause (or a stop) closed the gate before the commit.
            Err(StoreError::Aborted) => {
                set_state(cx, id, BootstrapState::Queued)?;
                match cx.wait(None) {
                    Wake::Ready => set_state(cx, id, BootstrapState::Running)?,
                    w => return stop_with(cx, id, w),
                }
            }
            other => return other,
        }
    }
}

/// A session's measures.
struct Measures {
    run: u64,
    finished_ms: u64,
    chars_in: u64,
    chars_out: u64,
}

/// Stage a packet's result: its checkpoint and its session's measures in
/// one transaction, whose commit the session's gate guards. A pause (or a
/// pause that came and went) or a stop since the session started rolls it
/// all back: `Err(Aborted)`, nothing written. The session stays `running`
/// until the publication gives it its outcome.
fn stage_gated(
    s: &mut Store,
    id: &str,
    cp: &Checkpoint,
    m: &Measures,
    gate: &RunGate,
) -> Result<()> {
    use crate::codec::to_i64;
    let project_id = s.project_id().to_owned();
    let tx = s.begin()?;
    tx.execute(
        "INSERT INTO bootstrap_checkpoint (project_id, bootstrap_id, packet, state, run_id, payload, updated_ms)
         VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)
         ON CONFLICT (bootstrap_id, packet) DO UPDATE SET state = excluded.state,
             run_id = excluded.run_id, payload = excluded.payload, updated_ms = excluded.updated_ms",
        rusqlite::params![
            project_id,
            id,
            cp.packet,
            cp.state,
            cp.run_id.map(to_i64),
            cp.payload,
            to_i64(cp.updated_ms)
        ],
    )?;
    let changed = tx.execute(
        "UPDATE run SET outcome = 'running', finished_ms = ?2, chars_in = ?3, chars_out = ?4
         WHERE id = ?1",
        rusqlite::params![
            to_i64(m.run),
            to_i64(m.finished_ms),
            to_i64(m.chars_in),
            to_i64(m.chars_out)
        ],
    )?;
    if changed == 0 {
        return Err(StoreError::UnknownRun(m.run));
    }
    // The last step before the commit.
    gate.check()?;
    tx.commit()?;
    Ok(())
}

/// A pending packet whose prompt no longer fits the limit, in one
/// transaction: with more than one piece of evidence, its first half stays
/// in it and the second becomes a new pending packet, each with the
/// sources of its own pieces only (a source with pieces in both stays open
/// until both are done); with one, it becomes
/// `skipped`, keeping its evidence and sources: never sent, its sources
/// `partly_read`, the input-limit gap declared (both derived from it).
/// `true` when a piece was left out.
fn split_packet(s: &mut Store, id: &str, cp: &Checkpoint, work: &Work, now: u64) -> Result<bool> {
    crate::scheduler::in_tx(s, |s| {
        if work.evidence.len() > 1 {
            let mid = work.evidence.len() / 2;
            // Each half with the sources of its own pieces only.
            let half = |range: std::ops::Range<usize>| work.part(range);
            let next = s
                .checkpoints(id)?
                .iter()
                .filter(|c| c.packet != META)
                .map(|c| c.packet)
                .max()
                .unwrap_or(0)
                .saturating_add(1);
            let mut first = cp.clone();
            first.state = PENDING.to_owned();
            first.run_id = None;
            first.payload = half(0..mid).encode();
            first.updated_ms = now;
            s.put_checkpoint(id, &first)?;
            s.put_checkpoint(
                id,
                &Checkpoint {
                    packet: next,
                    state: PENDING.to_owned(),
                    run_id: None,
                    payload: half(mid..work.evidence.len()).encode(),
                    updated_ms: now,
                },
            )?;
            Ok(false)
        } else {
            let mut skipped = cp.clone();
            skipped.state = SKIPPED.to_owned();
            skipped.run_id = None;
            // Its own piece's source only: no other source of the packet is
            // marked as read in part.
            skipped.payload = work.part(0..work.evidence.len()).encode();
            skipped.updated_ms = now;
            s.put_checkpoint(id, &skipped)?;
            Ok(true)
        }
    })
}

/// A failed session: count the attempt; at the last one the build fails.
/// `None`: try again (after the delay).
fn failed_attempt(
    cx: &Context<'_>,
    id: &str,
    cp: &Checkpoint,
    work: &mut Work,
    reason: &str,
) -> Result<Option<BootstrapState>> {
    work.attempts += 1;
    let now = cx.host.now().utc_ms;
    {
        let (id, mut cp) = (id.to_owned(), cp.clone());
        cp.payload = work.encode();
        cp.updated_ms = now;
        call(cx.writer, move |s| s.put_checkpoint(&id, &cp))?;
    }
    if work.attempts >= MAX_ATTEMPTS {
        settle_runs(cx, id, &RunOutcome::Interrupted)?;
        let state = BootstrapState::Failed {
            reason: format!("a packet failed {MAX_ATTEMPTS} times: {reason}"),
        };
        set_state(cx, id, state.clone())?;
        return Ok(Some(state));
    }
    match cx.wait(Some(now.saturating_add(retry_delay_ms(work.attempts)))) {
        Wake::Ready => Ok(None),
        w => stop_with(cx, id, w).map(Some),
    }
}

fn load_meta(cx: &Context<'_>, id: &str) -> Result<Option<Meta>> {
    let id = id.to_owned();
    let cps = call(cx.writer, move |s| s.checkpoints(&id))?;
    Ok(cps
        .iter()
        .find(|c| c.packet == META)
        .map(Meta::from_checkpoint))
}

/// Read the sources, store them as evidence, write the manifest, the packets
/// and the build's state.
fn prepare(cx: &Context<'_>, id: &str, options: &BootstrapOptions) -> Result<Meta> {
    let now = cx.host.now().utc_ms;
    let inv = inventory::collect(cx.host.as_ref(), cx.project_id, cx.project, options, now);
    let budget = packet::material_budget(run_cap(&cx.settings));
    let material = packet::read_sources(cx.host.as_ref(), cx.project, &inv, budget, now);
    let packets = packet::pack(&material.parts, budget);
    let mut gaps: BTreeSet<CoverageGap> = inv.gaps.iter().copied().collect();
    if material.manifest.iter().any(|(_, _, _, w)| w == OVER_LIMIT) {
        gaps.insert(CoverageGap::InputLimit);
    }
    let git = inv.git.as_ref().map(|_| packet::hash(&inv.git_state));
    let id = id.to_owned();
    let meta = call(cx.writer, move |s| {
        let mut ids: Vec<u64> = Vec::with_capacity(material.parts.len());
        for part in &material.parts {
            ids.push(s.insert_evidence(part.evidence.clone())?.id.seq());
        }
        let in_packets: BTreeSet<&str> = material.parts.iter().map(|p| p.source.as_str()).collect();
        for (key, class, hash, word) in &material.manifest {
            // A source read with nothing to send (a conversation of notices
            // only) is analysed as it is.
            let word = if word == PENDING && !in_packets.contains(key.as_str()) {
                ANALYSED
            } else {
                word.as_str()
            };
            s.put_bootstrap_source(
                &id,
                &BootstrapSource {
                    path: key.clone(),
                    class: *class,
                    hash: hash.clone(),
                    state: word.to_owned(),
                },
            )?;
        }
        let total = u32::try_from(packets.len()).unwrap_or(u32::MAX);
        for (n, indexes) in packets.iter().enumerate() {
            let mut work = Work::default();
            for i in indexes {
                let part = &material.parts[*i];
                work.evidence.push(ids[*i]);
                work.labels.push(part.label.clone());
                work.piece_sources.push(part.source.clone());
                if !work.sources.contains(&part.source) {
                    work.sources.push(part.source.clone());
                }
            }
            s.put_checkpoint(
                &id,
                &Checkpoint {
                    packet: u32::try_from(n).unwrap_or(u32::MAX - 1),
                    state: PENDING.to_owned(),
                    run_id: None,
                    payload: work.encode(),
                    updated_ms: now,
                },
            )?;
        }
        // Checkpoints of an earlier, unfinished preparation.
        for mut c in s.checkpoints(&id)? {
            if c.packet != META && c.packet >= total {
                c.state = VOID.to_owned();
                s.put_checkpoint(&id, &c)?;
            }
        }
        let read = s.read();
        let meta = Meta {
            state: PREPARED.to_owned(),
            gaps,
            cutoff: read.annotation_seq()?,
            base: read.version()?,
            version: None,
            git,
            done: Outcomes::default(),
        };
        s.put_checkpoint(&id, &meta.to_checkpoint(now))?;
        Ok(meta)
    })?;
    Ok(meta)
}

/// Addresses and summaries staged by done packets, for later ones.
fn candidates(checkpoints: &[Checkpoint]) -> Vec<String> {
    let mut out = Vec::new();
    for c in checkpoints
        .iter()
        .filter(|c| c.packet != META && c.state == DONE)
    {
        for (op, _) in Work::decode(&c.payload).tagged(c.packet) {
            if let MemoryOp::Create(m) = op {
                out.push(format!(
                    "{}: {}",
                    m.address,
                    m.summary.get().replace('\n', " ")
                ));
            }
        }
    }
    out
}

/// Words of four letters or more, to find relevant memories with.
fn terms(text: &str) -> BTreeSet<String> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| w.chars().count() >= 4)
        .map(str::to_lowercase)
        .take(4000)
        .collect()
}

/// The prompt of one packet, and the memories it shows.
/// The packet's prompt. Evidence and memories were redacted as they were
/// stored: a secret known only since (`secrets`) is hidden in the material
/// (evidence headers and excerpts, memory lines, candidates), never in the
/// instructions, and the delimiter id is chosen after.
fn build_prompt(
    cx: &Context<'_>,
    work: &Work,
    candidates: &[String],
    secrets: &[String],
) -> Result<(String, Vec<MemoryId>)> {
    let evidence = work.evidence.clone();
    let labels = work.labels.clone();
    let (blocks, memories) = call(cx.writer, move |s| {
        let read = s.read();
        let mut blocks = Vec::new();
        for (i, seq) in evidence.iter().enumerate() {
            if let Some(e) = read.evidence(EvidenceId::new(*seq))? {
                let label = labels.get(i).cloned().unwrap_or_default();
                blocks.push(Block {
                    header: packet::header(e.id, e.kind, &label),
                    text: e.excerpt.get().clone(),
                });
            }
        }
        let memories: Vec<Memory> = read
            .memories()?
            .into_iter()
            .filter(|m| m.room() != Room::Archive)
            .collect();
        Ok((blocks, memories))
    })?;
    let context = packet::context_budget(run_cap(&cx.settings));
    // Memories by relevance to the packet's words, within half the context.
    let words = terms(
        &blocks
            .iter()
            .map(|b| b.text.as_str())
            .collect::<Vec<_>>()
            .join(" "),
    );
    let mut ranked: Vec<(usize, &Memory)> = memories
        .iter()
        .map(|m| {
            let mine = terms(&format!("{} {}", m.address, m.summary));
            (mine.intersection(&words).count(), m)
        })
        .collect();
    ranked.sort_by_key(|r| std::cmp::Reverse(r.0));
    let mut shown = Vec::new();
    let mut lines = Vec::new();
    let mut used = 0;
    for (_, m) in ranked {
        let line = packet::memory_line(m);
        let c = line.chars().count() + 1;
        if used + c > context / 2 {
            break;
        }
        used += c;
        shown.push(m.id.clone());
        lines.push(line);
    }
    let mut cands = Vec::new();
    let mut used = 0;
    for c in candidates {
        let n = c.chars().count() + 1;
        if used + n > context / 2 {
            break;
        }
        used += n;
        cands.push(c.clone());
    }
    let blocks: Vec<Block> = blocks
        .iter()
        .map(|b| Block {
            header: crate::commands::hide_known(&b.header, secrets),
            text: crate::commands::hide_known(&b.text, secrets),
        })
        .collect();
    let mut lines: Vec<String> = lines
        .iter()
        .map(|l| crate::commands::hide_known(l, secrets))
        .collect();
    let mut cands: Vec<String> = cands
        .iter()
        .map(|c| crate::commands::hide_known(c, secrets))
        .collect();
    let mut texts: Vec<&str> = blocks
        .iter()
        .flat_map(|b| [b.header.as_str(), b.text.as_str()])
        .collect();
    texts.extend(lines.iter().map(String::as_str));
    texts.extend(cands.iter().map(String::as_str));
    let nonce = packet::fresh_id(&texts);
    // The whole prompt stays within the limit: context goes first.
    let cap = run_cap(&cx.settings);
    loop {
        let prompt = packet::prompt(&nonce, &lines, &cands, &blocks);
        if prompt.chars().count() <= cap || (lines.is_empty() && cands.is_empty()) {
            return Ok((prompt, shown));
        }
        if cands.pop().is_none() {
            lines.pop();
            shown.pop();
        }
    }
}

/// After extraction: the manifest says what was analysed, what is left (the
/// time limit), and what changed since it was read.
fn mark_sources(cx: &Context<'_>, id: &str, meta: &mut Meta, timed_out: bool) -> Result<()> {
    let id_owned = id.to_owned();
    let (sources, checkpoints) = call(cx.writer, move |s| {
        Ok((s.bootstrap_sources(&id_owned)?, s.checkpoints(&id_owned)?))
    })?;
    let mut done: BTreeSet<String> = BTreeSet::new();
    let mut open: BTreeSet<String> = BTreeSet::new();
    let mut skipped: BTreeSet<String> = BTreeSet::new();
    for c in checkpoints.iter().filter(|c| c.packet != META) {
        let w = Work::decode(&c.payload);
        if c.state == DONE {
            done.extend(w.sources);
        } else if c.state == PENDING {
            open.extend(w.sources);
        } else if c.state == SKIPPED {
            skipped.extend(w.sources);
        }
    }
    if !skipped.is_empty() {
        meta.gaps.insert(CoverageGap::InputLimit);
    }
    // Changed since read: files by content, the git state by its text.
    let git_now = cx.host.git_snapshot(cx.project);
    let mut updates: Vec<BootstrapSource> = Vec::new();
    for mut src in sources {
        let pending = src.state == PENDING;
        if !(pending || src.state == ANALYSED) {
            continue;
        }
        // A piece left out: read in part at most, never analysed.
        if skipped.contains(&src.path) {
            src.state = PARTLY_READ.to_owned();
            updates.push(src);
            continue;
        }
        let analysed =
            src.state == ANALYSED || (done.contains(&src.path) && !open.contains(&src.path));
        if !analysed {
            if timed_out && pending {
                src.state = TIME_LEFT.to_owned();
                updates.push(src);
            }
            continue;
        }
        let changed = if let Some(path) = src.path.strip_prefix("file:") {
            let now = cx
                .host
                .read_project_file(cx.project, path, inventory::MAX_FILE_BYTES)
                .map(|t| packet::hash(&t));
            now != src.hash
        } else if src.path == "git:state" {
            git_now
                .as_ref()
                .map(|g| packet::hash(&inventory::git_state_text(g)))
                != meta.git
        } else {
            false
        };
        src.state = if changed { CHANGED } else { ANALYSED }.to_owned();
        if changed {
            meta.gaps.insert(CoverageGap::ChangedSinceStart);
        }
        updates.push(src);
    }
    let id = id.to_owned();
    let m = meta.clone();
    let now = cx.host.now().utc_ms;
    // One transaction: the sources' states and the build's gaps together.
    call(cx.writer, move |s| {
        crate::scheduler::in_tx(s, |s| {
            for src in &updates {
                s.put_bootstrap_source(&id, src)?;
            }
            s.put_checkpoint(&id, &m.to_checkpoint(now))
        })
    })?;
    (cx.changed)();
    Ok(())
}

/// Whether a publication of this build reached the store before a stop.
fn already_published(cx: &Context<'_>, meta: &Meta) -> Result<bool> {
    let base = meta.base;
    call(cx.writer, move |s| {
        let read = s.read();
        Ok(read
            .versions(50)?
            .iter()
            .any(|v| v.version > base && v.cycle == Cycle::Bootstrap))
    })
}

fn finish_published(
    cx: &Context<'_>,
    id: &str,
    meta: &mut Meta,
    published: Option<u64>,
) -> Result<BootstrapState> {
    let now = cx.host.now().utc_ms;
    let version = match published.or(meta.version) {
        Some(v) => Some(v),
        None => {
            let base = meta.base;
            call(cx.writer, move |s| {
                Ok(s.read()
                    .versions(50)?
                    .iter()
                    .filter(|v| v.version > base && v.cycle == Cycle::Bootstrap)
                    .map(|v| v.version)
                    .min())
            })?
        }
    };
    meta.state = PUBLISHED.to_owned();
    meta.version = version;
    let (id_owned, m) = (id.to_owned(), meta.clone());
    call(cx.writer, move |s| finalise(s, &id_owned, &m, now))?;
    set_state(cx, id, BootstrapState::Completed)?;
    Ok(BootstrapState::Completed)
}

/// In one transaction: the build's checkpoint as `m` says (published) and
/// each staged session's run row settled with its packet's outcome, from
/// the same per-proposal record as the report. Rows already settled are
/// left alone, so it also repairs a build finalised before rows were
/// settled with it.
fn finalise(s: &mut Store, id: &str, m: &Meta, now: u64) -> Result<()> {
    crate::scheduler::in_tx(s, |s| {
        s.put_checkpoint(id, &m.to_checkpoint(now))?;
        let all = dispositions(s, id, m)?;
        for c in s.checkpoints(id)? {
            if c.packet == META || c.state != DONE {
                continue;
            }
            let Some(run) = c.run_id else { continue };
            let Some(entry) = s.run(run)? else { continue };
            if !matches!(entry.outcome, RunOutcome::Running | RunOutcome::Interrupted) {
                continue;
            }
            let mine: Vec<&Proposal> = all.iter().filter(|p| p.packet == c.packet).collect();
            finish_keeping(s, &entry, &outcome_of(&mine, m.version))?;
        }
        Ok(())
    })
}

/// What became of one proposal of a build (spec: every proposal ends in
/// exactly one of these).
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Disposition {
    /// In the published version as proposed.
    Applied,
    /// Kept in another form (a claim lowered, joined to a memory of the
    /// same subject, a value normalised), or nothing new to record;
    /// `in_version`: something of it is in the published version.
    Adjusted { reason: String, in_version: bool },
    /// Nothing of it kept (by the packet's check, reconciliation, the
    /// final check or the store), and why.
    Refused { reason: String },
}

/// One proposal of a build: its packet, its position in that packet's
/// reply, and what became of it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Proposal {
    pub packet: u32,
    pub index: u32,
    pub disposition: Disposition,
}

/// The reason given to a proposal nothing accounted for: never expected
/// (every refusal and adjustment is recorded); a test checks it never
/// appears.
pub const UNACCOUNTED: &str = "not accounted for by the publication";

/// Every proposal of build `id`, from its packets' checkpoints and the
/// publication's record in `m`, in packet and reply order.
fn dispositions(s: &Store, id: &str, m: &Meta) -> Result<Vec<Proposal>> {
    let refused: BTreeMap<Tag, &str> = m
        .done
        .refused
        .iter()
        .map(|(t, w)| (*t, w.as_str()))
        .collect();
    let mut adjusted: BTreeMap<Tag, &str> = BTreeMap::new();
    for (t, w) in &m.done.adjusted {
        adjusted.entry(*t).or_insert(w.as_str());
    }
    let mut out = Vec::new();
    for c in s.checkpoints(id)? {
        if c.packet == META || c.state != DONE {
            continue;
        }
        let w = Work::decode(&c.payload);
        for index in 0..w.proposed() {
            let tag = (c.packet, index);
            let stage_refused = w.reasons.iter().find(|(i, _)| *i == index);
            // Every adjustment of the proposal, on one line: the packet's
            // check's, then the publication's.
            let mut eased: Vec<String> = w
                .adjusted
                .iter()
                .filter(|(i, _)| *i == index)
                .map(|(_, why)| why.clone())
                .collect();
            if let Some(why) = adjusted.get(&tag) {
                eased.push(format!("at publication: {why}"));
            }
            let in_version = m.done.in_version.contains(&tag);
            let disposition = if let Some((_, why)) = stage_refused {
                Disposition::Refused {
                    reason: why.clone(),
                }
            } else if let Some(why) = refused.get(&tag) {
                Disposition::Refused {
                    reason: format!("at publication: {why}"),
                }
            } else if !eased.is_empty() {
                Disposition::Adjusted {
                    reason: eased.join("; "),
                    in_version,
                }
            } else if in_version {
                Disposition::Applied
            } else {
                Disposition::Adjusted {
                    reason: UNACCOUNTED.to_owned(),
                    in_version: false,
                }
            };
            out.push(Proposal {
                packet: c.packet,
                index,
                disposition,
            });
        }
    }
    Ok(out)
}

/// Every proposal of the latest build and what became of it; empty until
/// it is published.
pub fn proposals(s: &Store) -> Result<Vec<Proposal>> {
    let Some(row) = s.bootstrap(None)? else {
        return Ok(Vec::new());
    };
    let Some(meta) = s
        .checkpoints(&row.id)?
        .iter()
        .find(|c| c.packet == META)
        .map(Meta::from_checkpoint)
    else {
        return Ok(Vec::new());
    };
    if meta.state != PUBLISHED {
        return Ok(Vec::new());
    }
    dispositions(s, &row.id, &meta)
}

/// [`outcome_of`] for a list of proposals (tests and reports).
#[must_use]
pub fn outcome(list: &[Proposal], version: Option<u64>) -> RunOutcome {
    let refs: Vec<&Proposal> = list.iter().collect();
    outcome_of(&refs, version)
}

/// The outcome of a set of proposals (a packet's, or the whole build's):
/// published when something of them is in the version; refused ("N
/// proposed, 0 kept" and why) when they proposed and none was kept; else
/// nothing to change. The same rule for run rows and the report.
fn outcome_of(list: &[&Proposal], version: Option<u64>) -> RunOutcome {
    let n = |x: usize| u32::try_from(x).unwrap_or(u32::MAX);
    let kept = list
        .iter()
        .filter(|p| match &p.disposition {
            Disposition::Applied => true,
            Disposition::Adjusted { in_version, .. } => *in_version,
            Disposition::Refused { .. } => false,
        })
        .count();
    let why: Vec<&str> = list
        .iter()
        .filter_map(|p| match &p.disposition {
            Disposition::Refused { reason } => Some(reason.as_str()),
            _ => None,
        })
        .collect();
    match version {
        Some(version) if kept > 0 => RunOutcome::Published {
            version,
            applied: n(kept),
            rejected: n(why.len()),
        },
        _ if !why.is_empty() => {
            let proposed = n(list.len());
            RunOutcome::Refused {
                proposed,
                reason: refusal_summary(proposed, 0, &why),
            }
        }
        _ => RunOutcome::NoChange,
    }
}

/// Most distinct reasons a summary names; past it, how many more.
const SUMMARY_REASONS: usize = 3;

/// "N proposed, K kept: reason (count); ...", the most frequent reasons
/// first. Reasons are the validator's words (no model text).
#[must_use]
pub fn refusal_summary(proposed: u32, kept: u32, reasons: &[&str]) -> String {
    let mut counts: BTreeMap<&str, usize> = BTreeMap::new();
    for r in reasons {
        *counts.entry(r).or_default() += 1;
    }
    let mut ranked: Vec<(&str, usize)> = counts.into_iter().collect();
    ranked.sort_by(|a, b| b.1.cmp(&a.1).then(a.0.cmp(b.0)));
    let mut parts: Vec<String> = ranked
        .iter()
        .take(SUMMARY_REASONS)
        .map(|(r, n)| {
            if *n > 1 {
                format!("{r} ({n})")
            } else {
                (*r).to_owned()
            }
        })
        .collect();
    if ranked.len() > SUMMARY_REASONS {
        parts.push(format!(
            "and {} other reasons",
            ranked.len() - SUMMARY_REASONS
        ));
    }
    let head = format!("{proposed} proposed, {kept} kept");
    if parts.is_empty() {
        head
    } else {
        format!("{head}: {}", parts.join("; "))
    }
}

/// Reconcile the staged operations with the memory as it is and publish
/// them as one version. What the publication did with each proposal is
/// written in the publication's own transaction (with the build's state
/// `publishing` and its version), so a crash right after the commit
/// resumes with it.
/// `Err(Aborted)` when a pause or a stop closed the gate before the commit:
/// nothing was published.
fn publish(cx: &Context<'_>, id: &str, meta: &mut Meta) -> Result<BootstrapState> {
    let now = cx.host.now().utc_ms;
    let gate = RunGate::new(Arc::clone(&cx.host), Arc::clone(&cx.stops.stop));
    let secrets = cx.host.known_secrets();
    let id_owned = id.to_owned();
    let mut m = meta.clone();
    let published = call(cx.writer, move |s| {
        let checkpoints = s.checkpoints(&id_owned)?;
        let staged: Vec<(MemoryOp, Tag)> = checkpoints
            .iter()
            .filter(|c| c.packet != META && c.state == DONE)
            .flat_map(|c| Work::decode(&c.payload).tagged(c.packet))
            .collect();
        m.base = s.read().version()?;
        m.state = PUBLISHING.to_owned();
        s.put_checkpoint(&id_owned, &m.to_checkpoint(now))?;
        let mut base = m.base;
        let project_id = s.project_id().to_owned();
        let mut record = m.clone();
        let key = id_owned.clone();
        let out = publish_tagged(
            s,
            staged,
            &mut base,
            m.cutoff,
            &secrets,
            now,
            &|| gate.closed().is_some(),
            &mut |conn, version, done| {
                record.version = Some(version);
                record.done = done.clone();
                crate::queue::put_checkpoint_in(conn, &project_id, &key, &record.to_checkpoint(now))
            },
        );
        // The record the publication committed is never overwritten without
        // it: the base moves only together with the version and outcomes.
        let (published, done) = out?;
        m.done = done;
        m.version = published.map(|(v, _, _)| v);
        if base != m.base {
            m.base = base;
            s.put_checkpoint(&id_owned, &m.to_checkpoint(now))?;
        }
        Ok((m, published))
    })?;
    let (m, result) = published;
    *meta = m;
    finish_published(cx, id, meta, result.map(|(v, _, _)| v))
}

/// What a publication did: the version, proposals with something in it,
/// and proposals turned down.
pub type PublishedBuild = Option<(u64, u32, u32)>;

/// Where a staged operation came from: (packet, index of the proposal in
/// that packet's reply).
pub type Tag = (u32, u32);

/// What a publication did with each staged proposal, by provenance. A
/// refused proposal is in `refused` only; the others may be in both
/// `in_version` and `adjusted`.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Outcomes {
    /// Proposals with something in the published version.
    pub in_version: BTreeSet<Tag>,
    /// Proposals kept in another form, with the (first) reason.
    pub adjusted: Vec<(Tag, String)>,
    /// Proposals refused in whole or in part by reconciliation, the final
    /// check or the store, with the (first) reason.
    pub refused: Vec<(Tag, String)>,
}

/// The publication step of a build, on the writer, untagged: as
/// [`publish_tagged`], each operation its own proposal, nothing written
/// with it. Returns the publication, if any, and how many proposals were
/// turned down.
pub fn publish_staged(
    s: &mut Store,
    staged: Vec<MemoryOp>,
    base: &mut u64,
    cutoff: u64,
    secrets: &[String],
    now_ms: u64,
    closed: &dyn Fn() -> bool,
) -> Result<(PublishedBuild, u32)> {
    let tagged = staged
        .into_iter()
        .enumerate()
        .map(|(i, op)| (op, (0, u32::try_from(i).unwrap_or(u32::MAX))))
        .collect();
    let (published, done) = publish_tagged(
        s,
        tagged,
        base,
        cutoff,
        secrets,
        now_ms,
        closed,
        &mut |_, _, _| Ok(()),
    )?;
    Ok((
        published,
        u32::try_from(done.refused.len()).unwrap_or(u32::MAX),
    ))
}

/// The publication step of a build, on the writer: the staged operations
/// of every packet, in order, reconciled with the memory as it is now,
/// then judged again as one list ([`operations::recheck`]); exactly what
/// that final judgement returns is published (re-judged and possibly
/// lowered, never the staged originals), as one version at `base`
/// (updated when the memory moved). When the store still refuses an
/// operation, its item goes and the whole list is judged again. `closed`
/// is asked just before the commit: `true` aborts it (`Err(Aborted)`).
///
/// Each operation's [`Tag`] follows it through reconciliation (which may
/// join, fold, adjust or refuse it), the final check and the store, so the
/// result says, per proposal, what became of it. `then` runs inside the
/// publication's transaction with the version and that result.
#[allow(clippy::too_many_arguments, clippy::too_many_lines)]
pub fn publish_tagged(
    s: &mut Store,
    staged: Vec<(MemoryOp, Tag)>,
    base: &mut u64,
    cutoff: u64,
    secrets: &[String],
    now_ms: u64,
    closed: &dyn Fn() -> bool,
    then: &mut dyn FnMut(&rusqlite::Connection, u64, &Outcomes) -> Result<()>,
) -> Result<(PublishedBuild, Outcomes)> {
    let project_id = s.project_id().to_owned();
    let (ops, tags): (Vec<MemoryOp>, Vec<Tag>) = staged.into_iter().unzip();
    let rec = reconciled(s, ops)?;
    // Why each input was refused, or adjusted (first reason kept).
    let mut failed: BTreeMap<usize, String> = BTreeMap::new();
    for (i, why) in rec.refused {
        failed.entry(i).or_insert(why);
    }
    let mut eased: BTreeMap<usize, String> = BTreeMap::new();
    for (i, why) in rec.adjusted {
        eased.entry(i).or_insert(why);
    }
    let mut items: Vec<(Staged, Vec<usize>)> = rec
        .ops
        .into_iter()
        .map(Staged::Op)
        .zip(rec.origins)
        .collect();
    // A proposal is refused when any of its staged operations was (an
    // archive refused stays refused even if the source it cited still
    // joined the memory).
    let finish = |in_version: &BTreeSet<usize>,
                  failed: &BTreeMap<usize, String>,
                  eased: &BTreeMap<usize, String>|
     -> Outcomes {
        let mut out = Outcomes::default();
        let mut refused: BTreeSet<Tag> = BTreeSet::new();
        for (i, why) in failed {
            if let Some(t) = tags.get(*i)
                && refused.insert(*t)
            {
                out.refused.push((*t, why.clone()));
            }
        }
        let mut adjusted: BTreeSet<Tag> = BTreeSet::new();
        for (i, why) in eased {
            if let Some(t) = tags.get(*i)
                && !refused.contains(t)
                && adjusted.insert(*t)
            {
                out.adjusted.push((*t, why.clone()));
            }
        }
        for i in in_version {
            if let Some(t) = tags.get(*i)
                && !refused.contains(t)
            {
                out.in_version.insert(*t);
            }
        }
        out
    };
    for _ in 0..=items.len() {
        let list: Vec<Staged> = items.iter().map(|(x, _)| x.clone()).collect();
        let judged = operations::recheck(&s.read(), &list, cutoff, secrets);
        let mut turned = failed.clone();
        let mut weaker = eased.clone();
        let mut rejected_items: BTreeSet<usize> = BTreeSet::new();
        let origins = |k: usize| items.get(k).map(|(_, o)| o.clone()).unwrap_or_default();
        for r in &judged.rejected {
            rejected_items.insert(r.index);
            for i in origins(r.index) {
                turned.entry(i).or_insert_with(|| r.reason.clone());
            }
        }
        for r in &judged.adjusted {
            for i in origins(r.index) {
                weaker.entry(i).or_insert_with(|| r.reason.clone());
            }
        }
        if judged.is_empty() {
            return Ok((None, finish(&BTreeSet::new(), &turned, &weaker)));
        }
        let in_version: BTreeSet<usize> = items
            .iter()
            .enumerate()
            .filter(|(k, _)| !rejected_items.contains(k))
            .flat_map(|(_, (_, o))| o.iter().copied())
            .collect();
        let done = finish(&in_version, &turned, &weaker);
        let publication = judged.publication(
            &project_id,
            Cycle::Bootstrap,
            Some(*base),
            cutoff,
            None,
            now_ms,
        );
        let result = s.publish_with_then(
            publication,
            &mut |step| step == PublishStep::Commit && closed(),
            &mut |conn, p| then(conn, p.version, &done),
        );
        match result {
            Ok(p) => {
                let n = |x: usize| u32::try_from(x).unwrap_or(u32::MAX);
                let counts = (p.version, n(done.in_version.len()), n(done.refused.len()));
                return Ok((Some(counts), done));
            }
            Err(StoreError::EmptyPublication) => {
                return Ok((None, finish(&BTreeSet::new(), &turned, &weaker)));
            }
            Err(StoreError::Aborted) => return Err(StoreError::Aborted),
            Err(StoreError::StaleBase { found, .. }) => *base = found,
            Err(e) => {
                let subject = match &e {
                    StoreError::StandingUnsupported(x)
                    | StoreError::NotVerifiable(x)
                    | StoreError::AddressTaken(x)
                    | StoreError::IdTaken(x) => Some(x.clone()),
                    StoreError::UnknownMemory(x) => Some(x.to_string()),
                    _ => None,
                };
                let Some(subject) = subject else {
                    return Err(e);
                };
                let before = items.len();
                let why = e.to_string();
                items.retain(|(item, origin)| match item {
                    Staged::Op(op) if reconcile::names(op, &subject) => {
                        for i in origin {
                            failed.entry(*i).or_insert_with(|| why.clone());
                        }
                        false
                    }
                    _ => true,
                });
                if items.len() == before {
                    return Err(e);
                }
            }
        }
    }
    Ok((None, finish(&BTreeSet::new(), &failed, &eased)))
}

/// The staged operations, reconciled with the memory as it is now, with
/// each output's inputs and the inputs refused.
fn reconciled(s: &Store, staged: Vec<MemoryOp>) -> Result<reconcile::Reconciled> {
    let read = s.read();
    let existing = read.memories()?;
    let mut ids = reconcile::cited(&staged);
    for m in &existing {
        ids.extend(m.sources.iter().map(|r| r.evidence));
    }
    let facts: BTreeMap<EvidenceId, Fact> = reconcile::facts(&read, &ids)?;
    Ok(reconcile::reconcile(&existing, staged, &facts))
}

// ---- the report

/// What the latest build refused, kept in another form, and its outcome,
/// in the shape of the last report the view shows: one line per refused or
/// adjusted proposal, by packet and operation, in the validator's or the
/// store's words, never the model's text. Built from [`proposals`], like
/// the build's run rows, so they agree; `version` is the version the build
/// published (it may hold only sources a refused proposal cited).
/// `None` until the build is published.
pub fn report(s: &Store) -> Result<Option<DreamReport>> {
    let Some(row) = s.bootstrap(None)? else {
        return Ok(None);
    };
    let checkpoints = s.checkpoints(&row.id)?;
    let Some(meta) = checkpoints
        .iter()
        .find(|c| c.packet == META)
        .map(Meta::from_checkpoint)
    else {
        return Ok(None);
    };
    if meta.state != PUBLISHED {
        return Ok(None);
    }
    let all = dispositions(s, &row.id, &meta)?;
    let mut r = DreamReport::default();
    for p in &all {
        let at = format!(
            "packet {}, operation {}",
            u64::from(p.packet) + 1,
            u64::from(p.index) + 1
        );
        match &p.disposition {
            Disposition::Applied => {}
            Disposition::Adjusted { reason, .. } => r.adjusted.push(format!("{at}: {reason}")),
            Disposition::Refused { reason } => r.rejected.push(format!("{at}: {reason}")),
        }
    }
    for c in checkpoints
        .iter()
        .filter(|c| c.packet != META && c.state == DONE)
    {
        r.sessions += 1;
        if let Some(run) = c.run_id {
            r.run_id = r.run_id.max(Some(run));
            if let Some(e) = s.run(run)? {
                r.chars_in = r.chars_in.saturating_add(e.chars_in);
                r.chars_out = r.chars_out.saturating_add(e.chars_out);
                r.duration_ms = r.duration_ms.saturating_add(e.duration_ms.unwrap_or(0));
            }
        }
    }
    let refs: Vec<&Proposal> = all.iter().collect();
    r.outcome = Some(outcome_of(&refs, meta.version));
    r.version = meta.version;
    Ok(Some(r))
}

// ---- the view

/// The latest build as the view shows it: state, progress, coverage, gaps.
pub fn view(s: &Store) -> Result<BootstrapView> {
    let Some(row) = s.bootstrap(None)? else {
        return Ok(BootstrapView::default());
    };
    let sources = s.bootstrap_sources(&row.id)?;
    let checkpoints = s.checkpoints(&row.id)?;
    let packets: Vec<&Checkpoint> = checkpoints
        .iter()
        .filter(|c| c.packet != META && c.state != VOID)
        .collect();
    let meta = checkpoints
        .iter()
        .find(|c| c.packet == META)
        .map(Meta::from_checkpoint)
        .unwrap_or_default();
    let mut gaps = meta.gaps;
    let coverage = inventory::coverage(sources.iter().map(|src| {
        let bucket = match src.state.as_str() {
            ANALYSED | CHANGED => Bucket::Analysed,
            w if w.starts_with("excluded:") => Bucket::Excluded,
            _ => Bucket::Remaining,
        };
        if src.state == CHANGED {
            gaps.insert(CoverageGap::ChangedSinceStart);
        }
        if src.state == OVER_LIMIT || src.state == PARTLY_READ {
            gaps.insert(CoverageGap::InputLimit);
        }
        (src.class, bucket)
    }));
    // A piece left out declares the gap from the moment it is (before the
    // sources are marked), from the one place it is recorded.
    if packets.iter().any(|c| c.state == SKIPPED) {
        gaps.insert(CoverageGap::InputLimit);
    }
    let count = |f: &dyn Fn(&&&Checkpoint) -> bool| {
        u32::try_from(packets.iter().filter(f).count()).unwrap_or(u32::MAX)
    };
    Ok(BootstrapView {
        id: Some(row.id),
        state: row.state,
        packets_done: count(&|c| c.state == DONE || c.state == SKIPPED),
        packets_total: count(&|_| true),
        coverage: if sources.is_empty() {
            Vec::new()
        } else {
            coverage
        },
        gaps: gaps.into_iter().collect(),
        started_ms: Some(row.started_ms),
        updated_ms: Some(row.updated_ms),
        plan: None,
        planning: false,
    })
}

/// The manifest of a build, by class, for tests and reports.
pub fn manifest(s: &Store, id: &str) -> Result<Vec<(String, SourceClass, String)>> {
    Ok(s.bootstrap_sources(id)?
        .into_iter()
        .map(|b| (b.path, b.class, b.state))
        .collect())
}

/// Whether the latest build should resume on its own (the app stopped while
/// it was queued or running).
pub fn resumes_at_start(s: &Store) -> Result<bool> {
    Ok(s.bootstrap(None)?
        .is_some_and(|b| matches!(b.state, BootstrapState::Queued | BootstrapState::Running)))
}
