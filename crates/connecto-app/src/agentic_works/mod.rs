//! Project-owned PostgreSQL queues. The local journal precedes dispatch and
//! is acknowledged only after a token-guarded PostgreSQL completion commits.
pub mod agents;
mod cli;
mod cloudsql;
mod commands;
mod db;
mod questions;
mod store;
#[cfg(test)]
mod tests;

use connecto_core::{
    Binding, TermActivity, ToolKind, WorkspaceControl,
    agentic_works::{
        AgentStage, Database, DbStatus, GoogleCredential, ImportedKey, JobAgent, JobAgentChoice,
        JobQuestion, LinkedDatabase, MAX_PARALLEL, RunningJob, WorksAction, WorksControl,
        WorksDraft, WorksView, database_name,
    },
};
use connecto_workspace::Workspace;
use db::Result;
use serde_json::Value;
use std::{
    collections::{HashMap, HashSet},
    fs::OpenOptions,
    path::{Path, PathBuf},
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, Ordering},
    },
    thread,
    time::{Duration, Instant},
};
use tokio::{
    runtime::Runtime,
    task::JoinSet,
    time::{sleep, timeout},
};

struct State {
    workspace: Arc<Workspace>,
    vault: Arc<connecto_vault::Vault>,
    audit: connecto_audit::Audit,
    store: store::Store,
    root: PathBuf,
    stopping: AtomicBool,
    notices: Mutex<HashMap<String, String>>,
    /// (project, work type) pairs whose `setup` is running.
    setups: Mutex<HashMap<(String, String), Arc<SetupRun>>>,
    /// Projects whose leftover job folders were removed since start.
    swept: Mutex<HashSet<String>>,
    /// How each (project, database) answered when Connecto last looked.
    statuses: Mutex<HashMap<(String, String), DbStatus>>,
    /// The database each project took its last job from: the next look
    /// starts after it, so one busy database never starves another.
    served: Mutex<HashMap<String, String>>,
    /// Each project's jobs running now, from their claim until their
    /// outcome is written back (or left journalled).
    running: Mutex<HashMap<String, Vec<Running>>>,
    /// Queues (project, queue identity) changed for several jobs at once,
    /// or being changed.
    migrations: Mutex<HashMap<(String, String), Migration>>,
    /// Cloud SQL tokens and certificates, shared by every connection.
    cloudsql: cloudsql::Connector,
    /// Held while a link changes.
    linking: tokio::sync::Mutex<()>,
    /// Read while a cycle checks a link is still there and claims, journals
    /// or writes back a job of it; written while a database is removed, so
    /// no job is taken from a database once it is gone.
    claims: tokio::sync::RwLock<()>,
    /// Held while Connecto looks up, reserves and asks for a Cloud SQL
    /// password, so two commands never ask twice for one destination.
    asking: Mutex<()>,
    /// The running jobs' questions to the user (ADR 0040).
    ///
    /// Lock order: `questions`, then `running` (`State::ask` checks the job
    /// runs while it keeps the question). Nothing takes `questions` while
    /// holding `running`: whoever needs both outside `ask` takes them one
    /// after the other, each guard dropped first.
    questions: Mutex<questions::Questions>,
    /// Wakes the window when a question comes or goes.
    wake: Mutex<Option<Arc<dyn Fn() + Send + Sync>>>,
    /// What a job's agent is chosen from (ADR 0041): the looks at agents,
    /// shared by every job.
    agents: Arc<agents::Probes>,
    #[cfg(test)]
    hooks: tests::Hooks,
}
pub struct Works {
    state: Arc<State>,
    handle: tokio::runtime::Handle,
    thread: Mutex<Option<thread::JoinHandle<()>>>,
}
impl Works {
    pub fn start(
        root: &Path,
        workspace: Arc<Workspace>,
        vault: Arc<connecto_vault::Vault>,
        audit: connecto_audit::Audit,
        agents: Arc<dyn agents::AgentCheck>,
    ) -> Result<Arc<Self>> {
        let root = root.join("agentic_works");
        let mut directory = std::fs::DirBuilder::new();
        directory.recursive(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            directory.mode(0o700);
        }
        directory.create(&root).map_err(|e| e.to_string())?;
        let state = Arc::new(State {
            workspace,
            vault,
            audit,
            store: store::Store::open(&root)?,
            root,
            stopping: AtomicBool::new(false),
            notices: Mutex::default(),
            setups: Mutex::default(),
            swept: Mutex::default(),
            statuses: Mutex::default(),
            served: Mutex::default(),
            running: Mutex::default(),
            migrations: Mutex::default(),
            cloudsql: cloudsql::Connector::default(),
            linking: tokio::sync::Mutex::default(),
            claims: tokio::sync::RwLock::default(),
            #[cfg(test)]
            hooks: tests::Hooks::default(),
            asking: Mutex::default(),
            questions: Mutex::default(),
            wake: Mutex::default(),
            agents: agents::Probes::new(agents, agents::MAX_LOOKS, agents::KEEP_LOOKS),
        });
        let runtime = Runtime::new().map_err(|e| e.to_string())?;
        let handle = runtime.handle().clone();
        let runner = state.clone();
        let thread = thread::Builder::new()
            .name("agentic_works".into())
            .spawn(move || {
                runtime.block_on(runner.run());
                runtime.shutdown_timeout(Duration::from_secs(2));
            })
            .map_err(|e| e.to_string())?;
        Ok(Arc::new(Self {
            state,
            handle,
            thread: Mutex::new(Some(thread)),
        }))
    }
    /// Run `action` on the runtime; its outcome becomes the project's
    /// notice, and is sent to `done` when given.
    fn spawn_action(
        &self,
        project: String,
        action: WorksAction,
        done: Option<std::sync::mpsc::Sender<Result<String>>>,
    ) {
        let s = self.state.clone();
        // Setup runs in a terminal the user watches and may need to answer,
        // so publishing a type with one has no time limit.
        let watched = matches!(
            action,
            WorksAction::Publish { .. } | WorksAction::Setup { .. }
        );
        self.handle.spawn(async move {
            let result = if watched {
                s.action(&project, action).await
            } else {
                timeout(Duration::from_secs(20), s.action(&project, action))
                    .await
                    .unwrap_or_else(|_| Err("Operation timed out.".into()))
            };
            s.notice(&project, result.clone().unwrap_or_else(|e| e));
            if let Some(done) = done {
                let _ = done.send(result);
            }
        });
    }
    /// Wake the window with `wake` when a job's question comes or goes.
    pub fn set_wake(&self, wake: Arc<dyn Fn() + Send + Sync>) {
        *self.state.wake.lock().unwrap_or_else(|e| e.into_inner()) = Some(wake);
    }
    pub fn stop(&self) {
        self.state.stopping.store(true, Ordering::SeqCst);
        if let Some(t) = self.thread.lock().unwrap_or_else(|e| e.into_inner()).take() {
            let _ = t.join();
        }
    }
}
impl WorksControl for Works {
    fn view(&self, project: &str) -> WorksView {
        let statuses = self
            .state
            .statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        // Each lock in a statement of its own, so no guard is held while
        // another is taken (a guard in the struct literal below would live to
        // its end): `ask` holds the questions' lock, then the running jobs'.
        let running: Vec<RunningJob> = self
            .state
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
            .map(|list| {
                list.iter()
                    .map(|r| RunningJob {
                        database: r.db.clone(),
                        id: r.id,
                        kind: r.kind.clone(),
                        since: r.since.clone(),
                        agent: r
                            .agent
                            .as_ref()
                            .map(|c| c.agent.shown())
                            .unwrap_or_default(),
                        agent_stage: r.agent.as_ref().map(|c| c.stage).unwrap_or_default(),
                    })
                    .collect()
            })
            .unwrap_or_default();
        let notice = self
            .state
            .notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
            .cloned()
            .unwrap_or_default();
        let questions = self
            .state
            .questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting(Some(project), Instant::now());
        WorksView {
            databases: self
                .state
                .store
                .links(project)
                .unwrap_or_default()
                .into_iter()
                .map(|l| LinkedDatabase {
                    status: statuses
                        .get(&(project.to_owned(), l.name.clone()))
                        .cloned()
                        .unwrap_or_default(),
                    name: l.name,
                    database: l.database,
                })
                .collect(),
            google_keys: self
                .state
                .vault
                .host_secrets(project, &Binding::GoogleServiceAccount),
            types: self.state.store.kinds(project).unwrap_or_default(),
            parallel: self.state.store.parallel(project).unwrap_or(1),
            job_agent: self.state.store.job_agent(project).unwrap_or_default(),
            job_agent_choices: self.state.job_agent_choices(project),
            running,
            notice,
            questions,
        }
    }
    fn questions(&self) -> Vec<JobQuestion> {
        self.state
            .questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .waiting(None, Instant::now())
    }
    fn answer(&self, id: u64, answer: Option<String>) -> Result<()> {
        let answered = self
            .state
            .questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .answer(id, answer, Instant::now());
        self.state.woken();
        answered
    }
    fn submit(&self, project: String, action: WorksAction) {
        self.spawn_action(project, action, None);
    }
    fn draft(&self, project: &str, kind: &str) -> Result<WorksDraft> {
        let (terminal, databases) = self.state.store.author_of(project, kind)?;
        let relay = self
            .state
            .workspace
            .relay(project)
            .ok_or("Open the authoring project first.")?;
        let control = relay.terminals();
        let view = control.view();
        let author = view
            .terminals
            .iter()
            .find(|t| t.id == terminal)
            .ok_or("The authoring terminal has closed.")?;
        if author.in_turn || author.finished_turns == 0 {
            return Err("Wait for the authoring turn to finish.".into());
        }
        let raw = relay
            .terminal_reply(&terminal)
            .ok_or("The author has not returned a final reply yet.")?;
        Ok(WorksDraft {
            databases,
            ..draft_fields(&raw)?
        })
    }
    fn import_key(&self, project: &str, path: &Path) -> Result<ImportedKey> {
        let size = std::fs::metadata(path)
            .map_err(|_| "Cannot read that file.")?
            .len();
        if size > 64 * 1024 {
            return Err("That file is too large to be a service account key.".into());
        }
        let json = std::fs::read_to_string(path).map_err(|_| "Cannot read that file.")?;
        self.keep_key(project, &json)
    }
}
impl Works {
    /// Check a service account key's JSON and keep it in the vault for
    /// `project`, under a name made from its account.
    fn keep_key(&self, project: &str, json: &str) -> Result<ImportedKey> {
        let email = connecto_vault::check_service_account_key(json)?;
        // Named for the whole account, so two projects' accounts of the
        // same name never replace each other's key.
        let secret = key_secret_name(&email);
        self.state.vault.put_host_secret(
            project,
            &secret,
            &format!("Google Cloud service account {email}"),
            json.trim(),
            Binding::GoogleServiceAccount,
        )?;
        Ok(ImportedKey { secret, email })
    }
}
/// The vault name of a service account's key: readable, and ending with a
/// digest of the whole email, so two accounts never share one.
fn key_secret_name(email: &str) -> String {
    let account = email
        .strip_suffix(".gserviceaccount.com")
        .unwrap_or(email)
        .trim_end_matches(".iam");
    let digest = ring::digest::digest(&ring::digest::SHA256, email.as_bytes());
    let hex: String = digest
        .as_ref()
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect();
    let readable: String = secret_name("gcp-key-", account)
        .chars()
        .take(connecto_core::vault::MAX_NAME - hex.len() - 1)
        .collect();
    format!("{readable}-{hex}")
}
/// A password kept for a link attempt: removed when dropped, unless kept.
/// Dropping covers a failure and a cancelled (timed out) attempt alike.
struct Staged<'a> {
    vault: &'a connecto_vault::Vault,
    project: &'a str,
    name: Option<String>,
}
impl Staged<'_> {
    fn keep(&mut self) {
        self.name = None;
    }
}
impl Drop for Staged<'_> {
    fn drop(&mut self) {
        if let Some(name) = self.name.take() {
            let scope = connecto_core::ProviderScope::Project(self.project.to_owned());
            let _ = connecto_core::VaultControl::delete(self.vault, &scope, &name);
        }
    }
}
/// A vault secret name: `prefix` and `tail` with only the characters a name
/// may hold. Too long, it is cut and ends with a digest of the whole, so two
/// long names stay apart.
fn secret_name(prefix: &str, tail: &str) -> String {
    let tail: String = tail
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '-'
            }
        })
        .collect();
    let full = format!("{prefix}{tail}");
    if full.len() <= connecto_core::vault::MAX_NAME {
        return full;
    }
    let digest = ring::digest::digest(&ring::digest::SHA256, full.as_bytes());
    let hex: String = digest
        .as_ref()
        .iter()
        .take(4)
        .map(|b| format!("{b:02x}"))
        .collect();
    format!(
        "{}-{hex}",
        &full[..connecto_core::vault::MAX_NAME - hex.len() - 1]
    )
}
/// An author's reply (or a definition an agent gives `connecto works`): one
/// JSON object with `instructions`, `schema`, and optional `setup` and
/// `prepare`, each field ready for review.
fn draft_fields(raw: &str) -> Result<WorksDraft> {
    let v: Value = parse_json(raw)?;
    let instructions = v
        .get("instructions")
        .and_then(Value::as_str)
        .ok_or("Draft needs instructions.")?;
    let schema = v.get("schema").ok_or("Draft needs a JSON Schema.")?;
    let pretty = |v: &Value| serde_json::to_string_pretty(v).map_err(|e| e.to_string());
    let command = |key| {
        v.get(key)
            .filter(|c| !c.is_null())
            .map_or(Ok(String::new()), pretty)
    };
    Ok(WorksDraft {
        instructions: instructions.to_owned(),
        schema: pretty(schema)?,
        setup: command("setup")?,
        prepare: command("prepare")?,
        databases: None,
    })
}
fn parse_json(raw: &str) -> Result<Value> {
    serde_json::from_str(raw.trim()).map_err(|e| {
        format!(
            "Expected a single JSON value at line {}, column {}.",
            e.line(),
            e.column()
        )
    })
}
fn definition(instructions: &str, schema: &str) -> Result<jsonschema::Validator> {
    if !instructions.contains("{{prompt}}") {
        return Err("Instructions must include {{prompt}}.".into());
    }
    let schema = parse_json(schema)?;
    jsonschema::meta::validate(&schema).map_err(|e| format!("Invalid JSON Schema: {e}"))?;
    jsonschema::validator_for(&schema).map_err(|e| format!("Cannot compile JSON Schema: {e}"))
}
/// A definition checked whole: schema, both commands, and `{{prepared}}`.
struct Checked {
    validator: jsonschema::Validator,
    setup: Option<commands::Command>,
    prepare: Option<commands::Command>,
}
fn check(def: &store::Definition) -> Result<Checked> {
    let validator = definition(&def.instructions, &def.schema)?;
    let command = |raw: &Option<String>, name: &str| {
        raw.as_deref()
            .map(|r| commands::parse(r).map_err(|e| format!("{name}: {e}")))
            .transpose()
    };
    let setup = command(&def.setup, "Setup")?;
    let prepare = command(&def.prepare, "Prepare")?;
    if def.instructions.contains(commands::PREPARED) != prepare.is_some() {
        return Err(
            "Instructions must include {{prepared}} exactly when a prepare command is set.".into(),
        );
    }
    Ok(Checked {
        validator,
        setup,
        prepare,
    })
}
/// Put the prompt and the prepared JSON in the instructions in one pass, so
/// neither value's own text is ever taken for a placeholder.
fn fill(template: &str, prompt: &str, prepared: &str) -> String {
    let mut out = String::with_capacity(template.len() + prompt.len() + prepared.len());
    let mut rest = template;
    loop {
        let next = [("{{prompt}}", prompt), (commands::PREPARED, prepared)]
            .into_iter()
            .filter_map(|(key, value)| rest.find(key).map(|at| (at, key, value)))
            .min_by_key(|(at, ..)| *at);
        let Some((at, key, value)) = next else {
            out.push_str(rest);
            return out;
        };
        out.push_str(&rest[..at]);
        out.push_str(value);
        rest = &rest[at + key.len()..];
    }
}
/// A command field: None when blank.
fn optional(raw: Option<String>) -> Option<String> {
    raw.filter(|r| !r.trim().is_empty())
}
fn provider(cli: &str) -> Result<ToolKind> {
    match cli.trim() {
        "" | "agy" | "antigravity" => Ok(ToolKind::Antigravity),
        "claude" => Ok(ToolKind::Claude),
        "codex" => Ok(ToolKind::Codex),
        "pi" => Ok(ToolKind::Pi),
        "opencode" => Ok(ToolKind::OpenCode),
        _ => Err(format!("Unsupported session CLI: {cli}")),
    }
}
/// A model name a project's agent for jobs may be given: what a terminal's
/// `--model` takes (plain name characters, no leading `-`; `/` and `:` for
/// Pi's provider and thinking level).
fn valid_model(model: &str) -> bool {
    (1..=64).contains(&model.len())
        && model.as_bytes()[0].is_ascii_alphanumeric()
        && model.bytes().all(|byte| {
            byte.is_ascii_alphanumeric()
                || matches!(byte, b'-' | b'_' | b'.' | b'[' | b']' | b'/' | b':')
        })
}
impl State {
    /// The CLIs "Agent for jobs" offers: the agents the project's Tools
    /// allow, in the order every list of them uses, each with the models a
    /// new terminal of it is offered (the tool's own list, else the
    /// suggested ones; Pi's from its providers).
    fn job_agent_choices(&self, project: &str) -> Vec<JobAgentChoice> {
        let check = self.agents.check();
        let registry = self
            .workspace
            .view()
            .projects
            .iter()
            .find(|p| p.id == project)
            .and_then(|p| p.terminals.clone());
        connecto_core::ProjectTools::CHOICES
            .into_iter()
            .filter(|tool| check.allows(project, *tool))
            .map(|tool| JobAgentChoice {
                tool,
                models: check
                    .models(project, tool)
                    .or_else(|| registry.as_ref().and_then(|r| r.offered_models(tool)))
                    .unwrap_or_else(|| {
                        tool.suggested_models()
                            .iter()
                            .map(|m| (*m).to_owned())
                            .collect()
                    })
                    .into_iter()
                    // Only what the setting takes: every choice shown can
                    // be saved (a Pi model named with its provider may be
                    // too long for a launch's `--model`).
                    .filter(|m| valid_model(m))
                    .collect(),
            })
            .collect()
    }
    fn project_root(&self, project: &str) -> Result<PathBuf> {
        self.workspace
            .view()
            .projects
            .iter()
            .find(|p| p.id == project && !p.path.is_empty())
            .map(|p| PathBuf::from(&p.path))
            .ok_or_else(|| "Open the local project first.".into())
    }
    fn runner<'a>(
        &'a self,
        project: &'a str,
        kind: &'a str,
        root: &'a Path,
    ) -> commands::Runner<'a> {
        commands::Runner {
            audit: &self.audit,
            store: &self.store,
            project,
            kind,
            root,
        }
    }
    /// Claim the right to run `kind`'s setup with `cmd`; released on drop.
    /// Err holds the setup of that type running now.
    fn claim_setup(
        &self,
        project: &str,
        kind: &str,
        cmd: &commands::Command,
    ) -> std::result::Result<SetupClaim<'_>, Arc<SetupRun>> {
        let key = (project.to_owned(), kind.to_owned());
        let mut running = self.setups.lock().unwrap_or_else(|e| e.into_inner());
        if let Some(run) = running.get(&key) {
            return Err(Arc::clone(run));
        }
        let run = Arc::new(SetupRun {
            command: cmd.clone(),
            done: Mutex::new(None),
        });
        running.insert(key.clone(), Arc::clone(&run));
        Ok(SetupClaim {
            setups: &self.setups,
            key,
            run,
        })
    }
    /// Wait for the setup `run` to end, and give how it ended. Err when the
    /// job's deadline passes or it is stopped first.
    async fn await_setup(
        &self,
        run: &SetupRun,
        deadline: Instant,
        stop: Option<Stop<'_>>,
    ) -> Result<SetupEnd> {
        loop {
            if let Some(done) = run.result() {
                return Ok(done);
            }
            if Instant::now() >= deadline {
                return Err("Setup did not finish before the job's 30 minutes ran out.".into());
            }
            if self.stopping.load(Ordering::SeqCst) || stop.is_some_and(|stop| stop()) {
                return Err("Work cancelled during setup.".into());
            }
            sleep(Duration::from_millis(200)).await;
        }
    }
    /// Run `setup` and record when it last succeeded (or that it failed).
    /// A job's setup (`deadline` given) asked for while the type's setup
    /// with the same command runs already waits for that one and takes its
    /// result; a setup with another command (a revision being published) is
    /// waited for, and then this one runs. So is one whose own job stopped
    /// it (Pause, its database removed, its deadline): it says nothing of
    /// the command, and the waiting job runs setup itself. Without a
    /// deadline (the user's Setup) a running setup is refused.
    async fn recorded_setup(
        &self,
        project: &str,
        kind: &str,
        root: &Path,
        cmd: &commands::Command,
        deadline: Option<Instant>,
        stop: Option<Stop<'_>>,
    ) -> Result<()> {
        loop {
            match self.claim_setup(project, kind, cmd) {
                Ok(claim) => {
                    let ran = self
                        .run_setup(project, kind, root, cmd, deadline, stop)
                        .await;
                    // Stopped for this job's own reasons, not failed: no
                    // failure is recorded, and a job waiting takes over
                    // (the claim, dropped unfinished, says so).
                    if ran.is_err()
                        && (self.stopping.load(Ordering::SeqCst)
                            || stop.is_some_and(|stop| stop())
                            || deadline.is_some_and(|d| Instant::now() >= d))
                    {
                        return ran.map(drop);
                    }
                    let recorded =
                        self.store
                            .setup_ok(project, kind, ran.as_ref().ok().map(String::as_str));
                    let out = recorded.and(ran.map(drop));
                    claim.finish(&out);
                    return out;
                }
                Err(running) => {
                    let Some(deadline) = deadline else {
                        return Err(already_running(kind));
                    };
                    match self.await_setup(&running, deadline, stop).await? {
                        SetupEnd::Ran(shared) if running.command == *cmd => {
                            return shared.map_err(|e| {
                                format!(
                                    "Setup for {kind}, run meanwhile for another job, failed: {e}"
                                )
                            });
                        }
                        // Another command's, or stopped by its own job:
                        // this job runs setup itself.
                        SetupEnd::Ran(_) | SetupEnd::Abandoned => {}
                    }
                }
            }
        }
    }

    /// Run `setup` in a shell terminal of the agentic jobs project in
    /// Connecto Home, where the user watches it and can answer it. It has no
    /// time limit of its own; `deadline` is a job's, when a job needs it.
    /// Ok holds the time it succeeded. A failed run's terminal stays open so
    /// its output can be read. `stop` is a job's: when it says so (Pause,
    /// the job's database removed), setup is ended.
    #[cfg(unix)]
    async fn run_setup(
        &self,
        project: &str,
        kind: &str,
        root: &Path,
        cmd: &commands::Command,
        deadline: Option<Instant>,
        stop: Option<Stop<'_>>,
    ) -> Result<String> {
        let (script, line) = commands::setup_script(&self.root, root, kind, cmd)?;
        let outcome = self
            .setup_terminal(project, kind, root, &line, deadline, stop)
            .await;
        let _ = std::fs::remove_file(&script);
        outcome
    }
    #[cfg(unix)]
    async fn setup_terminal(
        &self,
        project: &str,
        kind: &str,
        root: &Path,
        line: &str,
        deadline: Option<Instant>,
        stop: Option<Stop<'_>>,
    ) -> Result<String> {
        let home = self.workspace.connecto_home().ok_or(
            "Choose Connecto Home in Settings first: setup runs in its agentic jobs project.",
        )?;
        let jobs = home.join(connecto_workspace::AGENTIC_JOBS_FOLDER);
        std::fs::create_dir_all(&jobs)
            .map_err(|e| format!("Cannot create {}: {e}", jobs.display()))?;
        let jobs = jobs.canonicalize().map_err(|e| e.to_string())?;
        let jobs_id = connecto_session::launch::project_id(&jobs);
        let expired = || deadline.is_some_and(|d| Instant::now() >= d);
        if self.workspace.relay(&jobs_id).is_none() {
            self.workspace.open_project(&jobs)?;
        }
        let opened = Instant::now();
        let relay = loop {
            if let Some(relay) = self.workspace.relay(&jobs_id) {
                break relay;
            }
            if opened.elapsed() > Duration::from_secs(20) || self.stopping.load(Ordering::SeqCst) {
                return Err("The agentic jobs project did not open.".into());
            }
            sleep(Duration::from_millis(100)).await;
        };
        let control = relay.terminals();
        let name: String = format!("setup {kind} · {}", self.project_name(project))
            .chars()
            .take(connecto_core::MAX_NAME_CHARS)
            .collect();
        let id = control.add_with(ToolKind::Shell, Some(name), None, None, None)?;
        if deadline.is_none() {
            self.workspace.activate(&jobs_id);
            self.workspace.set_active_terminal(&jobs_id, &id);
        }
        self.notice(
            project,
            format!("Setup for {kind} is running in the agentic jobs project. Answer it there if it asks."),
        );
        let start = Instant::now();
        let mut typed = false;
        let mut ready: Option<Instant> = None;
        let outcome = loop {
            if self.stopping.load(Ordering::SeqCst) {
                break Err("Connecto is closing.".to_owned());
            }
            if expired() {
                let _ = close(&*control, &id).await;
                break Err("Setup did not finish before the job's 30 minutes ran out.".into());
            }
            if stop.is_some_and(|stop| stop()) {
                let _ = close(&*control, &id).await;
                break Err("Work cancelled during setup.".into());
            }
            let view = control.view();
            let Some(entry) = view.terminals.iter().find(|e| e.id == id) else {
                break Err("The setup terminal was closed.".into());
            };
            let stage = entry.control.as_ref().map(|c| c.stage());
            if let Some(connecto_core::PaneStage::Failed { reason }) = &stage {
                break Err(format!("The setup terminal failed: {reason}"));
            }
            match (entry.session.status(), typed) {
                (connecto_core::SessionStatus::Exited { code, signal }, true) => {
                    break Ok((code, signal));
                }
                (connecto_core::SessionStatus::Exited { .. }, false) => {
                    break Err("The setup terminal ended before setup started.".into());
                }
                (connecto_core::SessionStatus::Unknown { reason }, true) => {
                    break Err(format!("The setup terminal failed: {reason}"));
                }
                // Typed half a second after the shell started: it reads the
                // line once its startup files have run.
                (connecto_core::SessionStatus::Running, false)
                    if stage == Some(connecto_core::PaneStage::Running)
                        && ready.get_or_insert_with(Instant::now).elapsed()
                            > Duration::from_millis(500) =>
                {
                    entry
                        .session
                        .send_input(format!("{line}\r").into_bytes())
                        .map_err(|e| e.to_string())?;
                    typed = true;
                }
                _ => {}
            }
            sleep(Duration::from_millis(150)).await;
        };
        let runner = self.runner(project, kind, root);
        let elapsed = start.elapsed();
        match outcome {
            Ok((0, None)) => {
                runner.journal_outcome("setup", None, "exit 0".into(), elapsed);
                let _ = close(&*control, &id).await;
                Ok(commands::now())
            }
            Ok((code, signal)) => {
                let outcome =
                    signal.map_or_else(|| format!("exit {code}"), |s| format!("signal {s}"));
                runner.journal_outcome("setup", None, outcome.clone(), elapsed);
                Err(format!(
                    "Setup for {kind} failed ({outcome}). Its terminal in the agentic jobs project shows why."
                ))
            }
            Err(e) => {
                runner.journal_outcome("setup", None, "not finished".into(), elapsed);
                Err(e)
            }
        }
    }

    /// Without a terminal to type into, setup runs hidden, within its
    /// `timeout_s`.
    #[cfg(not(unix))]
    async fn run_setup(
        &self,
        project: &str,
        kind: &str,
        root: &Path,
        cmd: &commands::Command,
        deadline: Option<Instant>,
        stop: Option<Stop<'_>>,
    ) -> Result<String> {
        self.notice(project, format!("Running setup for {kind}…"));
        let epoch = self.workspace.pause_switch().epoch();
        let cancel = || self.cancelled(project, epoch) || stop.is_some_and(|stop| stop());
        let limit = deadline.map_or(cmd.timeout, |d| d.saturating_duration_since(Instant::now()));
        self.runner(project, kind, root)
            .run_setup(cmd, limit, &cancel)
            .await
    }

    fn project_name(&self, project: &str) -> String {
        self.workspace
            .view()
            .projects
            .iter()
            .find(|p| p.id == project)
            .map(|p| p.name.clone())
            .unwrap_or_default()
    }

    /// Connect to a project's database, the one way every part of Agentic
    /// Works does. `report` gets a line for each step that succeeded.
    ///
    /// `typed` is a Cloud SQL password just typed, used instead of the saved
    /// one. A saved password is used only for the instance and user it was
    /// saved with (a linked database's), never offered to another.
    async fn connect(
        &self,
        project: &str,
        database: &Database,
        typed: Option<&str>,
        report: &mut cloudsql::Report,
    ) -> Result<tokio_postgres::Client> {
        self.connect_routed(project, database, typed, report)
            .await
            .map(|(client, _)| client)
    }

    /// [`Self::connect`], with what cancels a statement of that very
    /// connection: its own cancellation key, sent where it was made.
    async fn connect_routed(
        &self,
        project: &str,
        database: &Database,
        typed: Option<&str>,
        report: &mut cloudsql::Report,
    ) -> Result<(tokio_postgres::Client, Canceller)> {
        match database {
            Database::ConnectionString(name) => {
                let dsn = self.vault.works_credential(project, name)?;
                let db = db::open(&dsn).await?;
                report.push(format!("✓ Connected through {name}"));
                let token = db.cancel_token();
                Ok((db, Canceller::Direct(token)))
            }
            Database::CloudSql(sql) => {
                let key = match &sql.credential {
                    GoogleCredential::Key(name) if name.is_empty() => {
                        return Err(
                            "Import a service account key, or choose My gcloud login.".into()
                        );
                    }
                    GoogleCredential::Key(name) => Some(self.vault.google_key(project, name)?),
                    GoogleCredential::Adc => None,
                };
                let password = match typed.filter(|p| !p.is_empty()) {
                    _ if sql.iam_auth => None,
                    Some(typed) => Some(typed.to_owned()),
                    None if sql.password_secret.is_empty() => None,
                    None => {
                        let saved_for_it = self.store.links(project)?.iter().any(|l| {
                            matches!(&l.database, Database::CloudSql(l)
                                if l.password_secret == sql.password_secret
                                    && l.connection_name.trim() == sql.connection_name.trim()
                                    && l.user.trim() == sql.user.trim())
                        });
                        if !saved_for_it {
                            return Err(
                                "Enter the database password for this instance and user.".into()
                            );
                        }
                        Some(self.vault.works_credential(project, &sql.password_secret)?)
                    }
                };
                let (db, route) = self
                    .cloudsql
                    .connect_routed(sql, key.as_deref(), password.as_deref(), report)
                    .await?;
                let token = db.cancel_token();
                Ok((db, Canceller::Routed(token, route)))
            }
        }
    }

    /// Record how the database `db` of `project` answered.
    fn status(&self, project: &str, db: &str, outcome: std::result::Result<String, String>) {
        let (ok, message) = match outcome {
            Ok(m) => (true, m),
            Err(e) => (false, e),
        };
        self.statuses
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(
                (project.to_owned(), db.to_owned()),
                DbStatus {
                    ok: Some(ok),
                    message,
                    at: commands::now(),
                },
            );
    }
    /// Delete the Cloud SQL password a link kept, unless a link still uses it.
    fn drop_password(&self, project: &str, secret: &str) {
        if secret.is_empty() || self.store.password_in_use(project, secret).unwrap_or(true) {
            return;
        }
        let scope = connecto_core::ProviderScope::Project(project.to_owned());
        let _ = connecto_core::VaultControl::delete(&*self.vault, &scope, secret);
    }
    fn notice(&self, project: &str, message: String) {
        self.notices
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(project.to_owned(), message);
    }
    /// The token of the job whose agent runs in `caller`'s terminal: that
    /// very terminal, as the journal names it.
    /// A job's terminal is journalled just after its agent starts, so an
    /// agent that asks at once may come first: a terminal Connecto made for a
    /// job (`WorksCaller::job`) waits for it, [`ATTACH_WAIT`] at most.
    fn job_of(&self, caller: &connecto_core::agentic_works::WorksCaller) -> Result<String> {
        let until = Instant::now() + ATTACH_WAIT;
        loop {
            let found = self
                .store
                .pendings(&caller.project_id)?
                .into_iter()
                .find(|p| p.terminal.as_deref() == Some(caller.terminal.as_str()))
                .map(|p| p.token);
            match found {
                Some(token) => return Ok(token),
                None if caller.job && Instant::now() < until => {
                    thread::sleep(Duration::from_millis(50));
                }
                None => return Err(NOT_A_JOB.to_owned()),
            }
        }
    }
    /// `connecto works answer`: the reply to the caller's job's question.
    fn job_answer(&self, caller: &connecto_core::agentic_works::WorksCaller) -> Result<String> {
        let token = self.job_of(caller)?;
        // The terminal's own counters now, while its agent runs this in a
        // turn: only a reply kept after them is the job's answer.
        let relay = self
            .workspace
            .relay(&caller.project_id)
            .ok_or("Project closed")?;
        let now = questions::Mark {
            replies: relay
                .terminal_reply_numbered(&caller.terminal)
                .map_or(0, |(number, _)| number),
            turns: relay.terminal_finished_turns(&caller.terminal).unwrap_or(0),
        };
        let mut questions = self.questions.lock().unwrap_or_else(|e| e.into_inner());
        // Only the attempt of the job's terminal that asked reads it.
        if let Some(reply) = questions.read(&token, caller.attempt, now) {
            return Ok(reply.to_owned());
        }
        Err(if questions.pending(&token) {
            "No reply yet. End your turn now: a [connecto notice] tells you when it is in.".into()
        } else {
            "This job asked no question. Ask with `connecto works ask '<question>'`.".into()
        })
    }
    /// Wake the window: a job's question came or went.
    fn woken(&self) {
        let wake = self.wake.lock().unwrap_or_else(|e| e.into_inner()).clone();
        if let Some(wake) = wake {
            wake();
        }
    }
    /// `connecto works ask`: the question of the job whose agent runs in
    /// `caller`'s terminal (that very terminal, as the journal names it),
    /// shown to the user until they answer; `notify` hands the answer to
    /// that terminal when the job's loop finds its agent idle.
    fn ask(
        &self,
        caller: &connecto_core::agentic_works::WorksCaller,
        text: &str,
        notify: connecto_core::AskReply,
    ) -> Result<String> {
        let text = text.trim();
        if text.is_empty() {
            return Err("`works ask` takes the question: connecto works ask '<question>'".into());
        }
        if text.chars().count() > questions::MAX_QUESTION {
            return Err(format!(
                "The question is over {} characters: ask it shorter.",
                questions::MAX_QUESTION
            ));
        }
        let project = caller.project_id.as_str();
        let token = self.job_of(caller)?;
        let project_name = self.project_name(project);
        // The job found running and the question kept under the questions'
        // lock (then the running jobs'), which the end of the job's run takes
        // to close it: a question is never kept for a run that has ended.
        let mut questions = self.questions.lock().unwrap_or_else(|e| e.into_inner());
        let (question, deadline) = {
            let running = self.running.lock().unwrap_or_else(|e| e.into_inner());
            let job = running
                .get(project)
                .and_then(|list| list.iter().find(|r| r.token == token))
                .ok_or_else(|| NOT_A_JOB.to_owned())?;
            let answer_by = job
                .deadline
                .checked_sub(ANSWER_MARGIN)
                .unwrap_or(job.deadline);
            let left = answer_by.saturating_duration_since(Instant::now());
            let by = chrono::Local::now()
                + chrono::Duration::from_std(left).unwrap_or_else(|_| chrono::Duration::zero());
            (
                JobQuestion {
                    id: 0,
                    project: project.to_owned(),
                    project_name,
                    database: job.db.clone(),
                    job: job.id,
                    kind: job.kind.clone(),
                    terminal: caller.terminal.clone(),
                    text: connecto_redact::sanitize(text),
                    asked_at: commands::now(),
                    answer_by: by.to_rfc3339_opts(chrono::SecondsFormat::Secs, false),
                },
                answer_by,
            )
        };
        let job = question.job;
        let minutes = deadline.saturating_duration_since(Instant::now()).as_secs() / 60;
        questions.ask(
            question,
            &token,
            caller.attempt,
            deadline,
            Instant::now(),
            notify,
        )?;
        drop(questions);
        self.notice(project, format!("Job {job} asks the user a question."));
        self.woken();
        Ok(format!(
            "Asked the user (job {job}). End your turn now, without a final answer: a [connecto notice] wakes you when the reply is in, then run `connecto works answer`. Do not wait, sleep or poll. The user has up to {minutes} minutes to answer; if no answer comes you are told so, and then end the job with your final JSON."
        ))
    }
    async fn action(&self, project: &str, action: WorksAction) -> Result<String> {
        if self.workspace.relay(project).is_none() {
            return Err("Open the local project first.".into());
        }
        match action {
            WorksAction::Link {
                name,
                mut database,
                create,
                password,
            } => {
                let name = database_name(&name)?;
                // One link change at a time, so two never pick up or remove
                // each other's password.
                let _one = self.linking.lock().await;
                let before = self.store.linked(project, &name)?;
                if create && before.is_some() {
                    return Err(format!(
                        "This project already has a database named {name}: validate its queue instead, or choose another name."
                    ));
                }
                // A typed password is kept under a new name, and the link
                // moves to it only once linking succeeded: a failed attempt
                // never changes the password the workers use.
                let typed = password.filter(|p| !p.is_empty());
                let mut staged = Staged {
                    vault: &self.vault,
                    project,
                    name: None,
                };
                if let (Database::CloudSql(sql), Some(typed)) = (&mut database, &typed)
                    && !sql.iam_auth
                {
                    let name = secret_name(
                        "cloudsql-password-",
                        &uuid::Uuid::new_v4().simple().to_string()[..12],
                    );
                    self.vault.put_host_secret(
                        project,
                        &name,
                        &format!(
                            "Cloud SQL password of {} on {}",
                            sql.user.trim(),
                            sql.connection_name.trim()
                        ),
                        typed,
                        connecto_core::Binding::AgenticWorks,
                    )?;
                    sql.password_secret.clone_from(&name);
                    staged.name = Some(name);
                }
                let linked = async {
                    let mut db = self
                        .connect(project, &database, typed.as_deref(), &mut Vec::new())
                        .await?;
                    let queue = db::setup(&mut db, project, create).await?;
                    self.store.link(project, &name, &database, &queue)
                }
                .await;
                if linked.is_ok() {
                    staged.keep();
                }
                drop(staged);
                let unused = match &linked {
                    Err(_) => None,
                    Ok(()) => match (before, &database) {
                        (Some((Database::CloudSql(old), _)), Database::CloudSql(new))
                            if old.password_secret != new.password_secret =>
                        {
                            Some(old.password_secret)
                        }
                        (Some((Database::CloudSql(old), _)), Database::ConnectionString(_)) => {
                            Some(old.password_secret)
                        }
                        _ => None,
                    },
                };
                if let Some(secret) = unused {
                    self.drop_password(project, &secret);
                }
                linked?;
                Ok(format!("Database {name} linked."))
            }
            WorksAction::Unlink { name, force } => {
                let _one = self.linking.lock().await;
                // No claim is under way: once removed, nothing is taken from it.
                let _claims = self.claims.write().await;
                let name = database_name(&name)?;
                if self.store.linked(project, &name)?.is_none() {
                    return Err(format!("This project has no database named {name}."));
                }
                if !force && let Some(p) = self.store.pending(project, &name)?.first() {
                    return Err(format!(
                        "Job {} of the database {name} is running, or its outcome is still to be written back to it. Remove it once that is done, or force it: the outcome of every job of it is then lost (running ones are stopped), and the database marks those jobs timed out after 30 minutes.",
                        p.id
                    ));
                }
                // Nothing more is changed in its queue once this returns:
                // a change under way is stopped first, and when that cannot
                // be made sure of, the database stays linked.
                if let Some((_, queue)) = self.store.linked(project, &name)? {
                    self.end_migration(project, &queue).await.map_err(|e| {
                        format!("The database {name} is still linked. {e} Try again in a moment.")
                    })?;
                }
                let removed = self.store.unlink(project, &name)?;
                self.statuses
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .remove(&(project.to_owned(), name.clone()));
                if let Some(Database::CloudSql(sql)) = removed {
                    self.drop_password(project, &sql.password_secret);
                }
                Ok(format!(
                    "Removed the database {name}: Connecto takes no more jobs from it. Its queue is left in the database."
                ))
            }
            WorksAction::Test { database, password } => {
                let mut report = Vec::new();
                match self
                    .connect(project, &database, password.as_deref(), &mut report)
                    .await
                {
                    Ok(db) => report.push(match db::queue_present(&db).await {
                        Ok(true) => "✓ Queue schema found: use Validate existing queue.".into(),
                        Ok(false) => {
                            "✓ Queue schema absent: Create queue and link makes it.".into()
                        }
                        Err(e) => format!("✗ {e}"),
                    }),
                    Err(e) => report.push(format!("✗ {e}")),
                }
                Ok(report.join("\n"))
            }
            WorksAction::Author { kind, cli } => {
                if kind.trim().is_empty() || !matches!(cli, ToolKind::Claude | ToolKind::Codex) {
                    return Err("Choose a work type and Claude or Codex.".into());
                }
                if self.workspace.pause_switch().is_paused() {
                    return Err("Resume Connecto before starting an author.".into());
                }
                let relay = self.workspace.relay(project).ok_or("Project closed")?;
                let prompt = format!(
                    "Help me define the Agentic Works type {kind}. Ask me what this work should do, what the incoming prompt supplies and what JSON result I need. When agreed, return only a JSON object with instructions (a string containing the literal {{{{prompt}}}} placeholder) and schema (the expected JSON Schema), and optionally setup and prepare commands (argv arrays) that Connecto runs in the project root: setup once when publishing, prepare before each job with the job prompt on stdin, its JSON stdout replacing {{{{prepared}}}}. Write each command as {{\"argv\": [\"relative/path/or/program\", \"arg\"], \"timeout_s\": 600}}; instructions contain {{{{prepared}}}} exactly when prepare is set. prepare gets CONNECTO_JOB_DIR, a fresh folder for its files, and exits 75 when setup must run again. I will review and publish it in the agentic_works menu."
                );
                let id = relay.terminals().add_with(
                    cli,
                    Some("Agentic Works author".into()),
                    Some(prompt),
                    None,
                    None,
                )?;
                // A revision is published for the databases the type has.
                let scope = self.store.databases_of(project, &kind)?;
                self.store.author(project, &kind, &id, scope.as_deref())?;
                self.workspace.activate(project);
                self.workspace.set_active_terminal(project, &id);
                Ok("Authoring session opened. Return here to import its final reply.".into())
            }
            WorksAction::Publish {
                kind,
                instructions,
                schema,
                setup,
                prepare,
                databases,
            } => {
                if kind.trim().is_empty() {
                    return Err("Enter a work type.".into());
                }
                let databases = self.published_to(project, databases)?;
                let def = store::Definition {
                    instructions,
                    schema,
                    setup: optional(setup),
                    prepare: optional(prepare),
                };
                let checked = check(&def)?;
                // Setup runs before publishing: no job starts on a machine it
                // has not prepared.
                let mut ok_at = None;
                if let Some(cmd) = &checked.setup {
                    let root = self.project_root(project)?;
                    let claim = self
                        .claim_setup(project, &kind, cmd)
                        .map_err(|_| already_running(&kind))?;
                    let ran = self.run_setup(project, &kind, &root, cmd, None, None).await;
                    claim.finish(&ran.as_ref().map(drop).map_err(Clone::clone));
                    ok_at = Some(ran?);
                }
                self.store
                    .publish(project, &kind, &def, ok_at.as_deref(), databases.as_deref())?;
                Ok(match databases {
                    None => format!("Published {kind}. It serves every linked database."),
                    Some(list) => format!(
                        "Published {kind}. It serves the databases {} only.",
                        list.join(", ")
                    ),
                })
            }
            WorksAction::Parallel { jobs } => {
                if !(1..=MAX_PARALLEL).contains(&jobs) {
                    return Err(format!(
                        "Run from 1 to {MAX_PARALLEL} jobs at once, not {jobs}."
                    ));
                }
                // Changed while no claim is being admitted: once this returns,
                // every claim counts with the new number.
                let _admission = self.claims.write().await;
                self.store.set_parallel(project, jobs)?;
                Ok(if jobs == 1 {
                    "This project runs one job at a time.".into()
                } else {
                    format!("This project runs up to {jobs} jobs at once, shared by its databases.")
                })
            }
            WorksAction::JobAgent { agent } => {
                if let JobAgent::Cli { tool, model } = &agent {
                    let choices = self.job_agent_choices(project);
                    let Some(choice) = choices.iter().find(|c| c.tool == *tool) else {
                        return Err(format!(
                            "{} is not one of this project's Tools: allow it in the project's settings first, or choose another agent.",
                            tool.label()
                        ));
                    };
                    // A model from the CLI's list, or (None) its default model,
                    // which every CLI has, Pi and OpenCode with no list too.
                    if let Some(m) = model
                        && !(valid_model(m) && choice.models.contains(m))
                    {
                        return Err(format!(
                            "{m:?} is not one of {}'s models here. Choose one of: its default model (no --model){}.",
                            tool.label(),
                            choice
                                .models
                                .iter()
                                .map(|m| format!(", {m}"))
                                .collect::<String>()
                        ));
                    }
                }
                // Changed while no claim is being admitted: a job admitted
                // before keeps the agent read at its admission; every job
                // admitted after gets this one.
                let _admission = self.claims.write().await;
                self.store.set_job_agent(project, &agent)?;
                Ok(match agent {
                    JobAgent::Automatic => "Jobs that name no agent run on Antigravity, else the first usable stand-in (Automatic).".to_owned(),
                    JobAgent::Cli { .. } => format!(
                        "Jobs that name no agent run on {} from the next job on. Jobs whose type names an agent keep it.",
                        agent.label()
                    ),
                })
            }
            WorksAction::Setup { kind } => {
                let checked = check(&self.store.definition(project, &kind)?)?;
                let cmd = checked
                    .setup
                    .ok_or_else(|| format!("{kind} has no setup command."))?;
                let root = self.project_root(project)?;
                self.recorded_setup(project, &kind, &root, &cmd, None, None)
                    .await?;
                Ok(format!("Setup for {kind} succeeded."))
            }
        }
    }
    async fn run(self: Arc<Self>) {
        let mut tasks = JoinSet::new();
        let mut busy = HashMap::new();
        let mut next = Instant::now();
        while !self.stopping.load(Ordering::SeqCst) {
            while let Some(done) = tasks.try_join_next_with_id() {
                let task_id = match done {
                    Ok((id, ())) => id,
                    Err(error) => error.id(),
                };
                busy.retain(|_, running| *running != task_id);
            }
            if Instant::now() >= next {
                next = Instant::now() + Duration::from_secs(5);
                let pause = self.workspace.pause_switch();
                let epoch = pause.epoch();
                if !pause.is_paused() {
                    for p in &self.workspace.view().projects {
                        if p.terminals.is_none() || busy.contains_key(&p.id) {
                            continue;
                        }
                        // A project with journalled jobs but no database left
                        // still has their terminals to close.
                        if let Ok(links) = self.store.links(&p.id)
                            && (!links.is_empty()
                                || self.store.pendings(&p.id).is_ok_and(|j| !j.is_empty()))
                        {
                            let s = self.clone();
                            let id = p.id.clone();
                            let name = p.name.clone();
                            let task = tasks.spawn(async move {
                                if let Err(e) = s.cycle(&id, &name, epoch).await {
                                    s.notice(&id, e);
                                }
                            });
                            busy.insert(p.id.clone(), task.id());
                        }
                    }
                }
            }
            sleep(Duration::from_millis(100)).await;
        }
        while tasks.join_next().await.is_some() {}
    }
    fn cancelled(&self, project: &str, epoch: u64) -> bool {
        let pause = self.workspace.pause_switch();
        self.stopping.load(Ordering::SeqCst)
            || pause.is_paused()
            || pause.epoch() != epoch
            || self.workspace.relay(project).is_none()
    }
    /// A project's work: what an earlier run left is cleaned up, then jobs
    /// are taken from its databases, up to the project's number of jobs at
    /// once, each running in its own task, until none runs and none waits.
    /// The databases share the places (see [`next_database`]); one that does
    /// not answer is marked so and skipped: the others are still served.
    async fn cycle(self: &Arc<Self>, project: &str, name: &str, epoch: u64) -> Result<()> {
        let guard = OpenOptions::new()
            .create(true)
            .truncate(false)
            .write(true)
            .open(self.root.join(format!("{project}.lock")))
            .map_err(|e| e.to_string())?;
        match guard.try_lock() {
            Ok(()) => {}
            Err(std::fs::TryLockError::WouldBlock) => return Ok(()),
            Err(e) => return Err(e.to_string()),
        }
        // No job of this project runs now (they all run inside its cycle): a
        // journalled job's terminal is left from before, whatever its
        // database.
        for pending in self.store.pendings(project)? {
            self.recover_session(project, &pending).await?;
        }
        // Outcomes dropped with their database: their terminals are closed.
        self.store.clear_dropped(project)?;
        // Nor its job folders: remove them, once per start.
        if !self
            .swept
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .contains(project)
            && let Ok(root) = self.project_root(project)
        {
            commands::sweep(&root);
            self.swept
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .insert(project.to_owned());
        }
        let mut jobs = JoinSet::new();
        let looked = self.supervise(project, name, epoch, &mut jobs).await;
        // Each job ends on its own (its outcome, Pause, Stop, its deadline).
        while jobs.join_next().await.is_some() {}
        looked
    }
    /// Keep the project's places filled while jobs run: look again as soon
    /// as one ends, or every 5 seconds.
    async fn supervise(
        self: &Arc<Self>,
        project: &str,
        name: &str,
        epoch: u64,
        jobs: &mut JoinSet<()>,
    ) -> Result<()> {
        loop {
            while jobs.try_join_next().is_some() {}
            if self.cancelled(project, epoch) {
                return Ok(());
            }
            let took = self.fill(project, name, epoch, jobs).await?;
            if jobs.is_empty() && !took {
                return Ok(());
            }
            tokio::select! {
                _ = jobs.join_next(), if !jobs.is_empty() => {}
                () = sleep(Duration::from_secs(5)) => {}
            }
        }
    }
    /// One look at every database of `project`: outcomes waiting are written
    /// back, then jobs are taken while places are free, each from the
    /// database running the fewest, in turn after the one that gave the
    /// last job. Ok holds whether a job was taken.
    async fn fill(
        self: &Arc<Self>,
        project: &str,
        name: &str,
        epoch: u64,
        jobs: &mut JoinSet<()>,
    ) -> Result<bool> {
        let limit = self.store.parallel(project)?;
        let last = self
            .served
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
            .cloned();
        let order = rotation(self.store.links(project)?, last.as_deref());
        // The databases looked at for this round, and their connections.
        let mut done: HashSet<String> = HashSet::new();
        let mut conns: HashMap<String, tokio_postgres::Client> = HashMap::new();
        let running = self.running_tokens(project);
        for link in &order {
            let waiting = self
                .store
                .pending(project, &link.name)?
                .iter()
                .any(|p| !running.contains(&p.token));
            if !waiting {
                continue;
            }
            if self.cancelled(project, epoch) {
                return Ok(false);
            }
            let flushed = async {
                let mut db = self.open(project, link).await?;
                self.flush(&mut db, project, link, None).await?;
                Ok::<_, String>(db)
            }
            .await;
            match flushed {
                Ok(db) => {
                    conns.insert(link.name.clone(), db);
                }
                Err(e) => {
                    self.status(project, &link.name, Err(e));
                    done.insert(link.name.clone());
                }
            }
        }
        let mut took = false;
        loop {
            if self.cancelled(project, epoch) {
                break;
            }
            let counts = self.running_counts(project);
            if counts.values().sum::<usize>() >= limit as usize {
                break;
            }
            let Some(at) = next_database(&order, &counts, &done) else {
                break;
            };
            let link = &order[at];
            match self.take(project, name, link, epoch, &mut conns).await {
                Ok(Taken::Job(job, start, session, together)) => {
                    took = true;
                    let (s, project, link) = (self.clone(), project.to_owned(), link.clone());
                    jobs.spawn(async move {
                        s.run_job(&project, &link, *job, session, epoch, start, together)
                            .await;
                    });
                }
                Ok(Taken::Full) => break,
                Ok(Taken::Nothing) => {
                    done.insert(link.name.clone());
                    let message = match counts.get(&link.name) {
                        Some(&n) if n > 0 => format!("Running {n}; no other job waiting."),
                        _ => "No job waiting.".into(),
                    };
                    self.status(project, &link.name, Ok(message));
                }
                Ok(Taken::Gone) => {
                    done.insert(link.name.clone());
                }
                Ok(Taken::Cancelled) => break,
                Err(e) => {
                    done.insert(link.name.clone());
                    conns.remove(&link.name);
                    self.status(project, &link.name, Err(e));
                }
            }
        }
        Ok(took)
    }
    /// Connect to the database of `link`, within [`CONNECT_LIMIT`].
    async fn open(&self, project: &str, link: &store::Link) -> Result<tokio_postgres::Client> {
        timeout(
            CONNECT_LIMIT,
            self.connect(project, &link.database, None, &mut Vec::new()),
        )
        .await
        .map_err(|_| "Connecting to the database timed out.".to_owned())?
    }
    /// Take one job from the database of `link` and journal it, unless the
    /// project's places are all taken or the queue has that many jobs doing
    /// already. It is counted as running from here until its outcome is
    /// written back.
    async fn take(
        self: &Arc<Self>,
        project: &str,
        name: &str,
        link: &store::Link,
        epoch: u64,
        conns: &mut HashMap<String, tokio_postgres::Client>,
    ) -> Result<Taken> {
        if !conns.contains_key(&link.name) {
            let db = self.open(project, link).await?;
            conns.insert(link.name.clone(), db);
        }
        let Some(db) = conns.get_mut(&link.name) else {
            return Err("Lost the database connection.".into());
        };
        if self.cancelled(project, epoch) {
            return Ok(Taken::Cancelled);
        }
        // Several jobs at once: a queue made before is changed to allow
        // them, in a task of its own (building its index may take minutes);
        // until then it gives one job at a time, and the other queues are
        // served meanwhile.
        let several_ok =
            self.store.parallel(project)? <= 1 || self.allows_several(project, link).await;
        #[cfg(test)]
        self.hooks.admission(name);
        let start = Instant::now();
        // The link is checked, the number of jobs at once read, the job
        // claimed and journalled while no removal and no change of that
        // number can happen: a database removed since this round listed the
        // project's databases gives nothing, and a lower number holds from
        // the moment it is acknowledged.
        let claims = self.claims.read().await;
        if !self.still_linked(project, link)? {
            return Ok(Taken::Gone);
        }
        let cap = self.store.parallel(project)?;
        // Read under the same lock as the number of jobs: a change of it
        // waits for this admission, and holds for every one after it.
        let job_agent = self.store.job_agent(project)?;
        let running: usize = self.running_counts(project).values().sum();
        // A job admitted alone was told nothing of other jobs and may use
        // the project's files as its own: no other starts until it ends.
        if running >= cap as usize || self.running_alone(project) {
            return Ok(Taken::Full);
        }
        // Paused since this look began: nothing is taken by it. The claim
        // asks again once it has chosen a job, before committing.
        if self.cancelled(project, epoch) {
            return Ok(Taken::Cancelled);
        }
        let still = || !self.cancelled(project, epoch);
        let queue_cap = if several_ok { cap } else { 1 };
        let claimed = timeout(
            Duration::from_secs(6),
            db::claim(db, project, &link.queue, name, queue_cap, &still),
        )
        .await
        .map_err(|_| "Database claim timed out")??;
        let Some(job) = claimed else {
            return Ok(if self.cancelled(project, epoch) {
                Taken::Cancelled
            } else {
                Taken::Nothing
            });
        };
        self.served
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(project.to_owned(), link.name.clone());
        self.status(project, &link.name, Ok(format!("Running job {}.", job.id)));
        let session = format!("agentic_works_{}_{}", job.id, &job.token[..12]);
        self.store.begin(project, &link.name, &job, &session)?;
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(project.to_owned())
            .or_default()
            .push(Running {
                db: link.name.clone(),
                token: job.token.clone(),
                id: job.id,
                kind: job.kind.clone(),
                since: commands::now(),
                deadline: start + JOB_LIMIT,
                alone: cap == 1,
                agent: None,
                job_agent,
            });
        drop(claims);
        Ok(Taken::Job(Box::new(job), start, session, cap > 1))
    }
    /// Run one job taken from `link` to its end: its agent, its outcome
    /// journalled, its terminal closed and its folder removed, then the
    /// outcome written back to that database (or left journalled for the
    /// next look when it cannot be).
    #[allow(clippy::too_many_arguments)]
    async fn run_job(
        &self,
        project: &str,
        link: &store::Link,
        job: db::Job,
        session: String,
        epoch: u64,
        start: Instant,
        together: bool,
    ) {
        let deadline = start + JOB_LIMIT;
        let result = self
            .execute(
                project, &link.name, &job, &session, epoch, deadline, together,
            )
            .await;
        // Its run is over: its question and reply go, and an `ask` still on
        // its way while the terminal closes is refused (until the job leaves
        // the running jobs, below).
        self.questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .close(&job.token);
        self.woken();
        // A stand-in's outcome says who ran the job (ADR 0041).
        let agent = self.agent_of(project, &job.token);
        let reason_of = |e: String| failure_reason(e, agent.as_ref());
        let (status, value, reason) = match result {
            Ok(v) => ("done_from_agent", Some(v), None),
            Err(e) if Instant::now() >= deadline => ("timeout", None, Some(reason_of(e))),
            Err(e) => ("failed", None, Some(reason_of(e))),
        };
        let recorded = self.store.record(
            project,
            &job.token,
            status,
            value.as_ref(),
            reason.as_deref(),
        );
        // Its terminal is closed whether or not its outcome is still to be
        // written back (a forced removal drops that, not the terminal), and
        // nothing of the job is let go before it is: not its place, its
        // folder or its journal row. A close that fails is tried again;
        // when Connecto stops first, the journal row stays for the next start
        // to close it.
        loop {
            let closed = async {
                if let Some(journalled) = self
                    .store
                    .pendings(project)?
                    .into_iter()
                    .find(|p| p.token == job.token)
                {
                    self.recover_session(project, &journalled).await?;
                    if journalled.dropped {
                        self.store.clear(project, &job.token)?;
                    }
                }
                Ok::<_, String>(())
            }
            .await;
            match closed {
                Ok(()) => break,
                Err(e) => {
                    self.notice(project, format!("Job {}: {e} Trying again.", job.id));
                    if self.stopping.load(Ordering::SeqCst) {
                        return;
                    }
                    sleep(Duration::from_secs(1)).await;
                }
            }
        }
        if let Ok(root) = self.project_root(project) {
            commands::remove_job(&root, job.id, &job.token);
        }
        let written = match recorded {
            Err(e) => Err(e),
            _ if self.cancelled(project, epoch) => Ok(()),
            _ => {
                async {
                    let mut db = self.open(project, link).await?;
                    self.flush(&mut db, project, link, Some(&job.token)).await
                }
                .await
            }
        };
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .entry(project.to_owned())
            .or_default()
            .retain(|r| r.token != job.token);
        // No `ask` finds the job running now: its closed mark can go.
        self.questions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .release(&job.token);
        match written {
            Ok(()) => self.status(
                project,
                &link.name,
                Ok(ended_status(job.id, agent.as_ref())),
            ),
            Err(e) => self.notice(project, e),
        }
    }
    /// The agent the running job `token` runs on, once chosen.
    fn agent_of(&self, project: &str, token: &str) -> Option<Chosen> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
            .and_then(|list| list.iter().find(|r| r.token == token))
            .and_then(|r| r.agent.clone())
    }
    /// The project's agent for jobs read when the running job `token` was
    /// admitted.
    fn admitted_job_agent(&self, project: &str, token: &str) -> Result<JobAgent> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
            .and_then(|list| list.iter().find(|r| r.token == token))
            .map(|r| r.job_agent.clone())
            .ok_or_else(|| "The job is no longer running.".into())
    }
    /// Choose the agent that runs `job` and show it on the running job.
    /// A choice that depends on a check not over yet waits for it, up to
    /// [`CHOICE_WAIT`], while the job is not cancelled.
    async fn choose_agent(
        &self,
        project: &str,
        database: &str,
        job: &db::Job,
        deadline: Instant,
        cancel: &(dyn Fn() -> bool + Sync),
    ) -> Result<agents::Agent> {
        let setting =
            |s: &Option<String>| s.clone().filter(|v| !v.trim().is_empty() && v != "default");
        let until = (Instant::now() + CHOICE_WAIT).min(deadline);
        let root = self.project_root(project)?;
        let late = |what: &str| {
            format!(
                "Could not choose the job's agent: still not known after {} seconds ({what}).",
                CHOICE_WAIT.as_secs()
            )
        };
        // A job that names its agent keeps it, but not one the project's
        // Tools setting leaves out: it fails with the project's refusal,
        // naming the producer's choice, rather than run on another.
        if !agents::asks_default(&job.cli)
            && let Ok(tool) = provider(&job.cli)
            && !self.agents.check().allows(project, tool)
        {
            let allowed: Vec<&str> = connecto_core::ProjectTools::CHOICES
                .into_iter()
                .filter(|t| self.agents.check().allows(project, *t))
                .map(ToolKind::name)
                .collect();
            return Err(format!(
                "This job asks for {} (session_cli), which this project's Tools setting leaves out: it allows only {}. Change the job's session_cli, or turn {} on in the project's settings (Tools).",
                tool.name(),
                allowed.join(", "),
                tool.label()
            ));
        }
        // As it was when this job was admitted: a change since holds from
        // the next job on.
        let project_agent = self.admitted_job_agent(project, &job.token)?;
        let agent = loop {
            // Each candidate, in order and only as far as the choice goes,
            // looked at off the jobs' runtime; the wait for it ends when the
            // job is cancelled or its time is up, whatever the look does.
            let stop = || {
                if cancel() {
                    Some("Work cancelled before dispatch.".to_owned())
                } else if Instant::now() >= until {
                    Some(late("an agent is still being looked at"))
                } else {
                    None
                }
            };
            let ask = |tool: ToolKind| self.agents.ask(project, &root, tool, &stop);
            match agents::choose(
                &job.cli,
                setting(&job.model),
                setting(&job.effort),
                &project_agent,
                ask,
            )
            .await?
            {
                agents::Decision::Run(agent) => break agent,
                agents::Decision::Wait(what) => {
                    if cancel() {
                        return Err("Work cancelled before dispatch.".into());
                    }
                    if Instant::now() >= until {
                        return Err(late(&what));
                    }
                    self.status(
                        project,
                        database,
                        Ok(format!("Job {} waits to choose its agent: {what}.", job.id)),
                    );
                    sleep(Duration::from_millis(500)).await;
                }
            }
        };
        self.set_chosen(project, &job.token, &agent, AgentStage::Selected);
        self.status(
            project,
            database,
            Ok(format!("Job {}: selected {}.", job.id, agent.shown())),
        );
        Ok(agent)
    }
    /// Record the running job `token`'s agent, and how far it has come.
    fn set_chosen(&self, project: &str, token: &str, agent: &agents::Agent, stage: AgentStage) {
        if let Some(running) = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(project)
            .and_then(|list| list.iter_mut().find(|r| r.token == token))
        {
            running.agent = Some(Chosen {
                agent: agent.clone(),
                stage,
                stopped_at_start: false,
            });
        }
    }
    /// The running job `token`'s terminal was seen to stop at its start (its
    /// checks failed, or it needs a login): its program never ran.
    fn stopped_at_start(&self, project: &str, token: &str) {
        if let Some(chosen) = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get_mut(project)
            .and_then(|list| list.iter_mut().find(|r| r.token == token))
            .and_then(|r| r.agent.as_mut())
            .filter(|c| c.stage == AgentStage::Dispatched)
        {
            chosen.stopped_at_start = true;
        }
    }
    /// The tokens of the project's jobs running now.
    fn running_tokens(&self, project: &str) -> HashSet<String> {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
            .map(|list| list.iter().map(|r| r.token.clone()).collect())
            .unwrap_or_default()
    }
    /// Whether the queue of `link` allows several jobs doing. The first time
    /// it is asked, a task changes a queue made before (see
    /// [`db::allow_several`]), on a connection of its own; this waits for
    /// it a moment only, and the task goes on alone. It is called off when
    /// Connecto closes or the database is removed. A change that fails or is
    /// left to another Connecto is tried again at a later look.
    async fn allows_several(self: &Arc<Self>, project: &str, link: &store::Link) -> bool {
        let key = (project.to_owned(), link.queue.clone());
        match self
            .migrations
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&key)
        {
            Some(Migration::Ready) => return true,
            Some(Migration::Running { .. }) => return false,
            None => {}
        }
        #[cfg(test)]
        self.hooks.before_migration(&link.queue);
        {
            // Started only while the database is linked, and registered,
            // with its task, before a removal can look for it: a removal
            // waits for this, or this sees the database gone.
            let _claims = self.claims.read().await;
            if !self.still_linked(project, link).unwrap_or(false) {
                return false;
            }
            let mut known = self.migrations.lock().unwrap_or_else(|e| e.into_inner());
            if known.contains_key(&key) {
                return false;
            }
            let cancel = Arc::new(AtomicBool::new(false));
            let (report, ended) = tokio::sync::watch::channel(None);
            let task = tokio::spawn(Arc::clone(self).migrate(
                project.to_owned(),
                link.clone(),
                Arc::clone(&cancel),
                report,
            ));
            known.insert(
                key.clone(),
                Migration::Running {
                    cancel,
                    ended,
                    _task: task,
                },
            );
        }
        // A queue made since, or quickly changed: no look goes by at one
        // job at a time.
        let until = Instant::now() + Duration::from_secs(2);
        while Instant::now() < until {
            match self
                .migrations
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .get(&key)
            {
                Some(Migration::Ready) => return true,
                None => return false,
                Some(Migration::Running { .. }) => {}
            }
            sleep(Duration::from_millis(50)).await;
        }
        false
    }
    /// The change of `link`'s queue for several jobs at once (the task
    /// [`Self::allows_several`] starts). Called off (`cancel`), it stops the
    /// statement under way on the server with that connection's own
    /// cancellation key, sent where the connection was made, and tries
    /// again until that request is delivered (a removal waits for it, and
    /// fails while it is not). `report` says how the task ended. Its entry
    /// is let go only once nothing of it may still run.
    async fn migrate(
        self: Arc<Self>,
        project: String,
        link: store::Link,
        cancel: Arc<AtomicBool>,
        report: tokio::sync::watch::Sender<Option<Result<()>>>,
    ) {
        let key = (project.clone(), link.queue.clone());
        let canceller: Mutex<Option<Canceller>> = Mutex::new(None);
        let lost = AtomicBool::new(false);
        let change = async {
            let (mut db, can) = timeout(
                CONNECT_LIMIT,
                self.connect_routed(&project, &link.database, None, &mut Vec::new()),
            )
            .await
            .map_err(|_| "Connecting to the database timed out.".to_owned())??;
            *canceller.lock().unwrap_or_else(|e| e.into_inner()) = Some(can);
            // The old index goes only while the database is still this
            // project's, and the change was not called off.
            let keep = || {
                !cancel.load(Ordering::SeqCst)
                    && self.still_linked(&project, &link).unwrap_or(false)
            };
            let changed = db::allow_several(&mut db, &project, &link.queue, &keep).await;
            // Failed with the connection gone: no answer from the server,
            // whose session may still be changing the queue.
            if changed.is_err() && db.is_closed() {
                lost.store(true, Ordering::SeqCst);
            }
            changed
        };
        // Called off (the database removed, Connecto closing), or taking
        // too long.
        let deadline = Instant::now() + db::MIGRATION_LIMIT + Duration::from_secs(10);
        let called_off = async {
            while !self.stopping.load(Ordering::SeqCst)
                && !cancel.load(Ordering::SeqCst)
                && Instant::now() < deadline
            {
                sleep(Duration::from_millis(100)).await;
            }
        };
        // The change is kept, not dropped, while it is stopped: only its own
        // statement ending says the server stopped it. A cancellation sent
        // says nothing (a proxy on the way may drop it).
        let mut change = std::pin::pin!(change);
        let first = tokio::select! {
            changed = &mut change => Some(changed),
            () = called_off => None,
        };
        let stopped = first.is_none();
        let outcome = match first {
            Some(changed) => changed,
            None => loop {
                if self.stopping.load(Ordering::SeqCst) {
                    // Closing: the connection goes with Connecto, and the
                    // server stops the statement then.
                    return;
                }
                let can = canceller.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let sent = match can {
                    Some(can) => can.cancel().await,
                    // Still connecting: nothing runs yet to be cancelled.
                    None => Ok(()),
                };
                match timeout(Duration::from_secs(3), &mut change).await {
                    Ok(changed) => break changed,
                    Err(_) => {
                        let why = match sent {
                            Err(e) => format!("its server could not be told to stop it ({e})"),
                            Ok(()) => "it has not stopped on the server yet".to_owned(),
                        };
                        let _ = report.send(Some(Err(format!(
                            "Connecto is changing its queue for several jobs at once, and {why}."
                        ))));
                    }
                }
            },
        };
        // The connection was lost mid-change: the server may still run it.
        // It is over only once a fresh connection to the same queue can take
        // the lock the change holds for its whole session; until then it
        // stays tracked (a removal fails), and is told to stop meanwhile.
        if lost.load(Ordering::SeqCst) {
            loop {
                if self.stopping.load(Ordering::SeqCst) {
                    return;
                }
                let can = canceller.lock().unwrap_or_else(|e| e.into_inner()).clone();
                if let Some(can) = can {
                    let _ = can.cancel().await;
                }
                let over = async {
                    let mut db = self.open(&project, &link).await?;
                    db::change_over(&mut db, &project, &link.queue).await
                };
                match timeout(CONNECT_LIMIT, over).await {
                    Ok(Ok(true)) => break,
                    Ok(Ok(false)) => {
                        let _ = report.send(Some(Err(
                            "Connecto lost its connection while changing its queue for several jobs at once, and the server is still changing it.".into(),
                        )));
                    }
                    Ok(Err(e)) => {
                        let _ = report.send(Some(Err(format!(
                            "Connecto lost its connection while changing its queue for several jobs at once, and cannot make sure the server stopped ({e})."
                        ))));
                    }
                    Err(_) => {
                        let _ = report.send(Some(Err(
                            "Connecto lost its connection while changing its queue for several jobs at once, and cannot reach it again to make sure the server stopped.".into(),
                        )));
                    }
                }
                sleep(Duration::from_secs(3)).await;
            }
        }
        {
            let mut known = self.migrations.lock().unwrap_or_else(|e| e.into_inner());
            match &outcome {
                Ok(true) if !cancel.load(Ordering::SeqCst) => {
                    known.insert(key, Migration::Ready);
                }
                _ => {
                    known.remove(&key);
                }
            }
        }
        let _ = report.send(Some(Ok(())));
        if let Err(e) = outcome
            && !stopped
        {
            self.notice(
                &project,
                format!(
                    "The database {} cannot take several jobs at once yet ({e}): it gives one at a time meanwhile.",
                    link.name
                ),
            );
        }
    }
    /// Call off the change of `queue` under way and wait for it: Ok once
    /// Connecto changes nothing more in it (its statement stopped on the
    /// server); Err when that could not be made sure of in time, the
    /// change still tracked and its stopping still tried.
    async fn end_migration(&self, project: &str, queue: &str) -> Result<()> {
        let key = (project.to_owned(), queue.to_owned());
        let ended = {
            let mut known = self.migrations.lock().unwrap_or_else(|e| e.into_inner());
            match known.get(&key) {
                Some(Migration::Running { cancel, ended, .. }) => {
                    cancel.store(true, Ordering::SeqCst);
                    ended.clone()
                }
                Some(Migration::Ready) => {
                    known.remove(&key);
                    return Ok(());
                }
                None => return Ok(()),
            }
        };
        let mut ended = ended;
        let waited = timeout(Duration::from_secs(12), async {
            loop {
                let now = ended.borrow_and_update().clone();
                if let Some(Ok(())) = now {
                    return Ok(());
                }
                // Not ended, or its cancellation not delivered yet (it is
                // tried again): wait for news.
                if ended.changed().await.is_err() {
                    return now.and_then(Result::err).map_or(Ok(()), Err);
                }
            }
        })
        .await;
        match waited {
            Ok(Ok(())) => Ok(()),
            Ok(Err(e)) => Err(e),
            Err(_) => Err(ended
                .borrow()
                .clone()
                .and_then(Result::err)
                .unwrap_or_else(|| {
                    "Connecto could not stop the change of its queue in time.".to_owned()
                })),
        }
    }
    /// Whether a job admitted with one job at a time runs now.
    fn running_alone(&self, project: &str) -> bool {
        self.running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
            .is_some_and(|list| list.iter().any(|r| r.alone))
    }
    /// How many jobs of each database of the project run now.
    fn running_counts(&self, project: &str) -> HashMap<String, usize> {
        let mut counts = HashMap::new();
        if let Some(list) = self
            .running
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(project)
        {
            for r in list {
                *counts.entry(r.db.clone()).or_default() += 1;
            }
        }
        counts
    }
    /// Whether `link` is still the project's database of that name, with
    /// the same queue.
    fn still_linked(&self, project: &str, link: &store::Link) -> Result<bool> {
        Ok(self
            .store
            .linked(project, &link.name)?
            .is_some_and(|(_, queue)| queue == link.queue))
    }
    /// Write the outcomes journalled for `link` back to its database: the
    /// job `token`'s when given, otherwise every one of a job no longer
    /// running. Nothing is written once the database was removed.
    async fn flush(
        &self,
        db: &mut tokio_postgres::Client,
        project: &str,
        link: &store::Link,
        token: Option<&str>,
    ) -> Result<()> {
        let _claims = self.claims.read().await;
        if !self.still_linked(project, link)? {
            return Ok(());
        }
        let running = self.running_tokens(project);
        for pending in self.store.pending(project, &link.name)? {
            let wanted = token.map_or(!running.contains(&pending.token), |t| t == pending.token);
            if !wanted {
                continue;
            }
            timeout(
                Duration::from_secs(6),
                db::finish(
                    db,
                    project,
                    &link.queue,
                    pending.id,
                    &pending.token,
                    &pending.status,
                    pending.value.as_ref(),
                    pending.reason.as_deref(),
                ),
            )
            .await
            .map_err(|_| "Outcome retained locally after PostgreSQL timeout")??;
            self.store.clear(project, &pending.token)?;
        }
        Ok(())
    }
    async fn recover_session(&self, project: &str, pending: &store::Pending) -> Result<()> {
        #[cfg(test)]
        if self.hooks.close_fails() {
            return Err("Work session cleanup is pending (test).".into());
        }
        let Some(relay) = self.workspace.relay(project) else {
            return Err("Waiting for project to reopen for cleanup.".into());
        };
        let control = relay.terminals();
        let view = control.view();
        if let Some(entry) = view.terminals.iter().find(|e| {
            pending
                .terminal
                .as_ref()
                .map_or_else(|| e.name.as_ref() == Some(&pending.name), |id| id == &e.id)
        }) {
            close(&*control, &entry.id).await?;
        }
        Ok(())
    }
    /// Whether `caller` runs a job's agent, or one a job's agent started:
    /// as Connecto marked the terminal when it made it, or the terminal the
    /// journal names for the job running now.
    fn job_lineage(&self, caller: &connecto_core::agentic_works::WorksCaller) -> bool {
        caller.job
            || self
                .store
                .pendings(&caller.project_id)
                .map_or(true, |list| {
                    list.iter()
                        .any(|p| p.terminal.as_deref() == Some(caller.terminal.as_str()))
                })
    }
    #[allow(clippy::too_many_arguments)]
    async fn execute(
        &self,
        project: &str,
        database: &str,
        job: &db::Job,
        name: &str,
        epoch: u64,
        deadline: Instant,
        together: bool,
    ) -> Result<Value> {
        let (def, databases) = self.store.published(project, &job.kind)?;
        if let Some(list) = databases
            && !list.iter().any(|d| d == database)
        {
            return Err(format!(
                "The work type {} is not published for the database {database}.",
                job.kind
            ));
        }
        let checked = check(&def)?;
        // Pause, Stop, a closed project, or the job's database removed by
        // force: the job ends wherever it is, prepare and setup included.
        let cancel = || {
            self.cancelled(project, epoch)
                || self.store.dropped(project, &job.token).unwrap_or(true)
        };
        // Chosen before prepare and setup run: a job no agent can run ends
        // at once, saying why.
        let agent = self
            .choose_agent(project, database, job, deadline, &cancel)
            .await?;
        // Admitted with several jobs at once, it has a folder of its own,
        // and its agent is told so.
        let dir = if checked.prepare.is_some() || together {
            if cancel() {
                return Err("Work cancelled before dispatch.".into());
            }
            Some(commands::job_dir(
                &self.project_root(project)?,
                job.id,
                &job.token,
            )?)
        } else {
            None
        };
        let prepared = match (&checked.prepare, &dir) {
            (Some(cmd), Some(dir)) => {
                let root = self.project_root(project)?;
                let setup = checked.setup.as_ref().map(|setup| {
                    let (root, cancel) = (&root, &cancel);
                    move || {
                        self.recorded_setup(
                            project,
                            &job.kind,
                            root,
                            setup,
                            Some(deadline),
                            Some(cancel),
                        )
                    }
                });
                self.runner(project, &job.kind, &root)
                    .prepare(cmd, setup, job, dir, deadline, &cancel)
                    .await?
            }
            _ => String::new(),
        };
        let validator = checked.validator;
        let mut prompt = format!(
            "{}\n\nReturn your final answer as a single JSON value conforming to this schema:\n{}\n\n{ASK_NOTE}",
            fill(&def.instructions, &job.prompt, &prepared),
            def.schema
        );
        if together && let Some(dir) = &dir {
            prompt.push_str(&format!(
                "\n\nThis is job {} ({}). Other jobs of this project run at the same time, in the same project folder: keep the files this job writes in {}, which is this job's alone and is deleted when it ends. Do not change files that other jobs may use.",
                job.id,
                job.kind,
                dir.display()
            ));
        }
        let relay = self.workspace.relay(project).ok_or("Project closed")?;
        // The last check, the agent's start and its journal entry happen
        // while no database can be removed: a removal waits until the
        // terminal is journalled, and then stops it.
        let claims = self.claims.read().await;
        if cancel() {
            return Err("Work cancelled before dispatch.".into());
        }
        #[cfg(test)]
        self.hooks.before_dispatch();
        let control = relay.terminals();
        let id = control.add_work(
            agent.tool,
            name.to_owned(),
            prompt,
            agent.model.clone(),
            agent.effort.clone(),
            epoch,
        )?;
        self.set_chosen(project, &job.token, &agent, AgentStage::Dispatched);
        self.status(
            project,
            database,
            Ok(format!("Job {}: dispatched to {}.", job.id, agent.shown())),
        );
        let mut running = false;
        #[cfg(test)]
        self.hooks.launched(&job.token);
        #[cfg(test)]
        {
            let delay = self
                .hooks
                .attach_delay
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .take();
            if let Some(delay) = delay {
                sleep(delay).await;
            }
        }
        if let Err(e) = self.store.attach(project, &job.token, &id) {
            drop(claims);
            close(&*control, &id).await?;
            return Err(e);
        }
        drop(claims);
        loop {
            if Instant::now() >= deadline {
                break Err("Claim expired after 30 minutes.".into());
            }
            if self.cancelled(project, epoch) {
                break Err("Work cancelled by Stop, Pause, or project close.".into());
            }
            if self.store.dropped(project, &job.token)? {
                break Err(format!("The database {database} was removed."));
            }
            let view = control.view();
            let Some(entry) = view.terminals.iter().find(|e| e.id == id) else {
                break Err("Work session closed.".into());
            };
            if relay.terminal_event(&id) == Some("interrupted") {
                break Err("Provider turn interrupted.".into());
            }
            // The agent's program runs once the terminal is past its start
            // (its checks and login): only then did the job run on it.
            if !running && program_runs(entry.state, entry.finished_turns) {
                running = true;
                self.set_chosen(project, &job.token, &agent, AgentStage::Running);
                self.status(
                    project,
                    database,
                    Ok(format!("Running job {} on {}.", job.id, agent.shown())),
                );
            }
            match entry.state {
                TermActivity::NeedsUser | TermActivity::NeedsLogin => {
                    if entry.state == TermActivity::NeedsLogin {
                        self.stopped_at_start(project, &job.token);
                    }
                    break Err("The provider requires user input, approval or login.".into());
                }
                TermActivity::Failed => {
                    self.stopped_at_start(project, &job.token);
                    let reason = entry
                        .control
                        .as_ref()
                        .and_then(|c| match c.stage() {
                            connecto_core::PaneStage::Failed { reason } => Some(reason),
                            _ => None,
                        })
                        .unwrap_or_else(|| "Provider launch failed.".into());
                    break Err(connecto_redact::sanitize(&reason));
                }
                TermActivity::Interrupted | TermActivity::Exited => {
                    break Err(format!("Work session ended: {:?}", entry.state));
                }
                _ => {}
            }
            let idle = !entry.in_turn && entry.background.is_empty();
            // The reply to the agent's question (the answer, or that none
            // came) is handed over only while it is idle, as its next prompt:
            // its final JSON is then read from a turn after that one.
            if idle {
                let reply = self
                    .questions
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .take_reply(&job.token, Instant::now());
                if let Some(notify) = reply {
                    notify(questions::READY.to_owned());
                    self.woken();
                    sleep(Duration::from_millis(150)).await;
                    continue;
                }
            }
            // Once the job asked, its end is bound to its agent's reading of
            // the reply (`works answer`), from the terminal's own counters:
            // until then no turn ends the job, whatever JSON it ended with
            // (the turn that asked may end with a guess, even after the user
            // answered); after it, only a reply kept since.
            let phase = self
                .questions
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .phase(&job.token);
            let read = match phase {
                questions::Phase::Waiting => {
                    sleep(Duration::from_millis(150)).await;
                    continue;
                }
                questions::Phase::None => None,
                questions::Phase::Read(mark) => Some(mark),
            };
            // After the read, the terminal's own record of its turns alone
            // decides: the turns it ended, each with the reply it ended with,
            // read together. A turn is counted only once its background work
            // is over, and ends with what its last stop kept, or none: a
            // reply a stop kept while background work ran (interim JSON) is
            // the turn's only if no later stop of it replaced it, even with
            // nothing.
            let text = if let Some(mark) = read {
                let (turns, reply) = relay.terminal_last_turn(&id).unwrap_or((0, None));
                if turns <= mark.turns {
                    // The turn that read is still on, or its background work.
                    sleep(Duration::from_millis(150)).await;
                    continue;
                }
                // A counted turn that ended with no reply of its own, or with
                // one kept before the read, gives none.
                reply
                    .filter(|(number, _)| *number > mark.replies)
                    .map(|(_, text)| text)
            } else if entry.finished_turns > 0 && idle {
                relay.terminal_reply(&id)
            } else {
                sleep(Duration::from_millis(150)).await;
                continue;
            };
            break text
                .ok_or_else(|| "Provider returned no final assistant text.".to_owned())
                .and_then(|raw| {
                    let v = parse_json(&raw)?;
                    validator.validate(&v).map_err(|e| {
                        format!(
                            "Final JSON failed schema validation at {}",
                            e.instance_path()
                        )
                    })?;
                    Ok(v)
                });
        }
    }
}
/// A job's time, from its claim: the queue's own expiry too.
const JOB_LIMIT: Duration = Duration::from_secs(30 * 60);
/// A job's question waits for an answer until this long before its
/// deadline: then its agent is told none came, and has this long to end
/// with its JSON.
const ANSWER_MARGIN: Duration = Duration::from_secs(5 * 60);

