//! Which agent runs a job (ADR 0041). A job that names no agent, or names
//! Antigravity (the queue's default, `agy`), runs on Antigravity when the
//! project can use it, and otherwise on the first of [`STAND_INS`] it can.
//! A job that names another agent runs on that agent, as it always did.
//! A project may instead choose one CLI and model for the jobs that name
//! none ("Agent for jobs", [`JobAgent`]): they run there or fail, never on a
//! stand-in.
//!
//! The candidates are looked at one at a time, in order, and only until one
//! is usable: an agent after it is never asked about. When the choice
//! depends on something not known yet (an agent still being checked, Pi's
//! providers still being read), the job waits for it rather than guess.

use std::collections::HashMap;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, PoisonError};
use std::time::{Duration, Instant};

use connecto_core::agentic_works::JobAgent;
use connecto_core::{AgentBusy, AgentRow, ToolKind};

/// The model Codex is given when it stands in for Antigravity: GPT Terra,
/// as Codex lists it (`~/.codex/models_cache.json`, Codex 0.167).
pub const CODEX_TERRA: &str = connecto_core::terminals::CODEX_TERRA;
/// The model Claude Code is given when it stands in for Antigravity.
pub const CLAUDE_HAIKU: &str = "haiku";

/// Who stands in for Antigravity, in this order, each with the model it is
/// given (`None`: the model it is configured with).
pub const STAND_INS: [(ToolKind, Option<&str>); 4] = [
    (ToolKind::Codex, Some(CODEX_TERRA)),
    (ToolKind::Claude, Some(CLAUDE_HAIKU)),
    (ToolKind::Pi, None),
    (ToolKind::OpenCode, None),
];

/// Whether an agent can run a project's job now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Availability {
    Usable,
    /// It cannot: why.
    Unusable(String),
    /// Not known yet: what is still being found out.
    Pending(String),
}

/// Where a job's agents are looked at.
pub trait AgentCheck: Send + Sync {
    /// What the settings say of `tool` for `project`, read fresh at every
    /// claim (never kept): the project's Tools, Settings > Agents, the
    /// agent's row. `Usable` when they let it through to [`Self::look`].
    /// Cheap: small files and memory.
    fn settings(&self, project: &str, tool: ToolKind) -> Availability;
    /// What decides [`Self::look`]'s answer besides the project and agent:
    /// the profile the agent is run in and whose model providers it gets,
    /// resolved once from the settings at every claim. A look is kept under
    /// it and is given it: the look never reads them again.
    fn profile(&self, project: &str, tool: ToolKind) -> Profile {
        let _ = (project, tool);
        Profile::default()
    }
    /// What only the agent can say for the project (its folder `root`), in
    /// `profile`: Pi's providers, its sign-in. Slow: called off the jobs'
    /// runtime, shared by the jobs that ask and kept a while.
    fn look(&self, project: &str, root: &Path, tool: ToolKind, profile: &Profile) -> Availability;
    /// Whether the project's Tools, as saved, allow `tool`: the CLIs
    /// "Agent for jobs" offers. Cheap.
    fn allows(&self, project: &str, tool: ToolKind) -> bool {
        let _ = (project, tool);
        true
    }
    /// The models `tool` offers for the project when only the agents know
    /// them (Pi's providers); `None` leaves it to the terminals' own list.
    /// Cheap: never starts anything.
    fn models(&self, project: &str, tool: ToolKind) -> Option<Vec<String>> {
        let _ = (project, tool);
        None
    }
}

/// The profile an agent is run in for a project, and whose model providers
/// it gets, as a launch would resolve them.
#[derive(Debug, Clone, Default, PartialEq, Eq, Hash)]
pub struct Profile {
    /// The system's profile (`true`) or the project's own; `None` for an
    /// agent with no choice.
    pub system_config: Option<bool>,
    /// Whose model providers; `None` for an agent without them.
    pub scope: Option<connecto_core::ProviderScope>,
}

/// The agent a job runs on.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    pub tool: ToolKind,
    /// The model it is given; `None`: its own default.
    pub model: Option<String>,
    pub effort: Option<String>,
    /// It stands in for Antigravity.
    pub stand_in: bool,
    /// The project's "Agent for jobs" chose it, for a job that named none.
    pub for_project: bool,
}

impl Agent {
    /// "Codex, model gpt-6.1-terra", as the view and the job's outcome say it.
    #[must_use]
    pub fn describe(&self) -> String {
        match &self.model {
            Some(model) => format!("{}, model {model}", self.tool.label()),
            None => format!("{}, its default model", self.tool.label()),
        }
    }

