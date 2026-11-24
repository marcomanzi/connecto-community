//! Connecto's own state for a project's operations, in its data folder
//! (`<data>/ops/<project id>/`), never in the repository (spec §4): the
//! approvals, the Intercept switches, the settings, the run log and the
//! prompt log.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};

use connecto_core::ops::{HookMode, OpStats};
use serde_json::{Value, json};

pub const STATE_FILE: &str = "state.json";
pub const RUNS_FILE: &str = "runs.jsonl";
pub const PROMPTS_FILE: &str = "prompts.jsonl";
/// Runs kept.
pub const MAX_RUNS: usize = 500;
/// Prompts kept, and how much of each.
pub const MAX_PROMPTS: usize = 1000;
pub const PROMPT_CHARS: usize = 400;

/// `state.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    /// Slug to approved hash.
    pub approved: BTreeMap<String, String>,
    pub intercept: BTreeSet<String>,
    pub hook: HookMode,
    pub weekly: bool,
    /// When this project's operations were first looked at.
    pub first_seen_ms: u64,
    pub last_discovery_ms: Option<u64>,
    /// The weekly suggestion is hidden until then.
    pub later_until_ms: Option<u64>,
    pub discovery_terminal: Option<String>,
    /// `.connecto/ops/` was added to git's exclude file once. A person who
    /// removes the line later keeps it removed.
    pub git_excluded: bool,
}

impl State {
    #[must_use]
    pub fn new(now_ms: u64) -> Self {
        Self {
            approved: BTreeMap::new(),
            intercept: BTreeSet::new(),
            hook: HookMode::Suggest,
            weekly: true,
            first_seen_ms: now_ms,
            last_discovery_ms: None,
            later_until_ms: None,
            discovery_terminal: None,
            git_excluded: false,
        }
    }

    fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "approved": self.approved,
            "intercept": self.intercept,
            "hook": self.hook.word(),
            "weekly": self.weekly,
            "first_seen_ms": self.first_seen_ms,
            "last_discovery_ms": self.last_discovery_ms,
            "later_until_ms": self.later_until_ms,
            "discovery_terminal": self.discovery_terminal,
            "git_excluded": self.git_excluded,
        })
    }

    fn from_json(value: &Value, now_ms: u64) -> Self {
        let mut state = Self::new(now_ms);
        if let Some(map) = value["approved"].as_object() {
            for (slug, hash) in map {
                if let Some(hash) = hash.as_str()
                    && crate::spec::valid_slug(slug)
                {
                    state.approved.insert(slug.clone(), hash.to_owned());
                }
            }
        }
        if let Some(list) = value["intercept"].as_array() {
            state.intercept = list
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| crate::spec::valid_slug(s))
                .map(str::to_owned)
                .collect();
        }
        if let Some(hook) = value["hook"].as_str().and_then(HookMode::from_word) {
            state.hook = hook;
        }
        if let Some(weekly) = value["weekly"].as_bool() {
            state.weekly = weekly;
        }
        if let Some(first) = value["first_seen_ms"].as_u64() {
            state.first_seen_ms = first;
        }
        state.last_discovery_ms = value["last_discovery_ms"].as_u64();
        state.later_until_ms = value["later_until_ms"].as_u64();
        state.discovery_terminal = value["discovery_terminal"].as_str().map(str::to_owned);
        state.git_excluded = value["git_excluded"].as_bool().unwrap_or(false);
        state
    }
}

/// How a run started.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RunFrom {
    Dialog,
    Agent,
    Intercept,
    /// Typed by a person in a shell terminal.
    Shell,
}

impl RunFrom {
    #[must_use]
    pub fn word(self) -> &'static str {
        match self {
            Self::Dialog => "dialog",
            Self::Agent => "agent",
            Self::Intercept => "intercept",
            Self::Shell => "shell",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [Self::Dialog, Self::Agent, Self::Intercept, Self::Shell]
            .into_iter()
            .find(|f| f.word() == word)
    }
}