/// The longest a job's terminal waits, in `works ask` or `answer`, for the
/// journal to name it (written just after its agent starts). Well inside
/// the bridge's 25 seconds.
const ATTACH_WAIT: Duration = Duration::from_secs(10);

/// What a job's agent is told it may do when it is stuck (ADR 0040).
const ASK_NOTE: &str = "If you hit a problem or miss information this job needs and cannot find it yourself, you may ask the user one question at a time: run `connecto works ask '<your question>'` (or give the question on stdin with a heredoc), saying exactly what you need and why, then end your turn without a final answer. Do not wait, sleep or poll: a [connecto notice] wakes you when the reply is in; then run `connecto works answer` to read it, and continue. If it says no answer will come, end the job anyway. The 30 minutes of this job include the wait. However the job goes, always end it with the final JSON described above, and nothing else.";

/// Why `connecto works ask` or `answer` is refused to a terminal that runs
/// no job.
const NOT_A_JOB: &str = "Only an Agentic Works job's own agent can use `connecto works ask` and `answer`; this terminal runs no job now.";

/// The longest a database may take to answer a connection, Cloud SQL's
/// token and certificate included: one that does not answer is skipped.
const CONNECT_LIMIT: Duration = Duration::from_secs(20);

/// Whether a job must end now (a job's setup checks it as it runs).
type Stop<'a> = &'a (dyn Fn() -> bool + Send + Sync);