    /// What the view and the job's status say: [`Self::describe`], and that
    /// it stands in for Antigravity when it does.
    #[must_use]
    pub fn shown(&self) -> String {
        if self.stand_in {
            format!("{}, standing in for Antigravity", self.describe())
        } else if self.for_project {
            format!("{}, the project's agent for jobs", self.describe())
        } else {
            self.describe()
        }
    }
}

/// What to do with a job now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Decision {
    Run(Agent),
    /// The choice depends on something not known yet: ask again shortly.
    Wait(String),
}

/// Whether a job's session CLI asks for the default agent, Antigravity:
/// blank, or `agy` (the queue column's default, so it cannot be told from a
/// job that named none) or `antigravity`.
#[must_use]
pub fn asks_default(cli: &str) -> bool {
    matches!(cli.trim(), "" | "agy" | "antigravity")
}

/// The agent a job with this session CLI, model and effort runs on, in a
/// project whose "Agent for jobs" is `project`. For a job that asks for the
/// default agent: with a CLI chosen for the project, that CLI with the
/// project's model, asked about once (unusable: the job fails, naming why;
/// no stand-in); with Automatic, each candidate in order until one is usable
/// or pending. An `Err` from `ask` (the job cancelled, its time up) ends the
/// choice with that error. A job that names an agent keeps it.
///
/// # Errors
/// An unknown CLI, an error of `ask`, the project's agent unusable, or no
/// agent that can run a default job, with each agent tried and why it could
/// not.
pub async fn choose<F, Fut>(
    cli: &str,
    model: Option<String>,
    effort: Option<String>,
    project: &JobAgent,
    mut ask: F,
) -> Result<Decision, String>
where
    F: FnMut(ToolKind) -> Fut,
    Fut: Future<Output = Result<Availability, String>>,
{
    let tool = super::provider(cli)?;
    let asked = Agent {
        tool,
        model,
        effort,
        stand_in: false,
        for_project: false,
    };
    if !asks_default(cli) {
        return Ok(Decision::Run(asked));
    }
    if let JobAgent::Cli { tool, model } = project {
        let agent = Agent {
            tool: *tool,
            // The project's choice wins, its default model included: the
            // job's model was meant for Antigravity, and so was its effort,
            // kept only when the project chose Antigravity.
            model: model.clone(),
            effort: asked.effort.filter(|_| *tool == ToolKind::Antigravity),
            stand_in: false,
            for_project: true,
        };
        return match ask(agent.tool).await? {
            Availability::Usable => Ok(Decision::Run(agent)),
            Availability::Pending(what) => {
                Ok(Decision::Wait(format!("{}: {what}", agent.tool.label())))
            }
            Availability::Unusable(why) => Err(format!(
                "This project runs its jobs on {} (Agent for jobs, in Terminal > Agentic Works…), which cannot run this job: {why}. Fix that, or choose another agent or Automatic (`connecto works agent`).",
                agent.describe()
            )),
        };
    }
    let candidates = std::iter::once(asked).chain(STAND_INS.into_iter().map(|(tool, model)| {
        Agent {
            tool,
            model: model.map(str::to_owned),
            // The job's model and effort were meant for Antigravity.
            effort: None,
            stand_in: true,
            for_project: false,
        }
    }));
    let mut tried = Vec::new();
    for agent in candidates {
        let name = if agent.stand_in {
            agent.describe()
        } else {
            agent.tool.label().to_owned()
        };
        match ask(agent.tool).await? {
            Availability::Usable => return Ok(Decision::Run(agent)),
            // An agent before it in the order may yet turn out usable.
            Availability::Pending(what) => return Ok(Decision::Wait(format!("{name}: {what}"))),
            Availability::Unusable(why) => tried.push(format!("{name} ({why})")),
        }
    }
    Err(format!(
        "No agent can run this job. Tried, in order: {}. Install or activate one of them in Settings > Agents, or allow it in the project's settings (Tools).",
        tried.join("; ")
    ))
}