/// One line of `runs.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RunRecord {
    pub run: String,
    pub slug: String,
    pub from: RunFrom,
    pub started_ms: u64,
    pub duration_ms: u64,
    /// `exit 0`, `exit 2`, `timeout`, `not finished`, and `running` until
    /// the end is recorded (a later line of the same run).
    pub outcome: String,
    /// Its limit: a `running` line older than this and a margin is shown as
    /// `not finished` (the run never reported, or Connecto restarted).
    pub timeout_s: u64,
}

/// The outcome of a run that has not ended yet.
pub const RUNNING: &str = "running";
/// How long past its timeout a run that never reported is still waited for.
const REPORT_MARGIN_MS: u64 = 2 * 60 * 1000;

impl RunRecord {
    #[must_use]
    pub fn ok(&self) -> bool {
        self.outcome == "exit 0"
    }

    fn to_json(&self) -> Value {
        json!({
            "run": self.run,
            "slug": self.slug,
            "from": self.from.word(),
            "started_ms": self.started_ms,
            "duration_ms": self.duration_ms,
            "outcome": self.outcome,
            "timeout_s": self.timeout_s,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        Some(Self {
            run: value["run"].as_str()?.to_owned(),
            slug: value["slug"].as_str()?.to_owned(),
            from: RunFrom::from_word(value["from"].as_str()?)?,
            started_ms: value["started_ms"].as_u64()?,
            duration_ms: value["duration_ms"].as_u64().unwrap_or(0),
            outcome: value["outcome"].as_str()?.to_owned(),
            timeout_s: value["timeout_s"]
                .as_u64()
                .unwrap_or(crate::spec::DEFAULT_TIMEOUT_S),
        })
    }
}

/// What the hook did with a prompt.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PromptOutcome {
    /// No operation matched.
    None,
    Suggested(String),
    Intercepted(String),
}

/// One line of `prompts.jsonl`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PromptRecord {
    pub at_ms: u64,
    pub terminal: String,
    pub text: String,
    pub outcome: PromptOutcome,
}

impl PromptRecord {
    fn to_json(&self) -> Value {
        let (outcome, slug) = match &self.outcome {
            PromptOutcome::None => ("none", None),
            PromptOutcome::Suggested(s) => ("suggested", Some(s)),
            PromptOutcome::Intercepted(s) => ("intercepted", Some(s)),
        };
        json!({
            "at_ms": self.at_ms,
            "terminal": self.terminal,
            "text": self.text,
            "outcome": outcome,
            "slug": slug,
        })
    }

    fn from_json(value: &Value) -> Option<Self> {
        let slug = value["slug"].as_str().map(str::to_owned);
        let outcome = match (value["outcome"].as_str()?, slug) {
            ("suggested", Some(s)) => PromptOutcome::Suggested(s),
            ("intercepted", Some(s)) => PromptOutcome::Intercepted(s),
            _ => PromptOutcome::None,
        };
        Some(Self {
            at_ms: value["at_ms"].as_u64()?,
            terminal: value["terminal"].as_str().unwrap_or("").to_owned(),
            text: value["text"].as_str()?.to_owned(),
            outcome,
        })
    }
}

/// Whether a prompt is the person's own: not a relayed message, notice or
/// any other text Connecto typed (they start with `[connecto`).
#[must_use]
pub fn is_persons_prompt(text: &str) -> bool {
    let text = text.trim_start();
    !text.is_empty() && !text.starts_with("[connecto")
}

/// The files of one project. Its writes are not serialized here: the app
/// holds one lock around every write to a project's files.
#[derive(Debug, Clone)]
pub struct Store {
    dir: PathBuf,
}

fn write_atomic(path: &Path, bytes: &[u8]) -> std::io::Result<()> {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    let tmp = path.with_extension(format!("{}.{n}.tmp", std::process::id()));
    fs::write(&tmp, bytes)?;
    fs::rename(&tmp, path)
}