/// What one database gave when asked for a job.
enum Taken {
    /// A job, claimed and journalled: when it was taken, its terminal's
    /// name, and whether it was admitted with several jobs at once.
    Job(Box<db::Job>, Instant, String, bool),
    Nothing,
    /// Every place of the project is taken.
    Full,
    /// The database was removed since the round began.
    Gone,
    /// Pause, Stop or a closed project: look no further.
    Cancelled,
}

/// A queue's change for several jobs at once.
enum Migration {
    /// Under way: called off with `cancel`; `ended` says how its task ended
    /// (or that stopping it on the server failed, and is tried again).
    Running {
        cancel: Arc<AtomicBool>,
        ended: tokio::sync::watch::Receiver<Option<Result<()>>>,
        _task: tokio::task::JoinHandle<()>,
    },
    Ready,
}

/// What cancels the statement under way on one connection: its own
/// cancellation key (the server's process and a secret only that
/// connection was given), sent to the server the connection was made to.
/// A server that does not have that process with that secret ignores it.
#[derive(Clone)]
enum Canceller {
    /// A connection string's connection: to the address it was made to.
    Direct(tokio_postgres::CancelToken),
    /// Cloud SQL: through the same route, the same TLS.
    Routed(tokio_postgres::CancelToken, cloudsql::Route),
}