/// What the settings say of one agent for a project, as a terminal launch
/// there decides: allowed in the project's settings (Tools, as saved), not
/// switched off in Settings > Agents, its background check over, installed
/// and offered (a supported version). `Usable` lets it through to the look.
pub fn from_settings(row: Option<&AgentRow>, allowed: bool, off: &[ToolKind]) -> Availability {
    let Some(row) = row else {
        return Availability::Unusable("not supported on this computer".into());
    };
    if !allowed {
        return Availability::Unusable("turned off in the project's settings".into());
    }
    if off.contains(&row.tool) {
        return Availability::Unusable("deactivated in Settings > Agents".into());
    }
    if row.busy == Some(AgentBusy::Checking) {
        return Availability::Pending("Connecto is still checking it".into());
    }
    if !row.installed {
        return Availability::Unusable("not installed".into());
    }
    if !row.active {
        return Availability::Unusable(
            row.error
                .clone()
                .unwrap_or_else(|| "not offered in Settings > Agents".into()),
        );
    }
    Availability::Usable
}

/// The looks at agents under way and their recent results, shared by every
/// job of every project (third review): one look per project and agent at a
/// time, whatever number of jobs ask; a result kept a while; and no more
/// than a few looks running at once. A look counts as running until its work
/// on the blocking thread has ended, not until the jobs waiting for it stop
/// waiting (cancelled, or out of time).
pub struct Probes {
    check: Arc<dyn AgentCheck>,
    /// The most looks running at once.
    limit: usize,
    /// How long a result that is not pending is kept, in milliseconds.
    keep_ms: AtomicU64,
    inner: Mutex<ProbesInner>,
}

/// A look's project, agent and [`Profile`].
type LookKey = (String, ToolKind, Profile);

#[derive(Default)]
struct ProbesInner {
    looks: HashMap<LookKey, Look>,
    running: usize,
}

enum Look {
    Running(tokio::sync::watch::Receiver<Option<Availability>>),
    Done(Availability, Instant),
}

/// Ends a look's run when its work ends, even by a panic: its place is let
/// go and its result kept (a pending one is not).
struct LookRun {
    probes: Arc<Probes>,
    key: LookKey,
    result: Option<Availability>,
}

impl Drop for LookRun {
    fn drop(&mut self) {
        let mut inner = self.probes.lock();
        inner.running -= 1;
        match self.result.take() {
            Some(a) if !matches!(a, Availability::Pending(_)) => {
                inner
                    .looks
                    .insert(self.key.clone(), Look::Done(a, Instant::now()));
            }
            _ => {
                inner.looks.remove(&self.key);
            }
        }
    }
}

/// How long a look's result is kept by default.
pub const KEEP_LOOKS: Duration = Duration::from_secs(30);
/// The most looks at agents running at once.
pub const MAX_LOOKS: usize = 8;

impl Probes {
    #[must_use]
    pub fn new(check: Arc<dyn AgentCheck>, limit: usize, keep: Duration) -> Arc<Self> {
        Arc::new(Self {
            check,
            limit: limit.max(1),
            keep_ms: AtomicU64::new(u64::try_from(keep.as_millis()).unwrap_or(u64::MAX)),
            inner: Mutex::default(),
        })
    }

    /// Keep results this long from now on (tests that change what agents
    /// are usable keep none).
    #[cfg(test)]
    pub fn keep_for(&self, keep: Duration) {
        self.keep_ms.store(
            u64::try_from(keep.as_millis()).unwrap_or(u64::MAX),
            Ordering::SeqCst,
        );
    }

