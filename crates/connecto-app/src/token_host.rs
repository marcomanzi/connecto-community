//! Agent Token Optimization behind the Tokens tab of Project Settings
//! (docs/specs/token-optimization.md). Every command returns at once; the
//! reading and the study run on a thread of their own, and the view is an
//! `Arc` swapped under a short lock.
//!
//! The cuts are the project setting `token_optimization`, read by each Claude
//! terminal's launch (`connecto_session::launch`): nothing here touches a
//! running terminal or a file of the user's.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

use connecto_audit::{Audit, Record};
use connecto_core::WorkspaceControl;
use connecto_core::tokens::{
    AUTOCOMPACT_CHOICES, ProjectTokenView, StudyState, TokenControl, TokenReport, TokenSettings,
    TokenView,
};
use connecto_session::Wake;
use connecto_session::launch::Layout;
use connecto_session::project_settings;
use connecto_session::token_audit::{self, Session};
use connecto_session::utility_run::{UtilityJob, UtilityPurpose, UtilityRunner};
use connecto_workspace::Workspace;

/// How long the study's utility session may take.
const STUDY_LIMIT: Duration = Duration::from_secs(300);

/// What the host needs from the rest of the app; faked in tests.
pub trait TokenEnv: Send + Sync {
    /// The open project's folder.
    fn project_path(&self, project_id: &str) -> Option<PathBuf>;
    /// The Claude profile folders whose transcripts belong to the project.
    fn profiles(&self, project: &Path) -> Vec<PathBuf>;
    /// One utility session with `prompt`; its reply text, or why none.
    fn study(&self, project_id: &str, project: &Path, prompt: String) -> Result<String, String>;
}

/// The app's own environment: the workspace and a utility runner.
pub struct AppEnv {
    layout: Layout,
    workspace: Arc<Workspace>,
    runner: Arc<UtilityRunner>,
}

impl TokenEnv for AppEnv {
    fn project_path(&self, project_id: &str) -> Option<PathBuf> {
        self.workspace
            .view()
            .projects
            .iter()
            .find(|p| p.id == project_id)
            .map(|p| PathBuf::from(&p.path))
    }

    fn profiles(&self, project: &Path) -> Vec<PathBuf> {
        connecto_session::launch::claude_profiles(&self.layout, project)
    }

    fn study(&self, project_id: &str, project: &Path, prompt: String) -> Result<String, String> {
        // The model chosen for terminal titles (or the utility default).
        let choice = self.workspace.utility_settings().choice();
        let job = UtilityJob {
            project_id: project_id.to_owned(),
            project: project.to_owned(),
            choice,
            prompt,
            purpose: UtilityPurpose::TokenStudy,
            label: "token study".to_owned(),
        };
        let cancel = AtomicBool::new(false);
        self.runner
            .run(&job, Instant::now() + STUDY_LIMIT, &cancel)
            .map(|reply| reply.text)
            .map_err(|err| err.to_string())
    }
}

/// Start the host for the app.
pub fn start(
    layout: &Layout,
    workspace: &Arc<Workspace>,
    audit: Audit,
    wake: &Wake,
) -> TokenHandle {
    let runner = Arc::new(UtilityRunner::new(layout.clone(), Arc::clone(wake)));
    let stop = Arc::clone(&runner);
    workspace.on_stop_everything(Arc::new(move || stop.cancel_all()));
    TokenHandle(TokenHost::new(
        layout.data_root.clone(),
        Arc::new(AppEnv {
            layout: layout.clone(),
            workspace: Arc::clone(workspace),
            runner,
        }),
        Some(audit),
        Arc::clone(wake),
    ))
}

#[derive(Default)]
struct State {
    projects: HashMap<String, ProjectTokenView>,
    /// The sessions the last analysis read, for the after figures.
    sessions: HashMap<String, Arc<Vec<Session>>>,
}

pub struct TokenHost {
    data_root: PathBuf,
    env: Arc<dyn TokenEnv>,
    audit: Option<Audit>,
    wake: Wake,
    state: Mutex<State>,
    view: Mutex<Arc<TokenView>>,
}

fn lock<T>(m: &Mutex<T>) -> MutexGuard<'_, T> {
    m.lock().unwrap_or_else(PoisonError::into_inner)
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

impl TokenHost {
    pub fn new(
        data_root: PathBuf,
        env: Arc<dyn TokenEnv>,
        audit: Option<Audit>,
        wake: Wake,
    ) -> Arc<Self> {
        Arc::new(Self {
            data_root,
            env,
            audit,
            wake,
            state: Mutex::new(State::default()),
            view: Mutex::new(Arc::new(TokenView {
                projects: Vec::new(),
                available: true,
            })),
        })
    }