impl Canceller {
    async fn cancel(&self) -> Result<()> {
        let sent = match self {
            Self::Direct(token) => {
                let tls = native_tls::TlsConnector::new()
                    .map_err(|_| "Cannot initialize PostgreSQL TLS".to_owned())?;
                timeout(
                    Duration::from_secs(5),
                    token.cancel_query(postgres_native_tls::MakeTlsConnector::new(tls)),
                )
                .await
            }
            Self::Routed(token, route) => {
                let stream = route.stream().await?;
                timeout(
                    Duration::from_secs(5),
                    token.cancel_query_raw(stream, tokio_postgres::NoTls),
                )
                .await
            }
        };
        sent.map_err(|_| "The database server did not take the cancellation in time.".to_owned())?
            .map_err(|_| "The database server could not be reached to cancel.".to_owned())
    }
}

/// A job running now, as the project's view lists it.
struct Running {
    db: String,
    token: String,
    id: i64,
    kind: String,
    since: String,
    /// Its 30 minutes end then.
    deadline: Instant,
    /// Admitted with one job at a time: no folder of its own, and no word
    /// of other jobs in its prompt.
    alone: bool,
    /// The agent it runs on, once chosen (ADR 0041).
    agent: Option<Chosen>,
    /// The project's agent for jobs when it was admitted: its agent is
    /// chosen with this, whatever the setting says by then.
    job_agent: JobAgent,
}