    /// Where the agents are looked at.
    #[must_use]
    pub fn check(&self) -> &dyn AgentCheck {
        self.check.as_ref()
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, ProbesInner> {
        self.inner.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Whether `tool` can run a job of `project` (its folder `root`): a
    /// result kept from a recent look, the look under way, or a new one when
    /// there is room. While waiting, `stop` is asked every 200 ms; what it
    /// gives ends the wait with that error (the look goes on, for the next
    /// job that asks).
    ///
    /// # Errors
    /// What `stop` gave.
    pub async fn ask(
        self: &Arc<Self>,
        project: &str,
        root: &Path,
        tool: ToolKind,
        stop: &(dyn Fn() -> Option<String> + Sync),
    ) -> Result<Availability, String> {
        // Read fresh: a job after a change of Tools or Settings > Agents
        // never uses a look kept from before it.
        let settings = self.check.settings(project, tool);
        if settings != Availability::Usable {
            return Ok(settings);
        }
        // Resolved once: the look is kept under this profile and run in it.
        let profile = self.check.profile(project, tool);
        let key = (project.to_owned(), tool, profile.clone());
        loop {
            let found = {
                let mut inner = self.lock();
                let keep = Duration::from_millis(self.keep_ms.load(Ordering::SeqCst));
                match inner.looks.get(&key) {
                    Some(Look::Done(a, at)) if at.elapsed() < keep => return Ok(a.clone()),
                    Some(Look::Running(rx)) => Some(rx.clone()),
                    _ if inner.running >= self.limit => None,
                    _ => {
                        let (tx, rx) = tokio::sync::watch::channel(None);
                        inner.looks.insert(key.clone(), Look::Running(rx.clone()));
                        inner.running += 1;
                        let mut run = LookRun {
                            probes: Arc::clone(self),
                            key: key.clone(),
                            result: None,
                        };
                        let (id, root): (String, PathBuf) = (project.to_owned(), root.to_owned());
                        let profile = profile.clone();
                        tokio::task::spawn_blocking(move || {
                            let a = run.probes.check.look(&id, &root, tool, &profile);
                            run.result = Some(a.clone());
                            drop(run);
                            let _ = tx.send(Some(a));
                        });
                        Some(rx)
                    }
                }
            };
            let Some(mut rx) = found else {
                // No room: wait for a look to end.
                if let Some(why) = stop() {
                    return Err(why);
                }
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            };
            loop {
                if let Some(a) = rx.borrow_and_update().clone() {
                    return Ok(a);
                }
                tokio::select! {
                    changed = rx.changed() => {
                        // Its work ended with no result (a panic): the job
                        // fails saying so; nothing is tried again here.
                        if changed.is_err() {
                            return Err(format!(
                                "Could not tell whether {} can run the job: its check failed.",
                                tool.label()
                            ));
                        }
                    }
                    () = tokio::time::sleep(Duration::from_millis(200)) => {
                        if let Some(why) = stop() {
                            return Err(why);
                        }
                    }
                }
            }
        }
    }

    /// How many looks are running now.
    #[must_use]
    #[cfg(test)]
    pub fn running(&self) -> usize {
        self.lock().running
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use super::*;
    use Availability::{Pending, Unusable, Usable};
    use ToolKind::{Antigravity, Claude, Codex, OpenCode, Pi};

    /// Each agent's availability, and the agents asked about, in order.
    struct Asked {
        set: Vec<(ToolKind, Availability)>,
        asked: std::sync::Mutex<Vec<ToolKind>>,
    }

    impl Asked {
        fn usable(tools: &[ToolKind]) -> Self {
            Self {
                set: [Antigravity, Codex, Claude, Pi, OpenCode]
                    .into_iter()
                    .map(|t| {
                        (
                            t,
                            if tools.contains(&t) {
                                Usable
                            } else {
                                Unusable("not installed".into())
                            },
                        )
                    })
                    .collect(),
                asked: std::sync::Mutex::default(),
            }
        }

        fn with(mut self, tool: ToolKind, a: Availability) -> Self {
            self.set.iter_mut().find(|(t, _)| *t == tool).unwrap().1 = a;
            self
        }

        fn pick(&self, cli: &str) -> Result<Decision, String> {
            self.pick_in(cli, &JobAgent::Automatic)
        }

        /// As [`Self::pick`], in a project whose agent for jobs is `project`.
        fn pick_in(&self, cli: &str, project: &JobAgent) -> Result<Decision, String> {
            let ask = |tool: ToolKind| {
                self.asked.lock().unwrap().push(tool);
                let a = self.set.iter().find(|(t, _)| *t == tool).unwrap().1.clone();
                std::future::ready(Ok(a))
            };
            let decision = choose(
                cli,
                Some("gemini-pro".into()),
                Some("high".into()),
                project,
                ask,
            );
            tokio::runtime::Builder::new_current_thread()
                .build()
                .unwrap()
                .block_on(decision)
        }

        fn asked(&self) -> Vec<ToolKind> {
            self.asked.lock().unwrap().clone()
        }
    }

    fn run(cli: &str, usable: &[ToolKind]) -> Agent {
        match Asked::usable(usable).pick(cli).unwrap() {
            Decision::Run(agent) => agent,
            Decision::Wait(why) => panic!("waits: {why}"),
        }
    }

    #[test]
    fn with_antigravity_a_default_job_runs_there_as_before() {
        for cli in ["", " ", "agy", "antigravity"] {
            let agent = run(cli, &[Antigravity, Codex, Claude, Pi, OpenCode]);
            assert_eq!(
                agent,
                Agent {
                    tool: Antigravity,
                    model: Some("gemini-pro".into()),
                    effort: Some("high".into()),
                    stand_in: false,
                    for_project: false,
                }
            );
        }
    }

    /// The regression: without Antigravity, a default job went to
    /// Antigravity anyway (and failed); it now runs on Codex with Terra.
    #[test]
    fn without_antigravity_a_default_job_runs_on_codex_terra() {
        let agent = run("agy", &[Codex, Claude, Pi, OpenCode]);
        assert_eq!(
            agent,
            Agent {
                tool: Codex,
                model: Some(CODEX_TERRA.into()),
                effort: None,
                stand_in: true,
                for_project: false,
            }
        );
        assert_eq!(agent.describe(), "Codex, model gpt-6.1-terra");
    }

    #[test]
    fn each_stand_in_is_taken_in_order() {
        let steps: [(&[ToolKind], ToolKind, Option<&str>); 4] = [
            (&[Codex, Claude, Pi, OpenCode], Codex, Some(CODEX_TERRA)),
            (&[Claude, Pi, OpenCode], Claude, Some(CLAUDE_HAIKU)),
            (&[Pi, OpenCode], Pi, None),
            (&[OpenCode], OpenCode, None),
        ];
        for (usable, tool, model) in steps {
            let agent = run("", usable);
            assert_eq!(
                (
                    agent.tool,
                    agent.model.as_deref(),
                    agent.effort,
                    agent.stand_in
                ),
                (tool, model, None, true),
                "{usable:?}"
            );
        }
        assert_eq!(run("", &[OpenCode, Pi]).tool, Pi);
        assert_eq!(run("", &[OpenCode, Claude]).tool, Claude);
    }

    #[test]
    fn with_none_the_job_fails_naming_the_order_tried() {
        let err = Asked::usable(&[]).pick("").unwrap_err();
        assert_eq!(
            err,
            "No agent can run this job. Tried, in order: Antigravity (not installed); Codex, model gpt-6.1-terra (not installed); Claude, model haiku (not installed); Pi, its default model (not installed); OpenCode, its default model (not installed). Install or activate one of them in Settings > Agents, or allow it in the project's settings (Tools).",
        );
    }

    #[test]
    fn an_agent_named_explicitly_never_falls_back_nor_waits() {
        for (cli, tool) in [
            ("claude", Claude),
            ("codex", Codex),
            ("pi", Pi),
            ("opencode", OpenCode),
        ] {
            let none = Asked::usable(&[]);
            let decision = none.pick(cli).unwrap();
            assert_eq!(
                decision,
                Decision::Run(Agent {
                    tool,
                    model: Some("gemini-pro".into()),
                    effort: Some("high".into()),
                    stand_in: false,
                    for_project: false,
                })
            );
            assert!(none.asked().is_empty(), "{cli}: no agent is asked about");
        }
        assert!(Asked::usable(&[Codex]).pick("shell").is_err());
    }

    fn claude_sonnet() -> JobAgent {
        JobAgent::Cli {
            tool: Claude,
            model: Some("sonnet".into()),
        }
    }

    /// The regression (Agent for jobs): a project that chose Claude with
    /// Sonnet runs its unnamed jobs there, with Antigravity usable, and the
    /// job's own model and effort (meant for Antigravity) dropped. Only
    /// Claude is asked about.
    #[test]
    fn a_projects_chosen_agent_runs_its_unnamed_jobs() {
        for cli in ["", "agy", "antigravity"] {
            let all = Asked::usable(&[Antigravity, Codex, Claude, Pi, OpenCode]);
            let decision = all.pick_in(cli, &claude_sonnet()).unwrap();
            let agent = Agent {
                tool: Claude,
                model: Some("sonnet".into()),
                effort: None,
                stand_in: false,
                for_project: true,
            };
            assert_eq!(decision, Decision::Run(agent.clone()), "{cli:?}");
            assert_eq!(all.asked(), [Claude]);
            assert_eq!(
                agent.shown(),
                "Claude, model sonnet, the project's agent for jobs"
            );
        }
        // The CLI's default model: no model passed.
        let default = JobAgent::Cli {
            tool: Pi,
            model: None,
        };
        let decision = Asked::usable(&[Pi]).pick_in("", &default).unwrap();
        assert!(matches!(decision, Decision::Run(a) if a.tool == Pi && a.model.is_none()));
        // Antigravity chosen: the job's effort was meant for it and is kept.
        let agy = JobAgent::Cli {
            tool: Antigravity,
            model: Some("gemini-4-argon-high".into()),
        };
        let decision = Asked::usable(&[Antigravity]).pick_in("agy", &agy).unwrap();
        assert!(matches!(decision, Decision::Run(a)
            if a.model.as_deref() == Some("gemini-4-argon-high") && a.effort.as_deref() == Some("high")));
    }

    /// A job that names its agent keeps it, and its own model, whatever the
    /// project chose.
    #[test]
    fn a_named_job_ignores_the_projects_agent() {
        let none = Asked::usable(&[]);
        let decision = none.pick_in("codex", &claude_sonnet()).unwrap();
        assert!(matches!(decision, Decision::Run(a)
            if a.tool == Codex && a.model.as_deref() == Some("gemini-pro") && !a.for_project));
        assert!(none.asked().is_empty());
    }

    /// The chosen agent unusable: the job fails naming it and why, with no
    /// stand-in, though Antigravity and Codex are usable. Pending: it waits.
    #[test]
    fn an_unusable_chosen_agent_fails_the_job_without_a_stand_in() {
        let snap = Asked::usable(&[Antigravity, Codex]);
        let err = snap.pick_in("", &claude_sonnet()).unwrap_err();
        assert_eq!(
            err,
            "This project runs its jobs on Claude, model sonnet (Agent for jobs, in Terminal > Agentic Works…), which cannot run this job: not installed. Fix that, or choose another agent or Automatic (`connecto works agent`)."
        );
        assert_eq!(snap.asked(), [Claude]);
        let snap =
            Asked::usable(&[Codex]).with(Claude, Pending("Connecto is still checking it".into()));
        assert_eq!(
            snap.pick_in("", &claude_sonnet()).unwrap(),
            Decision::Wait("Claude: Connecto is still checking it".into())
        );
    }

    /// An agent still being checked is not guessed at. Antigravity pending:
    /// wait, even with Codex usable. Antigravity out and Codex pending: wait,
    /// even with Claude usable.
    #[test]
    fn a_choice_that_depends_on_a_pending_check_waits() {
        let pending = || Pending("Connecto is still checking it".into());
        let snap = Asked::usable(&[Codex, Claude]).with(Antigravity, pending());
        assert_eq!(
            snap.pick("").unwrap(),
            Decision::Wait("Antigravity: Connecto is still checking it".into())
        );
        let snap = Asked::usable(&[Claude]).with(Codex, pending());
        assert!(matches!(snap.pick("").unwrap(), Decision::Wait(w) if w.starts_with("Codex")));
    }

    /// Review finding (second review): candidates are asked about lazily, in
    /// order. With Codex usable, Pi (whose providers may be slow to read) and
    /// the agents after it are never asked about, pending or not.
    #[test]
    fn agents_after_the_chosen_one_are_never_asked_about() {
        let snap = Asked::usable(&[Codex]).with(Pi, Pending("reading its providers".into()));
        assert!(matches!(snap.pick("").unwrap(), Decision::Run(a) if a.tool == Codex));
        assert_eq!(snap.asked(), [Antigravity, Codex]);
        let snap = Asked::usable(&[Antigravity, Codex]);
        snap.pick("agy").unwrap();
        assert_eq!(snap.asked(), [Antigravity]);
    }

    /// An `Err` of the question (the job cancelled while it was asked) ends
    /// the choice with it.
    #[test]
    fn a_question_that_fails_ends_the_choice() {
        let decision = choose("", None, None, &JobAgent::Automatic, |_| {
            std::future::ready(Err("Work cancelled before dispatch.".to_owned()))
        });
        let got = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap()
            .block_on(decision);
        assert_eq!(got, Err("Work cancelled before dispatch.".into()));
    }

    fn row(tool: ToolKind) -> AgentRow {
        AgentRow {
            tool,
            name: tool.label().into(),
            source: String::new(),
            installed: true,
            version: None,
            signed_in: None,
            active: true,
            busy: None,
            error: None,
            signin_requested: false,
            node_prompt: None,
        }
    }

    #[test]
    fn the_settings_are_allowed_off_checked_installed_and_offered() {
        assert_eq!(from_settings(Some(&row(Codex)), true, &[]), Usable);
        assert_eq!(
            from_settings(None, true, &[]),
            Unusable("not supported on this computer".into())
        );
        // The project's saved Tools come first: definite even mid-check.
        let checking = AgentRow {
            busy: Some(AgentBusy::Checking),
            ..row(Codex)
        };
        assert_eq!(
            from_settings(Some(&checking), false, &[]),
            Unusable("turned off in the project's settings".into())
        );
        assert_eq!(
            from_settings(Some(&row(Codex)), true, &[Codex]),
            Unusable("deactivated in Settings > Agents".into())
        );
        assert!(matches!(
            from_settings(Some(&checking), true, &[]),
            Pending(_)
        ));
        let missing = AgentRow {
            installed: false,
            ..row(Codex)
        };
        assert_eq!(
            from_settings(Some(&missing), true, &[]),
            Unusable("not installed".into())
        );
        let unsupported = AgentRow {
            active: false,
            error: Some("OpenCode 2 is not supported".into()),
            ..row(OpenCode)
        };
        assert_eq!(
            from_settings(Some(&unsupported), true, &[]),
            Unusable("OpenCode 2 is not supported".into())
        );
        // The row's sign-in is the system profile's: not looked at; the
        // project's own look decides.
        let system_out = AgentRow {
            signed_in: Some(false),
            ..row(Codex)
        };
        assert_eq!(from_settings(Some(&system_out), true, &[]), Usable);
    }

    /// A look held open until released, counting the looks of each agent.
    #[derive(Default)]
    struct HeldLooks {
        held: Mutex<bool>,
        looks: Mutex<Vec<(String, ToolKind)>>,
        /// The agents the project's settings turn off.
        off: Mutex<Vec<ToolKind>>,
        /// Its looks panic.
        panics: bool,
    }
    impl AgentCheck for HeldLooks {
        fn settings(&self, _: &str, tool: ToolKind) -> Availability {
            if self.off.lock().unwrap().contains(&tool) {
                Unusable("turned off in the project's settings".into())
            } else {
                Usable
            }
        }
        fn look(&self, project: &str, _: &Path, tool: ToolKind, _: &Profile) -> Availability {
            self.looks.lock().unwrap().push((project.to_owned(), tool));
            assert!(!self.panics, "a look that fails");
            while *self.held.lock().unwrap() {
                std::thread::sleep(Duration::from_millis(10));
            }
            Usable
        }
    }

    /// Third review, finding 1: 32 jobs claimed at once ask about the same
    /// agent: one look runs. Jobs that stop waiting (cancelled, out of time)
    /// leave it running, and the jobs after them join it instead of starting
    /// another. Looks of other projects wait for room while the limit is
    /// reached, counting a look until its work has ended. Its result is then
    /// kept a while.
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn many_claims_share_one_look_within_a_bound() {
        let check = Arc::new(HeldLooks::default());
        *check.held.lock().unwrap() = true;
        // A failed assertion lets the held looks go, so the runtime can end.
        struct Release(Arc<HeldLooks>);
        impl Drop for Release {
            fn drop(&mut self) {
                *self.0.held.lock().unwrap_or_else(PoisonError::into_inner) = false;
            }
        }
        let _release = Release(Arc::clone(&check));
        let probes = Probes::new(check.clone(), 2, Duration::from_secs(30));
        let root = Path::new("/");
        let never = || None;
        let mut jobs = Vec::new();
        for _ in 0..32 {
            let probes = Arc::clone(&probes);
            jobs.push(tokio::spawn(async move {
                probes.ask("p", Path::new("/"), Codex, &|| None).await
            }));
        }
        tokio::time::sleep(Duration::from_millis(300)).await;
        assert_eq!(check.looks.lock().unwrap().len(), 1);
        // Jobs that give up: the look runs on, and is still counted.
        let cancelled = || Some("Work cancelled before dispatch.".to_owned());
        for _ in 0..8 {
            assert_eq!(
                probes.ask("p", root, Codex, &cancelled).await,
                Err("Work cancelled before dispatch.".into())
            );
        }
        assert_eq!(check.looks.lock().unwrap().len(), 1);
        assert_eq!(probes.running(), 1);
        // Other projects: one more fits the limit of 2; the rest wait.
        for project in ["q1", "q2", "q3"] {
            let probes = Arc::clone(&probes);
            tokio::spawn(async move { probes.ask(project, Path::new("/"), Codex, &|| None).await });
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
        assert_eq!(check.looks.lock().unwrap().len(), 2);
        assert_eq!(probes.running(), 2);
        *check.held.lock().unwrap() = false;
        for job in jobs {
            assert_eq!(job.await.unwrap(), Ok(Usable));
        }
        let deadline = Instant::now() + Duration::from_secs(10);
        while probes.running() > 0 || check.looks.lock().unwrap().len() < 4 {
            assert!(Instant::now() < deadline, "the waiting looks never ran");
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        // Kept: asked again, no new look.
        assert_eq!(probes.ask("p", root, Codex, &never).await, Ok(Usable));
        assert_eq!(check.looks.lock().unwrap().len(), 4);
        // Kept no more: a new look.
        probes.keep_for(Duration::ZERO);
        assert_eq!(probes.ask("p", root, Codex, &never).await, Ok(Usable));
        assert_eq!(check.looks.lock().unwrap().len(), 5);
    }

    /// Fourth review, finding 1: with results kept as in production, a job
    /// claimed after the project's Tools turned Codex off is not given the
    /// look kept from the job before; the look itself is still kept.
    #[tokio::test]
    async fn the_settings_are_read_fresh_though_the_look_is_kept() {
        let check = Arc::new(HeldLooks::default());
        let probes = Probes::new(check.clone(), MAX_LOOKS, KEEP_LOOKS);
        let (root, never) = (Path::new("/"), || None);
        assert_eq!(probes.ask("p", root, Codex, &never).await, Ok(Usable));
        check.off.lock().unwrap().push(Codex);
        assert_eq!(
            probes.ask("p", root, Codex, &never).await,
            Ok(Unusable("turned off in the project's settings".into()))
        );
        check.off.lock().unwrap().clear();
        assert_eq!(probes.ask("p", root, Codex, &never).await, Ok(Usable));
        assert_eq!(check.looks.lock().unwrap().len(), 1, "the look was kept");
    }

    /// Fourth review, finding 2: a look that panics ends the ask with an
    /// error at once, not a new look straight away, and frees its place.
    #[tokio::test]
    async fn a_look_that_panics_is_an_error_not_a_retry() {
        let check = Arc::new(HeldLooks {
            panics: true,
            ..HeldLooks::default()
        });
        let probes = Probes::new(check.clone(), MAX_LOOKS, KEEP_LOOKS);
        let got = tokio::time::timeout(
            Duration::from_secs(5),
            probes.ask("p", Path::new("/"), Codex, &|| None),
        )
        .await
        .expect("the ask ended");
        assert_eq!(
            got,
            Err("Could not tell whether Codex can run the job: its check failed.".into())
        );
        assert_eq!(check.looks.lock().unwrap().len(), 1);
        assert_eq!(probes.running(), 0);
    }

    /// Usable in the system's profile only; the settings say System until
    /// `isolated` is set; the look of project "other" is held while `held`.
    #[derive(Default)]
    struct Profiled {
        isolated: Mutex<bool>,
        held: Mutex<bool>,
    }
    impl AgentCheck for Profiled {
        fn settings(&self, _: &str, _: ToolKind) -> Availability {
            Usable
        }
        fn profile(&self, _: &str, _: ToolKind) -> Profile {
            Profile {
                system_config: Some(!*self.isolated.lock().unwrap()),
                scope: None,
            }
        }
        fn look(&self, project: &str, _: &Path, _: ToolKind, profile: &Profile) -> Availability {
            while project == "other" && *self.held.lock().unwrap() {
                std::thread::sleep(Duration::from_millis(10));
            }
            if profile.system_config == Some(true) {
                Usable
            } else {
                Unusable("not signed in for this project".into())
            }
        }
    }

    /// Sixth review: the settings change between a job's read of the
    /// profile and the start of its look (waiting for a place): the look runs
    /// in the profile it is kept under, so the answer kept for the system's
    /// is the system's.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_look_is_kept_under_the_profile_it_ran_in() {
        let check = Arc::new(Profiled::default());
        *check.held.lock().unwrap() = true;
        let probes = Probes::new(check.clone(), 1, KEEP_LOOKS);
        let root = Path::new("/");
        // The one place is taken.
        let other = {
            let probes = Arc::clone(&probes);
            tokio::spawn(async move { probes.ask("other", Path::new("/"), Codex, &|| None).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert_eq!(probes.running(), 1);
        // A job reads System and waits for the place.
        let job = {
            let probes = Arc::clone(&probes);
            tokio::spawn(async move { probes.ask("p", Path::new("/"), Codex, &|| None).await })
        };
        tokio::time::sleep(Duration::from_millis(100)).await;
        // The project switches to its own profile before the job's look starts.
        *check.isolated.lock().unwrap() = true;
        *check.held.lock().unwrap() = false;
        other.await.unwrap().unwrap();
        assert_eq!(
            job.await.unwrap(),
            Ok(Usable),
            "it ran in the profile it read"
        );
        // Kept under the system's: asked again with System, the same answer.
        *check.isolated.lock().unwrap() = false;
        assert_eq!(probes.ask("p", root, Codex, &|| None).await, Ok(Usable));
        // And the project's own profile gets its own look.
        *check.isolated.lock().unwrap() = true;
        assert_eq!(
            probes.ask("p", root, Codex, &|| None).await,
            Ok(Unusable("not signed in for this project".into()))
        );
    }
}