    /// Change the state under one lock, then publish the view.
    fn with_state<R>(&self, change: impl FnOnce(&mut State) -> R) -> R {
        let result = {
            let mut state = lock(&self.state);
            let result = change(&mut state);
            let mut projects: Vec<ProjectTokenView> = state.projects.values().cloned().collect();
            projects.sort_by(|a, b| a.project_id.cmp(&b.project_id));
            *lock(&self.view) = Arc::new(TokenView {
                projects,
                available: true,
            });
            result
        };
        (self.wake)();
        result
    }

    /// One project's entry, made from its saved settings the first time.
    fn entry<'a>(&self, state: &'a mut State, project_id: &str) -> &'a mut ProjectTokenView {
        state
            .projects
            .entry(project_id.to_owned())
            .or_insert_with(|| ProjectTokenView {
                project_id: project_id.to_owned(),
                settings: project_settings::load_token_optimization(&self.data_root, project_id),
                ..ProjectTokenView::default()
            })
    }

    /// Change one project's state and publish the view.
    fn edit(&self, project_id: &str, change: impl FnOnce(&mut ProjectTokenView)) {
        self.with_state(|state| change(self.entry(state, project_id)));
    }

    /// Show the settings as saved now, with the After figures they give,
    /// under the state lock: whatever saved last, the view ends on the file.
    fn sync(&self, state: &mut State, project_id: &str) {
        let settings = project_settings::load_token_optimization(&self.data_root, project_id);
        let applied = settings
            .applied_at
            .as_deref()
            .and_then(token_audit::parse_utc);
        let after = match (applied, state.sessions.get(project_id)) {
            (Some(applied), Some(sessions)) => {
                Some(token_audit::after(sessions, applied, &settings))
            }
            _ => None,
        };
        let entry = self.entry(state, project_id);
        entry.settings = settings;
        if let Some(report) = &mut entry.report {
            report.after = after;
        }
    }

    fn record(&self, project_id: &str, action: &str, outcome: &str, reason: String) {
        if let Some(audit) = &self.audit {
            audit.append_nowait(Record {
                actor: "connecto".into(),
                source: "token_optimization".into(),
                target: "claude".into(),
                action: format!("token_optimization.{action}"),
                outcome: outcome.into(),
                reason,
                project: project_id.into(),
                ..Record::default()
            });
        }
    }

    /// Save a change of the setting, then show what the file holds.
    fn save(
        &self,
        project_id: &str,
        change: impl FnOnce(&mut TokenSettings),
    ) -> Result<(), String> {
        let saved =
            project_settings::update_token_optimization(&self.data_root, project_id, change)
                .map_err(|err| format!("the setting could not be saved: {err}"));
        self.with_state(|state| {
            self.sync(state, project_id);
            if saved.is_ok() {
                self.entry(state, project_id).error = None;
            }
        });
        saved
    }

    /// The whole analysis, on its own thread.
    fn run_analysis(self: &Arc<Self>, project_id: &str, project: &Path) {
        let progress = |text: String| self.edit(project_id, |p| p.busy = Some(text));
        progress("finding sessions…".to_owned());
        let profiles = self.env.profiles(project);
        let found = token_audit::find_transcripts(&profiles, project, now_ms());
        let total = found.files.len();
        progress(format!("reading {total} sessions…"));
        let read = token_audit::read_sessions(&found, &|done, total| {
            if done % 5 == 0 {
                self.edit(project_id, |p| {
                    p.busy = Some(format!("reading {total} sessions… ({done} done)"));
                });
            }
        });
        let sessions = read.kept;
        let ids = token_audit::plugin_ids(&profiles, project);
        let latest = token_audit::latest_recorded(&sessions);
        let proposals = token_audit::proposals(latest, &sessions, &read.short_uses, &ids);
        let totals = token_audit::totals(&sessions, &found);
        let biggest = token_audit::biggest(&sessions);
        let report = TokenReport {
            analyzed_ms: now_ms(),
            loaded: token_audit::loaded_counts(latest),
            compact: token_audit::compact_estimate(&sessions),
            after: None,
            biggest: biggest.map(token_audit::describe),
            totals,
            proposals,
        };
        let summary = biggest.map(token_audit::study_summary);
        let counts = format!(
            "{} sessions, {} calls, {} proposals",
            report.totals.sessions,
            report.totals.calls,
            report.proposals.len()
        );
        // The report, the sessions and the settings as saved now, at once.
        self.with_state(|state| {
            state
                .sessions
                .insert(project_id.to_owned(), Arc::new(sessions));
            let p = self.entry(state, project_id);
            p.report = Some(report);
            p.study = if summary.is_some() {
                StudyState::Running
            } else {
                StudyState::NotRun
            };
            p.busy = summary
                .as_ref()
                .map(|_| "studying the biggest session…".to_owned());
            self.sync(state, project_id);
        });
        self.record(project_id, "analyze", "ok", counts);
        let Some(summary) = summary else {
            return;
        };
        // The audit names a category, never the reason's text: a failed
        // run's reason can carry what the provider replied.
        let (study, reason) =
            match self
                .env
                .study(project_id, project, token_audit::study_prompt(&summary))
            {
                Err(why) => (Err(why), "the utility session failed".to_owned()),
                Ok(reply) => match token_audit::parse_study(&reply) {
                    Ok(findings) => {
                        let n = findings.len();
                        (Ok(findings), format!("{n} findings"))
                    }
                    Err(why) => (Err(why), "the reply was not the JSON asked for".to_owned()),
                },
            };
        let outcome = if study.is_ok() { "ok" } else { "failed" };
        self.edit(project_id, |p| {
            p.busy = None;
            p.study = match study {
                Ok(findings) => StudyState::Done(findings),
                Err(why) => StudyState::Unavailable(why),
            };
        });
        self.record(project_id, "study", outcome, reason);
    }
}