/// Whether a job's terminal in `state` (with `finished_turns`) runs the
/// agent's program: past its start, its checks and its login.
fn program_runs(state: TermActivity, finished_turns: u64) -> bool {
    finished_turns > 0
        || matches!(
            state,
            TermActivity::Working
                | TermActivity::NeedsUser
                | TermActivity::Idle
                | TermActivity::Interrupted
                | TermActivity::Done
        )
}

/// How far a job's agent was seen to come: "Ran on X", "Dispatched to X",
/// "Selected X", and whether it was seen not to start: before its terminal
/// was made, or its terminal seen to stop at its start (failed checks, a
/// login). Seen only exited with no turn ended, it may have run between two
/// looks: no more than "Dispatched to" is said. The job's failure reason and
/// the database's status both say it from this.
fn came(chosen: &Chosen) -> (String, bool) {
    let agent = chosen.agent.shown();
    match chosen.stage {
        AgentStage::Running => (format!("Ran on {agent}"), false),
        AgentStage::Dispatched => (format!("Dispatched to {agent}"), chosen.stopped_at_start),
        AgentStage::Selected => (format!("Selected {agent}"), true),
    }
}

/// Why a job failed, and for a stand-in how far its agent came ([`came`]).
fn failure_reason(reason: String, chosen: Option<&Chosen>) -> String {
    let Some(c) = chosen.filter(|c| c.agent.stand_in || c.agent.for_project) else {
        return reason;
    };
    match came(c) {
        (said, true) => format!("{reason} ({said}; it had not started.)"),
        (said, false) => format!("{reason} ({said}.)"),
    }
}