/// The last `keep` records of a log.
fn read_lines<T>(path: &Path, keep: usize, parse: impl Fn(&Value) -> Option<T>) -> Vec<T> {
    let Ok(text) = fs::read_to_string(path) else {
        return Vec::new();
    };
    let mut all: Vec<T> = text
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .filter_map(|v| parse(&v))
        .collect();
    if all.len() > keep {
        all.drain(..all.len() - keep);
    }
    all
}

/// Append one line, keeping only the last `keep` (the file is rewritten
/// when it holds half as many again).
/// Lines are at least this long, so a smaller file is never read back to
/// count them.
const MIN_LINE: u64 = 80;

fn append_capped(path: &Path, line: &Value, keep: usize) -> std::io::Result<()> {
    let mut file = fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(path)?;
    writeln!(file, "{line}")?;
    let size = file.metadata().map(|m| m.len()).unwrap_or(0);
    drop(file);
    if size < (keep + keep / 2) as u64 * MIN_LINE {
        return Ok(());
    }
    let text = fs::read_to_string(path)?;
    let count = text.lines().count();
    if count > keep + keep / 2 {
        let kept: Vec<&str> = text.lines().skip(count - keep).collect();
        let mut out = kept.join("\n");
        out.push('\n');
        write_atomic(path, out.as_bytes())?;
    }
    Ok(())
}

impl Store {
    /// `<data root>/ops/<project id>`. The id must already be safe as a
    /// folder name (the relay's hex id).
    #[must_use]
    pub fn new(data_root: &Path, project_id: &str) -> Self {
        Self {
            dir: data_root.join("ops").join(project_id),
        }
    }

    #[must_use]
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    fn ensure(&self) -> std::io::Result<()> {
        let mut builder = fs::DirBuilder::new();
        builder.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            builder.mode(0o700);
        }
        builder.create(&self.dir)
    }

    /// The state, or a new one (saved, so `first_seen_ms` sticks).
    #[must_use]
    pub fn load(&self, now_ms: u64) -> State {
        match fs::read_to_string(self.dir.join(STATE_FILE))
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            Some(value) => State::from_json(&value, now_ms),
            None => {
                let state = State::new(now_ms);
                let _ = self.save(&state);
                state
            }
        }
    }

    /// # Errors
    ///
    /// The folder or file could not be written.
    pub fn save(&self, state: &State) -> std::io::Result<()> {
        self.ensure()?;
        let text = serde_json::to_string_pretty(&state.to_json()).unwrap_or_default();
        write_atomic(&self.dir.join(STATE_FILE), text.as_bytes())
    }

    /// # Errors
    ///
    /// The log could not be written.
    pub fn record_run(&self, record: &RunRecord) -> std::io::Result<()> {
        self.ensure()?;
        let path = self.dir.join(RUNS_FILE);
        // A run has up to two lines: compacted by run, never by line.
        append_capped(&path, &record.to_json(), MAX_RUNS * 2)?;
        let size = fs::metadata(&path).map(|m| m.len()).unwrap_or(0);
        if size < (MAX_RUNS * 2 + MAX_RUNS) as u64 * MIN_LINE {
            return Ok(());
        }
        let kept: Vec<String> = self
            .runs(0)
            .into_iter()
            .map(|r| r.to_json().to_string())
            .collect();
        let mut out = kept.join("\n");
        out.push('\n');
        write_atomic(&path, out.as_bytes())
    }

    /// One record per run, its latest line, oldest first; a run still
    /// `running` long past its timeout is `not finished`.
    #[must_use]
    pub fn runs(&self, now_ms: u64) -> Vec<RunRecord> {
        let lines = read_lines(
            &self.dir.join(RUNS_FILE),
            MAX_RUNS * 2,
            RunRecord::from_json,
        );
        let mut order: Vec<String> = Vec::new();
        let mut latest: std::collections::HashMap<String, RunRecord> =
            std::collections::HashMap::new();
        for record in lines {
            if !latest.contains_key(&record.run) {
                order.push(record.run.clone());
            }
            latest.insert(record.run.clone(), record);
        }
        let mut out: Vec<RunRecord> = order
            .into_iter()
            .filter_map(|run| latest.remove(&run))
            .map(|mut r| {
                let limit = r.started_ms + r.timeout_s * 1000 + REPORT_MARGIN_MS;
                if r.outcome == RUNNING && now_ms > limit {
                    r.outcome = "not finished".to_owned();
                }
                r
            })
            .collect();
        if out.len() > MAX_RUNS {
            out.drain(..out.len() - MAX_RUNS);
        }
        out
    }

    /// Keep a person's prompt, cut to [`PROMPT_CHARS`].
    ///
    /// # Errors
    ///
    /// The log could not be written.
    pub fn record_prompt(&self, record: &PromptRecord) -> std::io::Result<()> {
        self.ensure()?;
        let mut record = record.clone();
        if record.text.chars().count() > PROMPT_CHARS {
            record.text = record.text.chars().take(PROMPT_CHARS).collect();
        }
        append_capped(&self.dir.join(PROMPTS_FILE), &record.to_json(), MAX_PROMPTS)
    }

    #[must_use]
    pub fn prompts(&self) -> Vec<PromptRecord> {
        read_lines(
            &self.dir.join(PROMPTS_FILE),
            MAX_PROMPTS,
            PromptRecord::from_json,
        )
    }
}