/// What the window is given: the host behind [`TokenControl`].
#[derive(Clone)]
pub struct TokenHandle(pub Arc<TokenHost>);

impl std::ops::Deref for TokenHandle {
    type Target = TokenHost;
    fn deref(&self) -> &TokenHost {
        &self.0
    }
}

impl TokenControl for TokenHandle {
    fn view(&self) -> Arc<TokenView> {
        Arc::clone(&lock(&self.view))
    }

    fn open(&self, project_id: &str) {
        let known = lock(&self.state).projects.contains_key(project_id);
        if !known {
            self.edit(project_id, |_| {});
        }
    }

    fn analyze(&self, project_id: &str) -> Result<(), String> {
        let project = self
            .env
            .project_path(project_id)
            .ok_or_else(|| "the project is not open".to_owned())?;
        // Checked and taken under one lock: two clicks start one analysis.
        let admitted = self.with_state(|state| {
            let p = self.entry(state, project_id);
            if p.busy.is_some() {
                return false;
            }
            p.busy = Some("finding sessions…".to_owned());
            p.error = None;
            true
        });
        if !admitted {
            return Err("an analysis is already running".to_owned());
        }
        let host = Arc::clone(&self.0);
        let id = project_id.to_owned();
        std::thread::Builder::new()
            .name("token-analysis".to_owned())
            .spawn(move || host.run_analysis(&id, &project))
            .map(|_| ())
            .map_err(|err| {
                self.edit(project_id, |p| p.busy = None);
                format!("the analysis could not start: {err}")
            })
    }

    fn apply(
        &self,
        project_id: &str,
        plugins_off: Vec<String>,
        skills_off: Vec<String>,
    ) -> Result<(), String> {
        let sessions = lock(&self.state).sessions.get(project_id).cloned();
        let Some(sessions) = sessions else {
            return Err("Analyze past sessions first.".to_owned());
        };
        if plugins_off.is_empty() && skills_off.is_empty() {
            return Err("Nothing is ticked.".to_owned());
        }
        let before = token_audit::before(&sessions);
        let stamp = token_audit::format_utc(now_ms());
        let mut added = (0usize, 0usize);
        let saved = self.save(project_id, |t| {
            // Added to what was cut before: those no longer show to tick.
            for id in plugins_off {
                if !t.plugins_off.contains(&id) {
                    t.plugins_off.push(id);
                    added.0 += 1;
                }
            }
            for name in skills_off {
                if !t.skills_off.contains(&name) {
                    t.skills_off.push(name);
                    added.1 += 1;
                }
            }
            // Nothing new: the measurement keeps its start.
            if added != (0, 0) {
                t.applied_at = Some(stamp);
                t.before = Some(before);
            }
        });
        if let Err(why) = saved {
            self.record(project_id, "apply", "failed", why.clone());
            return Err(why);
        }
        if added != (0, 0) {
            self.record(
                project_id,
                "apply",
                "ok",
                format!("{} plugin ids, {} skills", added.0, added.1),
            );
        }
        Ok(())
    }

    fn undo(&self, project_id: &str) -> Result<(), String> {
        let saved = self.save(project_id, |t| {
            t.plugins_off.clear();
            t.skills_off.clear();
            t.applied_at = None;
            t.before = None;
        });
        if let Err(why) = saved {
            self.record(project_id, "undo", "failed", why.clone());
            return Err(why);
        }
        self.record(project_id, "undo", "ok", "all cuts removed".to_owned());
        Ok(())
    }

    fn set_autocompact(&self, project_id: &str, tokens: Option<u32>) -> Result<(), String> {
        if tokens.is_some_and(|t| !AUTOCOMPACT_CHOICES.contains(&t)) {
            return Err("not one of the choices".to_owned());
        }
        self.save(project_id, |t| t.autocompact = tokens)?;
        self.record(
            project_id,
            "apply",
            "ok",
            format!(
                "autocompact {}",
                tokens.map_or_else(|| "auto".to_owned(), |t| t.to_string())
            ),
        );
        Ok(())
    }
}

#[cfg(test)]
#[path = "token_host_tests.rs"]
mod tests;