/// The database's status when a job has ended, from the same evidence as
/// its failure reason ([`came`]).
fn ended_status(id: i64, chosen: Option<&Chosen>) -> String {
    let Some(c) = chosen else {
        return format!("Ran job {id}.");
    };
    match (c.stage, came(c)) {
        (AgentStage::Running, _) => format!("Ran job {id} on {}.", c.agent.shown()),
        (_, (said, true)) => format!("Job {id} ended before its agent started ({said})."),
        (_, (said, false)) => format!("Job {id} ended ({said})."),
    }
}

/// A running job's agent, and how far it has come.
#[derive(Clone)]
struct Chosen {
    agent: agents::Agent,
    stage: AgentStage,
    /// Dispatched, and seen to stop at its start (failed checks, a login).
    stopped_at_start: bool,
}

/// The longest a job waits for the checks its agent's choice depends on
/// (ADR 0041): an agent still being checked, Pi's providers being asked.
const CHOICE_WAIT: Duration = Duration::from_secs(120);

/// The database to take the next job from: among those not yet looked at
/// in this round, the one running the fewest jobs, the first in `order` (the
/// turn) among equals. With one place this is simply the next in turn.
fn next_database(
    order: &[store::Link],
    running: &HashMap<String, usize>,
    done: &HashSet<String>,
) -> Option<usize> {
    order
        .iter()
        .enumerate()
        .filter(|(_, l)| !done.contains(&l.name))
        .min_by_key(|(at, l)| (running.get(&l.name).copied().unwrap_or(0), *at))
        .map(|(at, _)| at)
}