/// What one operation's runs and prompts add up to.
#[must_use]
pub fn stats(slug: &str, runs: &[RunRecord], prompts: &[PromptRecord]) -> OpStats {
    let mine: Vec<&RunRecord> = runs.iter().filter(|r| r.slug == slug).collect();
    let total: u64 = mine.iter().map(|r| r.duration_ms).sum();
    let count = u32::try_from(mine.len()).unwrap_or(u32::MAX);
    OpStats {
        runs: count,
        failures: u32::try_from(
            mine.iter()
                .filter(|r| !r.ok() && r.outcome != RUNNING)
                .count(),
        )
        .unwrap_or(u32::MAX),
        average_ms: if mine.is_empty() {
            0
        } else {
            total / mine.len() as u64
        },
        last: mine.last().map(|r| (r.started_ms, r.outcome.clone())),
        by_agent: u32::try_from(mine.iter().filter(|r| r.from == RunFrom::Agent).count())
            .unwrap_or(u32::MAX),
        suggested: u32::try_from(
            prompts
                .iter()
                .filter(|p| matches!(&p.outcome, PromptOutcome::Suggested(s) if s == slug))
                .count(),
        )
        .unwrap_or(u32::MAX),
        intercepted: u32::try_from(
            prompts
                .iter()
                .filter(|p| matches!(&p.outcome, PromptOutcome::Intercepted(s) if s == slug))
                .count(),
        )
        .unwrap_or(u32::MAX),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips_and_starts_on_the_safe_side() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path(), "abc");
        let mut state = store.load(5);
        assert_eq!(state.hook, HookMode::Suggest);
        assert!(state.weekly);
        assert!(state.intercept.is_empty());
        assert_eq!(state.first_seen_ms, 5);
        state.approved.insert("tests".into(), "h".into());
        state.intercept.insert("tests".into());
        state.hook = HookMode::Off;
        state.last_discovery_ms = Some(9);
        store.save(&state).unwrap();
        assert_eq!(store.load(100), state);
    }

    #[test]
    fn bad_slugs_in_the_state_are_dropped() {
        let value = json!({"approved": {"../x": "h", "ok": "h"}, "intercept": ["../x", "ok"]});
        let state = State::from_json(&value, 1);
        assert_eq!(state.approved.keys().collect::<Vec<_>>(), ["ok"]);
        assert_eq!(state.intercept.iter().collect::<Vec<_>>(), ["ok"]);
    }

    #[test]
    fn logs_are_capped_and_stats_add_up() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path(), "abc");
        for i in 0..(MAX_RUNS + MAX_RUNS / 2 + 3) {
            store
                .record_run(&RunRecord {
                    run: format!("r{i}"),
                    slug: if i % 2 == 0 { "a" } else { "b" }.into(),
                    from: if i % 3 == 0 {
                        RunFrom::Agent
                    } else {
                        RunFrom::Dialog
                    },
                    started_ms: i as u64,
                    duration_ms: 10,
                    outcome: if i % 5 == 0 { "exit 1" } else { "exit 0" }.into(),
                    timeout_s: 600,
                })
                .unwrap();
        }
        let runs = store.runs(0);
        assert_eq!(runs.len(), MAX_RUNS, "only the last are read");
        assert_eq!(
            runs.last().unwrap().run,
            format!("r{}", MAX_RUNS + MAX_RUNS / 2 + 2)
        );
        let long = "x".repeat(PROMPT_CHARS + 50);
        store
            .record_prompt(&PromptRecord {
                at_ms: 1,
                terminal: "claude-1".into(),
                text: long,
                outcome: PromptOutcome::Suggested("a".into()),
            })
            .unwrap();
        let prompts = store.prompts();
        assert_eq!(prompts[0].text.chars().count(), PROMPT_CHARS);
        let a = stats("a", &runs, &prompts);
        assert!(a.runs > 0 && a.failures > 0 && a.by_agent > 0);
        assert_eq!(a.average_ms, 10);
        assert_eq!(a.suggested, 1);
        assert_eq!(stats("none", &runs, &prompts), OpStats::default());
    }

    #[test]
    fn a_run_is_its_latest_line_and_a_lost_one_is_not_finished() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path(), "abc");
        let record = |run: &str, outcome: &str| RunRecord {
            run: run.into(),
            slug: "a".into(),
            from: RunFrom::Agent,
            started_ms: 1000,
            duration_ms: 5,
            outcome: outcome.into(),
            timeout_s: 10,
        };
        store.record_run(&record("r1", RUNNING)).unwrap();
        store.record_run(&record("r2", RUNNING)).unwrap();
        store.record_run(&record("r1", "exit 0")).unwrap();
        let runs = store.runs(2000);
        assert_eq!(runs.len(), 2);
        assert_eq!(
            (runs[0].run.as_str(), runs[0].outcome.as_str()),
            ("r1", "exit 0")
        );
        assert_eq!(runs[1].outcome, RUNNING, "still within its time");
        let later = store.runs(1000 + 10_000 + REPORT_MARGIN_MS + 1);
        assert_eq!(later[1].outcome, "not finished");
        assert_eq!(stats("a", &runs, &[]).failures, 0, "running is no failure");
    }

    #[test]
    fn compaction_keeps_the_last_runs_whole() {
        let dir = tempfile::tempdir().unwrap();
        let store = Store::new(dir.path(), "abc");
        let total = MAX_RUNS * 3;
        for i in 0..total {
            for outcome in [RUNNING, "exit 0"] {
                store
                    .record_run(&RunRecord {
                        run: format!("r{i}"),
                        slug: "a".into(),
                        from: RunFrom::Agent,
                        started_ms: i as u64,
                        duration_ms: 1,
                        outcome: outcome.into(),
                        timeout_s: 600,
                    })
                    .unwrap();
            }
        }
        let runs = store.runs(0);
        assert_eq!(runs.len(), MAX_RUNS);
        assert!(runs.iter().all(|r| r.outcome == "exit 0"));
        assert_eq!(runs.last().unwrap().run, format!("r{}", total - 1));
    }

    #[test]
    fn connecto_lines_are_not_the_persons() {
        assert!(is_persons_prompt("deploy please"));
        assert!(!is_persons_prompt(
            "[connecto from claude-2, request r1.] hi"
        ));
        assert!(!is_persons_prompt("  [connecto notice] x"));
        assert!(!is_persons_prompt("   "));
    }
}