/// The project's databases in the order to look at them: those after
/// `last` (the one that gave the last job) first, then the rest, `last`
/// itself at the end.
fn rotation(mut links: Vec<store::Link>, last: Option<&str>) -> Vec<store::Link> {
    if let Some(at) = last.and_then(|last| links.iter().position(|l| l.name == last)) {
        links.rotate_left(at + 1);
    }
    links
}

impl State {
    /// The databases a type is published for: None for every one. Each
    /// name must be a linked database.
    fn published_to(
        &self,
        project: &str,
        databases: Option<Vec<String>>,
    ) -> Result<Option<Vec<String>>> {
        let Some(list) = databases else {
            return Ok(None);
        };
        let linked: Vec<String> = self
            .store
            .links(project)?
            .into_iter()
            .map(|l| l.name)
            .collect();
        let mut out: Vec<String> = Vec::new();
        for name in list {
            let name = database_name(&name)?;
            if !linked.contains(&name) {
                return Err(format!(
                    "This project has no database named {name}. Its databases: {}.",
                    if linked.is_empty() {
                        "none".to_owned()
                    } else {
                        linked.join(", ")
                    }
                ));
            }
            if !out.contains(&name) {
                out.push(name);
            }
        }
        if out.is_empty() {
            return Err("Name at least one database to publish the work type for.".into());
        }
        out.sort();
        Ok(Some(out))
    }
}

fn already_running(kind: &str) -> String {
    format!("Setup for {kind} is already running.")
}

/// One run of a type's setup: its command, and how it ended. Whoever
/// waited for it keeps that, whatever runs next.
struct SetupRun {
    command: commands::Command,
    done: Mutex<Option<SetupEnd>>,
}
impl SetupRun {
    fn result(&self) -> Option<SetupEnd> {
        self.done.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }
}

/// How a setup run ended.
#[derive(Clone, Debug)]
enum SetupEnd {
    /// It ran to its end: the command's result.
    Ran(Result<()>),
    /// Its own job stopped it first (Pause, its database removed, its
    /// deadline, Connecto closing): nothing is known of the command.
    Abandoned,
}

/// The right to run a (project, work type) setup: gives its result to
/// those waiting, and is released on drop.
struct SetupClaim<'a> {
    setups: &'a Mutex<HashMap<(String, String), Arc<SetupRun>>>,
    key: (String, String),
    run: Arc<SetupRun>,
}
impl SetupClaim<'_> {
    fn finish(&self, result: &Result<()>) {
        *self.run.done.lock().unwrap_or_else(|e| e.into_inner()) =
            Some(SetupEnd::Ran(result.clone()));
    }
}
impl Drop for SetupClaim<'_> {
    fn drop(&mut self) {
        {
            let mut done = self.run.done.lock().unwrap_or_else(|e| e.into_inner());
            if done.is_none() {
                *done = Some(SetupEnd::Abandoned);
            }
        }
        self.setups
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(&self.key);
    }
}
async fn close(control: &dyn connecto_core::TerminalsControl, id: &str) -> Result<()> {
    for _ in 0..30 {
        if !control.view().terminals.iter().any(|e| e.id == id) {
            return Ok(());
        }
        control.close(id)?;
        sleep(Duration::from_millis(100)).await;
    }
    Err("Work session cleanup is pending.".into())
}
