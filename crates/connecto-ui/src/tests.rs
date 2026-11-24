//! The window against fake sessions, a fake workspace and fake terminal lists.
//!
//! Real egui input runs through the real code. Only what sits behind the
//! `connecto-core` traits is fake, so no test starts a CLI or touches the disk,
//! the network or a credential.

#![allow(clippy::unwrap_used)]

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use connecto_core::{
    AuthState, GridSize, HistoryEntry, InputRefused, PaneControl, PaneStage, PauseSwitch,
    ProjectView, RecentProject, ResizeOutcome, SaveState, SelectOp, SelectionText, SessionStatus,
    SnapshotOutcome, TermActivity, TermSnapshot, TerminalEntry, TerminalSession, TerminalsControl,
    TerminalsView, ToolKind, Transcript, UtilityEffort, UtilityModel, UtilityRun, UtilitySettings,
    UtilityStatus, WorkspaceControl, WorkspaceView,
};
use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2, ViewportCommand};

use crate::app::viewport_for_tests;
use crate::app::{App, Diagnostics, PaneConfig};
use crate::pane::PANE_PADDING;
use crate::theme;
use crate::tree::{Hit, LIGHTS_BAND, RAIL_WIDTH, SIDEBAR_WIDTH};

#[cfg(not(target_os = "macos"))]
mod in_window_menu;
mod project_files;

/// The in-window menu bar's height; macOS has a native menu and none.
#[cfg(target_os = "macos")]
const MENU_BAR: f32 = 0.0;
#[cfg(not(target_os = "macos"))]
const MENU_BAR: f32 = crate::menu::BAR_HEIGHT;

// ---------- fakes ----------

/// `(anchor, head)` cells; no head until a drag moves.
type FakeSelection = ((u16, u16), Option<(u16, u16)>);

#[derive(Default)]
struct FakeSession {
    input: Mutex<Vec<u8>>,
    sizes: Mutex<Vec<GridSize>>,
    polls: AtomicUsize,
    refuse: bool,
    /// The screen's rows, for the selection, as `fill` wrote them.
    screen: Mutex<Vec<String>>,
    /// The selection as `(anchor, head)` cells; no head until a drag moves.
    selected: Mutex<Option<FakeSelection>>,
    selects: Mutex<Vec<SelectOp>>,
    scrolls: Mutex<Vec<i32>>,
    /// The emulator is busy: selection changes are refused.
    busy: AtomicBool,
    /// Moves when a remote terminal reattaches.
    generation: std::sync::atomic::AtomicU64,
    /// The program asked for the mouse, and clicks reach it (as a remote
    /// terminal's do).
    mouse: AtomicBool,
}

impl TerminalSession for FakeSession {
    fn attachment_generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        self.polls.fetch_add(1, Ordering::Relaxed);
        if self.mouse.load(Ordering::Relaxed) && !out.modes.mouse_click {
            out.modes.mouse_click = true;
            out.modes.mouse_drag = true;
            out.modes.mouse_sgr = true;
            out.cols = 80;
            out.rows = 24;
            out.cells = vec![connecto_core::Cell::default(); 80 * 24];
            return SnapshotOutcome::Updated;
        }
        SnapshotOutcome::Unchanged
    }

    fn forwards_mouse(&self) -> bool {
        self.mouse.load(Ordering::Relaxed)
    }

    fn send_input(&self, bytes: Vec<u8>) -> Result<(), InputRefused> {
        if self.refuse {
            return Err(InputRefused::Closed);
        }
        self.input.lock().unwrap().extend(bytes);
        Ok(())
    }

    fn resize(&self, size: GridSize) -> ResizeOutcome {
        self.sizes.lock().unwrap().push(size);
        ResizeOutcome::Applied
    }

    fn scroll(&self, lines: i32) -> bool {
        self.scrolls.lock().unwrap().push(lines);
        true
    }

    fn status(&self) -> SessionStatus {
        SessionStatus::Running
    }

    fn select(&self, op: SelectOp) -> bool {
        if self.busy.load(Ordering::Relaxed) {
            return false;
        }
        self.selects.lock().unwrap().push(op);
        let mut selected = self.selected.lock().unwrap();
        match op {
            SelectOp::Start { row, col } => *selected = Some(((row, col), None)),
            SelectOp::Extend { row, col } => {
                if let Some((_, head)) = selected.as_mut() {
                    *head = Some((row, col));
                }
            }
            SelectOp::All => {
                let rows = self.screen.lock().unwrap().len() as u16;
                *selected = Some(((0, 0), Some((rows.saturating_sub(1), u16::MAX))));
            }
            SelectOp::Clear => *selected = None,
        }
        true
    }

    fn selection_text(&self) -> SelectionText {
        let Some((anchor, Some(head))) = *self.selected.lock().unwrap() else {
            return SelectionText::Nothing;
        };
        let (first, last) = if anchor <= head {
            (anchor, head)
        } else {
            (head, anchor)
        };
        let screen = self.screen.lock().unwrap();
        let mut lines = Vec::new();
        for row in first.0..=last.0 {
            let chars: Vec<char> = screen[usize::from(row)].chars().collect();
            let start = if row == first.0 {
                usize::from(first.1)
            } else {
                0
            };
            let end = if row == last.0 {
                usize::from(last.1).saturating_add(1)
            } else {
                chars.len()
            }
            .min(chars.len());
            let line: String = chars.get(start..end).unwrap_or(&[]).iter().collect();
            lines.push(line.trim_end().to_owned());
        }
        SelectionText::Text(lines.join("\n").trim_end_matches('\n').to_owned())
    }
}

struct FakeControl {
    stage: Mutex<PaneStage>,
    logins: Mutex<u32>,
    starts: Mutex<u32>,
    live: AtomicBool,
    warnings: Mutex<Arc<Vec<String>>>,
    injecting: AtomicBool,
    waiting: AtomicBool,
}

impl Default for FakeControl {
    fn default() -> Self {
        Self {
            stage: Mutex::new(PaneStage::Running),
            logins: Mutex::new(0),
            starts: Mutex::new(0),
            live: AtomicBool::new(true),
            warnings: Mutex::default(),
            injecting: AtomicBool::new(false),
            waiting: AtomicBool::new(false),
        }
    }
}

impl PaneControl for FakeControl {
    fn stage(&self) -> PaneStage {
        self.stage.lock().unwrap().clone()
    }

    fn request_login(&self) {
        *self.logins.lock().unwrap() += 1;
    }

    fn request_start(&self) {
        *self.starts.lock().unwrap() += 1;
    }

    fn request_stop(&self) {}

    fn request_restart(&self) {}

    fn is_live(&self) -> bool {
        self.live.load(Ordering::Relaxed)
    }

    fn warnings(&self) -> Arc<Vec<String>> {
        Arc::clone(&self.warnings.lock().unwrap())
    }

    fn rejected_hook_frames(&self) -> u64 {
        0
    }

    fn injecting(&self) -> bool {
        self.injecting.load(Ordering::Relaxed)
    }

    fn message_waiting(&self) -> bool {
        self.waiting.load(Ordering::Relaxed)
    }
}

#[derive(Default)]
struct Slot {
    session: Arc<FakeSession>,
    control: Arc<FakeControl>,
}

impl Slot {
    fn typed(&self) -> Vec<u8> {
        self.session.input.lock().unwrap().clone()
    }
}

/// A terminal list the tests steer, and what was asked of it.
struct FakeTerminals {
    view: Mutex<TerminalsView>,
    adds: Mutex<Vec<(ToolKind, Option<String>)>>,
    closes: Mutex<Vec<String>>,
    renames: Mutex<Vec<(String, Option<String>)>>,
    restarts: Mutex<Vec<String>>,
    stops: Mutex<Vec<String>>,
    continues: Mutex<Vec<String>>,
    transcripts: Mutex<Vec<String>>,
    titlings: Mutex<Vec<String>>,
    runs: Mutex<Vec<UtilityRun>>,
    utility_panes: Mutex<HashMap<u64, TerminalEntry>>,
    refuse: Mutex<Option<String>>,
    /// The files read through the terminals' host: (terminal, path).
    fetches: Mutex<Vec<(String, String)>>,
    /// The files sent to the terminals' host: (terminal, name, bytes).
    uploads: Mutex<Vec<(String, String, Vec<u8>)>>,
    /// Run during each upload, as something on the host would meanwhile.
    on_upload: Mutex<Option<Box<dyn Fn() + Send + Sync>>>,
    /// A remote project's settings and tokens, as its host keeps them.
    remote_settings: Mutex<Option<RemoteSettings>>,
    /// A remote project's operations, as its host keeps them.
    remote_ops: Mutex<Option<Arc<dyn connecto_core::ops::OpsControl>>>,
    /// How often the window asked for the history to be kept fresh.
    watches: std::sync::atomic::AtomicUsize,
    /// Terminals opened with a prompt.
    prompted: Mutex<Vec<Prompted>>,
    /// Agents the terminals' machine cannot start (a host from before them).
    not_offered: Mutex<Vec<ToolKind>>,
    /// A remote host's Tools setting has not come yet.
    agents_loading: std::sync::atomic::AtomicBool,
}

/// A terminal opened with a prompt: (tool, name, prompt, model).
type Prompted = (ToolKind, String, String, Option<String>);

/// A remote project's package and token controls.
type RemoteSettings = (
    Arc<dyn connecto_core::PackagesControl>,
    Arc<dyn connecto_core::TokenControl>,
);

impl FakeTerminals {
    fn answer(&self) -> Result<(), String> {
        self.refuse.lock().unwrap().clone().map_or(Ok(()), Err)
    }

    fn edit(&self, change: impl FnOnce(&mut TerminalsView)) {
        change(&mut self.view.lock().unwrap());
    }
}

impl TerminalsControl for FakeTerminals {
    fn view(&self) -> Arc<TerminalsView> {
        Arc::new(self.view.lock().unwrap().clone())
    }

    fn watch_history(&self) {
        self.watches
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
    }

    fn remote_packages(&self, _shown_as: &str) -> Option<Arc<dyn connecto_core::PackagesControl>> {
        self.remote_settings
            .lock()
            .unwrap()
            .as_ref()
            .map(|(packages, _)| Arc::clone(packages))
    }

    fn remote_tokens(&self, _shown_as: &str) -> Option<Arc<dyn connecto_core::TokenControl>> {
        self.remote_settings
            .lock()
            .unwrap()
            .as_ref()
            .map(|(_, tokens)| Arc::clone(tokens))
    }

    fn remote_ops(&self) -> Option<Arc<dyn connecto_core::ops::OpsControl>> {
        self.remote_ops.lock().unwrap().clone()
    }

    fn offers(&self, tool: ToolKind) -> bool {
        !self.not_offered.lock().unwrap().contains(&tool)
    }

    fn agents_loading(&self) -> bool {
        self.agents_loading
            .load(std::sync::atomic::Ordering::SeqCst)
    }

    fn add(&self, tool: ToolKind, name: Option<String>) -> Result<(), String> {
        self.adds.lock().unwrap().push((tool, name));
        self.answer()
    }

    fn add_prompted(
        &self,
        tool: ToolKind,
        name: String,
        prompt: String,
        model: Option<String>,
    ) -> Result<String, String> {
        self.answer()?;
        self.prompted
            .lock()
            .unwrap()
            .push((tool, name.clone(), prompt, model));
        // In the list at once, as the registry's terminal is.
        self.edit(|view| {
            if let Some(mut entry) = view.terminals.first().cloned() {
                entry.id = "prompted-1".to_owned();
                entry.name = Some(name);
                entry.parent = None;
                view.terminals.push(entry);
            }
        });
        Ok("prompted-1".to_owned())
    }

    fn close(&self, id: &str) -> Result<(), String> {
        self.closes.lock().unwrap().push(id.to_owned());
        self.answer()
    }

    fn rename(&self, id: &str, name: Option<String>) -> Result<(), String> {
        self.renames.lock().unwrap().push((id.to_owned(), name));
        self.answer()
    }

    fn restart(&self, id: &str) -> Result<(), String> {
        self.restarts.lock().unwrap().push(id.to_owned());
        self.answer()
    }

    fn stop_background(&self, id: &str, task: Option<&str>) -> Result<String, String> {
        // `terminal`, or `terminal/task` for one task.
        let stopped = task.map_or_else(|| id.to_owned(), |task| format!("{id}/{task}"));
        self.stops.lock().unwrap().push(stopped);
        self.answer()
            .map(|()| "Stopping 1 background shell.".to_owned())
    }

    fn continue_from(&self, history_id: &str) -> Result<(), String> {
        self.continues.lock().unwrap().push(history_id.to_owned());
        self.answer()
    }

    fn request_transcript(&self, history_id: &str) -> Result<(), String> {
        self.transcripts.lock().unwrap().push(history_id.to_owned());
        self.answer()
    }

    fn start_titling(&self, id: &str) -> Result<(), String> {
        self.titlings.lock().unwrap().push(id.to_owned());
        self.answer()
    }

    fn utility_pane(&self, run: u64) -> Option<TerminalEntry> {
        self.utility_panes.lock().unwrap().get(&run).cloned()
    }

    fn utility_runs(&self) -> Arc<Vec<UtilityRun>> {
        Arc::new(self.runs.lock().unwrap().clone())
    }

    fn fetch_file(&self, id: &str, path: &str) -> Result<(String, Vec<u8>), String> {
        self.fetches
            .lock()
            .unwrap()
            .push((id.to_owned(), path.to_owned()));
        self.answer()?;
        Ok((
            format!("/host/web/{path}"),
            b"# Plan\n\nShip it.\n".to_vec(),
        ))
    }

    fn upload_file(&self, id: &str, name: &str, bytes: &[u8]) -> Result<String, String> {
        self.answer()?;
        if let Some(meanwhile) = &*self.on_upload.lock().unwrap() {
            meanwhile();
        }
        self.uploads
            .lock()
            .unwrap()
            .push((id.to_owned(), name.to_owned(), bytes.to_vec()));
        Ok(format!("/host/uploads/{name}"))
    }
}

/// A workspace that applies what it is asked, as the real one does.
struct FakeWorkspace {
    view: Mutex<WorkspaceView>,
    pause: Arc<PauseSwitch>,
    opened: Mutex<Vec<PathBuf>>,
    closed: Mutex<Vec<String>>,
    renamed: Mutex<Vec<(String, Option<String>)>>,
    activations: Mutex<Vec<String>>,
    cleared: AtomicUsize,
    stopped: AtomicUsize,
    utility: Mutex<UtilitySettings>,
    utility_sets: Mutex<Vec<UtilitySettings>>,
    modes: Mutex<connecto_core::LaunchModes>,
    secrets_in_system_store: AtomicBool,
    refuse: Mutex<Option<String>>,
    /// New Project and Delete Project (`tests/project_files.rs`).
    files: project_files::FakeFiles,
}

impl FakeWorkspace {
    fn edit(&self, change: impl FnOnce(&mut WorkspaceView)) {
        change(&mut self.view.lock().unwrap());
    }

    fn answer(&self) -> Result<(), String> {
        self.refuse.lock().unwrap().clone().map_or(Ok(()), Err)
    }
}

impl WorkspaceControl for FakeWorkspace {
    fn view(&self) -> Arc<WorkspaceView> {
        Arc::new(self.view.lock().unwrap().clone())
    }

    fn open_project(&self, path: &Path) -> Result<(), String> {
        self.opened.lock().unwrap().push(path.to_owned());
        self.answer()
    }

    fn activate(&self, project: &str) {
        self.activations.lock().unwrap().push(project.to_owned());
        self.edit(|view| view.active = Some(project.to_owned()));
    }

    fn close_project(&self, project: &str) -> Result<(), String> {
        self.closed.lock().unwrap().push(project.to_owned());
        self.answer()
    }

    fn clone_project(
        &self,
        url: &str,
        parent: &Path,
    ) -> Result<connecto_core::project_files::Cloning, String> {
        self.files.clone_project(url, parent)
    }

    fn plan_delete(
        &self,
        project: &str,
    ) -> Result<connecto_core::project_files::DeletePlan, String> {
        self.files.plan_delete(project)
    }

    fn delete_project(
        &self,
        plan: &connecto_core::project_files::DeletePlan,
    ) -> Result<Arc<dyn connecto_core::project_files::ProjectTask>, String> {
        self.files.delete_project(plan)
    }

    fn release_delete_plan(&self, plan: &connecto_core::project_files::DeletePlan) {
        self.files.release_delete_plan(plan);
    }

    fn rename_project(&self, project: &str, name: Option<String>) -> Result<(), String> {
        self.renamed
            .lock()
            .unwrap()
            .push((project.to_owned(), name));
        self.answer()
    }

    fn move_project(&self, _project: &str, _index: usize) {}

    fn create_group(&self, name: &str) -> Result<(), String> {
        self.answer()?;
        self.edit(|view| {
            let id = format!("group-{}", view.groups.len() + 1);
            view.groups.push(connecto_core::ProjectGroup {
                id,
                name: name.to_owned(),
            });
        });
        Ok(())
    }

    fn rename_group(&self, group: &str, name: &str) -> Result<(), String> {
        self.answer()?;
        self.edit(|view| {
            if let Some(g) = view.groups.iter_mut().find(|g| g.id == group) {
                g.name = name.to_owned();
            }
        });
        Ok(())
    }

    fn delete_group(&self, group: &str) -> Result<(), String> {
        self.answer()?;
        self.edit(|view| {
            view.groups.retain(|g| g.id != group);
            for project in &mut view.projects {
                if project.group.as_deref() == Some(group) {
                    project.group = None;
                }
            }
        });
        Ok(())
    }

    fn set_project_group(&self, project: &str, group: Option<&str>) -> Result<(), String> {
        self.answer()?;
        self.edit(|view| {
            if let Some(p) = view.projects.iter_mut().find(|p| p.id == project) {
                p.group = group.map(str::to_owned);
            }
        });
        Ok(())
    }

    fn set_expanded(&self, project: &str, expanded: bool) {
        self.edit(|view| {
            if let Some(found) = view.projects.iter_mut().find(|p| p.id == project) {
                found.expanded = expanded;
            }
        });
    }

    fn set_sidebar_collapsed(&self, collapsed: bool) {
        self.edit(|view| view.sidebar_collapsed = collapsed);
    }

    fn set_active_terminal(&self, project: &str, terminal: &str) {
        self.edit(|view| {
            if let Some(found) = view.projects.iter_mut().find(|p| p.id == project) {
                found.active_terminal = Some(terminal.to_owned());
            }
        });
    }

    fn clear_recents(&self) {
        self.cleared.fetch_add(1, Ordering::Relaxed);
    }

    fn pause_switch(&self) -> Arc<PauseSwitch> {
        Arc::clone(&self.pause)
    }

    fn stop_everything(&self) {
        self.stopped.fetch_add(1, Ordering::Relaxed);
    }

    fn launch_modes(&self) -> connecto_core::LaunchModes {
        *self.modes.lock().unwrap()
    }

    fn set_launch_modes(&self, modes: connecto_core::LaunchModes) {
        *self.modes.lock().unwrap() = modes;
    }

    fn secrets_in_system_store(&self) -> bool {
        self.secrets_in_system_store.load(Ordering::SeqCst)
    }

    fn set_secrets_in_system_store(&self, on: bool) {
        self.secrets_in_system_store.store(on, Ordering::SeqCst);
    }

    fn utility_settings(&self) -> UtilitySettings {
        *self.utility.lock().unwrap()
    }

    fn set_utility_settings(&self, settings: UtilitySettings) {
        self.utility_sets.lock().unwrap().push(settings);
        *self.utility.lock().unwrap() = settings;
    }
}

// ---------- builders ----------

struct Spec {
    id: &'static str,
    tool: ToolKind,
    name: Option<&'static str>,
    parent: Option<&'static str>,
    asked_by: Option<&'static str>,
    state: TermActivity,
    turns: u64,
    /// Working only because background work goes on at the prompt.
    background: bool,
    /// That background work.
    tasks: Vec<connecto_core::BackgroundWork>,
}

fn spec(id: &'static str, tool: ToolKind) -> Spec {
    Spec {
        id,
        tool,
        name: None,
        parent: None,
        asked_by: None,
        state: TermActivity::Idle,
        turns: 0,
        background: false,
        tasks: Vec::new(),
    }
}

impl Spec {
    /// Working, but only background work at the prompt: no turn is running.
    fn background(mut self) -> Self {
        self.state = TermActivity::Working;
        self.background = true;
        self
    }

    /// Background work at the prompt: these tasks run.
    fn running(mut self, tasks: Vec<connecto_core::BackgroundWork>) -> Self {
        self = self.background();
        self.tasks = tasks;
        self
    }

    fn named(mut self, name: &'static str) -> Self {
        self.name = Some(name);
        self
    }

    fn child_of(mut self, parent: &'static str) -> Self {
        self.parent = Some(parent);
        self
    }

    fn asked_by(mut self, asker: &'static str) -> Self {
        self.asked_by = Some(asker);
        self
    }

    fn state(mut self, state: TermActivity) -> Self {
        self.state = state;
        self
    }
}

struct Proj {
    id: &'static str,
    terminals: Arc<FakeTerminals>,
    slots: HashMap<&'static str, Arc<Slot>>,
}

impl Proj {
    fn slot(&self, id: &str) -> &Slot {
        self.slots.get(id).unwrap()
    }

    fn typed_anywhere(&self) -> bool {
        self.slots.values().any(|slot| !slot.typed().is_empty())
    }

    fn view(&self) -> ProjectView {
        ProjectView {
            id: self.id.to_owned(),
            name: self.id.to_owned(),
            path: format!("/work/{}", self.id),
            terminals: Some(Arc::clone(&self.terminals) as Arc<dyn TerminalsControl>),
            error: None,
            expanded: true,
            active_terminal: None,
            group: None,
        }
    }

    /// Replace the list, keeping the sessions of ids that stay.
    fn set(&mut self, specs: Vec<Spec>) {
        let entries = specs
            .into_iter()
            .map(|spec| {
                let slot = Arc::clone(self.slots.entry(spec.id).or_default());
                TerminalEntry {
                    id: spec.id.to_owned(),
                    name: spec.name.map(str::to_owned),
                    parent: spec.parent.map(str::to_owned),
                    asked_by: spec.asked_by.map(str::to_owned),
                    tool: spec.tool,
                    state: spec.state,
                    in_turn: spec.state == TermActivity::Working && !spec.background,
                    background: spec.tasks,
                    finished_turns: spec.turns,
                    session: Arc::clone(&slot.session) as Arc<dyn TerminalSession>,
                    control: Some(Arc::clone(&slot.control) as Arc<dyn PaneControl>),
                    desired_running: true,
                    shown_id: None,
                    activity: 0,
                }
            })
            .collect();
        self.terminals.edit(|view| view.terminals = entries);
    }
}

fn project(id: &'static str, specs: Vec<Spec>) -> Proj {
    let mut proj = Proj {
        id,
        terminals: Arc::new(FakeTerminals {
            view: Mutex::new(TerminalsView {
                terminals: Vec::new(),
                history: Arc::default(),
                transcript: None,
                save: SaveState::Saved,
                last_error: None,
                warning: None,
            }),
            adds: Mutex::default(),
            closes: Mutex::default(),
            renames: Mutex::default(),
            restarts: Mutex::default(),
            stops: Mutex::default(),
            continues: Mutex::default(),
            transcripts: Mutex::default(),
            titlings: Mutex::default(),
            runs: Mutex::default(),
            utility_panes: Mutex::default(),
            refuse: Mutex::default(),
            fetches: Mutex::default(),
            uploads: Mutex::default(),
            on_upload: Mutex::default(),
            remote_settings: Mutex::default(),
            remote_ops: Mutex::default(),
            watches: std::sync::atomic::AtomicUsize::new(0),
            prompted: Mutex::default(),
            not_offered: Mutex::default(),
            agents_loading: std::sync::atomic::AtomicBool::new(false),
        }),
        slots: HashMap::new(),
    };
    proj.set(specs);
    proj
}

fn workspace(projects: &[&Proj]) -> Arc<FakeWorkspace> {
    Arc::new(FakeWorkspace {
        view: Mutex::new(WorkspaceView {
            projects: projects.iter().map(|p| p.view()).collect(),
            active: projects.first().map(|p| p.id.to_owned()),
            ..WorkspaceView::default()
        }),
        pause: Arc::default(),
        opened: Mutex::default(),
        closed: Mutex::default(),
        renamed: Mutex::default(),
        activations: Mutex::default(),
        cleared: AtomicUsize::new(0),
        stopped: AtomicUsize::new(0),
        utility: Mutex::default(),
        utility_sets: Mutex::default(),
        modes: Mutex::default(),
        secrets_in_system_store: AtomicBool::new(false),
        refuse: Mutex::default(),
        files: project_files::FakeFiles::default(),
    })
}

// ---------- harness ----------

struct W {
    ctx: egui::Context,
    app: App,
    ws: Arc<FakeWorkspace>,
    width: f32,
    /// The window's height; a long sheet is tested in a tall window.
    height: f32,
    copied: Vec<String>,
    commands: Vec<ViewportCommand>,
    repaint_delay: std::time::Duration,
}

fn picked() -> Option<PathBuf> {
    Some(PathBuf::from("/picked/folder"))
}

fn fixed_clock() -> (u64, i32) {
    (1_794_139_200_000, 0)
}

impl W {
    fn new(ws: &Arc<FakeWorkspace>) -> Self {
        Self::with_automations(ws, None)
    }

    fn with_automations(
        ws: &Arc<FakeWorkspace>,
        automations: Option<Arc<dyn connecto_core::AutomationsControl>>,
    ) -> Self {
        Self::with_all(ws, automations, Arc::new(connecto_core::NoPackages))
    }

    fn with_packages(
        ws: &Arc<FakeWorkspace>,
        packages: Arc<dyn connecto_core::PackagesControl>,
    ) -> Self {
        Self::with_all(ws, None, packages)
    }

    fn with_all(
        ws: &Arc<FakeWorkspace>,
        automations: Option<Arc<dyn connecto_core::AutomationsControl>>,
        packages: Arc<dyn connecto_core::PackagesControl>,
    ) -> Self {
        Self::with_remote(ws, automations, packages, Arc::new(connecto_core::NoRemote))
    }

    fn with_remote(
        ws: &Arc<FakeWorkspace>,
        automations: Option<Arc<dyn connecto_core::AutomationsControl>>,
        packages: Arc<dyn connecto_core::PackagesControl>,
        remote: Arc<dyn connecto_core::RemoteControl>,
    ) -> Self {
        Self::with_diagnostics(
            ws,
            Diagnostics {
                workspace: Some(Arc::clone(ws) as Arc<dyn WorkspaceControl>),
                automations,
                packages,
                remote,
                ..Diagnostics::default()
            },
        )
    }

    fn with_diagnostics(ws: &Arc<FakeWorkspace>, diagnostics: Diagnostics) -> Self {
        let mut app = App::new(Vec::new(), Arc::default(), diagnostics);
        app.set_test_hooks(picked, fixed_clock);
        let ctx = egui::Context::default();
        theme::apply(&ctx);
        let mut w = Self {
            ctx,
            app,
            ws: Arc::clone(ws),
            width: 1280.0,
            height: 760.0,
            copied: Vec::new(),
            commands: Vec::new(),
            repaint_delay: std::time::Duration::MAX,
        };
        w.frame(vec![]);
        w.frame(vec![]);
        w
    }

    /// One pass. A key pressed in it and not released in it is released in the
    /// pass after, as a real key is.
    fn frame(&mut self, events: Vec<Event>) -> Vec<ViewportCommand> {
        let releases: Vec<Event> = events
            .iter()
            .filter_map(|event| match event {
                Event::Key {
                    key,
                    pressed: true,
                    modifiers,
                    ..
                } if !events.iter().any(|other| {
                    matches!(other,
                        Event::Key { key: k, pressed: false, .. } if k == key)
                }) =>
                {
                    Some(Event::Key {
                        key: *key,
                        physical_key: None,
                        pressed: false,
                        repeat: false,
                        modifiers: *modifiers,
                    })
                }
                _ => None,
            })
            .collect();
        let mut commands = self.pass(events);
        if !releases.is_empty() {
            commands.extend(self.pass(releases));
        }
        commands
    }

    /// Drop files on the window, the pointer at `at`.
    fn drop_files(&mut self, paths: &[&str], at: Pos2) {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(self.width, self.height),
            )),
            events: vec![Event::PointerMoved(at)],
            dropped_files: paths
                .iter()
                .map(|path| {
                    std::sync::Arc::new(FakeDrop(std::path::PathBuf::from(path)))
                        as egui::DroppedFileHandle
                })
                .collect(),
            ..RawInput::default()
        };
        let app = &mut self.app;
        let mut output = self.ctx.run_ui(input, |ui| app.draw(ui));
        output.textures_delta.clear();
    }

    fn pass(&mut self, events: Vec<Event>) -> Vec<ViewportCommand> {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(self.width, self.height),
            )),
            events,
            ..RawInput::default()
        };
        let app = &mut self.app;
        let mut output = self.ctx.run_ui(input, |ui| app.draw(ui));
        output.textures_delta.clear();
        for command in output.platform_output.commands {
            if let egui::OutputCommand::CopyText(text) = command {
                self.copied.push(text);
            }
        }
        self.repaint_delay = output
            .viewport_output
            .values()
            .map(|viewport| viewport.repaint_delay)
            .min()
            .unwrap_or(std::time::Duration::MAX);
        let commands: Vec<ViewportCommand> = output
            .viewport_output
            .into_values()
            .flat_map(|viewport| viewport.commands)
            .collect();
        self.commands.extend(commands.iter().cloned());
        commands
    }

    fn press(&mut self, pos: Pos2) -> Vec<ViewportCommand> {
        let mut commands = self.frame(vec![Event::PointerMoved(pos)]);
        commands.extend(self.frame(vec![button(pos, true)]));
        commands
    }

    fn release(&mut self, pos: Pos2) -> Vec<ViewportCommand> {
        let mut commands = self.frame(vec![button(pos, false)]);
        commands.extend(self.frame(vec![]));
        commands
    }

    fn click(&mut self, pos: Pos2) {
        self.press(pos);
        self.release(pos);
    }

    fn rect(&self, hit: &Hit) -> Rect {
        let found = self
            .app
            .hits
            .tree
            .iter()
            .find(|(candidate, _)| candidate == hit)
            .map(|(_, rect)| *rect);
        assert!(
            found.is_some(),
            "{hit:?} was not drawn: {:?}",
            self.app.hits.tree
        );
        found.unwrap()
    }

    fn has(&self, hit: &Hit) -> bool {
        self.app
            .hits
            .tree
            .iter()
            .any(|(candidate, _)| candidate == hit)
    }

    fn click_hit(&mut self, hit: &Hit) {
        let at = self.rect(hit).center();
        self.click(at);
    }

    fn key(&mut self, key: Key, modifiers: Modifiers) {
        self.frame(vec![key_down(key, modifiers)]);
    }

    fn type_text(&mut self, text: &str) {
        self.frame(vec![Event::Text(text.to_owned())]);
    }

    /// The order the terminals are drawn in, top to bottom.
    fn terminal_rows(&self, project: &str) -> Vec<String> {
        let mut rows: Vec<(f32, String)> = self
            .app
            .hits
            .tree
            .iter()
            .filter_map(|(hit, rect)| match hit {
                Hit::Terminal(p, t) if p == project => Some((rect.min.y, t.clone())),
                _ => None,
            })
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        rows.into_iter().map(|(_, id)| id).collect()
    }

    fn shown(&self) -> Option<(String, String)> {
        self.app.shown_for_tests()
    }
}

fn button(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    }
}

fn key_down(key: Key, modifiers: Modifiers) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed: true,
        repeat: false,
        modifiers,
    }
}

fn key_event(key: Key, pressed: bool) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers: Modifiers::NONE,
    }
}

fn cmd() -> Modifiers {
    Modifiers {
        mac_cmd: true,
        command: true,
        ..Modifiers::NONE
    }
}

fn cmd_key(key: Key) -> Event {
    key_down(key, cmd())
}

fn enter() -> Vec<Event> {
    vec![key_event(Key::Enter, true), key_event(Key::Enter, false)]
}

fn esc() -> Vec<Event> {
    vec![key_event(Key::Escape, true), key_event(Key::Escape, false)]
}

fn is_drag(commands: &[ViewportCommand]) -> bool {
    commands
        .iter()
        .any(|c| matches!(c, ViewportCommand::StartDrag))
}

use ToolKind::{Claude, Codex};

/// Two projects: quill has a parent with two children, and a second terminal;
/// connecto has one.
fn two_projects() -> (Proj, Proj, Arc<FakeWorkspace>) {
    let quill = project(
        "quill",
        vec![
            spec("claude-1", Claude).named("Ledger retries"),
            spec("codex-2", Codex)
                .named("Review sync diff")
                .child_of("claude-1"),
            spec("claude-3", Claude)
                .child_of("claude-1")
                .asked_by("codex-2"),
            spec("codex-1", Codex).named("API docs pass"),
        ],
    );
    let connecto = project("connecto", vec![spec("claude", Claude)]);
    let ws = workspace(&[&quill, &connecto]);
    (quill, connecto, ws)
}

#[derive(Debug)]
struct FakeDrop(std::path::PathBuf);

impl egui::DroppedFile for FakeDrop {
    fn path(&self) -> &std::path::Path {
        &self.0
    }
    fn bytes(&self) -> Result<Vec<u8>, String> {
        Err("not readable".into())
    }
}

// ---------- the tree ----------

#[test]
fn children_are_drawn_under_their_parent_and_a_grandchild_at_child_level() {
    let quill = project(
        "quill",
        vec![
            spec("claude-1", Claude).named("Ledger retries"),
            spec("codex-1", Codex),
            spec("codex-2", Codex).child_of("claude-1"),
            spec("claude-3", Claude)
                .child_of("claude-1")
                .asked_by("codex-2"),
        ],
    );
    let ws = workspace(&[&quill]);
    let w = W::new(&ws);
    // The list has codex-1 before the children; the tree puts them under claude-1.
    assert_eq!(
        w.terminal_rows("quill"),
        ["claude-1", "codex-2", "claude-3", "codex-1"]
    );
    let parent = w.rect(&Hit::Terminal("quill".into(), "claude-1".into()));
    let child = w.rect(&Hit::Terminal("quill".into(), "codex-2".into()));
    let grandchild = w.rect(&Hit::Terminal("quill".into(), "claude-3".into()));
    assert!(child.min.x > parent.min.x, "children are indented");
    assert!(
        (grandchild.min.x - child.min.x).abs() < 0.5,
        "same level as a child"
    );
}

#[test]
fn an_orphan_child_is_drawn_at_top_level() {
    let quill = project(
        "quill",
        vec![
            spec("codex-2", Codex).child_of("gone"),
            spec("claude", Claude),
        ],
    );
    let ws = workspace(&[&quill]);
    let w = W::new(&ws);
    assert_eq!(w.terminal_rows("quill"), ["codex-2", "claude"]);
    let a = w.rect(&Hit::Terminal("quill".into(), "codex-2".into()));
    let b = w.rect(&Hit::Terminal("quill".into(), "claude".into()));
    assert!((a.min.x - b.min.x).abs() < 0.5);
}

#[test]
fn a_collapsed_project_hides_its_terminals_and_shows_a_count() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert!(w.has(&Hit::Terminal("quill".into(), "claude-1".into())));
    w.click_hit(&Hit::Twisty("quill".into()));
    w.frame(vec![]);
    assert!(!w.has(&Hit::Terminal("quill".into(), "claude-1".into())));
    assert!(!ws.view.lock().unwrap().projects[0].expanded);
    assert!(!quill.typed_anywhere());
}

#[test]
fn a_parent_can_fold_its_children() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::Kids("quill".into(), "claude-1".into()));
    w.frame(vec![]);
    assert_eq!(w.terminal_rows("quill"), ["claude-1", "codex-1"]);
    w.click_hit(&Hit::Kids("quill".into(), "claude-1".into()));
    w.frame(vec![]);
    assert_eq!(w.terminal_rows("quill").len(), 4);
}

#[test]
fn selecting_a_project_shows_its_remembered_terminal_and_sends_nothing_anywhere() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert_eq!(w.shown(), Some(("quill".into(), "claude-1".into())));
    // Only the selected project lists its terminals.
    assert!(!w.has(&Hit::Terminal("connecto".into(), "claude".into())));
    w.click_hit(&Hit::Project("connecto".into()));
    assert_eq!(w.shown(), Some(("connecto".into(), "claude".into())));
    assert_eq!(*ws.activations.lock().unwrap(), ["connecto"]);
    assert!(w.has(&Hit::Terminal("connecto".into(), "claude".into())));
    assert!(!w.has(&Hit::Terminal("quill".into(), "claude-1".into())));
    assert!(!quill.typed_anywhere() && !connecto.typed_anywhere());
    for project in [&quill, &connecto] {
        let terminals = &project.terminals;
        assert!(terminals.adds.lock().unwrap().is_empty());
        assert!(terminals.closes.lock().unwrap().is_empty());
        assert!(terminals.restarts.lock().unwrap().is_empty());
    }
    // Typing now reaches the terminal on show and no other.
    w.frame(vec![]);
    w.type_text("x");
    assert_eq!(connecto.slot("claude").typed(), b"x");
    assert!(!quill.typed_anywhere());
}

#[test]
fn dropping_files_on_the_terminal_inserts_their_escaped_paths_and_runs_nothing() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let content = w.app.hits.content;
    // Over the sidebar: not the terminal's.
    w.drop_files(&["/tmp/x.png"], w.app.hits.sidebar.center());
    assert!(!quill.typed_anywhere());
    w.drop_files(&["/tmp/a b.png", "/tmp/c.txt"], content.center());
    #[cfg(not(windows))]
    let expected: &[u8] = b"/tmp/a\\ b.png /tmp/c.txt ";
    // PowerShell quoting (platform::quote_dropped_path).
    #[cfg(windows)]
    let expected: &[u8] = b"'/tmp/a b.png' /tmp/c.txt ";
    assert_eq!(quill.slot("claude-1").typed(), expected);
    assert!(!connecto.typed_anywhere());
}

/// On this computer's terminals a paste is the clipboard's text, as it
/// always was, files copied with it or not: the pasteboard is not even read.
#[cfg(target_os = "macos")]
#[test]
fn files_copied_and_pasted_on_a_local_terminal_paste_their_text_as_before() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let dir = tempfile::tempdir().unwrap();
    let file = dir.path().join("a b.png");
    std::fs::write(&file, b"a").unwrap();
    let reads = Arc::new(std::sync::atomic::AtomicUsize::new(0));
    let counted = Arc::clone(&reads);
    w.app.set_clipboard_files(Arc::new(move || {
        counted.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        crate::clipboard_files::finder_copy(std::slice::from_ref(&file))
    }));
    w.frame(vec![Event::Paste("a b.png".into())]);
    assert_eq!(quill.slot("claude-1").typed(), b"a b.png");
    assert!(!connecto.typed_anywhere());
    assert_eq!(reads.load(std::sync::atomic::Ordering::SeqCst), 0);
}

/// An Agentic Works job's question over the window (ADR 0040): it is shown
/// with its job, a file dropped on the terminal meanwhile goes nowhere, and
/// No answer reaches Agentic Works.
#[test]
fn a_jobs_question_holds_the_window_until_answered() {
    use connecto_core::agentic_works::{
        ImportedKey, JobQuestion, WorksAction, WorksControl, WorksDraft, WorksView,
    };
    #[derive(Default)]
    struct Works {
        questions: Mutex<Vec<JobQuestion>>,
        answers: Mutex<Vec<(u64, Option<String>)>>,
    }
    impl WorksControl for Works {
        fn view(&self, _: &str) -> WorksView {
            WorksView::default()
        }
        fn submit(&self, _: String, _: WorksAction) {}
        fn draft(&self, _: &str, _: &str) -> Result<WorksDraft, String> {
            Err(String::new())
        }
        fn import_key(&self, _: &str, _: &std::path::Path) -> Result<ImportedKey, String> {
            Err(String::new())
        }
        fn questions(&self) -> Vec<JobQuestion> {
            self.questions.lock().unwrap().clone()
        }
        fn answer(&self, id: u64, answer: Option<String>) -> Result<(), String> {
            self.answers.lock().unwrap().push((id, answer));
            self.questions.lock().unwrap().retain(|q| q.id != id);
            Ok(())
        }
    }
    let (quill, _connecto, ws) = two_projects();
    let works = Arc::new(Works::default());
    works.questions.lock().unwrap().push(JobQuestion {
        id: 4,
        project: "quill".into(),
        project_name: "quill".into(),
        database: "dev".into(),
        job: 17,
        kind: "review".into(),
        terminal: "claude-9".into(),
        text: "Which season?".into(),
        asked_at: "10:00".into(),
        answer_by: "10:25".into(),
    });
    let mut w = W::with_diagnostics(
        &ws,
        Diagnostics {
            workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
            works: Some(Arc::clone(&works) as Arc<dyn WorksControl>),
            ..Diagnostics::default()
        },
    );
    w.frame(vec![]);
    let texts = &w.app.hits.job_question.texts;
    assert!(
        texts
            .iter()
            .any(|t| t.contains("job 17 of the work type review, from the database dev")),
        "{texts:?}"
    );
    assert!(
        texts.iter().any(|t| t.contains("Which season?")),
        "{texts:?}"
    );
    let content = w.app.hits.content;
    w.drop_files(&["/tmp/a.png"], content.center());
    w.frame(vec![]);
    assert!(
        !quill.typed_anywhere(),
        "a drop under the question was typed"
    );
    let none = w.app.hits.job_question.rects["job-question-none"].center();
    w.click(none);
    w.frame(vec![]);
    assert_eq!(*works.answers.lock().unwrap(), [(4, None)]);
    assert!(w.app.hits.job_question.texts.is_empty());
    // With the question gone, a drop reaches the terminal again.
    w.drop_files(&["/tmp/a.png"], content.center());
    assert!(quill.typed_anywhere());
}

#[test]
fn a_pasted_image_is_saved_and_its_path_pasted_into_the_terminal() {
    let (quill, connecto, ws) = two_projects();
    let folder = crate::pasted::tests::Folder::new();
    let mut w = W::with_diagnostics(
        &ws,
        Diagnostics {
            workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
            pasted: Some(Arc::clone(&folder) as Arc<dyn connecto_core::PastedFiles>),
            ..Diagnostics::default()
        },
    );
    let image = egui::ColorImage::new([2, 2], vec![egui::Color32::BLUE; 4]);
    w.frame(vec![Event::PasteImage(Arc::new(image))]);
    // Saved off the UI thread; pasted on a later frame.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
    while quill.slot("claude-1").typed().is_empty() {
        assert!(
            std::time::Instant::now() < deadline,
            "the path never arrived"
        );
        std::thread::sleep(std::time::Duration::from_millis(10));
        w.frame(vec![]);
    }
    let typed = String::from_utf8(quill.slot("claude-1").typed()).unwrap();
    let file = std::fs::read_dir(folder.0.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert!(file.extension().is_some_and(|e| e == "png"));
    assert!(typed.ends_with(' '), "{typed:?}");
    assert!(
        typed.contains(file.file_name().unwrap().to_str().unwrap()),
        "{typed:?}"
    );
    assert_eq!(folder.1.lock().unwrap().as_slice(), ["quill"]);
    assert!(!connecto.typed_anywhere());
}

#[test]
fn a_pasted_image_without_a_store_says_so_and_types_nothing() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let image = egui::ColorImage::new([1, 1], vec![egui::Color32::BLUE; 1]);
    w.frame(vec![Event::PasteImage(Arc::new(image))]);
    w.frame(vec![]);
    assert!(!quill.typed_anywhere());
    let notice = w
        .app
        .pane_notice(&("quill".to_owned(), "claude-1".to_owned()))
        .unwrap();
    assert!(notice.contains("not available"), "{notice}");
}

#[test]
fn only_the_selected_project_lists_terminals_and_each_remembers_its_last_one() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert_eq!(w.terminal_rows("quill").len(), 4);
    assert!(!w.has(&Hit::Terminal("connecto".into(), "claude".into())));
    w.click_hit(&Hit::Terminal("quill".into(), "codex-1".into()));
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("quill".into(), "codex-1".into())));
    w.click_hit(&Hit::Project("connecto".into()));
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("connecto".into(), "claude".into())));
    assert!(!w.has(&Hit::Terminal("quill".into(), "codex-1".into())));
    w.click_hit(&Hit::Project("quill".into()));
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("quill".into(), "codex-1".into())));
    assert!(!w.has(&Hit::Terminal("connecto".into(), "claude".into())));
    assert!(!quill.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn background_projects_keep_their_panes_but_only_the_shown_terminal_is_polled() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert_eq!(
        w.app.pane_count(),
        5,
        "a pane for every terminal of every project"
    );
    let before = connecto
        .slot("claude")
        .session
        .polls
        .load(Ordering::Relaxed);
    w.frame(vec![]);
    assert_eq!(
        connecto
            .slot("claude")
            .session
            .polls
            .load(Ordering::Relaxed),
        before
    );
    assert!(quill.slot("claude-1").session.polls.load(Ordering::Relaxed) > 0);
    assert!(
        connecto
            .slot("claude")
            .session
            .sizes
            .lock()
            .unwrap()
            .is_empty()
    );
    // After a switch the other one is polled and resized, and keeps being the
    // same pane.
    w.click_hit(&Hit::Project("connecto".into()));
    w.frame(vec![]);
    assert_eq!(
        connecto.slot("claude").session.sizes.lock().unwrap().len(),
        1
    );
    assert_eq!(w.app.pane_count(), 5);
}

#[test]
fn switching_projects_does_not_stop_start_or_type() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    for _ in 0..3 {
        w.click_hit(&Hit::Project("connecto".into()));
        w.click_hit(&Hit::Project("quill".into()));
    }
    for slot in quill.slots.values().chain(connecto.slots.values()) {
        assert_eq!(*slot.control.starts.lock().unwrap(), 0);
        assert_eq!(*slot.control.logins.lock().unwrap(), 0);
        assert!(slot.typed().is_empty());
    }
    assert_eq!(ws.stopped.load(Ordering::Relaxed), 0);
    assert!(ws.closed.lock().unwrap().is_empty());
}

#[test]
fn a_project_shows_its_most_urgent_dot_and_the_unseen_ring_clears_when_shown() {
    let quill = project("quill", vec![spec("claude", Claude)]);
    let mut bg = project("bg", vec![spec("a", Claude), spec("b", Codex)]);
    let ws = workspace(&[&quill, &bg]);
    let mut w = W::new(&ws);
    let dot = |w: &W, id: &str| {
        let view = w.ws.view();
        let index = view.projects.iter().position(|p| p.id == id).unwrap();
        let tview = view.projects[index].terminals.as_ref().unwrap().view();
        crate::tree::project_dot(&w.app.seen_for_tests(), &view.projects[index], Some(&tview))
    };
    assert_eq!(dot(&w, "bg"), crate::dots::Dot::Idle);
    // A turn ends in a background project: the ring shows, nobody sent a thing.
    bg.set(vec![
        spec("a", Claude).state(TermActivity::Idle),
        Spec {
            turns: 1,
            ..spec("b", Codex)
        },
    ]);
    w.frame(vec![]);
    assert_eq!(dot(&w, "bg"), crate::dots::Dot::Unseen);
    // Something more urgent beats it.
    bg.set(vec![
        spec("a", Claude).state(TermActivity::NeedsUser),
        Spec {
            turns: 1,
            ..spec("b", Codex)
        },
    ]);
    w.frame(vec![]);
    assert_eq!(dot(&w, "bg"), crate::dots::Dot::NeedsUser);
    bg.set(vec![
        spec("a", Claude).state(TermActivity::Working),
        Spec {
            turns: 1,
            ..spec("b", Codex)
        },
    ]);
    w.frame(vec![]);
    assert_eq!(dot(&w, "bg"), crate::dots::Dot::Working);
    bg.set(vec![
        spec("a", Claude),
        Spec {
            turns: 1,
            ..spec("b", Codex)
        },
    ]);
    w.frame(vec![]);
    assert_eq!(dot(&w, "bg"), crate::dots::Dot::Unseen);
    // Showing the terminal clears the mark, and only its.
    w.click_hit(&Hit::Project("bg".into()));
    w.frame(vec![]);
    w.click_hit(&Hit::Terminal("bg".into(), "b".into()));
    w.frame(vec![]);
    assert_eq!(dot(&w, "bg"), crate::dots::Dot::Idle);
    assert!(!bg.typed_anywhere());
}

#[test]
fn an_agent_adding_a_terminal_does_not_change_what_is_shown() {
    let (mut quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let mut specs = vec![
        spec("claude-1", Claude).named("Ledger retries"),
        spec("codex-1", Codex),
        spec("codex-9", Codex).child_of("claude-1"),
    ];
    specs.push(spec("claude-9", Claude));
    quill.set(specs);
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("quill".into(), "claude-1".into())));
    w.type_text("k");
    assert_eq!(quill.slot("claude-1").typed(), b"k");
    assert!(quill.slot("codex-9").typed().is_empty());
}

#[test]
fn closing_the_shown_terminal_shows_its_neighbour() {
    let (mut quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::Terminal("quill".into(), "codex-1".into()));
    quill.set(vec![
        spec("claude-1", Claude),
        spec("codex-2", Codex).child_of("claude-1"),
        spec("claude-3", Claude).child_of("claude-1"),
    ]);
    w.frame(vec![]);
    assert_eq!(
        w.shown().map(|(_, t)| t).as_deref().map(|t| t == "codex-1"),
        Some(false)
    );
    w.frame(vec![]);
    w.type_text("n");
    let typed: usize = quill.slots.values().map(|slot| slot.typed().len()).sum();
    assert_eq!(
        typed, 1,
        "the new terminal on show gets the key, and one only"
    );
}

#[test]
fn close_on_a_child_calls_close() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    // The close button shows on hover.
    let child = w.rect(&Hit::Terminal("quill".into(), "claude-3".into()));
    w.frame(vec![Event::PointerMoved(child.center())]);
    w.frame(vec![]);
    w.click_hit(&Hit::Close("quill".into(), "claude-3".into()));
    assert_eq!(*quill.terminals.closes.lock().unwrap(), ["claude-3"]);
    assert_eq!(
        w.shown(),
        Some(("quill".into(), "claude-1".into())),
        "a close button does not also show its row"
    );
}

#[test]
fn a_refused_request_shows_its_reason_under_the_header() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    *quill.terminals.refuse.lock().unwrap() = Some("could not save".to_owned());
    let child = w.rect(&Hit::Terminal("quill".into(), "claude-3".into()));
    w.frame(vec![Event::PointerMoved(child.center())]);
    w.frame(vec![]);
    let before = w.app.hits.notice_height;
    w.click_hit(&Hit::Close("quill".into(), "claude-3".into()));
    w.frame(vec![]);
    assert!(w.app.hits.notice_height > before);
    assert_eq!(
        w.app.request_error_for_tests().as_deref(),
        Some("could not save")
    );
}

// ---------- the header and renaming ----------

fn rename_start(w: &mut W) -> Rect {
    let row = w.rect(&Hit::Terminal("quill".into(), "claude-1".into()));
    w.click(row.center());
    w.click(row.center());
    w.frame(vec![]);
    w.frame(vec![]);
    row
}

#[test]
fn a_double_click_on_the_name_renames_in_place_and_enter_saves() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    rename_start(&mut w);
    assert!(w.app.is_renaming());
    // The old name is selected; typing replaces it.
    w.type_text("Ledger v2");
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(
        *quill.terminals.renames.lock().unwrap(),
        [("claude-1".to_owned(), Some("Ledger v2".to_owned()))]
    );
    assert!(!w.app.is_renaming());
    assert!(
        !quill.typed_anywhere(),
        "the typed name did not reach the terminal"
    );
}

#[test]
fn escape_cancels_a_rename_and_an_empty_name_clears_it() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    rename_start(&mut w);
    w.type_text("x");
    w.frame(esc());
    w.frame(vec![]);
    assert!(quill.terminals.renames.lock().unwrap().is_empty());
    assert!(!w.app.is_renaming());
    assert!(
        !quill.typed_anywhere(),
        "Esc did not reach the terminal either"
    );

    // Command-R renames too; deleting everything and pressing Enter clears.
    w.frame(vec![cmd_key(Key::R)]);
    w.frame(vec![]);
    assert!(w.app.is_renaming());
    w.frame(vec![key_down(Key::Backspace, Modifiers::NONE)]);
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(
        *quill.terminals.renames.lock().unwrap(),
        [("claude-1".to_owned(), None)]
    );
}

#[test]
fn a_project_row_renames_the_same_way() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    ws.activate("connecto");
    w.frame(vec![]);
    // Selecting it folded the other project, so the row moved up.
    let row = w.rect(&Hit::Project("connecto".into())).center();
    w.click(row);
    w.click(row);
    w.frame(vec![]);
    w.frame(vec![]);
    assert!(w.app.is_renaming());
    w.type_text("Gateway");
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(
        *ws.renamed.lock().unwrap(),
        [("connecto".to_owned(), Some("Gateway".to_owned()))]
    );
}

#[test]
fn a_refused_rename_reports_why() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    *quill.terminals.refuse.lock().unwrap() = Some("name is too long".to_owned());
    rename_start(&mut w);
    w.type_text("x");
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(
        w.app.request_error_for_tests().as_deref(),
        Some("name is too long")
    );
}

#[test]
fn there_is_no_terminal_header_and_the_terminal_starts_at_the_top() {
    let (_quill, _connecto, ws) = two_projects();
    let w = W::new(&ws);
    // Only the test build's FAKE MODEL banner and, off macOS, the menu bar
    // may sit above it.
    let top = if cfg!(feature = "test-endpoints") {
        22.0
    } else {
        0.0
    } + MENU_BAR;
    assert!(
        (w.app.hits.content.min.y - top).abs() < 0.5,
        "{:?}",
        w.app.hits.content
    );
    assert!((w.app.hits.content.min.x - SIDEBAR_WIDTH).abs() < 0.5);
    assert!(
        (w.app.hits.content.height() - (760.0 - top)).abs() < 0.5,
        "the grid uses the full height"
    );
}

// ---------- the chooser ----------

#[test]
fn the_chooser_adds_the_highlighted_tool_with_its_name_on_enter() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::T)]);
    w.frame(vec![]);
    assert!(w.app.chooser_open());
    w.type_text("Write tests");
    w.frame(vec![key_down(Key::ArrowDown, Modifiers::NONE)]);
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(
        *quill.terminals.adds.lock().unwrap(),
        [(Codex, Some("Write tests".to_owned()))]
    );
    assert!(!w.app.chooser_open());
    assert!(
        !quill.typed_anywhere(),
        "neither the name nor Enter reached a terminal"
    );
}

#[test]
fn a_click_on_a_tool_adds_it_and_the_new_terminal_comes_to_the_front() {
    let (mut quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::NewRow("quill".into()));
    w.frame(vec![]);
    assert!(w.app.chooser_open());
    let claude = w.app.hits.chooser.choices[0].center();
    w.click(claude);
    assert_eq!(*quill.terminals.adds.lock().unwrap(), [(Claude, None)]);
    // The registry adds it; the window shows the terminal the user asked for.
    let mut specs = vec![
        spec("claude-1", Claude),
        spec("codex-2", Codex).child_of("claude-1"),
        spec("claude-3", Claude).child_of("claude-1"),
        spec("codex-1", Codex),
    ];
    specs.push(spec("claude-4", Claude));
    quill.set(specs);
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("quill".into(), "claude-4".into())));
}

#[test]
fn escape_closes_the_chooser_without_adding() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::T)]);
    w.frame(vec![]);
    w.frame(esc());
    w.frame(vec![]);
    assert!(!w.app.chooser_open());
    assert!(quill.terminals.adds.lock().unwrap().is_empty());
    assert!(!quill.typed_anywhere());
}

#[test]
fn new_claude_and_new_codex_add_at_once_without_a_chooser() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![key_down(Key::Num1, Modifiers { alt: true, ..cmd() })]);
    w.frame(vec![key_down(Key::Num2, Modifiers { alt: true, ..cmd() })]);
    assert_eq!(
        *quill.terminals.adds.lock().unwrap(),
        [(Claude, None), (Codex, None)]
    );
    assert!(!quill.typed_anywhere());
}

#[test]
fn the_chooser_says_what_it_knows_of_each_tools_login() {
    let (quill, _connecto, ws) = two_projects();
    *quill.slot("codex-1").control.stage.lock().unwrap() = PaneStage::NeedsLogin {
        auth: AuthState::LoggedOut,
    };
    let view = quill.terminals.view();
    assert_eq!(crate::tree::tool_login_hint(&view, Claude), "logged in");
    assert_eq!(crate::tree::tool_login_hint(&view, Codex), "not logged in");
    let _ = ws;
}

// ---------- history ----------

fn closed(
    id: &str,
    terminal: &str,
    name: Option<&str>,
    tool: ToolKind,
    at_ms: u64,
) -> HistoryEntry {
    HistoryEntry {
        id: id.to_owned(),
        terminal_id: terminal.to_owned(),
        name: name.map(str::to_owned),
        tool,
        parent: None,
        closed_at_ms: at_ms,
        task: Some(format!("task of {terminal}")),
        last_reply: Some(format!("reply of {terminal}")),
        resumable: id != "h3",
    }
}

const NOW: u64 = 1_794_139_200_000;
const HOUR: u64 = 3_600_000;

fn with_history() -> (Proj, Proj, Arc<FakeWorkspace>) {
    let (quill, connecto, ws) = two_projects();
    quill.terminals.edit(|view| {
        view.history = Arc::new(vec![
            closed(
                "h1",
                "claude-5",
                Some("Write 503 test cases"),
                Claude,
                NOW - HOUR,
            ),
            closed(
                "h2",
                "codex-4",
                Some("Map gateway errors"),
                Codex,
                NOW - 14 * HOUR,
            ),
            closed("h3", "claude-2", None, Claude, NOW - 60 * HOUR),
        ]);
    });
    connecto.terminals.edit(|view| {
        view.history = Arc::new(vec![closed(
            "m1",
            "claude-7",
            Some("Load test"),
            Claude,
            NOW - 2 * HOUR,
        )]);
    });
    (quill, connecto, ws)
}

fn open_history(w: &mut W) {
    w.frame(vec![cmd_key(Key::Y)]);
    w.frame(vec![]);
    w.frame(vec![]);
    assert!(w.app.history_open());
}

fn listed(w: &W) -> Vec<String> {
    let mut rows: Vec<(f32, String)> = w
        .app
        .hits
        .history
        .rows
        .iter()
        .map(|((_, id), rect)| (rect.min.y, id.clone()))
        .collect();
    rows.sort_by(|a, b| a.0.total_cmp(&b.0));
    rows.into_iter().map(|(_, id)| id).collect()
}

#[test]
fn history_lists_this_project_newest_first_and_every_project_on_request() {
    let (_quill, _connecto, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    assert_eq!(listed(&w), ["h1", "h2", "h3"]);
    let all = w.app.hits.history.filters[1].center();
    w.click(all);
    w.frame(vec![]);
    assert_eq!(listed(&w), ["h1", "m1", "h2", "h3"]);
    let codex = w.app.hits.history.filters[3].center();
    w.click(codex);
    w.frame(vec![]);
    assert_eq!(listed(&w), ["h2"]);
    w.click(codex);
    w.frame(vec![]);
    let this = w.app.hits.history.filters[0].center();
    w.click(this);
    w.frame(vec![]);
    assert_eq!(listed(&w), ["h1", "h2", "h3"]);
}

#[test]
fn typing_in_the_search_filters_on_name_id_task_and_reply() {
    let (quill, _connecto, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    w.type_text("503");
    w.frame(vec![]);
    assert_eq!(listed(&w), ["h1"]);
    w.frame(vec![key_down(Key::A, cmd())]);
    w.type_text("REPLY OF CODEX-4");
    w.frame(vec![]);
    assert_eq!(listed(&w), ["h2"]);
    w.frame(vec![key_down(Key::A, cmd())]);
    w.type_text("zzz");
    w.frame(vec![]);
    assert!(listed(&w).is_empty());
    assert!(
        !quill.typed_anywhere(),
        "search text never reaches a terminal"
    );
}

#[test]
fn continue_resumes_the_selected_entry_and_a_transcript_only_entry_offers_no_continue() {
    let (quill, _connecto, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    // The first row is selected; click the second to select it.
    let second = w.app.hits.history.rows[1].1.center();
    w.click(second);
    w.frame(vec![]);
    let button = w.app.hits.history.continue_button.unwrap().center();
    w.click(button);
    assert_eq!(*quill.terminals.continues.lock().unwrap(), ["h2"]);
    assert!(!w.app.history_open(), "continuing closes the sheet");

    // h3 has no session to resume.
    open_history(&mut w);
    let third = w.app.hits.history.rows[2].1.center();
    w.click(third);
    w.frame(vec![]);
    assert!(w.app.hits.history.continue_button.is_none());
    assert!(w.app.hits.history.view_button.is_some());
}

#[test]
fn view_transcript_loads_it_and_command_c_copies_the_selection() {
    let (quill, _connecto, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    let view = w.app.hits.history.view_button.unwrap().center();
    w.click(view);
    w.frame(vec![]);
    assert_eq!(*quill.terminals.transcripts.lock().unwrap(), ["h1"]);
    // The registry loads it off-thread.
    quill.terminals.edit(|v| {
        v.transcript = Some(Transcript {
            history_id: "h1".to_owned(),
            text: Arc::new("line one\nline two\n".to_owned()),
        });
    });
    w.frame(vec![]);
    w.frame(vec![]);
    w.frame(vec![key_down(Key::A, cmd())]);
    w.frame(vec![Event::Copy]);
    assert_eq!(
        w.copied.last().map(String::as_str),
        Some("line one\nline two\n")
    );
    assert!(!quill.typed_anywhere());
    // The text is read-only: typing changes nothing and Esc goes back.
    w.type_text("zzz");
    w.frame(esc());
    w.frame(vec![]);
    assert!(w.app.history_open());
    assert!(w.app.hits.history.transcript.is_none());
}

#[test]
fn escape_closes_the_sheet_and_gives_the_keyboard_back_without_a_stray_key() {
    let (quill, _connecto, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    w.frame(esc());
    w.frame(vec![]);
    assert!(!w.app.history_open());
    w.frame(vec![]);
    w.type_text("k");
    assert_eq!(quill.slot("claude-1").typed(), b"k", "no Esc byte first");
}

#[test]
fn empty_history_says_so() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_history(&mut w);
    assert!(listed(&w).is_empty());
}

// ---------- the rail ----------

#[test]
fn the_sidebar_collapses_to_a_rail_of_tiles_with_dots_and_back() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert!((w.app.hits.sidebar.width() - SIDEBAR_WIDTH).abs() < 0.5);
    w.frame(vec![cmd_key(Key::Backslash)]);
    w.frame(vec![]);
    assert!(ws.view.lock().unwrap().sidebar_collapsed);
    assert!((w.app.hits.sidebar.width() - RAIL_WIDTH).abs() < 0.5);
    assert!(w.has(&Hit::Tile("quill".into())) && w.has(&Hit::Tile("connecto".into())));
    assert!(!w.has(&Hit::Terminal("quill".into(), "claude-1".into())));
    w.click_hit(&Hit::Tile("connecto".into()));
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("connecto".into(), "claude".into())));
    // The terminal header now reaches the traffic lights, and drags the window.
    w.click_hit(&Hit::Collapse);
    w.frame(vec![]);
    assert!(!ws.view.lock().unwrap().sidebar_collapsed);
    assert!(w.has(&Hit::Terminal("connecto".into(), "claude".into())));
}

// ---------- window drag ----------

#[test]
fn the_sidebar_header_drags_the_window_and_double_click_zooms() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let header = w.rect(&Hit::Header);
    let spot = Pos2::new(header.center().x - 40.0, header.min.y + LIGHTS_BAND / 2.0);
    let mut commands = w.press(spot);
    commands.extend(w.frame(vec![Event::PointerMoved(spot + Vec2::new(30.0, 0.0))]));
    commands.extend(w.frame(vec![Event::PointerMoved(spot + Vec2::new(60.0, 0.0))]));
    assert!(is_drag(&commands), "{commands:?}");
    w.release(spot + Vec2::new(60.0, 0.0));
    let mut commands = Vec::new();
    for _ in 0..2 {
        commands.extend(w.frame(vec![Event::PointerMoved(spot)]));
        commands.extend(w.frame(vec![button(spot, true)]));
        commands.extend(w.frame(vec![button(spot, false)]));
    }
    assert!(
        commands
            .iter()
            .any(|c| matches!(c, ViewportCommand::Maximized(true))),
        "{commands:?}"
    );
}

#[test]
fn a_press_on_a_header_button_never_starts_a_window_drag() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let plus = w.rect(&Hit::HeaderPlus);
    let start = plus.center();
    let mut commands = w.press(start);
    commands.extend(w.frame(vec![Event::PointerMoved(start + Vec2::new(6.0, 0.0))]));
    commands.extend(w.frame(vec![Event::PointerMoved(start + Vec2::new(8.0, 0.0))]));
    commands.extend(w.release(start + Vec2::new(8.0, 0.0)));
    assert!(!is_drag(&commands), "{commands:?}");
}

#[test]
fn the_plus_menu_asks_for_a_folder_and_opens_it() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::HeaderPlus);
    assert!(ws.opened.lock().unwrap().is_empty(), "a menu first");
    w.click_hit(&Hit::PlusOpenProject);
    w.frame(vec![]);
    assert_eq!(
        *ws.opened.lock().unwrap(),
        [PathBuf::from("/picked/folder")]
    );
}

#[test]
fn nothing_in_the_terminal_area_starts_a_window_drag() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    for spot in [Pos2::new(700.0, 4.0), Pos2::new(700.0, 300.0)] {
        let mut commands = w.press(spot);
        commands.extend(w.frame(vec![Event::PointerMoved(spot + Vec2::new(30.0, 0.0))]));
        commands.extend(w.frame(vec![Event::PointerMoved(spot + Vec2::new(60.0, 0.0))]));
        assert!(!is_drag(&commands), "{spot:?}");
        w.release(spot + Vec2::new(60.0, 0.0));
    }
}

#[test]
fn the_rail_top_strip_drags_the_window_but_not_from_its_buttons() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::Backslash)]);
    w.frame(vec![]);
    let strip = w.rect(&Hit::Header);
    let tile = w.rect(&Hit::Tile("quill".into()));
    assert!(
        strip.max.y <= tile.min.y + 0.5,
        "the strip is above the first tile"
    );
    // Left of the buttons: off macOS there is no band above them.
    let blank = Pos2::new(strip.min.x + 4.0, strip.min.y + 10.0);
    let mut commands = w.press(blank);
    commands.extend(w.frame(vec![Event::PointerMoved(blank + Vec2::new(0.0, 5.0))]));
    commands.extend(w.frame(vec![Event::PointerMoved(blank + Vec2::new(0.0, 10.0))]));
    assert!(is_drag(&commands), "{commands:?}");
    w.release(blank);
    let button = w.rect(&Hit::Collapse).center();
    let mut commands = w.press(button);
    commands.extend(w.frame(vec![Event::PointerMoved(button + Vec2::new(2.0, 0.0))]));
    commands.extend(w.frame(vec![Event::PointerMoved(button + Vec2::new(4.0, 0.0))]));
    assert!(!is_drag(&commands));
}

#[test]
fn command_r_with_a_collapsed_sidebar_or_project_shows_the_row_to_rename() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::Backslash)]);
    w.frame(vec![]);
    w.click_hit(&Hit::Tile("quill".into()));
    w.frame(vec![cmd_key(Key::R)]);
    w.frame(vec![]);
    w.frame(vec![]);
    assert!(!ws.view.lock().unwrap().sidebar_collapsed);
    assert!(w.app.is_renaming());
    w.type_text("N");
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(quill.terminals.renames.lock().unwrap().len(), 1);
    assert!(!quill.typed_anywhere());
}

#[test]
fn a_rename_nothing_can_draw_is_dropped_rather_than_holding_the_keyboard() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::R)]);
    w.frame(vec![]);
    assert!(w.app.is_renaming());
    // The sidebar collapses under it: no row can show the field.
    ws.edit(|view| view.sidebar_collapsed = true);
    for _ in 0..4 {
        w.frame(vec![]);
    }
    assert!(!w.app.is_renaming());
    w.type_text("k");
    assert_eq!(quill.slot("claude-1").typed(), b"k");
}

// ---------- menu shortcuts ----------

#[test]
fn no_menu_shortcut_reaches_a_terminal() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let mods = |m: Modifiers| m;
    let shift = Modifiers {
        shift: true,
        ..cmd()
    };
    let alt = Modifiers { alt: true, ..cmd() };
    let chords = [
        (Key::O, cmd()),
        (Key::T, cmd()),
        (Key::Num1, alt),
        (Key::Num2, alt),
        (Key::R, cmd()),
        (Key::Y, cmd()),
        (Key::Period, cmd()),
        (Key::Backslash, cmd()),
        (Key::J, cmd()),
        (Key::Num1, cmd()),
        (Key::Num2, cmd()),
        (Key::K, shift),
        (Key::Tab, mods(Modifiers::CTRL)),
        (
            Key::Tab,
            Modifiers {
                shift: true,
                ctrl: true,
                ..Modifiers::NONE
            },
        ),
        (Key::W, shift),
    ];
    for (key, modifiers) in chords {
        w.frame(vec![key_down(key, modifiers)]);
        // Close whatever opened, so the next chord meets a plain window.
        if w.app.chooser_open()
            || w.app.history_open()
            || w.app.utility_open()
            || w.app.automations_open()
            || w.app.is_renaming()
        {
            w.frame(esc());
        }
        w.frame(vec![]);
        w.frame(vec![]);
        // And go back to the first terminal.
        if w.ws.view().sidebar_collapsed {
            w.frame(vec![cmd_key(Key::Backslash)]);
        }
        assert!(
            !quill.typed_anywhere() && !connecto.typed_anywhere(),
            "{key:?} {modifiers:?} reached a terminal: {:?}",
            quill.slot("claude-1").typed()
        );
    }
}

#[test]
fn with_a_native_menu_the_chord_is_dropped_and_not_acted_on_twice() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.app.set_menu_owns_shortcuts(true);
    w.frame(vec![cmd_key(Key::T), key_down(Key::Tab, Modifiers::CTRL)]);
    w.frame(vec![]);
    assert!(!w.app.chooser_open(), "the menu acts, not the key");
    assert!(!quill.typed_anywhere());
    // The menu's click arrives as a command.
    w.app.queue_for_tests(crate::commands::Command::NewTerminal);
    w.frame(vec![]);
    w.frame(vec![]);
    assert!(w.app.chooser_open());
}

#[test]
fn control_tab_cycles_terminals_and_command_digits_switch_projects() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![key_down(Key::Tab, Modifiers::CTRL)]);
    assert_eq!(w.shown(), Some(("quill".into(), "codex-2".into())));
    w.frame(vec![key_down(
        Key::Tab,
        Modifiers {
            shift: true,
            ctrl: true,
            ..Modifiers::NONE
        },
    )]);
    assert_eq!(w.shown(), Some(("quill".into(), "claude-1".into())));
    w.frame(vec![cmd_key(Key::Num2)]);
    assert_eq!(w.shown(), Some(("connecto".into(), "claude".into())));
    w.frame(vec![cmd_key(Key::Num1)]);
    assert_eq!(w.shown().map(|(p, _)| p).as_deref(), Some("quill"));
}

#[test]
fn next_needing_me_finds_a_waiting_terminal_in_any_project() {
    let quill = project("quill", vec![spec("claude", Claude)]);
    let connecto = project(
        "connecto",
        vec![
            spec("a", Claude),
            spec("b", Codex).state(TermActivity::NeedsUser),
        ],
    );
    let ws = workspace(&[&quill, &connecto]);
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::J)]);
    assert_eq!(w.shown(), Some(("connecto".into(), "b".into())));
    // Nothing else waits: it stays.
    w.frame(vec![cmd_key(Key::J)]);
    assert_eq!(w.shown(), Some(("connecto".into(), "b".into())));
    assert!(!quill.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn close_terminal_close_project_pause_and_stop_everything_reach_the_workspace() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::W)]);
    assert_eq!(*quill.terminals.closes.lock().unwrap(), ["claude-1"]);
    w.frame(vec![key_down(
        Key::W,
        Modifiers {
            shift: true,
            ..cmd()
        },
    )]);
    assert_eq!(*ws.closed.lock().unwrap(), ["quill"]);
    assert!(!ws.pause.is_paused());
    w.frame(vec![cmd_key(Key::Period)]);
    assert!(ws.pause.is_paused());
    w.frame(vec![cmd_key(Key::Period)]);
    assert!(!ws.pause.is_paused());
    w.app
        .queue_for_tests(crate::commands::Command::StopEverything);
    w.frame(vec![]);
    assert_eq!(ws.stopped.load(Ordering::Relaxed), 1);
    w.app
        .queue_for_tests(crate::commands::Command::ClearRecents);
    w.frame(vec![]);
    assert_eq!(ws.cleared.load(Ordering::Relaxed), 1);
}

#[test]
fn close_finished_children_closes_only_done_children() {
    let quill = project(
        "quill",
        vec![
            spec("claude-1", Claude),
            spec("codex-2", Codex)
                .child_of("claude-1")
                .state(TermActivity::Done),
            spec("claude-3", Claude)
                .child_of("claude-1")
                .state(TermActivity::Working),
            spec("claude-4", Claude)
                .child_of("claude-1")
                .state(TermActivity::Done),
            spec("codex-9", Codex).state(TermActivity::Done),
        ],
    );
    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    w.frame(vec![key_down(
        Key::K,
        Modifiers {
            shift: true,
            ..cmd()
        },
    )]);
    assert_eq!(
        *quill.terminals.closes.lock().unwrap(),
        ["codex-2", "claude-4"]
    );
}

/// A saved terminal that waited at launch starts when it is the one on show,
/// once, and one that is not looked at stays as it is.
#[test]
fn a_waiting_terminal_starts_when_it_is_shown_and_only_then() {
    let waiting = |project: &Proj| {
        project.terminals.edit(|view| {
            for entry in &mut view.terminals {
                if entry.state == TermActivity::NotStarted {
                    entry.desired_running = false;
                }
            }
        });
    };
    let quill = project(
        "quill",
        vec![spec("claude", Claude).state(TermActivity::NotStarted)],
    );
    waiting(&quill);
    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    w.frame(vec![]);
    assert_eq!(*quill.terminals.restarts.lock().unwrap(), ["claude"]);

    let other = project(
        "other",
        vec![
            spec("claude", Claude),
            spec("codex", Codex).state(TermActivity::NotStarted),
        ],
    );
    waiting(&other);
    let ws = workspace(&[&other]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    assert_eq!(w.app.shown_for_tests().unwrap().1, "claude");
    assert!(other.terminals.restarts.lock().unwrap().is_empty());
}

#[test]
fn an_exited_terminal_offers_restart_in_the_pane_and_a_click_asks_the_registry() {
    let quill = project(
        "quill",
        vec![spec("claude", Claude).state(TermActivity::Exited)],
    );
    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    let key = ("quill".to_owned(), "claude".to_owned());
    let restart = w.app.pane_hits(&key)[0];
    assert!(
        restart.min.y > w.app.hits.content.center().y,
        "inside the pane, not a header"
    );
    w.click(restart.center());
    assert_eq!(*quill.terminals.restarts.lock().unwrap(), ["claude"]);
    assert!(!quill.typed_anywhere());
}

#[test]
fn a_failed_terminal_shows_its_reason_with_log_in_and_restart_and_no_start() {
    let quill = project(
        "quill",
        vec![spec("claude", Claude).state(TermActivity::Failed)],
    );
    *quill.slot("claude").control.stage.lock().unwrap() = PaneStage::Failed {
        reason: "no".into(),
    };
    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    let key = ("quill".to_owned(), "claude".to_owned());
    assert_eq!(w.app.pane_hits(&key).len(), 2, "Log in and Restart");
    let restart = w.app.pane_hits(&key)[1].center();
    w.click(restart);
    assert_eq!(*quill.terminals.restarts.lock().unwrap(), ["claude"]);
    assert_eq!(*quill.slot("claude").control.starts.lock().unwrap(), 0);
}

// ---------- the pulse ----------

#[test]
fn the_window_repaints_for_the_pulse_only_while_something_works() {
    let mut quill = project("quill", vec![spec("claude", Claude)]);
    let mut bg = project("bg", vec![spec("a", Codex)]);
    let ws = workspace(&[&quill, &bg]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    assert_eq!(
        w.repaint_delay,
        std::time::Duration::MAX,
        "idle windows do not repaint"
    );
    // Work in a background project still animates its dot.
    bg.set(vec![spec("a", Codex).state(TermActivity::Working)]);
    w.frame(vec![]);
    assert!(
        w.repaint_delay < std::time::Duration::from_millis(200),
        "{:?}",
        w.repaint_delay
    );
    bg.set(vec![spec("a", Codex)]);
    w.frame(vec![]);
    w.frame(vec![]);
    assert_eq!(w.repaint_delay, std::time::Duration::MAX);
    quill.set(vec![spec("claude", Claude).state(TermActivity::Working)]);
    w.frame(vec![]);
    assert!(w.repaint_delay < std::time::Duration::from_millis(200));
}

// ---------- empty states ----------

#[test]
fn with_no_project_the_window_invites_one_and_lists_the_recents() {
    let ws = workspace(&[]);
    ws.edit(|view| {
        view.recents = vec![
            RecentProject {
                path: "/work/quill".into(),
                name: "quill".into(),
            },
            RecentProject {
                path: "/work/connecto".into(),
                name: "connecto".into(),
            },
        ];
    });
    let mut w = W::new(&ws);
    let open = w.app.hits.open_button.unwrap().center();
    assert_eq!(w.app.hits.recent_rows.len(), 2);
    w.click(open);
    w.frame(vec![]);
    assert_eq!(
        *ws.opened.lock().unwrap(),
        [PathBuf::from("/picked/folder")]
    );
    let second = w.app.hits.recent_rows[1].center();
    w.click(second);
    w.frame(vec![]);
    assert_eq!(
        ws.opened.lock().unwrap()[1],
        PathBuf::from("/work/connecto")
    );
    // The sidebar has its header and no rows.
    assert!(w.has(&Hit::HeaderPlus));
}

#[test]
fn the_chooser_offers_a_plain_shell_last_and_a_click_adds_it() {
    let empty = project("empty", vec![]);
    let ws = workspace(&[&empty]);
    let mut w = W::new(&ws);
    // Claude, Codex, Pi, Antigravity, OpenCode, then the shell.
    assert_eq!(w.app.hits.inline.choices.len(), 6);
    let shell = w.app.hits.inline.choices[5].center();
    w.click(shell);
    assert_eq!(
        *empty.terminals.adds.lock().unwrap(),
        [(connecto_core::ToolKind::Shell, None)]
    );
}

#[test]
fn a_project_with_no_terminals_shows_the_chooser_in_place() {
    let empty = project("empty", vec![]);
    let ws = workspace(&[&empty]);
    let mut w = W::new(&ws);
    assert_eq!(w.app.hits.inline.choices.len(), 6);
    let codex = w.app.hits.inline.choices[1].center();
    w.click(codex);
    assert_eq!(*empty.terminals.adds.lock().unwrap(), [(Codex, None)]);
}

#[test]
fn repeating_backspace_in_the_inline_name_keeps_the_caret_at_the_end() {
    let empty = project("empty", vec![]);
    let ws = workspace(&[&empty]);
    let mut w = W::new(&ws);
    let id = egui::Id::unique("chooser-name");
    let field = w.app.hits.inline.name.unwrap().center();
    w.click(field);
    w.frame(vec![]);
    w.type_text("abcdefghijklmnopqrstuvwxyzabcdefghijklmnopqrstuvwxyz");
    w.frame(vec![]);
    let caret = |w: &W| {
        egui::TextEdit::load_state(&w.ctx, id)
            .and_then(|state| state.cursor.char_range())
            .map(|range| range.primary.index.0)
    };
    let mut left: usize = 52;
    assert_eq!(caret(&w), Some(left));
    for per_frame in [1, 3, 7] {
        while left > 0 && left > 52 - per_frame * 5 {
            let backspace = Event::Key {
                key: Key::Backspace,
                physical_key: None,
                pressed: true,
                repeat: true,
                modifiers: Modifiers::NONE,
            };
            let n = per_frame.min(left);
            w.frame(vec![backspace; n]);
            left -= n;
            assert_eq!(caret(&w), Some(left), "{per_frame} per frame");
        }
    }
}

#[test]
fn a_project_that_failed_or_is_opening_says_so() {
    let ws = workspace(&[]);
    ws.edit(|view| {
        view.projects = vec![ProjectView {
            id: "gone".into(),
            name: "gone".into(),
            path: "/gone".into(),
            terminals: None,
            error: Some("The folder no longer exists.".into()),
            expanded: true,
            active_terminal: None,
            group: None,
        }];
        view.active = Some("gone".into());
    });
    let mut w = W::new(&ws);
    w.frame(vec![]);
    assert!(w.app.hits.open_button.is_none());
}

#[test]
fn a_standing_warning_and_the_save_state_show_under_the_header() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let plain = w.app.hits.notice_height;
    quill.terminals.edit(|view| {
        view.save = SaveState::Unsaved;
        view.warning = Some("terminals.json was unreadable".to_owned());
        view.last_error = Some("disk full".to_owned());
    });
    ws.edit(|view| view.warning = Some("workspace.json was unreadable".to_owned()));
    w.frame(vec![]);
    assert!(w.app.hits.notice_height > plain);
    let texts: Vec<String> = w
        .app
        .notices_for_tests()
        .into_iter()
        .map(|(text, _)| text)
        .collect();
    assert_eq!(texts.len(), 4, "{texts:?}");
}

// ---------- keyboard rules kept from the tab strip ----------

#[test]
fn clicking_anything_in_the_sidebar_never_types_into_a_pty() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    for hit in [
        Hit::Project("connecto".into()),
        Hit::Twisty("quill".into()),
        Hit::Terminal("quill".into(), "codex-1".into()),
        Hit::Twisty("quill".into()),
        Hit::Twisty("quill".into()),
        Hit::Pause,
        Hit::Pause,
        Hit::Collapse,
        Hit::Tile("quill".into()),
        Hit::Collapse,
    ] {
        w.click_hit(&hit);
        w.frame(vec![]);
    }
    assert!(!quill.typed_anywhere() && !connecto.typed_anywhere());
}

/// Whatever sidebar control has the keyboard, Enter and Space stay away from
/// the terminal.
#[test]
fn no_activation_key_leaks_into_the_terminal_from_any_control() {
    for key in [Key::Enter, Key::Space] {
        let (quill, connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        let ids: Vec<egui::Id> = vec![
            egui::Id::unique(("tree-terminal", "quill", "codex-1")),
            egui::Id::unique(("tree-project", "connecto")),
            egui::Id::unique(("tree-new", "quill")),
            egui::Id::unique("pause-toggle"),
        ];
        for id in ids {
            w.ctx.memory_mut(|memory| memory.request_focus(id));
            w.frame(vec![]);
            let mut events = vec![key_event(key, true)];
            if key == Key::Space {
                events.push(Event::Text(" ".into()));
            }
            events.push(key_event(key, false));
            w.frame(events);
            w.frame(vec![]);
            // Close whatever the key opened.
            if w.app.chooser_open() || w.app.history_open() || w.app.is_renaming() {
                w.frame(esc());
            }
            w.frame(vec![]);
            assert!(
                !quill.typed_anywhere() && !connecto.typed_anywhere(),
                "{id:?} with {key:?} leaked"
            );
        }
    }
}

// ---------- the panes: input, selection, login ----------

fn one_terminal() -> (Proj, W) {
    let quill = project("quill", vec![spec("claude", Claude), spec("codex", Codex)]);
    let ws = workspace(&[&quill]);
    let w = W::new(&ws);
    (quill, w)
}

fn fill(proj: &Proj, w: &mut W, key: (&str, &str), text: &str) {
    let cols = 100_u16;
    let mut cells = vec![connecto_core::Cell::default(); usize::from(cols) * 40];
    for (cell, ch) in cells.iter_mut().zip(text.chars()) {
        cell.ch = ch;
    }
    *proj.slot(key.1).session.screen.lock().unwrap() = cells
        .chunks(usize::from(cols))
        .map(|row| row.iter().map(|cell| cell.ch).collect())
        .collect();
    let pane = w
        .app
        .pane_mut(&(key.0.to_owned(), key.1.to_owned()))
        .unwrap();
    pane.snapshot = TermSnapshot {
        cols,
        rows: 40,
        cells,
        ..TermSnapshot::default()
    };
}

fn drag(w: &mut W, from: Pos2, to: Pos2) {
    w.frame(vec![Event::PointerMoved(from)]);
    w.frame(vec![button(from, true)]);
    w.frame(vec![Event::PointerMoved(to)]);
    w.frame(vec![Event::PointerMoved(to)]);
    w.frame(vec![button(to, false)]);
}

fn cell_origin(w: &W) -> Pos2 {
    w.app.hits.content.min + Vec2::splat(PANE_PADDING)
}

#[test]
fn a_drag_then_cmd_c_copies_exactly_the_selected_cells() {
    let (quill, mut w) = one_terminal();
    fill(
        &quill,
        &mut w,
        ("quill", "claude"),
        "https://example.test/code",
    );
    let cell = w.app.cell_for_tests();
    let at = |col: f32| cell_origin(&w) + Vec2::new((col + 0.5) * cell.width, cell.height * 0.5);
    let (from, to) = (at(8.0), at(19.0));
    drag(&mut w, from, to);
    assert!(w.copied.is_empty(), "a drag alone must not copy");
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied, ["example.test"]);
    assert!(!quill.typed_anywhere());
}

#[test]
fn a_double_click_selects_the_whole_word_and_cmd_shift_c_copies_it() {
    let (quill, mut w) = one_terminal();
    fill(
        &quill,
        &mut w,
        ("quill", "claude"),
        "see src/main.rs:12, now",
    );
    let session = Arc::clone(&quill.slot("claude").session);
    let cell = w.app.cell_for_tests();
    let at = cell_origin(&w) + Vec2::new(6.5 * cell.width, cell.height * 0.5);
    w.frame(vec![Event::PointerMoved(at)]);
    w.frame(vec![button(at, true), button(at, false)]);
    w.frame(vec![button(at, true), button(at, false)]);
    w.frame(vec![]);
    let selects = session.selects.lock().unwrap().clone();
    assert_eq!(
        selects[selects.len() - 2..],
        [
            SelectOp::Start { row: 0, col: 4 },
            SelectOp::Extend { row: 0, col: 17 }
        ],
        "{selects:?}"
    );
    w.frame(vec![key_down(
        Key::C,
        Modifiers::COMMAND | Modifiers::SHIFT,
    )]);
    assert_eq!(w.copied, ["src/main.rs:12"]);
    assert!(!quill.typed_anywhere());
}

/// A selection is started once and then only extended, so the session keeps
/// its start on the text while the view scrolls. Held above the screen the
/// drag scrolls back through history and keeps extending.
#[test]
fn a_drag_held_past_the_top_scrolls_history_and_keeps_its_start() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "some text");
    let session = Arc::clone(&quill.slot("claude").session);
    let cell = w.app.cell_for_tests();
    let origin = cell_origin(&w);
    let from = origin + Vec2::new(cell.width * 3.5, cell.height * 5.5);
    let above = origin + Vec2::new(cell.width * 1.5, -cell.height * 2.5);
    w.frame(vec![Event::PointerMoved(from)]);
    w.frame(vec![button(from, true)]);
    w.frame(vec![Event::PointerMoved(above)]);
    w.frame(vec![Event::PointerMoved(above)]);
    w.frame(vec![]);
    let scrolls = session.scrolls.lock().unwrap().clone();
    // Three rows past the top, at one step per interval whatever the frame rate.
    assert!(
        !scrolls.is_empty() && scrolls.iter().all(|lines| *lines == 3),
        "{scrolls:?}"
    );
    w.frame(vec![button(above, false)]);
    let selects = session.selects.lock().unwrap().clone();
    assert_eq!(selects[0], SelectOp::Start { row: 5, col: 3 });
    assert!(
        selects[1..]
            .iter()
            .all(|op| *op == SelectOp::Extend { row: 0, col: 1 }),
        "{selects:?}"
    );
    assert!(selects.len() >= 3);
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied, ["ome text"]);
}

/// A drag end or a clear the busy emulator refused is sent once it is free,
/// even after the button is up, so a copy gets the range the user made.
#[test]
fn a_selection_change_refused_while_busy_is_sent_later() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "0123456789");
    let session = Arc::clone(&quill.slot("claude").session);
    let cell = w.app.cell_for_tests();
    let at = |col: f32| cell_origin(&w) + Vec2::new((col + 0.5) * cell.width, cell.height * 0.5);
    let (from, mid, to) = (at(1.0), at(3.0), at(7.0));
    w.frame(vec![Event::PointerMoved(from)]);
    w.frame(vec![button(from, true)]);
    w.frame(vec![Event::PointerMoved(mid)]);
    w.frame(vec![Event::PointerMoved(mid)]);
    session.busy.store(true, Ordering::Relaxed);
    w.frame(vec![Event::PointerMoved(to)]);
    w.frame(vec![button(to, false)]);
    w.frame(vec![]);
    assert_eq!(
        session.selects.lock().unwrap().last(),
        Some(&SelectOp::Extend { row: 0, col: 3 })
    );
    session.busy.store(false, Ordering::Relaxed);
    w.frame(vec![]);
    assert_eq!(
        session.selects.lock().unwrap().last(),
        Some(&SelectOp::Extend { row: 0, col: 7 })
    );
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied, ["1234567"]);

    // A click clears; refused while busy, the clear still reaches the session.
    w.frame(vec![cmd_key(Key::A)]);
    session.busy.store(true, Ordering::Relaxed);
    w.frame(vec![button(from, true)]);
    w.frame(vec![button(from, false)]);
    w.frame(vec![]);
    assert_ne!(
        session.selects.lock().unwrap().last(),
        Some(&SelectOp::Clear)
    );
    session.busy.store(false, Ordering::Relaxed);
    w.frame(vec![]);
    assert_eq!(
        session.selects.lock().unwrap().last(),
        Some(&SelectOp::Clear)
    );
    assert_eq!(session.selection_text(), SelectionText::Nothing);
}

/// A selection whose text has gone is not copied: on macOS nothing happens.
#[cfg(target_os = "macos")]
#[test]
fn a_selection_whose_text_has_gone_copies_nothing() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "text");
    w.frame(vec![cmd_key(Key::A)]);
    *quill.slot("claude").session.selected.lock().unwrap() = None;
    w.frame(vec![Event::Copy]);
    assert!(w.copied.is_empty());
    let key = ("quill".to_owned(), "claude".to_owned());
    assert!(w.app.pane_mut(&key).unwrap().selection.is_none());
    assert!(!quill.typed_anywhere());
}

#[cfg(target_os = "macos")]
#[test]
fn cmd_c_without_a_selection_copies_nothing_and_sends_nothing() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "secret on screen");
    w.frame(vec![Event::Copy]);
    assert!(w.copied.is_empty());
    assert!(!quill.typed_anywhere());
}

/// Off macOS Ctrl+C arrives as Copy: with nothing selected it interrupts,
/// and after a copy the selection is gone so the next one interrupts too.
#[cfg(not(target_os = "macos"))]
#[test]
fn ctrl_c_without_a_selection_interrupts_and_copies_nothing() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "secret on screen");
    w.frame(vec![Event::Copy]);
    assert!(w.copied.is_empty());
    assert_eq!(quill.slot("claude").typed(), [3]);
    w.frame(vec![Event::Cut]);
    assert_eq!(quill.slot("claude").typed(), [3, 0x18]);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn ctrl_c_on_a_selection_copies_it_once_then_interrupts() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "first line");
    w.frame(vec![key_down(
        Key::A,
        Modifiers {
            ctrl: true,
            shift: true,
            command: true,
            ..Modifiers::NONE
        },
    )]);
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied.len(), 1);
    assert!(w.copied[0].starts_with("first line"));
    assert!(!quill.typed_anywhere());
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied.len(), 1);
    assert_eq!(quill.slot("claude").typed(), [3]);
}

#[test]
fn typing_clears_the_selection() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "text");
    let start = cell_origin(&w) + Vec2::new(2.0, 2.0);
    drag(&mut w, start, start + Vec2::new(60.0, 0.0));
    let key = ("quill".to_owned(), "claude".to_owned());
    assert!(w.app.pane_mut(&key).unwrap().selection.is_some());
    w.frame(vec![Event::Text("a".into())]);
    assert!(w.app.pane_mut(&key).unwrap().selection.is_none());
}

#[test]
fn cmd_a_selects_the_screen_cmd_c_copies_it_and_nothing_reaches_the_pty() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "first line");
    w.frame(vec![cmd_key(Key::A)]);
    let key = ("quill".to_owned(), "claude".to_owned());
    assert!(w.app.pane_mut(&key).unwrap().selection.is_some());
    assert_eq!(
        *quill.slot("claude").session.selects.lock().unwrap(),
        [SelectOp::All]
    );
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied.len(), 1);
    assert!(w.copied[0].starts_with("first line"));
    assert!(!quill.typed_anywhere());
    w.frame(vec![Event::Text("x".into())]);
    assert!(w.app.pane_mut(&key).unwrap().selection.is_none());
}

#[test]
fn cmd_a_on_an_unfocused_terminal_selects_nothing() {
    let (quill, mut w) = one_terminal();
    fill(&quill, &mut w, ("quill", "claude"), "first line");
    w.ctx.memory_mut(|memory| memory.stop_text_input());
    w.frame(vec![cmd_key(Key::A)]);
    let key = ("quill".to_owned(), "claude".to_owned());
    assert!(w.app.pane_mut(&key).unwrap().selection.is_none());
    assert!(!quill.typed_anywhere());
}

#[test]
fn cmd_v_reaches_only_the_shown_terminal() {
    let (quill, mut w) = one_terminal();
    w.frame(vec![Event::Paste("code-123".into())]);
    assert_eq!(quill.slot("claude").typed(), b"code-123");
    assert!(quill.slot("codex").typed().is_empty());
}

#[test]
fn the_shown_terminal_starts_focused_and_alone_receives_keys() {
    let (quill, mut w) = one_terminal();
    w.frame(vec![Event::Text("a".into())]);
    assert_eq!(quill.slot("claude").typed(), b"a");
    assert!(quill.slot("codex").typed().is_empty());
}

#[test]
fn clicking_a_terminal_row_moves_keys_to_it_and_only_it() {
    let (quill, mut w) = one_terminal();
    w.click_hit(&Hit::Terminal("quill".into(), "codex".into()));
    w.frame(vec![Event::Text("b".into())]);
    assert_eq!(quill.slot("codex").typed(), b"b");
    assert!(quill.slot("claude").typed().is_empty());
    w.click_hit(&Hit::Terminal("quill".into(), "claude".into()));
    w.frame(vec![Event::Text("c".into())]);
    assert_eq!(quill.slot("claude").typed(), b"c");
    assert_eq!(quill.slot("codex").typed(), b"b");
}

#[test]
fn typing_on_the_frame_after_a_click_reaches_the_new_terminal() {
    let (quill, mut w) = one_terminal();
    let at = w
        .rect(&Hit::Terminal("quill".into(), "codex".into()))
        .center();
    w.press(at);
    w.frame(vec![button(at, false)]);
    w.frame(vec![Event::Text("k".into())]);
    assert_eq!(quill.slot("codex").typed(), b"k");
    w.frame(vec![Event::Paste("code-9".into())]);
    assert_eq!(quill.slot("codex").typed(), b"kcode-9");
}

#[test]
fn control_c_goes_to_the_shown_terminal() {
    let (quill, mut w) = one_terminal();
    w.click_hit(&Hit::Terminal("quill".into(), "codex".into()));
    w.key(Key::C, Modifiers::CTRL);
    assert_eq!(quill.slot("codex").typed(), [3]);
    assert!(quill.slot("claude").typed().is_empty());
}

#[test]
fn navigation_keys_reach_the_child_and_do_not_move_focus() {
    let (quill, mut w) = one_terminal();
    w.key(Key::Tab, Modifiers::NONE);
    w.key(Key::Escape, Modifiers::NONE);
    w.key(Key::ArrowUp, Modifiers::NONE);
    w.frame(vec![Event::Text("z".into())]);
    assert_eq!(quill.slot("claude").typed(), b"\t\x1b\x1b[Az");
}

#[test]
fn a_row_chosen_by_enter_does_not_also_type_it() {
    let (quill, mut w) = one_terminal();
    let id = egui::Id::unique(("tree-terminal", "quill", "codex"));
    w.ctx.memory_mut(|memory| memory.request_focus(id));
    w.frame(vec![]);
    let mut events = enter();
    events.push(Event::Text("ls".into()));
    events.push(key_event(Key::Enter, true));
    events.push(key_event(Key::Enter, false));
    w.frame(events);
    assert_eq!(w.shown(), Some(("quill".into(), "codex".into())));
    assert_eq!(quill.slot("codex").typed(), b"ls\r");
}

#[test]
fn a_session_refusing_input_shows_a_notice_and_spares_the_others() {
    let quill = project("quill", vec![spec("claude", Claude), spec("codex", Codex)]);
    // Rebuild the second terminal with a refusing session.
    let refusing = Arc::new(FakeSession {
        refuse: true,
        ..FakeSession::default()
    });
    quill.terminals.edit(|view| {
        view.terminals[1].session = Arc::clone(&refusing) as Arc<dyn TerminalSession>;
    });
    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    w.click_hit(&Hit::Terminal("quill".into(), "codex".into()));
    w.frame(vec![Event::Text("x".into())]);
    let codex = ("quill".to_owned(), "codex".to_owned());
    assert!(w.app.pane_mut(&codex).unwrap().notice.is_some());
    assert!(
        w.app
            .pane_mut(&("quill".to_owned(), "claude".to_owned()))
            .unwrap()
            .notice
            .is_none()
    );
    w.click_hit(&Hit::Terminal("quill".into(), "claude".into()));
    w.frame(vec![Event::Text("y".into())]);
    assert_eq!(quill.slot("claude").typed(), b"y");
}

#[test]
fn a_window_resize_reaches_the_shown_session_only() {
    let (quill, mut w) = one_terminal();
    assert_eq!(quill.slot("claude").session.sizes.lock().unwrap().len(), 1);
    w.width = 900.0;
    w.frame(vec![]);
    w.frame(vec![]);
    let sizes = quill.slot("claude").session.sizes.lock().unwrap().clone();
    assert_eq!(sizes.len(), 2);
    assert!(sizes[1].cols < sizes[0].cols);
    assert!(
        quill.slot("codex").session.sizes.lock().unwrap().is_empty(),
        "hidden: no work"
    );
    w.click_hit(&Hit::Terminal("quill".into(), "codex".into()));
    w.frame(vec![]);
    assert_eq!(quill.slot("codex").session.sizes.lock().unwrap().len(), 1);
}

fn managed(stage: PaneStage) -> (Proj, W) {
    let (quill, mut w) = one_terminal();
    *quill.slot("claude").control.stage.lock().unwrap() = stage;
    w.frame(vec![]);
    w.frame(vec![]);
    (quill, w)
}

fn login_button(w: &W) -> Pos2 {
    let key = ("quill".to_owned(), "claude".to_owned());
    w.app.pane_hits(&key)[0].center()
}

fn second_button(w: &W) -> Pos2 {
    let key = ("quill".to_owned(), "claude".to_owned());
    w.app.pane_hits(&key)[1].center()
}

#[test]
fn a_pane_that_needs_login_offers_it_and_only_a_click_requests_it() {
    let (quill, mut w) = managed(PaneStage::NeedsLogin {
        auth: AuthState::LoggedOut,
    });
    w.frame(vec![Event::Text("x".into())]);
    let control = &quill.slot("claude").control;
    assert_eq!(*control.logins.lock().unwrap(), 0);
    let at = login_button(&w);
    w.click(at);
    assert_eq!(
        *control.logins.lock().unwrap(),
        0,
        "the first click only asks"
    );
    w.frame(vec![]);
    let at = login_button(&w);
    w.click(at);
    assert_eq!(*control.logins.lock().unwrap(), 1);
    assert_eq!(*control.starts.lock().unwrap(), 0);
}

#[test]
fn cancelling_a_login_leaves_it_unrequested() {
    let (quill, mut w) = managed(PaneStage::Stopped);
    let at = login_button(&w);
    w.click(at);
    w.frame(vec![]);
    let cancel = second_button(&w);
    w.click(cancel);
    let control = &quill.slot("claude").control;
    assert_eq!(*control.logins.lock().unwrap(), 0);
    assert_eq!(*control.starts.lock().unwrap(), 0);
    w.frame(vec![]);
    assert!(
        !w.app
            .pane_mut(&("quill".to_owned(), "claude".to_owned()))
            .unwrap()
            .login_armed
    );
}

#[test]
fn a_stopped_or_failed_pane_offers_log_in_and_no_start() {
    for stage in [
        PaneStage::Stopped,
        PaneStage::Failed {
            reason: "no".into(),
        },
    ] {
        let (quill, w) = managed(stage);
        let hits = w.app.pane_hits(&("quill".to_owned(), "claude".to_owned()));
        assert_eq!(hits.len(), 1, "only Log in");
        assert_eq!(*quill.slot("claude").control.starts.lock().unwrap(), 0);
    }
}

#[test]
fn a_terminal_shows_only_its_state_on_the_second_line() {
    let quill = project(
        "quill",
        vec![spec("claude", Claude).state(TermActivity::Failed)],
    );
    let entry = quill.terminals.view().terminals[0].clone();
    let dot = crate::dots::Dot::of(entry.state, false);
    let now = std::time::Instant::now();
    assert_eq!(crate::tree::meta_line(&entry, dot, now), "failed");
    let named = TerminalEntry {
        name: Some("Ledger".into()),
        ..entry
    };
    assert_eq!(crate::tree::meta_line(&named, dot, now), "failed");
}

#[test]
fn every_row_shows_the_id_agents_use_left_of_its_state() {
    let quill = project(
        "quill",
        vec![
            spec("claude", Claude),
            spec("codex-27", Codex),
            spec("antigravity-3", ToolKind::Antigravity),
        ],
    );
    let view = quill.terminals.view();
    for entry in &view.terminals {
        // Unnamed, named, and named after the id: always the CLI's id.
        let named = TerminalEntry {
            name: Some("Ledger review".into()),
            ..entry.clone()
        };
        let same = TerminalEntry {
            name: Some(entry.id.clone()),
            ..entry.clone()
        };
        let expected = match entry.id.as_str() {
            "antigravity-3" => "agy-3",
            id => id,
        };
        for row in [entry, &named, &same] {
            assert_eq!(crate::tree::row_id(row), expected, "{:?}", row.name);
        }
    }
    // A host's terminal on a remote machine: its id there, not the wire's
    // number, in the row, the title and the tooltip.
    let remote = TerminalEntry {
        id: "17".into(),
        name: Some("app-session".into()),
        shown_id: Some("claude-12".into()),
        ..view.terminals[0].clone()
    };
    assert_eq!(crate::tree::row_id(&remote), "claude-12");
    let tip = crate::tree::row_tip(&remote, std::time::Instant::now());
    assert!(tip.starts_with("Terminal id: claude-12"), "{tip}");
    let unnamed = TerminalEntry {
        name: None,
        shown_id: Some("antigravity-4".into()),
        ..remote
    };
    assert_eq!(crate::tree::row_id(&unnamed), "agy-4");
    assert_eq!(crate::tree::title(&unnamed), "antigravity-4");
}

fn work(
    id: &str,
    kind: &str,
    label: &str,
    since: std::time::Instant,
) -> connecto_core::BackgroundWork {
    connecto_core::BackgroundWork {
        id: id.to_owned(),
        kind: kind.to_owned(),
        label: label.to_owned(),
        description: None,
        since,
    }
}

/// Background tasks never take the state's place: the row says the state,
/// and an arrow on the right, closed at first, opens the tasks under the row,
/// each with what it is for, its age and its own ■; a subagent has none.
#[test]
fn background_tasks_keep_the_state_and_open_from_an_arrow_to_stop_one() {
    let start = std::time::Instant::now();
    let described =
        |id: &str, kind: &str, label: &str, description: &str| connecto_core::BackgroundWork {
            description: Some(description.to_owned()),
            ..work(id, kind, label, start)
        };
    let quill = project(
        "quill",
        vec![
            spec("claude", Claude)
                .named("siri-intents")
                .running(vec![described(
                    "b1",
                    "shell",
                    "cargo test -p connecto-net",
                    "Fix host-side Siri intents",
                )]),
            spec("claude-2", Claude).running(vec![
                described("b2", "shell", "npm test -- --watch", "Watch the tests"),
                work("a3", "subagent", "Review the diff", start),
            ]),
        ],
    );
    let one = quill.terminals.view().terminals[0].clone();
    let dot = crate::dots::Dot::of(one.state, false);
    let meta = crate::tree::meta_line(&one, dot, start);
    assert_eq!(meta, "1 in background · 0 s");
    assert!(!meta.contains("Siri"), "{meta}");
    // In a turn, the turn is the state, whatever runs beside it.
    let in_turn = TerminalEntry {
        in_turn: true,
        ..one.clone()
    };
    assert_eq!(crate::tree::meta_line(&in_turn, dot, start), "working");

    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    let task =
        |tid: &str, id: &str| Hit::StopBackground("quill".into(), tid.into(), Some(id.into()));
    let all = |tid: &str| Hit::StopBackground("quill".into(), tid.into(), None);
    let arrow = |tid: &str| Hit::BackgroundList("quill".into(), tid.into());
    // Closed: the arrow on the right of each row, and no stop yet.
    for tid in ["claude", "claude-2"] {
        assert!(w.has(&arrow(tid)), "{tid}");
        assert!(!w.has(&all(tid)), "{tid}");
        let row = w.rect(&Hit::Terminal("quill".into(), tid.into()));
        assert!(w.rect(&arrow(tid)).center().x > row.center().x, "{tid}");
    }
    assert!(!w.has(&task("claude", "b1")));
    // One task opens too, and its ■ stops just it.
    w.click_hit(&arrow("claude"));
    w.frame(vec![]);
    assert!(w.has(&task("claude", "b1")));
    assert!(!w.has(&task("claude-2", "b2")));
    w.click_hit(&task("claude", "b1"));
    assert_eq!(*quill.terminals.stops.lock().unwrap(), ["claude/b1"]);
    // Two: a ■ for the shell only, not the subagent.
    w.click_hit(&arrow("claude-2"));
    w.frame(vec![]);
    assert!(w.has(&task("claude-2", "b2")));
    assert!(!w.has(&task("claude-2", "a3")));
    w.click_hit(&task("claude-2", "b2"));
    assert_eq!(
        *quill.terminals.stops.lock().unwrap(),
        ["claude/b1", "claude-2/b2"]
    );
    // The arrow closes the list again.
    w.click_hit(&arrow("claude"));
    w.frame(vec![]);
    assert!(!w.has(&task("claude", "b1")));
}

/// Every text the last pass painted: what it says, where, and whether it was
/// cut to fit.
fn painted_texts(w: &mut W) -> Vec<(String, Rect, bool)> {
    fn collect(shape: &egui::Shape, out: &mut Vec<(String, Rect, bool)>) {
        match shape {
            egui::Shape::Text(text) => out.push((
                text.galley.text().to_owned(),
                text.galley.rect.translate(text.pos.to_vec2()),
                text.galley.elided,
            )),
            egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect(s, out)),
            _ => {}
        }
    }
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(
            Pos2::ZERO,
            Vec2::new(w.width, w.height),
        )),
        ..RawInput::default()
    };
    let app = &mut w.app;
    let output = w.ctx.run_ui(input, |ui| app.draw(ui));
    let mut texts = Vec::new();
    for clipped in &output.shapes {
        collect(&clipped.shape, &mut texts);
    }
    texts
}

/// On the narrowest row (shown, so with its close and robot, with children
/// and background tasks, so with both arrows) a long id gives way: the state
/// is drawn whole, left of the arrows, and the id is cut or left out.
#[test]
fn a_long_id_gives_way_to_the_state_beside_both_arrows() {
    let start = std::time::Instant::now();
    let quill = project(
        "quill",
        vec![
            spec("claude-1234567890", Claude)
                .named("siri-intents")
                .running(vec![work("b1", "shell", "sleep 600", start)])
                .state(TermActivity::NeedsUser),
            spec("claude-2", Claude).child_of("claude-1234567890"),
            spec("claude-9876543210", Claude)
                .named("ledger")
                .running(vec![work("b2", "shell", "sleep 600", start)]),
            spec("claude-3", Claude).child_of("claude-9876543210"),
        ],
    );
    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    for (tid, state) in [
        ("claude-1234567890", "waiting for you"),
        // Too narrow for the count: the shorter form, whole.
        ("claude-9876543210", "background"),
    ] {
        w.click_hit(&Hit::Terminal("quill".into(), tid.into()));
        w.frame(vec![]);
        let row = w.rect(&Hit::Terminal("quill".into(), tid.into()));
        let arrow = w.rect(&Hit::BackgroundList("quill".into(), tid.into()));
        assert!(w.has(&Hit::Kids("quill".into(), tid.into())), "{tid}");
        assert!(w.has(&Hit::Close("quill".into(), tid.into())), "{tid}");
        let texts = painted_texts(&mut w);
        let line = texts
            .iter()
            .find(|(text, at, _)| text.starts_with(state) && row.contains(at.center()));
        assert!(line.is_some(), "{state} not on the row: {texts:?}");
        let line = line.unwrap();
        assert!(!line.2, "{state} was cut: {texts:?}");
        assert!(line.1.max.x <= arrow.min.x, "{state} runs under the arrow");
        // The id, if shown at all, is cut and sits left of the state.
        if let Some(id) = texts
            .iter()
            .find(|(text, at, _)| text == tid && row.contains(at.center()))
        {
            assert!(id.2 && id.1.max.x < line.1.min.x, "{id:?}");
        }
    }
}

/// An open list closes for good once its tasks have all ended: a later
/// task on the same terminal starts with the list closed, whether or not
/// the row was drawn while nothing ran.
#[test]
fn a_later_batch_of_background_tasks_starts_closed() {
    let start = std::time::Instant::now();
    let mut quill = project(
        "quill",
        vec![spec("claude", Claude).running(vec![work("b1", "shell", "sleep 1", start)])],
    );
    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    let arrow = Hit::BackgroundList("quill".into(), "claude".into());
    let stop = |id: &str| Hit::StopBackground("quill".into(), "claude".into(), Some(id.into()));
    w.click_hit(&arrow);
    w.frame(vec![]);
    assert!(w.has(&stop("b1")));
    // All end, and the row is drawn with nothing running.
    quill.set(vec![spec("claude", Claude)]);
    w.frame(vec![]);
    assert!(!w.has(&arrow));
    quill.set(vec![
        spec("claude", Claude).running(vec![work("b2", "shell", "sleep 2", start)]),
    ]);
    w.frame(vec![]);
    assert!(w.has(&arrow));
    assert!(!w.has(&stop("b2")));
    // All end and a new one starts between two frames.
    w.click_hit(&arrow);
    w.frame(vec![]);
    assert!(w.has(&stop("b2")));
    quill.set(vec![
        spec("claude", Claude).running(vec![work("b3", "shell", "sleep 3", start)]),
    ]);
    w.frame(vec![]);
    assert!(!w.has(&stop("b3")));
    // While one of the tasks it was opened on runs, it stays open.
    w.click_hit(&arrow);
    w.frame(vec![]);
    quill.set(vec![spec("claude", Claude).running(vec![
        work("b3", "shell", "sleep 3", start),
        work("b4", "shell", "sleep 4", start),
    ])]);
    w.frame(vec![]);
    quill.set(vec![
        spec("claude", Claude).running(vec![work("b4", "shell", "sleep 4", start)]),
    ]);
    w.frame(vec![]);
    assert!(w.has(&stop("b4")));
}

/// The row's Stop background shells reaches the terminal list; a refusal
/// shows where other refused requests do.
#[test]
fn stopping_background_shells_asks_the_terminal_list_and_shows_a_refusal() {
    let quill = project(
        "quill",
        vec![spec("claude", Claude).running(vec![work(
            "b1",
            "shell",
            "sleep 100",
            std::time::Instant::now(),
        )])],
    );
    let ws = workspace(&[&quill]);
    let mut w = W::new(&ws);
    let stop = crate::commands::Command::StopBackgroundAt {
        project: "quill".into(),
        terminal: "claude".into(),
        task: None,
    };
    w.app.queue_for_tests(stop.clone());
    w.frame(vec![]);
    assert_eq!(*quill.terminals.stops.lock().unwrap(), ["claude"]);
    assert!(w.app.request_error_for_tests().is_none());
    *quill.terminals.refuse.lock().unwrap() =
        Some("Stopping background shells is not supported on Windows yet.".into());
    w.app.queue_for_tests(stop);
    w.frame(vec![]);
    assert!(
        w.app
            .request_error_for_tests()
            .is_some_and(|text| text.contains("not supported on Windows"))
    );
}

/// A terminal with background work at its prompt says so with the oldest
/// task's age, is long-running after 15 minutes, and lists each task with
/// its age on hover.
#[test]
fn background_work_shows_its_age_and_the_tasks_and_turns_long_running() {
    use std::time::Duration;

    let start = std::time::Instant::now();
    let quill = project(
        "quill",
        vec![spec("claude", Claude).named("Ledger").running(vec![
            work("b1", "shell", "python3 grind.py big.log", start),
            work(
                "a2",
                "subagent",
                "Review the diff",
                start + Duration::from_secs(300),
            ),
        ])],
    );
    let entry = quill.terminals.view().terminals[0].clone();
    let dot = crate::dots::Dot::of(entry.state, false);
    let soon = start + Duration::from_secs(600);
    assert_eq!(
        crate::tree::meta_line(&entry, dot, soon),
        "2 in background · 10 min"
    );
    assert!(!crate::tree::long_running(&entry, soon));
    let later = start + Duration::from_secs(8400);
    assert_eq!(
        crate::tree::meta_line(&entry, dot, later),
        "2 in background · 2 h 20 min"
    );
    assert!(crate::tree::long_running(&entry, later));
    assert!(crate::tree::long_running(
        &entry,
        start + Duration::from_secs(900)
    ));
    let tip = crate::tree::row_tip(&entry, later);
    assert!(tip.starts_with("Terminal id: claude\n"), "{tip}");
    assert!(tip.contains("running for a long time"), "{tip}");
    assert!(
        tip.contains("• python3 grind.py big.log (shell, 2 h 20 min)"),
        "{tip}"
    );
    assert!(
        tip.contains("• Review the diff (subagent, 2 h 15 min)"),
        "{tip}"
    );
    assert!(
        tip.contains("The arrow lists them; ■ stops one shell."),
        "{tip}"
    );
    // Only a subagent: the tip says only shells can be stopped.
    let agent_only = TerminalEntry {
        background: entry.background[1..].to_vec(),
        name: None,
        ..entry.clone()
    };
    let tip = crate::tree::row_tip(&agent_only, soon);
    assert!(!tip.contains("Terminal id"), "{tip}");
    assert!(tip.ends_with(crate::tree::NOT_A_SHELL), "{tip}");
    // Asked by another agent: that line wins, as before.
    let asked = TerminalEntry {
        asked_by: Some("codex".into()),
        ..entry
    };
    assert_eq!(crate::tree::meta_line(&asked, dot, later), "asked by codex");
}

#[test]
fn text_typed_in_the_frame_a_rename_ends_does_not_reach_the_terminal() {
    let (quill, _m, ws) = two_projects();
    let mut w = W::new(&ws);
    rename_start(&mut w);
    w.frame(vec![
        Event::Text("x".into()),
        key_event(Key::Enter, true),
        key_event(Key::Enter, false),
    ]);
    w.frame(vec![]);
    assert!(!w.app.is_renaming());
    assert!(
        !quill.typed_anywhere(),
        "{:?}",
        quill.slot("claude-1").typed()
    );
    // A project rename too.
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let row = w.rect(&Hit::Project("connecto".into())).center();
    w.click(row);
    w.click(row);
    w.frame(vec![]);
    w.frame(vec![
        Event::Text("y".into()),
        key_event(Key::Enter, true),
        key_event(Key::Enter, false),
    ]);
    w.frame(vec![]);
    assert!(!quill.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn continue_by_space_types_no_space_into_the_terminal() {
    let (quill, _m, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    let id = w.app.hits.history.continue_id.unwrap();
    w.ctx.memory_mut(|memory| memory.request_focus(id));
    w.frame(vec![]);
    w.frame(vec![
        key_event(Key::Space, true),
        Event::Text(" ".into()),
        key_event(Key::Space, false),
    ]);
    w.frame(vec![]);
    assert_eq!(*quill.terminals.continues.lock().unwrap(), ["h1"]);
    assert!(
        !quill.typed_anywhere(),
        "{:?}",
        quill.slot("claude-1").typed()
    );
}

#[test]
fn enter_on_view_transcript_only_views_and_does_not_also_continue() {
    let (quill, _m, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    let id = w.app.hits.history.view_id.unwrap();
    w.ctx.memory_mut(|memory| memory.request_focus(id));
    w.frame(vec![]);
    w.frame(enter());
    w.frame(vec![]);
    assert!(quill.terminals.continues.lock().unwrap().is_empty());
    assert_eq!(*quill.terminals.transcripts.lock().unwrap(), ["h1"]);
    assert!(w.app.history_open());
    assert!(!quill.typed_anywhere());
}

#[test]
fn enter_on_a_sidebar_row_does_not_also_pick_in_the_inline_chooser() {
    let empty = project("empty", vec![]);
    let other = project("other", vec![spec("claude", Claude)]);
    let ws = workspace(&[&empty, &other]);
    let mut w = W::new(&ws);
    let id = egui::Id::unique(("tree-project", "other"));
    w.ctx.memory_mut(|memory| memory.request_focus(id));
    w.frame(vec![]);
    w.frame(enter());
    w.frame(vec![]);
    assert!(empty.terminals.adds.lock().unwrap().is_empty());
    assert!(other.terminals.adds.lock().unwrap().is_empty());
}

#[test]
fn space_on_a_rail_tile_or_the_pause_switch_types_nothing() {
    let (quill, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::Backslash)]);
    w.frame(vec![]);
    for id in [
        egui::Id::unique(("rail-tile", "connecto")),
        egui::Id::unique("pause-toggle"),
    ] {
        w.ctx.memory_mut(|memory| memory.request_focus(id));
        w.frame(vec![]);
        w.frame(vec![
            key_event(Key::Space, true),
            Event::Text(" ".into()),
            key_event(Key::Space, false),
        ]);
        w.frame(vec![]);
    }
    assert!(!quill.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn a_preparing_pane_offers_no_commands() {
    let (quill, w) = managed(PaneStage::Preparing {
        step: "Checking login".to_owned(),
    });
    assert!(
        w.app
            .pane_hits(&("quill".to_owned(), "claude".to_owned()))
            .is_empty()
    );
    assert_eq!(*quill.slot("claude").control.logins.lock().unwrap(), 0);
}

#[test]
fn standing_warnings_take_room_from_the_grid_and_stay_in_every_stage() {
    for stage in [PaneStage::LoggingIn, PaneStage::Running, PaneStage::Stopped] {
        let (quill, mut w) = managed(stage);
        let key = ("quill".to_owned(), "claude".to_owned());
        assert_eq!(w.app.pane_mut(&key).unwrap().warning_lines(), 0);
        let rows = w
            .app
            .pane_mut(&key)
            .unwrap()
            .applied_size
            .map(|size| size.rows);
        *quill.slot("claude").control.warnings.lock().unwrap() =
            Arc::new(vec!["one".into(), "two".into()]);
        w.frame(vec![]);
        assert_eq!(w.app.pane_mut(&key).unwrap().warning_lines(), 2);
        let shrunk = w
            .app
            .pane_mut(&key)
            .unwrap()
            .applied_size
            .map(|size| size.rows);
        assert!(shrunk < rows, "{shrunk:?} from {rows:?}");
    }
}

#[test]
fn a_terminal_connecto_types_into_says_so_and_still_takes_keys() {
    let (quill, mut w) = managed(PaneStage::Running);
    let key = ("quill".to_owned(), "claude".to_owned());
    assert!(!w.app.pane_mut(&key).unwrap().drew_injecting);
    quill
        .slot("claude")
        .control
        .injecting
        .store(true, Ordering::Relaxed);
    w.frame(vec![]);
    assert!(w.app.pane_mut(&key).unwrap().drew_injecting);
    assert_eq!(
        crate::pane::INJECTING_TEXT,
        "Connecto is sending a message…"
    );
    // The keys still go to the session, which holds them until it is done.
    w.frame(vec![Event::Text("x".into())]);
    assert!(quill.slot("claude").typed().ends_with(b"x"));
    quill
        .slot("claude")
        .control
        .injecting
        .store(false, Ordering::Relaxed);
    w.frame(vec![]);
    assert!(!w.app.pane_mut(&key).unwrap().drew_injecting);
    // A message that waits for the user's line says so.
    let control = &quill.slot("claude").control;
    control.waiting.store(true, Ordering::Relaxed);
    w.frame(vec![]);
    assert!(w.app.pane_mut(&key).unwrap().drew_waiting);
    control.waiting.store(false, Ordering::Relaxed);
    w.frame(vec![]);
    assert!(!w.app.pane_mut(&key).unwrap().drew_waiting);
}

#[test]
fn an_unknown_login_state_is_not_shown_as_logged_out() {
    let (_quill, mut w) = managed(PaneStage::NeedsLogin {
        auth: AuthState::Unknown,
    });
    let key = ("quill".to_owned(), "claude".to_owned());
    assert_eq!(
        w.app.pane_mut(&key).unwrap().status_line().0,
        "login status unknown"
    );
}

#[test]
fn a_wake_is_timed_until_the_next_frame_takes_it() {
    let slot = crate::app::WakeSlot::default();
    slot.wake();
    assert_ne!(slot.pending_since_us(), 0);
}

// ---------- static shells ----------

fn static_app(count: usize) -> (egui::Context, App, Vec<Arc<FakeSession>>) {
    let sessions: Vec<Arc<FakeSession>> = (0..count).map(|_| Arc::default()).collect();
    let configs = sessions
        .iter()
        .enumerate()
        .map(|(index, session)| PaneConfig {
            label: format!("shell-{index}"),
            session: Ok(Arc::clone(session) as Arc<dyn TerminalSession>),
            control: None,
            tool: None,
        })
        .collect();
    let mut app = App::new(configs, Arc::default(), Diagnostics::default());
    let ctx = egui::Context::default();
    run_pass(&ctx, &mut app, vec![]);
    run_pass(&ctx, &mut app, vec![]);
    (ctx, app, sessions)
}

fn run_pass(ctx: &egui::Context, app: &mut App, events: Vec<Event>) {
    let input = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1280.0, 760.0))),
        events,
        ..RawInput::default()
    };
    let mut output = ctx.run_ui(input, |ui| app.draw(ui));
    output.textures_delta.clear();
}

#[test]
fn a_static_shell_runs_without_a_sidebar_and_its_tabs_switch_panes() {
    let (ctx, mut app, sessions) = static_app(2);
    assert_eq!(app.hits.head.tab_rects.len(), 2);
    assert!(app.hits.tree.is_empty(), "no sidebar");
    run_pass(&ctx, &mut app, vec![Event::Text("a".into())]);
    assert_eq!(*sessions[0].input.lock().unwrap(), b"a");
    let second = app.hits.head.tab_rects[1].center();
    run_pass(&ctx, &mut app, vec![Event::PointerMoved(second)]);
    run_pass(&ctx, &mut app, vec![button(second, true)]);
    run_pass(&ctx, &mut app, vec![button(second, false)]);
    run_pass(&ctx, &mut app, vec![]);
    run_pass(&ctx, &mut app, vec![Event::Text("b".into())]);
    assert_eq!(*sessions[1].input.lock().unwrap(), b"b");
    assert_eq!(*sessions[0].input.lock().unwrap(), b"a");
}

#[test]
fn a_static_shell_with_one_pane_has_no_tabs_and_shortcuts_stay_out_of_it() {
    let (ctx, mut app, sessions) = static_app(1);
    assert!(app.hits.head.tab_rects.is_empty());
    run_pass(
        &ctx,
        &mut app,
        vec![cmd_key(Key::T), key_down(Key::Tab, Modifiers::CTRL)],
    );
    assert!(sessions[0].input.lock().unwrap().is_empty());
}

#[test]
fn a_pane_that_failed_to_start_leaves_the_other_usable() {
    let session = Arc::new(FakeSession::default());
    let configs = vec![
        PaneConfig {
            label: "broken".to_owned(),
            session: Err("no such program".to_owned()),
            control: None,
            tool: None,
        },
        PaneConfig {
            label: "ok".to_owned(),
            session: Ok(Arc::clone(&session) as Arc<dyn TerminalSession>),
            control: None,
            tool: None,
        },
    ];
    let mut app = App::new(configs, Arc::default(), Diagnostics::default());
    let ctx = egui::Context::default();
    run_pass(&ctx, &mut app, vec![]);
    run_pass(&ctx, &mut app, vec![]);
    let second = app.hits.head.tab_rects[1].center();
    run_pass(&ctx, &mut app, vec![Event::PointerMoved(second)]);
    run_pass(&ctx, &mut app, vec![button(second, true)]);
    run_pass(&ctx, &mut app, vec![button(second, false)]);
    run_pass(&ctx, &mut app, vec![]);
    run_pass(&ctx, &mut app, vec![Event::Text("k".into())]);
    assert_eq!(*session.input.lock().unwrap(), b"k");
}

#[test]
fn the_window_hides_its_own_title_and_title_bar() {
    let viewport = viewport_for_tests();
    assert_eq!(viewport.fullsize_content_view, Some(true));
    assert_eq!(viewport.titlebar_shown, Some(false));
    assert_eq!(viewport.title_shown, Some(false));
    assert_eq!(viewport.min_inner_size, Some(Vec2::new(640.0, 300.0)));
    assert!(viewport.icon.is_some(), "the app icon decodes");
}

#[test]
fn only_a_scripted_build_keeps_the_window_on_top() {
    let level = viewport_for_tests().window_level;
    if cfg!(feature = "ui-test-driver") {
        assert_eq!(level, Some(egui::WindowLevel::AlwaysOnTop));
    } else {
        assert_eq!(level, None);
    }
}

#[cfg(feature = "ui-test-driver")]
#[test]
fn a_start_position_is_two_finite_numbers() {
    assert_eq!(
        crate::app::start_position_for_tests("-1920, 40.5"),
        Some(egui::pos2(-1920.0, 40.5))
    );
    for bad in ["", "10", "10,", "a,b", "inf,0", "1,NaN"] {
        assert_eq!(crate::app::start_position_for_tests(bad), None, "{bad}");
    }
}

// ---------- utility sessions ----------

fn hover_row(w: &mut W, project: &str, terminal: &str) {
    let row = w.rect(&Hit::Terminal(project.into(), terminal.into()));
    w.frame(vec![Event::PointerMoved(row.center())]);
    w.frame(vec![]);
}

fn run(id: u64, terminal: &str, status: UtilityStatus) -> UtilityRun {
    UtilityRun {
        id,
        project: "quill".into(),
        terminal_id: terminal.into(),
        terminal_label: format!("{terminal} label"),
        started_ms: fixed_clock().0 - 5 * 60_000,
        finished_ms: None,
        model: UtilityModel::Haiku,
        effort: UtilityEffort::Low,
        status,
    }
}

fn open_utility(w: &mut W) {
    w.app
        .queue_for_tests(crate::commands::Command::UtilitySessions);
    w.frame(vec![]);
    w.frame(vec![]);
    assert!(w.app.utility_open());
}

#[test]
fn robot_click_starts_titling_without_selecting_the_row() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    hover_row(&mut w, "quill", "claude-3");
    w.click_hit(&Hit::Robot("quill".into(), "claude-3".into()));
    assert_eq!(*quill.terminals.titlings.lock().unwrap(), ["claude-3"]);
    assert_eq!(w.shown(), Some(("quill".into(), "claude-1".into())));
}

#[test]
fn robot_shows_exactly_when_the_close_button_does_and_sits_left_of_it() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let robot = Hit::Robot("quill".into(), "claude-3".into());
    let close = Hit::Close("quill".into(), "claude-3".into());
    assert_eq!(w.has(&robot), w.has(&close));
    assert!(!w.has(&robot), "a quiet row shows neither");
    hover_row(&mut w, "quill", "claude-3");
    assert!(w.has(&robot) && w.has(&close));
    assert!(w.rect(&robot).max.x <= w.rect(&close).min.x + 0.5);
    // The row on show has both without a hover.
    let shown = ("quill".to_owned(), "claude-1".to_owned());
    w.frame(vec![Event::PointerMoved(Pos2::new(900.0, 400.0))]);
    w.frame(vec![]);
    assert!(w.has(&Hit::Robot(shown.0.clone(), shown.1.clone())));
    assert!(w.has(&Hit::Close(shown.0, shown.1)));
}

#[test]
fn a_running_titling_pulses_the_robot_and_ignores_clicks() {
    let (quill, _connecto, ws) = two_projects();
    quill
        .terminals
        .runs
        .lock()
        .unwrap()
        .push(run(1, "claude-3", UtilityStatus::Running));
    let mut w = W::new(&ws);
    let robot = Hit::Robot("quill".into(), "claude-3".into());
    let close = Hit::Close("quill".into(), "claude-3".into());
    assert!(!w.has(&robot), "busy does not show it when the X is hidden");
    hover_row(&mut w, "quill", "claude-3");
    assert!(w.has(&robot) && w.has(&close));
    assert!(w.rect(&robot).max.x <= w.rect(&close).min.x + 0.5);
    w.click_hit(&robot);
    assert!(quill.terminals.titlings.lock().unwrap().is_empty());
    assert_eq!(w.shown(), Some(("quill".into(), "claude-1".into())));
}

#[test]
fn the_empty_state_wraps_in_a_narrow_window_and_rows_have_accessible_labels() {
    use crate::utility::{UtilityInput, UtilityOut, UtilitySheet, sheet};
    let ctx = egui::Context::default();
    theme::apply(&ctx);
    ctx.enable_accesskit();
    let runs = [
        run(
            2,
            "claude-3",
            UtilityStatus::Done {
                title: "Fix login redirect".into(),
            },
        ),
        run(
            1,
            "claude-1",
            UtilityStatus::Failed {
                reason: "claude exited 1".into(),
            },
        ),
    ];
    let mut labels = Vec::new();
    for (list, narrow) in [(&runs[..], false), (&runs[..0], true)] {
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(300.0, 600.0));
        let input = UtilityInput {
            runs: list,
            settings: UtilitySettings::default(),
            now_ms: fixed_clock().0,
            utc_offset_s: 0,
            openable: &std::collections::HashSet::from([2]),
        };
        let mut state = UtilitySheet::default();
        let mut out = UtilityOut::default();
        let raw = RawInput {
            screen_rect: Some(area),
            ..RawInput::default()
        };
        let mut output = ctx.run_ui(raw.clone(), |ui| {
            sheet(&ui.ctx().clone(), area, &mut state, &input, &mut out);
        });
        output.textures_delta.clear();
        // Accessibility nodes are built from the pass before.
        let mut output = ctx.run_ui(raw, |ui| {
            sheet(&ui.ctx().clone(), area, &mut state, &input, &mut out);
        });
        output.textures_delta.clear();
        if narrow {
            assert!(out.hits.empty);
            let text = out.hits.empty_text.unwrap();
            assert!(text.max.x <= area.max.x - 16.0, "{text:?}");
            assert!(text.height() > 20.0, "wrapped onto several lines");
        } else if let Some(update) = output.platform_output.accesskit_update {
            for (_, node) in update.nodes {
                if let Some(label) = node.label() {
                    labels.push(label.to_owned());
                }
            }
        }
    }
    assert!(
        labels
            .iter()
            .any(|l| l == "Open session of claude-3 label, Fix login redirect, done"),
        "{labels:?}"
    );
    assert!(
        labels
            .iter()
            .any(|l| l == "claude-1 label, claude exited 1, failed, session closed"),
        "{labels:?}"
    );
}

#[test]
fn a_refused_titling_shows_its_reason() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    *quill.terminals.refuse.lock().unwrap() = Some("already running".to_owned());
    hover_row(&mut w, "quill", "claude-3");
    w.click_hit(&Hit::Robot("quill".into(), "claude-3".into()));
    w.frame(vec![]);
    assert_eq!(
        w.app.request_error_for_tests().as_deref(),
        Some("already running")
    );
}

#[test]
fn the_menu_command_opens_and_closes_the_utility_sheet() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_utility(&mut w);
    assert!(w.app.hits.utility.empty, "no runs shows the empty state");
    w.frame(esc());
    w.frame(vec![]);
    assert!(!w.app.utility_open());
}

#[test]
fn the_sheet_lists_the_projects_runs() {
    let (quill, _connecto, ws) = two_projects();
    *quill.terminals.runs.lock().unwrap() = vec![
        run(
            2,
            "claude-3",
            UtilityStatus::Done {
                title: "Fix login redirect".into(),
            },
        ),
        run(
            1,
            "claude-1",
            UtilityStatus::Failed {
                reason: "claude exited 1".into(),
            },
        ),
    ];
    let mut w = W::new(&ws);
    open_utility(&mut w);
    let rows = &w.app.hits.utility.rows;
    assert_eq!(rows.iter().map(|r| r.0).collect::<Vec<_>>(), [2, 1]);
    assert_eq!(rows[0].1[0], "claude-3 label");
    assert_eq!(rows[0].1[1], "Fix login redirect");
    assert!(rows[0].1[2].contains("Haiku") && rows[0].1[2].contains("Low"));
    assert_eq!(rows[0].1[3], "5 min ago");
    assert_eq!(rows[1].1[1], "claude exited 1");
}

#[test]
fn choosing_model_and_effort_writes_the_settings() {
    let (_quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_utility(&mut w);
    assert!(
        w.app.hits.utility.models.is_empty(),
        "closed until the gear"
    );
    let gear = w.app.hits.utility.gear.unwrap();
    w.click(gear.center());
    w.frame(vec![]);
    let model = w
        .app
        .hits
        .utility
        .models
        .iter()
        .find(|(m, _)| *m == UtilityModel::Opus)
        .unwrap()
        .1;
    w.click(model.center());
    w.frame(vec![]);
    let effort = w
        .app
        .hits
        .utility
        .efforts
        .iter()
        .find(|(e, _)| *e == UtilityEffort::Max)
        .unwrap()
        .1;
    w.click(effort.center());
    w.frame(vec![]);
    assert_eq!(
        *ws.utility_sets.lock().unwrap(),
        [
            UtilitySettings {
                model: UtilityModel::Opus,
                effort: UtilityEffort::Low
            },
            UtilitySettings {
                model: UtilityModel::Opus,
                effort: UtilityEffort::Max
            },
        ]
    );
}

fn give_pane(quill: &Proj, run: u64) -> Arc<Slot> {
    let slot = Arc::new(Slot::default());
    quill.terminals.utility_panes.lock().unwrap().insert(
        run,
        TerminalEntry {
            id: format!("utility-{run}"),
            name: Some(format!("Utility: run {run}")),
            parent: None,
            asked_by: None,
            tool: ToolKind::Claude,
            state: TermActivity::Idle,
            in_turn: false,
            background: Vec::new(),
            finished_turns: 0,
            session: Arc::clone(&slot.session) as Arc<dyn TerminalSession>,
            control: Some(Arc::clone(&slot.control) as Arc<dyn PaneControl>),
            desired_running: false,
            shown_id: None,
            activity: 0,
        },
    );
    slot
}

fn utility_runs_fixture(quill: &Proj) -> Arc<Slot> {
    *quill.terminals.runs.lock().unwrap() = vec![
        run(
            2,
            "claude-3",
            UtilityStatus::Failed {
                reason: "blocked at the trust screen: open the session and finish it once".into(),
            },
        ),
        run(
            1,
            "claude-1",
            UtilityStatus::Done {
                title: "Old title".into(),
            },
        ),
    ];
    give_pane(quill, 2)
}

fn click_run(w: &mut W, id: u64) {
    let rect = w
        .app
        .hits
        .utility
        .row_rects
        .iter()
        .find(|(run, _)| *run == id)
        .unwrap()
        .1;
    w.click(rect.center());
    w.frame(vec![]);
    w.frame(vec![]);
}

#[test]
fn clicking_a_run_opens_its_session_and_typing_reaches_it() {
    let (quill, _connecto, ws) = two_projects();
    let slot = utility_runs_fixture(&quill);
    let mut w = W::new(&ws);
    open_utility(&mut w);
    click_run(&mut w, 2);
    assert!(!w.app.utility_open(), "the sheet closes");
    assert_eq!(
        w.app.utility_view_for_tests(),
        Some(("quill".into(), "utility-2".into()))
    );
    assert!(
        w.app.hits.utility_banner.is_some(),
        "labelled as a utility session"
    );
    w.type_text("y");
    assert_eq!(slot.typed(), b"y");
    assert!(!quill.typed_anywhere(), "no tree terminal got the key");
}

#[test]
fn a_run_whose_session_is_closed_does_not_open() {
    let (quill, _connecto, ws) = two_projects();
    let _ = utility_runs_fixture(&quill);
    let mut w = W::new(&ws);
    open_utility(&mut w);
    click_run(&mut w, 1);
    assert!(w.app.utility_open(), "the sheet stays");
    assert_eq!(w.app.utility_view_for_tests(), None);
}

#[test]
fn selecting_a_tree_terminal_goes_back_from_a_utility_session() {
    let (quill, _connecto, ws) = two_projects();
    let slot = utility_runs_fixture(&quill);
    let mut w = W::new(&ws);
    open_utility(&mut w);
    click_run(&mut w, 2);
    assert!(w.app.utility_view_for_tests().is_some());
    w.click_hit(&Hit::Terminal("quill".into(), "claude-3".into()));
    w.frame(vec![]);
    assert_eq!(w.app.utility_view_for_tests(), None);
    assert!(
        w.app.hits.utility_banner.is_none()
            || w.shown() == Some(("quill".into(), "claude-3".into()))
    );
    assert_eq!(w.shown(), Some(("quill".into(), "claude-3".into())));
    w.type_text("k");
    assert_eq!(quill.slot("claude-3").typed(), b"k");
    assert!(slot.typed().is_empty());
}

#[test]
fn a_session_the_backend_closed_falls_back_to_the_terminal() {
    let (quill, _connecto, ws) = two_projects();
    let _ = utility_runs_fixture(&quill);
    let mut w = W::new(&ws);
    open_utility(&mut w);
    click_run(&mut w, 2);
    quill.terminals.utility_panes.lock().unwrap().clear();
    w.frame(vec![]);
    w.frame(vec![]);
    assert_eq!(w.app.utility_view_for_tests(), None);
}

#[test]
fn rapid_robot_clicks_start_one_titling_even_before_the_run_shows() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    hover_row(&mut w, "quill", "claude-3");
    let robot = Hit::Robot("quill".into(), "claude-3".into());
    // Two clicks in one pass, then more in the passes after. The fake lists no
    // run at all, as in the gap before the real one appears.
    let at = w.rect(&robot).center();
    w.frame(vec![
        Event::PointerMoved(at),
        button(at, true),
        button(at, false),
        button(at, true),
        button(at, false),
    ]);
    w.click(at);
    w.frame(vec![]);
    w.click(at);
    assert_eq!(*quill.terminals.titlings.lock().unwrap(), ["claude-3"]);
    // Once a newer run has finished, the robot works again.
    quill.terminals.runs.lock().unwrap().push(run(
        1,
        "claude-3",
        UtilityStatus::Done { title: "T".into() },
    ));
    w.frame(vec![]);
    w.frame(vec![]);
    w.click(at);
    assert_eq!(
        *quill.terminals.titlings.lock().unwrap(),
        ["claude-3", "claude-3"]
    );
}

#[test]
fn a_refused_titling_does_not_leave_the_robot_busy() {
    let (quill, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    hover_row(&mut w, "quill", "claude-3");
    *quill.terminals.refuse.lock().unwrap() = Some("no".to_owned());
    let robot = Hit::Robot("quill".into(), "claude-3".into());
    w.click_hit(&robot);
    *quill.terminals.refuse.lock().unwrap() = None;
    hover_row(&mut w, "quill", "claude-3");
    w.click_hit(&robot);
    assert_eq!(quill.terminals.titlings.lock().unwrap().len(), 2);
}

// ---------- automations ----------

mod automations_sheet {
    use super::*;
    use connecto_core::{
        AutomationItem, AutomationOutcome, AutomationsControl, AutomationsView, HistoryRecord,
        KeyStatus, PendingConfirmation,
    };

    #[derive(Default)]
    pub(super) struct FakeAutomations {
        view: Mutex<AutomationsView>,
        calls: Mutex<Vec<String>>,
        create_result: Mutex<Option<String>>,
        /// What `history_for` answers, and how often it was asked, by slug.
        audit: Mutex<Vec<HistoryRecord>>,
        audit_reads: Mutex<Vec<String>>,
        clear_audit_result: Mutex<Option<String>>,
        root_result: Mutex<Option<String>>,
        login_result: Mutex<Option<String>>,
        /// The error every group call answers with, when set.
        group_result: Mutex<Option<String>>,
        /// What a session request does to the workspace, as the backend does.
        navigate: Mutex<Option<(Arc<FakeWorkspace>, String, String)>>,
    }

    impl FakeAutomations {
        fn go(&self) {
            if let Some((ws, project, terminal)) = &*self.navigate.lock().unwrap() {
                ws.set_active_terminal(project, terminal);
            }
        }
        fn log(&self, call: String) {
            self.calls.lock().unwrap().push(call);
        }
        pub(super) fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }
    }

    impl AutomationsControl for FakeAutomations {
        fn view(&self) -> Arc<AutomationsView> {
            Arc::new(self.view.lock().unwrap().clone())
        }
        fn set_api_key(&self, key: &str) -> Result<(), String> {
            self.log(format!("set_api_key {key}"));
            Ok(())
        }
        fn clear_api_key(&self) {
            self.log("clear_api_key".into());
        }
        fn history_for(&self, slug: &str) -> Vec<HistoryRecord> {
            self.audit_reads.lock().unwrap().push(slug.to_owned());
            self.audit.lock().unwrap().clone()
        }
        fn clear_history_for(&self, slug: &str) -> Result<(), String> {
            self.log(format!("clear_history_for {slug}"));
            match self.clear_audit_result.lock().unwrap().clone() {
                Some(error) => Err(error),
                None => {
                    self.audit.lock().unwrap().clear();
                    Ok(())
                }
            }
        }
        fn set_root(&self, path: PathBuf) -> Result<(), String> {
            self.log(format!("set_root {}", path.display()));
            self.go();
            self.root_result.lock().unwrap().clone().map_or(Ok(()), Err)
        }
        fn open_login(&self, tool: ToolKind) -> Result<(), String> {
            self.log(format!("open_login {}", tool.name()));
            self.go();
            self.login_result
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err)
        }
        fn create(&self, name: &str, agent: ToolKind) -> Result<(), String> {
            self.log(format!("create {name} {}", agent.name()));
            self.go();
            self.create_result
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err)
        }
        fn create_described(
            &self,
            name: &str,
            agent: ToolKind,
            screen: &str,
            action: &str,
        ) -> Result<(), String> {
            self.log(format!(
                "create_described {name} {} | {screen} | {action}",
                agent.name()
            ));
            self.go();
            self.create_result
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err)
        }
        fn open_session(&self, slug: &str) -> Result<(), String> {
            self.log(format!("open_session {slug}"));
            self.go();
            Ok(())
        }
        fn set_enabled(&self, slug: &str, enabled: bool) {
            self.log(format!("set_enabled {slug} {enabled}"));
        }
        fn delete(&self, slug: &str) {
            self.log(format!("delete {slug}"));
        }
        fn set_threshold(&self, slug: &str, threshold: f64) {
            self.log(format!("set_threshold {slug} {threshold:.2}"));
        }
        fn approve(&self, pending: u64) {
            self.log(format!("approve {pending}"));
        }
        fn dismiss(&self, pending: u64) {
            self.log(format!("dismiss {pending}"));
        }
        fn clear_history(&self) {
            self.log("clear_history".into());
        }
        fn create_group(&self, title: &str) -> Result<String, String> {
            self.log(format!("create_group {title}"));
            self.group_result
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok("made".to_owned()), Err)
        }
        fn rename_group(&self, id: &str, title: &str) -> Result<(), String> {
            self.log(format!("rename_group {id} {title}"));
            self.group_result
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err)
        }
        fn delete_group(&self, id: &str) -> Result<(), String> {
            self.log(format!("delete_group {id}"));
            self.group_result
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err)
        }
        fn set_group(&self, slug: &str, group: Option<&str>) -> Result<(), String> {
            self.log(format!("set_group {slug} {}", group.unwrap_or("-")));
            self.group_result
                .lock()
                .unwrap()
                .clone()
                .map_or(Ok(()), Err)
        }
    }

    fn item(slug: &str) -> AutomationItem {
        AutomationItem {
            slug: slug.into(),
            name: "Accept edits".into(),
            enabled: false,
            agent: ToolKind::Claude,
            question_type: "choice".into(),
            instructions: String::new(),
            matches: "Yes".into(),
            keys: vec!["Enter".into()],
            threshold: 0.8,
            model: String::new(),
            unfinished: false,
            errors: Vec::new(),
            last: None,
            trigger: vec!["Do you want to proceed".into()],
            needs_trigger: false,
        }
    }

    fn pending(id: u64) -> PendingConfirmation {
        PendingConfirmation {
            id,
            created_ms: 0,
            project_id: "quill".into(),
            project: "quill".into(),
            terminal_id: "claude-1".into(),
            terminal_name: None,
            slug: "accept-edits".into(),
            automation: "Accept edits".into(),
            keys: vec!["y".into()],
            confidence: 0.62,
            threshold: 0.8,
            screen: String::new(),
        }
    }

    fn setup() -> (W, Arc<FakeAutomations>) {
        let (_quill, _connecto, ws) = two_projects();
        let fake = Arc::new(FakeAutomations::default());
        fake.view.lock().unwrap().root = Some(PathBuf::from("/automations"));
        let w = W::with_automations(&ws, Some(Arc::clone(&fake) as Arc<dyn AutomationsControl>));
        (w, fake)
    }

    /// Open the sheet as the user finds it: every row collapsed.
    fn open_collapsed(w: &mut W) {
        w.app.queue_for_tests(crate::commands::Command::Automations);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.automations_open());
    }

    /// Click a row's name to open or close its details.
    fn toggle_row(w: &mut W, slug: &str) {
        let rect = w.app.hits.automations.rects[&format!("row:{slug}")];
        w.click(rect.min + egui::vec2(10.0, rect.height() / 2.0));
        w.frame(vec![]);
    }

    /// Open the sheet with every row expanded, for the tests of the details.
    fn open(w: &mut W) {
        open_collapsed(w);
        let rows: Vec<String> = w
            .app
            .hits
            .automations
            .rects
            .keys()
            .filter_map(|k| k.strip_prefix("row:").map(str::to_owned))
            .collect();
        for slug in rows {
            toggle_row(w, &slug);
        }
    }

    /// Open the sheet on its Setup tab: the key, the folder and the logins.
    fn open_setup(w: &mut W) {
        open(w);
        click(w, "tab:setup");
    }

    #[test]
    fn setup_is_a_tab_of_its_own_and_the_list_comes_first() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().key = KeyStatus::Present;
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        open(&mut w);
        let rects = &w.app.hits.automations.rects;
        assert!(!rects.contains_key("field:key"), "the key is on Setup");
        assert!(!rects.contains_key("open-setup"), "nothing is missing");
        assert!(rects.contains_key("tab:Setup"));
        assert!(rects["enabled:accept-edits"].min.y < 220.0, "{rects:?}");
        click(&mut w, "tab:setup");
        assert!(w.app.hits.automations.rects.contains_key("field:key"));
        assert!(shows(&w, "Folder: /automations"));
    }

    #[test]
    fn an_incomplete_setup_is_said_on_the_list_with_a_way_there() {
        let (mut w, fake) = without_root();
        fake.view.lock().unwrap().key = KeyStatus::Missing;
        open(&mut w);
        assert!(shows(&w, "The Jev API key is not set."));
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("tab:Setup (incomplete)")
        );
        click(&mut w, "open-setup");
        assert!(shows(&w, "Key: not set"));
        assert!(shows(&w, "Folder: not set"));
    }

    #[test]
    fn rows_start_collapsed_and_open_on_a_click_but_not_on_the_switch() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        open_collapsed(&mut w);
        let rects = &w.app.hits.automations.rects;
        assert!(rects.contains_key("enabled:accept-edits"));
        assert!(!rects.contains_key("delete:accept-edits"));
        assert!(!rects.contains_key("raise:accept-edits"));
        assert!(shows(&w, "\"Do you want to proceed\""));
        // The switch toggles the automation and leaves the row as it is.
        click(&mut w, "enabled:accept-edits");
        assert_eq!(fake.calls(), ["set_enabled accept-edits true"]);
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("delete:accept-edits")
        );
        toggle_row(&mut w, "accept-edits");
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("delete:accept-edits")
        );
        click(&mut w, "enabled:accept-edits");
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("delete:accept-edits")
        );
        toggle_row(&mut w, "accept-edits");
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("delete:accept-edits")
        );
    }

    /// A Claude and a Codex automation to tell apart.
    fn mixed(fake: &FakeAutomations) {
        let mut codex = item("cx-folder");
        codex.agent = ToolKind::Codex;
        codex.name = "Codex folder".into();
        let mut other = item("cl-two");
        other.name = "Claude two".into();
        fake.view.lock().unwrap().automations = vec![item("cl-one"), other, codex];
    }

    #[test]
    fn the_strip_counts_each_agent_and_a_tab_filters_the_list() {
        let (mut w, fake) = setup();
        mixed(&fake);
        open_collapsed(&mut w);
        // All, Claude, Codex, with their counts; a shell cannot have automations.
        for text in ["All (3)", "Claude (2)", "Codex (1)"] {
            assert!(shows(&w, text), "{text}");
        }
        assert!(!shows(&w, "Shell"));
        let rects = |w: &W| w.app.hits.automations.rects.clone();
        assert!(
            ["row:cl-one", "row:cl-two", "row:cx-folder"]
                .iter()
                .all(|key| rects(&w).contains_key(*key))
        );
        click(&mut w, "filter:Codex");
        assert!(rects(&w).contains_key("row:cx-folder"));
        assert!(!rects(&w).contains_key("row:cl-one") && !rects(&w).contains_key("row:cl-two"));
        click(&mut w, "filter:Claude");
        assert!(rects(&w).contains_key("row:cl-one") && !rects(&w).contains_key("row:cx-folder"));
        click(&mut w, "filter:All");
        assert!(rects(&w).contains_key("row:cx-folder") && rects(&w).contains_key("row:cl-one"));
    }

    #[test]
    fn new_creates_for_the_selected_agent_and_the_tab_is_remembered() {
        let (mut w, fake) = setup();
        mixed(&fake);
        open_collapsed(&mut w);
        click(&mut w, "filter:Codex");
        click(&mut w, "new");
        let field = center(&w, "field:name");
        w.click(field);
        w.type_text("Folder two");
        w.frame(vec![]);
        // The New form's agent follows the tab.
        click(&mut w, "create");
        assert_eq!(fake.calls().last().unwrap(), "create Folder two codex");
        // Closed and opened again, the same tab is selected.
        w.frame(esc());
        w.frame(vec![]);
        assert!(!w.app.automations_open(), "closed");
        open_collapsed(&mut w);
        assert!(w.app.hits.automations.rects.contains_key("row:cx-folder"));
        assert!(!w.app.hits.automations.rects.contains_key("row:cl-one"));
    }

    /// A click anywhere on the row's line opens and closes it, not only on the
    /// arrow; the switch keeps its own click.
    #[test]
    fn a_click_anywhere_on_a_rows_line_toggles_it() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        open_collapsed(&mut w);
        let row = w.app.hits.automations.rects["row:accept-edits"];
        let switch = w.app.hits.automations.rects["enabled:accept-edits"];
        let opened = |w: &W| {
            w.app
                .hits
                .automations
                .rects
                .contains_key("delete:accept-edits")
        };
        let y = row.center().y;
        // On the arrow, on the name, in the middle of the line, and in the empty
        // part just left of the switch.
        for x in [
            row.min.x + 10.0,
            row.min.x + 60.0,
            row.center().x,
            switch.min.x - 8.0,
        ] {
            assert!(!opened(&w), "closed before the click at {x}");
            w.click(egui::pos2(x, y));
            w.frame(vec![]);
            assert!(opened(&w), "a click at x={x} opens the row");
            w.click(egui::pos2(x, y));
            w.frame(vec![]);
            assert!(!opened(&w), "a second click at x={x} closes it");
        }
        // The switch toggles the automation and leaves the row as it was.
        let before = fake.calls().len();
        click(&mut w, "enabled:accept-edits");
        assert_eq!(fake.calls().len(), before + 1);
        assert!(!opened(&w));
    }

    #[test]
    fn a_new_automation_opens_expanded() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("first")];
        open_collapsed(&mut w);
        assert!(!w.app.hits.automations.rects.contains_key("delete:first"));
        fake.view.lock().unwrap().automations = vec![item("first"), item("second")];
        w.frame(vec![]);
        w.frame(vec![]);
        let rects = &w.app.hits.automations.rects;
        assert!(rects.contains_key("delete:second"));
        assert!(!rects.contains_key("delete:first"));
    }

    #[test]
    fn automations_are_listed_under_their_agent() {
        let (mut w, fake) = setup();
        let mut codex = item("codex-one");
        codex.agent = ToolKind::Codex;
        fake.view.lock().unwrap().automations = vec![codex];
        open_collapsed(&mut w);
        let texts = &w.app.hits.automations.texts;
        let at = |t: &str| texts.iter().position(|x| x == t);
        assert!(at("Claude").is_some() && at("Codex").is_some(), "{texts:?}");
        assert!(shows(&w, "No Claude automations yet"));
        assert!(!shows(&w, "No Codex automations yet"));
        assert!(at("Claude") < at("No Claude automations yet"));
        assert!(at("No Claude automations yet") < at("Codex"));
        assert!(at("Codex") < at("Accept edits"));
    }

    #[test]
    fn the_new_button_is_in_the_header_above_the_list() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        open_collapsed(&mut w);
        let rects = &w.app.hits.automations.rects;
        assert!(rects["new"].min.y < rects["row:accept-edits"].min.y);
        assert!(rects["new"].center().y == rects["tab:automations"].center().y);
    }

    #[test]
    fn the_new_form_is_open_with_no_automation_and_behind_a_button_with_one() {
        let (mut w, _fake) = setup();
        open_collapsed(&mut w);
        assert!(w.app.hits.automations.rects.contains_key("field:name"));
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        open_collapsed(&mut w);
        assert!(!w.app.hits.automations.rects.contains_key("field:name"));
        click(&mut w, "new");
        assert!(w.app.hits.automations.rects.contains_key("field:name"));
        click(&mut w, "new");
        assert!(!w.app.hits.automations.rects.contains_key("field:name"));
    }

    fn center(w: &W, key: &str) -> Pos2 {
        let found = w.app.hits.automations.rects.get(key);
        assert!(
            found.is_some(),
            "{key} not drawn: {:?}",
            w.app.hits.automations.rects.keys()
        );
        found.unwrap().center()
    }

    fn click(w: &mut W, key: &str) {
        let at = center(w, key);
        w.click(at);
        w.frame(vec![]);
    }

    fn shows(w: &W, text: &str) -> bool {
        w.app
            .hits
            .automations
            .texts
            .iter()
            .any(|line| line.contains(text))
    }

    #[test]
    fn the_menu_command_opens_and_closes_the_sheet() {
        let (mut w, _fake) = setup();
        open_setup(&mut w);
        assert!(shows(&w, "Key: not set") || shows(&w, "Key: checking"));
        w.frame(esc());
        w.frame(vec![]);
        assert!(!w.app.automations_open());
    }

    #[test]
    fn opening_it_closes_the_other_sheets() {
        let (mut w, _fake) = setup();
        open_utility(&mut w);
        open(&mut w);
        assert!(!w.app.utility_open());
        open_utility(&mut w);
        assert!(!w.app.automations_open());
    }

    #[test]
    fn saving_and_clearing_the_key_call_the_control() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().key = KeyStatus::Missing;
        open_setup(&mut w);
        let field = center(&w, "field:key");
        w.click(field);
        w.type_text("jev-secret");
        w.frame(vec![]);
        click(&mut w, "save-key");
        assert_eq!(fake.calls(), ["set_api_key jev-secret"]);
        fake.view.lock().unwrap().key = KeyStatus::Present;
        w.frame(vec![]);
        assert!(shows(&w, "Key: set"));
        assert!(!shows(&w, "jev-secret"), "the key is never displayed");
        click(&mut w, "clear-key");
        assert_eq!(fake.calls().last().unwrap(), "clear_api_key");
    }

    #[test]
    fn create_calls_the_control_and_shows_errors() {
        let (mut w, fake) = setup();
        *fake.create_result.lock().unwrap() = Some("name taken".into());
        open(&mut w);
        let field = center(&w, "field:name");
        w.click(field);
        w.type_text("Accept edits");
        w.frame(vec![]);
        click(&mut w, "agent:Codex");
        click(&mut w, "create");
        assert_eq!(fake.calls(), ["create Accept edits codex"]);
        w.frame(vec![]);
        assert!(shows(&w, "name taken"));
        assert!(w.app.automations_open());
        *fake.create_result.lock().unwrap() = None;
        click(&mut w, "create");
        assert!(
            !w.app.automations_open(),
            "the sheet gives way to the terminal"
        );
    }

    #[test]
    fn approve_and_dismiss_call_the_control() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().pending = vec![pending(7), pending(8)];
        open(&mut w);
        assert!(shows(&w, "62% sure"));
        click(&mut w, "approve:7");
        click(&mut w, "dismiss:8");
        assert_eq!(fake.calls(), ["approve 7", "dismiss 8"]);
    }

    #[test]
    fn a_pending_confirmation_shows_a_banner_outside_the_sheet() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().pending = vec![pending(7)];
        w.frame(vec![]);
        w.frame(vec![]);
        let banner = &w.app.hits.automations_banner.rects;
        assert!(banner.contains_key("banner-approve"), "banner drawn");
        let at = banner["banner-approve"].center();
        w.click(at);
        w.frame(vec![]);
        assert_eq!(fake.calls(), ["approve 7"]);
        // Review opens the sheet.
        let at = w
            .app
            .hits
            .automations_banner
            .rects
            .get("banner-review")
            .unwrap()
            .center();
        w.click(at);
        w.frame(vec![]);
        assert!(w.app.automations_open());
    }

    #[test]
    fn automation_controls_call_the_control() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        open(&mut w);
        click(&mut w, "enabled:accept-edits");
        click(&mut w, "raise:accept-edits");
        click(&mut w, "lower:accept-edits");
        // Delete asks first.
        click(&mut w, "delete:accept-edits");
        assert!(!fake.calls().iter().any(|c| c.starts_with("delete")));
        click(&mut w, "confirm-delete:accept-edits");
        assert_eq!(
            fake.calls(),
            [
                "set_enabled accept-edits true",
                "set_threshold accept-edits 0.85",
                "set_threshold accept-edits 0.75",
                "delete accept-edits"
            ]
        );
    }

    #[test]
    fn open_session_closes_the_sheet() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        open(&mut w);
        click(&mut w, "open:accept-edits");
        assert_eq!(fake.calls(), ["open_session accept-edits"]);
        assert!(!w.app.automations_open());
    }

    #[test]
    fn history_lists_calls_and_clears() {
        let (mut w, fake) = setup();
        {
            let mut view = fake.view.lock().unwrap();
            view.automations = vec![item("accept-edits")];
            view.history = Arc::new(vec![HistoryRecord {
                id: 1,
                request: None,
                response: None,
                time_ms: 1_794_139_190_000,
                project: "quill".into(),
                terminal_id: "claude-1".into(),
                terminal_name: None,
                automation: "accept-edits".into(),
                answer: Some("Yes".into()),
                confidence: Some(0.93),
                outcome: AutomationOutcome::Fired,
                message: None,
                latency_ms: 420,
                input_tokens: 0,
                output_tokens: 0,
                screen: String::new(),
            }]);
        }
        open(&mut w);
        click(&mut w, "tab:History (1)");
        assert!(shows(&w, "fired"));
        assert!(shows(&w, "93%"));
        assert!(shows(&w, "420 ms"));
        click(&mut w, "clear-history");
        assert_eq!(fake.calls(), ["clear_history"]);
    }

    #[test]
    fn without_a_handle_the_sheet_says_it_is_unavailable() {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        open(&mut w);
        assert!(shows(&w, "not available"));
        w.frame(esc());
        w.frame(vec![]);
        assert!(!w.app.automations_open());
    }

    #[test]
    fn a_press_on_a_confirmation_that_expires_does_not_approve_the_next_one() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().pending = vec![pending(7), pending(8)];
        open(&mut w);
        let at = center(&w, "approve:7");
        w.press(at);
        // Confirmation 7 expires; 8 takes its place under the pointer.
        fake.view.lock().unwrap().pending = vec![pending(8)];
        w.frame(vec![]);
        w.release(at);
        w.frame(vec![]);
        assert!(
            fake.calls().is_empty(),
            "nothing approved: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn the_banner_ignores_a_press_begun_on_an_expired_confirmation() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().pending = vec![pending(7), pending(8)];
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .automations_banner
                .rects
                .contains_key("banner-approve")
        );
        let at = w.app.hits.automations_banner.rects["banner-approve"].center();
        w.press(at);
        fake.view.lock().unwrap().pending = vec![pending(8)];
        w.frame(vec![]);
        w.release(at);
        w.frame(vec![]);
        assert!(
            fake.calls().is_empty(),
            "nothing approved: {:?}",
            fake.calls()
        );
    }

    fn undoable(w: &W, secret: &str) -> bool {
        let Some(state) = egui::TextEdit::load_state(&w.ctx, crate::automations::key_field_id())
        else {
            return false;
        };
        let current = (egui::text::CCursorRange::default(), String::new());
        let mut undoer = state.undoer();
        undoer.has_undo(&current)
            && undoer
                .undo(&current)
                .is_some_and(|(_, text)| text.contains(secret))
    }

    /// Put the secret into the field's undo history, as typing over time does.
    fn seed_history(w: &W, secret: &str) {
        let id = crate::automations::key_field_id();
        let mut state = egui::TextEdit::load_state(&w.ctx, id).unwrap_or_default();
        let mut undoer = state.undoer();
        undoer.add_undo(&(egui::text::CCursorRange::default(), secret.to_owned()));
        state.set_undoer(undoer);
        egui::TextEdit::store_state(&w.ctx, id, state);
        assert!(undoable(w, secret), "the history holds the secret");
    }

    #[test]
    fn saving_the_key_wipes_the_field_and_its_undo_history() {
        let (mut w, fake) = setup();
        open_setup(&mut w);
        let field = center(&w, "field:key");
        w.click(field);
        w.type_text("jev-secret");
        w.frame(vec![]);
        seed_history(&w, "jev-secret");
        click(&mut w, "save-key");
        assert_eq!(fake.calls(), ["set_api_key jev-secret"]);
        assert_eq!(w.app.automations_key_input().as_deref(), Some(""));
        assert!(!undoable(&w, "jev-secret"));
    }

    #[test]
    fn clearing_the_key_wipes_what_was_typed() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().key = KeyStatus::Present;
        open_setup(&mut w);
        let field = center(&w, "field:key");
        w.click(field);
        w.type_text("jev-secret");
        w.frame(vec![]);
        seed_history(&w, "jev-secret");
        click(&mut w, "clear-key");
        assert_eq!(w.app.automations_key_input().as_deref(), Some(""));
        assert!(!undoable(&w, "jev-secret"));
    }

    #[test]
    fn closing_the_sheet_wipes_an_unsaved_key() {
        let (mut w, _fake) = setup();
        open_setup(&mut w);
        let field = center(&w, "field:key");
        w.click(field);
        w.type_text("jev-secret");
        w.frame(vec![]);
        seed_history(&w, "jev-secret");
        w.frame(esc());
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.automations_open());
        assert!(!undoable(&w, "jev-secret"));
    }

    fn assert_delete_prompt_reachable(w: &mut W, fake: &FakeAutomations) {
        open(w);
        click(w, "delete:accept-edits");
        let sheet_right = w.app.hits.main.min.x + 540.0_f32.min(w.app.hits.main.width());
        for key in ["confirm-delete:accept-edits", "cancel-delete:accept-edits"] {
            let rect = w.app.hits.automations.rects[key];
            assert!(
                rect.max.x <= sheet_right && rect.min.x >= w.app.hits.main.min.x,
                "{key} at {rect:?} is outside the sheet ending at {sheet_right}"
            );
        }
        click(w, "confirm-delete:accept-edits");
        assert_eq!(fake.calls(), ["delete accept-edits"]);
    }

    #[test]
    fn the_delete_prompt_stays_reachable_in_a_narrow_window() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        w.width = 640.0;
        w.frame(vec![]);
        assert_delete_prompt_reachable(&mut w, &fake);
    }

    #[test]
    fn the_delete_prompt_stays_reachable_with_a_long_name() {
        let (mut w, fake) = setup();
        let mut long = item("accept-edits");
        long.name = "Accept every edit the agent proposes in the current project".into();
        fake.view.lock().unwrap().automations = vec![long];
        assert_delete_prompt_reachable(&mut w, &fake);
    }

    #[test]
    fn a_press_on_a_row_that_is_removed_does_not_act_on_the_next_row() {
        for control in [
            "enabled:first",
            "raise:first",
            "lower:first",
            "open:first",
            "delete:first",
        ] {
            let (mut w, fake) = setup();
            fake.view.lock().unwrap().automations = vec![item("first"), item("second")];
            open(&mut w);
            let at = center(&w, control);
            w.press(at);
            // The first row goes; the second moves up under the pointer.
            fake.view.lock().unwrap().automations = vec![item("second")];
            w.frame(vec![]);
            w.release(at);
            w.frame(vec![]);
            assert!(fake.calls().is_empty(), "{control}: {:?}", fake.calls());
            assert!(
                !w.app
                    .hits
                    .automations
                    .rects
                    .contains_key("confirm-delete:second"),
                "{control}: no delete prompt opened"
            );
        }
    }

    #[test]
    fn open_session_shows_the_authoring_terminal_over_an_earlier_pick() {
        for create in [false, true] {
            let (_quill, _connecto, ws) = two_projects();
            let fake = Arc::new(FakeAutomations::default());
            if !create {
                // With a row listed, the form is below the fold of the window.
                fake.view.lock().unwrap().automations = vec![item("accept-edits")];
            }
            fake.view.lock().unwrap().root = Some(PathBuf::from("/automations"));
            *fake.navigate.lock().unwrap() =
                Some((Arc::clone(&ws), "quill".into(), "claude-3".into()));
            let mut w =
                W::with_automations(&ws, Some(Arc::clone(&fake) as Arc<dyn AutomationsControl>));
            // The user picked another terminal of the project first.
            w.click_hit(&Hit::Terminal("quill".into(), "codex-1".into()));
            assert_eq!(w.shown(), Some(("quill".into(), "codex-1".into())));
            open(&mut w);
            if create {
                let field = center(&w, "field:name");
                w.click(field);
                w.type_text("New one");
                w.frame(vec![]);
                click(&mut w, "create");
            } else {
                click(&mut w, "open:accept-edits");
            }
            assert!(!w.app.automations_open());
            w.frame(vec![]);
            w.frame(vec![]);
            assert_eq!(
                w.shown(),
                Some(("quill".into(), "claude-3".into())),
                "create: {create}"
            );
        }
    }

    fn without_root() -> (W, Arc<FakeAutomations>) {
        let (w, fake) = setup();
        fake.view.lock().unwrap().root = None;
        (w, fake)
    }

    #[test]
    fn the_folder_is_shown_or_reported_not_set() {
        let (mut w, _fake) = setup();
        open_setup(&mut w);
        assert!(shows(&w, "Folder: /automations"));
        let (mut w, _fake) = without_root();
        open_setup(&mut w);
        assert!(shows(&w, "Folder: not set"));
    }

    #[test]
    fn the_folder_is_chosen_in_settings_not_here() {
        let (mut w, fake) = without_root();
        open_setup(&mut w);
        assert!(
            !w.app.hits.automations.rects.contains_key("choose-root"),
            "no folder picker in this sheet"
        );
        click(&mut w, "open-settings");
        assert!(fake.calls().is_empty(), "nothing is set from here");
        assert!(!w.app.automations_open());
        assert!(w.app.settings_open());
    }

    #[test]
    fn the_login_buttons_open_a_terminal_once_a_folder_is_set() {
        let (mut w, fake) = without_root();
        open_setup(&mut w);
        click(&mut w, "login-claude");
        click(&mut w, "login-codex");
        assert!(fake.calls().is_empty(), "disabled without a folder");
        let (mut w, fake) = setup();
        open_setup(&mut w);
        click(&mut w, "login-codex");
        assert_eq!(fake.calls(), ["open_login codex"]);
        assert!(!w.app.automations_open());
        let (mut w, fake) = setup();
        *fake.login_result.lock().unwrap() = Some("no terminal".into());
        open_setup(&mut w);
        click(&mut w, "login-claude");
        w.frame(vec![]);
        assert_eq!(fake.calls(), ["open_login claude"]);
        assert!(shows(&w, "no terminal"));
        assert!(w.app.automations_open());
    }

    #[test]
    fn create_and_open_session_are_disabled_without_a_folder() {
        let (mut w, fake) = without_root();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        open(&mut w);
        assert!(shows(&w, "Set Connecto Home in Settings first"));
        click(&mut w, "new");
        let field = center(&w, "field:name");
        w.click(field);
        w.type_text("Accept edits");
        w.frame(vec![]);
        click(&mut w, "create");
        click(&mut w, "open:accept-edits");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        assert!(w.app.automations_open());
    }

    #[test]
    fn opening_a_login_shows_its_terminal_over_an_earlier_pick() {
        let (_quill, _connecto, ws) = two_projects();
        let fake = Arc::new(FakeAutomations::default());
        *fake.navigate.lock().unwrap() = Some((Arc::clone(&ws), "quill".into(), "claude-3".into()));
        fake.view.lock().unwrap().root = Some(PathBuf::from("/automations"));
        let mut w =
            W::with_automations(&ws, Some(Arc::clone(&fake) as Arc<dyn AutomationsControl>));
        w.click_hit(&Hit::Terminal("quill".into(), "codex-1".into()));
        open_setup(&mut w);
        click(&mut w, "login-claude");
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.shown(), Some(("quill".into(), "claude-3".into())));
    }

    fn last(
        outcome: AutomationOutcome,
        answer: Option<&str>,
        confidence: Option<f32>,
        repeat: u32,
    ) -> connecto_core::LastCheck {
        connecto_core::LastCheck {
            // 2026-11-07 12:00 UTC, the fixed clock's day.
            at: std::time::UNIX_EPOCH
                + std::time::Duration::from_millis(1_794_139_200_000 - 60_000),
            terminal: "claude-3".into(),
            outcome,
            answer: answer.map(Into::into),
            confidence,
            repeat,
        }
    }

    fn with_last(last: Option<connecto_core::LastCheck>) -> (W, Arc<FakeAutomations>) {
        let (mut w, fake) = setup();
        let mut one = item("accept-edits");
        one.last = last;
        fake.view.lock().unwrap().automations = vec![one];
        open(&mut w);
        (w, fake)
    }

    #[test]
    fn each_row_shows_its_last_result() {
        use AutomationOutcome::{Asked, Error, Fired, NoMatch};
        let cases = [
            (None, "Not checked yet"),
            (
                Some(last(NoMatch, None, None, 4)),
                "Last checked 11:59 · claude-3 · no match ×4",
            ),
            (
                Some(last(NoMatch, None, None, 1)),
                "Last checked 11:59 · claude-3 · no match",
            ),
            (
                Some(last(Fired, Some("Yes"), Some(0.96), 1)),
                "Fired 11:59 · 96%",
            ),
            (
                Some(last(Asked, Some("Yes"), Some(0.72), 1)),
                "Asked 11:59 · 72%",
            ),
            (Some(last(Error, Some("401"), None, 1)), "Error 11:59 · 401"),
        ];
        for (check, expected) in cases {
            let (w, _fake) = with_last(check);
            assert!(
                w.app.hits.automations.texts.iter().any(|t| t == expected),
                "{expected}: {:?}",
                w.app.hits.automations.texts
            );
        }
    }

    fn call(id: u64, request: Option<&str>, response: Option<&str>) -> HistoryRecord {
        HistoryRecord {
            id,
            time_ms: 1_794_139_190_000,
            terminal_id: "claude-3".into(),
            automation: "accept-edits".into(),
            answer: Some("Yes".into()),
            confidence: Some(0.93),
            outcome: AutomationOutcome::Fired,
            latency_ms: 420,
            request: request.map(Into::into),
            response: response.map(Into::into),
            ..HistoryRecord::default()
        }
    }

    fn reads(fake: &FakeAutomations) -> usize {
        fake.audit_reads.lock().unwrap().len()
    }

    #[test]
    fn history_for_is_read_only_while_the_audit_panel_is_open() {
        let (mut w, fake) = with_last(None);
        *fake.audit.lock().unwrap() = vec![call(1, None, None)];
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(reads(&fake), 0, "closed: never read");
        click(&mut w, "audit:accept-edits");
        assert_eq!(reads(&fake), 1);
        assert!(shows(&w, "420 ms"));
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(reads(&fake), 1, "cached while the view is unchanged");
        fake.view.lock().unwrap().revision += 1;
        w.frame(vec![]);
        assert_eq!(reads(&fake), 2, "read again when the view changes");
        click(&mut w, "audit:accept-edits");
        let after = reads(&fake);
        fake.view.lock().unwrap().revision += 1;
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(reads(&fake), after, "closed again: no more reads");
        assert_eq!(fake.audit_reads.lock().unwrap()[0], "accept-edits");
    }

    #[test]
    fn a_call_shows_its_request_and_response_and_copies_them() {
        let (mut w, fake) = with_last(None);
        *fake.audit.lock().unwrap() = vec![call(
            5,
            Some(r#"{"model":"jev","state":"Allow?"}"#),
            Some("502 bad gateway"),
        )];
        click(&mut w, "audit:accept-edits");
        assert!(!shows(&w, "bad gateway"), "collapsed until opened");
        click(&mut w, "call:accept-edits:5");
        assert!(shows(&w, "Request JSON"));
        assert!(shows(&w, "Response JSON"));
        assert!(
            shows(&w, "\"model\": \"jev\""),
            "valid JSON is pretty-printed: {:?}",
            w.app.hits.automations.texts
        );
        assert!(
            shows(&w, "502 bad gateway"),
            "text that is not JSON is kept as is"
        );
        click(&mut w, "copy-request:accept-edits:5");
        click(&mut w, "copy-response:accept-edits:5");
        assert_eq!(w.copied.len(), 2);
        assert!(w.copied[0].contains("\"state\": \"Allow?\""));
        assert_eq!(w.copied[1], "502 bad gateway");
    }

    #[test]
    fn audit_controls_are_scoped_by_row() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("first"), item("second")];
        *fake.audit.lock().unwrap() = vec![call(1, None, None)];
        open(&mut w);
        // A press on the first row's toggle, whose row then goes, must not
        // open the second row's panel.
        let at = center(&w, "audit:first");
        w.press(at);
        fake.view.lock().unwrap().automations = vec![item("second")];
        w.frame(vec![]);
        w.release(at);
        w.frame(vec![]);
        assert_eq!(reads(&fake), 0, "nothing opened");
        // Opening one row's panel leaves the other closed.
        fake.view.lock().unwrap().automations = vec![item("first"), item("second")];
        w.frame(vec![]);
        click(&mut w, "audit:second");
        assert_eq!(fake.audit_reads.lock().unwrap().as_slice(), ["second"]);
        assert!(w.app.hits.automations.rects.contains_key("audit:first"));
    }

    fn type_into(w: &mut W, key: &str, text: &str) {
        let at = center(w, key);
        w.click(at);
        w.type_text(text);
        w.frame(vec![]);
    }

    #[test]
    fn both_description_fields_call_create_described() {
        let (mut w, fake) = setup();
        open(&mut w);
        type_into(&mut w, "field:name", "Keep going");
        type_into(&mut w, "field:screen", "Claude asks to proceed");
        type_into(&mut w, "field:action", "choose option 2");
        // The text survives the frames in between.
        w.frame(vec![]);
        assert_eq!(
            w.app.automations_form(),
            Some((
                "Keep going".into(),
                "Claude asks to proceed".into(),
                "choose option 2".into()
            ))
        );
        click(&mut w, "agent:Codex");
        click(&mut w, "create");
        assert_eq!(
            fake.calls(),
            ["create_described Keep going codex | Claude asks to proceed | choose option 2"]
        );
    }

    #[test]
    fn empty_description_fields_call_the_screenshot_create() {
        let (mut w, fake) = setup();
        open(&mut w);
        type_into(&mut w, "field:name", "Keep going");
        click(&mut w, "create");
        assert_eq!(fake.calls(), ["create Keep going claude"]);
    }

    #[test]
    fn a_half_filled_description_shows_a_hint_and_creates_nothing() {
        for (screen, action) in [("Claude asks", ""), ("", "choose option 2")] {
            let (mut w, fake) = setup();
            open(&mut w);
            type_into(&mut w, "field:name", "Keep going");
            if !screen.is_empty() {
                type_into(&mut w, "field:screen", screen);
            }
            if !action.is_empty() {
                type_into(&mut w, "field:action", action);
            }
            assert!(shows(&w, "Fill in both fields"));
            click(&mut w, "create");
            assert!(fake.calls().is_empty(), "{:?}", fake.calls());
            assert!(w.app.automations_open());
        }
    }

    #[test]
    fn the_description_clears_after_success_and_stays_after_an_error() {
        let (mut w, fake) = setup();
        *fake.create_result.lock().unwrap() = Some("name taken".into());
        open(&mut w);
        type_into(&mut w, "field:name", "Keep going");
        type_into(&mut w, "field:screen", "Claude asks");
        type_into(&mut w, "field:action", "choose 2");
        click(&mut w, "create");
        w.frame(vec![]);
        assert!(shows(&w, "name taken"));
        assert_eq!(
            w.app.automations_form(),
            Some(("Keep going".into(), "Claude asks".into(), "choose 2".into())),
            "an error keeps what was typed"
        );
        *fake.create_result.lock().unwrap() = None;
        click(&mut w, "create");
        assert!(!w.app.automations_open());
        open(&mut w);
        assert_eq!(
            w.app.automations_form(),
            Some((String::new(), String::new(), String::new()))
        );
    }

    #[test]
    fn describing_is_disabled_without_a_folder() {
        let (mut w, fake) = without_root();
        open(&mut w);
        type_into(&mut w, "field:name", "Keep going");
        type_into(&mut w, "field:screen", "Claude asks");
        type_into(&mut w, "field:action", "choose 2");
        click(&mut w, "create");
        assert!(fake.calls().is_empty());
    }

    fn hovers(w: &W) -> &[String] {
        &w.app.hits.automations.hovers
    }

    #[test]
    fn trigger_phrases_are_shown_and_long_ones_cut_short() {
        let (mut w, fake) = setup();
        let long = "Claude in Chrome extension detected and it is waiting for you to confirm the connection";
        let mut one = item("accept-edits");
        one.trigger = vec![long.into(), "Yes, use my browser".into()];
        fake.view.lock().unwrap().automations = vec![one];
        open(&mut w);
        let line = w
            .app
            .hits
            .automations
            .texts
            .iter()
            .find(|t| t.starts_with("Trigger:"))
            .cloned()
            .unwrap_or_default();
        assert!(line.ends_with("+ \"Yes, use my browser\""), "{line}");
        assert!(line.contains('…'), "the long phrase is cut: {line}");
        assert!(!line.contains("confirm the connection"), "{line}");
        assert_eq!(
            hovers(&w),
            [format!("Trigger: \"{long}\" + \"Yes, use my browser\"")],
            "the full text is the hover"
        );
    }

    #[test]
    fn a_row_without_trigger_text_warns_and_offers_open_session_beside_it() {
        let (mut w, fake) = setup();
        let mut one = item("accept-edits");
        one.trigger = Vec::new();
        one.needs_trigger = true;
        fake.view.lock().unwrap().automations = vec![one, item("fine")];
        open(&mut w);
        let warning = "Needs trigger text — not running. Open session to add one.";
        assert_eq!(
            w.app
                .hits
                .automations
                .texts
                .iter()
                .filter(|t| t.as_str() == warning)
                .count(),
            1,
            "only the row that needs it"
        );
        assert!(shows(&w, "Trigger: none"));
        click(&mut w, "open:accept-edits");
        assert_eq!(fake.calls(), ["open_session accept-edits"]);
    }

    #[test]
    fn a_press_on_a_row_that_needs_a_trigger_does_not_act_on_the_next_row() {
        let (mut w, fake) = setup();
        let mut first = item("first");
        first.needs_trigger = true;
        fake.view.lock().unwrap().automations = vec![first, item("second")];
        open(&mut w);
        let at = center(&w, "open:first");
        w.press(at);
        fake.view.lock().unwrap().automations = vec![item("second")];
        w.frame(vec![]);
        w.release(at);
        w.frame(vec![]);
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[test]
    fn setup_explains_the_trigger() {
        let (mut w, _fake) = setup();
        open_setup(&mut w);
        assert!(shows(&w, "trigger text appears"));
    }

    fn rects(w: &W, slug: &str, names: &[&str]) -> Vec<(String, Rect)> {
        names
            .iter()
            .map(|name| {
                let key = format!("{name}:{slug}");
                let found = w.app.hits.automations.rects.get(&key);
                assert!(found.is_some(), "{key} not drawn");
                (key, *found.unwrap())
            })
            .collect()
    }

    const CONTROLS: [&str; 4] = ["lower", "raise", "audit", "delete"];

    fn row_with_controls() -> (W, Arc<FakeAutomations>) {
        let (w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("accept-edits")];
        (w, fake)
    }

    fn overlap_y(a: Rect, b: Rect) -> bool {
        a.min.y < b.max.y && b.min.y < a.max.y
    }

    fn sheet_right(w: &W) -> f32 {
        w.app.hits.main.max.x
    }

    #[test]
    fn the_controls_share_one_row_at_the_default_width() {
        let (mut w, _fake) = row_with_controls();
        open(&mut w);
        let mut all = rects(&w, "accept-edits", &CONTROLS);
        all.extend(rects(&w, "accept-edits", &["open"]));
        let first = all[0].1;
        for (key, rect) in &all {
            assert!(
                overlap_y(first, *rect),
                "{key} {rect:?} is off the row {first:?}"
            );
        }
        let get = |name: &str| all.iter().find(|(k, _)| k.starts_with(name)).unwrap().1;
        assert!(
            get("raise").max.x < get("open").min.x,
            "left group, then right group"
        );
        assert!(get("open").max.x < get("audit").min.x);
        assert!(
            get("audit").max.x < get("delete").min.x,
            "Delete is last and lightest"
        );
        assert!(get("delete").max.x <= sheet_right(&w));
    }

    #[test]
    fn the_control_groups_wrap_whole_in_a_narrow_window() {
        let (mut w, fake) = row_with_controls();
        w.width = 640.0;
        w.frame(vec![]);
        open(&mut w);
        let left = rects(&w, "accept-edits", &["lower", "raise"]);
        let right = rects(&w, "accept-edits", &["open", "audit", "delete"]);
        for group in [&left, &right] {
            for (key, rect) in group {
                assert!(overlap_y(group[0].1, *rect), "{key} dangles from its group");
            }
        }
        assert!(
            right[0].1.min.y >= left[0].1.max.y,
            "the right group is a row of its own"
        );
        for (key, rect) in left.iter().chain(right.iter()) {
            assert!(
                rect.min.x >= w.app.hits.main.min.x && rect.max.x <= sheet_right(&w),
                "{key} at {rect:?} is outside the sheet"
            );
        }
        // And still reachable.
        click(&mut w, "open:accept-edits");
        assert_eq!(fake.calls(), ["open_session accept-edits"]);
    }

    #[test]
    fn the_status_pill_says_active_off_or_needs_trigger() {
        let (mut w, fake) = setup();
        let mut active = item("active");
        active.enabled = true;
        let off = item("off");
        let mut needs = item("needs");
        needs.needs_trigger = true;
        needs.enabled = true;
        fake.view.lock().unwrap().automations = vec![active, off, needs];
        open(&mut w);
        let texts = &w.app.hits.automations.texts;
        for word in ["Active", "Off", "Needs trigger"] {
            assert_eq!(
                texts.iter().filter(|t| t.as_str() == word).count(),
                1,
                "{word}: {texts:?}"
            );
        }
        for slug in ["active", "off", "needs"] {
            let pill = w.app.hits.automations.rects[&format!("status:{slug}")];
            assert!(pill.max.x <= sheet_right(&w) && pill.min.x > w.app.hits.main.min.x + 300.0);
        }
    }

    #[test]
    fn the_audit_toggle_shows_whether_it_is_open() {
        let (mut w, fake) = row_with_controls();
        open(&mut w);
        // The panel under the row is what says it is open.
        assert!(!shows(&w, "Audit · "));
        click(&mut w, "audit:accept-edits");
        assert!(shows(&w, "Audit · "));
        assert_eq!(fake.audit_reads.lock().unwrap().len(), 1);
        click(&mut w, "audit:accept-edits");
        assert!(!shows(&w, "Audit · "));
    }

    #[test]
    fn the_audit_panel_sits_below_the_controls_inside_the_card() {
        let (mut w, fake) = row_with_controls();
        *fake.audit.lock().unwrap() = vec![call(1, None, None)];
        open(&mut w);
        click(&mut w, "audit:accept-edits");
        let controls = w.app.hits.automations.rects["audit:accept-edits"];
        let call_row = w.app.hits.automations.rects["call:accept-edits:1"];
        assert!(call_row.min.y >= controls.max.y);
    }

    #[test]
    fn delete_hides_while_confirming_and_keep_restores_it() {
        let (mut w, fake) = row_with_controls();
        open(&mut w);
        click(&mut w, "delete:accept-edits");
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("delete:accept-edits")
        );
        click(&mut w, "cancel-delete:accept-edits");
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("delete:accept-edits")
        );
        click(&mut w, "delete:accept-edits");
        click(&mut w, "confirm-delete:accept-edits");
        assert_eq!(fake.calls(), ["delete accept-edits"]);
    }

    #[test]
    fn esc_cancels_a_pending_confirm_before_it_closes_the_sheet() {
        let (mut w, fake) = row_with_controls();
        open(&mut w);
        click(&mut w, "delete:accept-edits");
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("confirm-delete:accept-edits")
        );
        w.frame(esc());
        w.frame(vec![]);
        assert!(
            w.app.automations_open(),
            "the first Esc only cancels the prompt"
        );
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("confirm-delete:accept-edits")
        );
        // The same for Clear audit.
        *fake.audit.lock().unwrap() = vec![call(1, None, None)];
        click(&mut w, "audit:accept-edits");
        click(&mut w, "clear-audit:accept-edits");
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("confirm-clear:accept-edits")
        );
        w.frame(esc());
        w.frame(vec![]);
        assert!(w.app.automations_open());
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("confirm-clear:accept-edits")
        );
        assert!(fake.calls().is_empty(), "nothing was deleted or cleared");
        w.frame(esc());
        w.frame(vec![]);
        assert!(!w.app.automations_open(), "the next Esc closes the sheet");
    }

    #[test]
    fn the_close_button_closes_the_sheet_even_with_a_confirm_open() {
        let (mut w, _fake) = row_with_controls();
        open(&mut w);
        click(&mut w, "delete:accept-edits");
        let close = w
            .ctx
            .data(|d| d.get_temp::<Rect>(crate::sheet::close_button_id()))
            .unwrap_or(Rect::NOTHING);
        w.click(close.center());
        w.frame(vec![]);
        assert!(!w.app.automations_open());
    }

    fn open_audit(w: &mut W, fake: &FakeAutomations, calls: usize) {
        *fake.audit.lock().unwrap() = (1..=calls as u64).map(|id| call(id, None, None)).collect();
        click(w, "audit:accept-edits");
    }

    #[test]
    fn clearing_an_audit_asks_first_then_calls_the_control_and_refetches() {
        let (mut w, fake) = row_with_controls();
        open(&mut w);
        open_audit(&mut w, &fake, 3);
        assert!(shows(&w, "Audit · 3 calls"));
        click(&mut w, "clear-audit:accept-edits");
        assert!(shows(&w, "Clear 3 calls for Accept edits?"));
        assert!(fake.calls().is_empty(), "nothing before the confirm");
        let before = reads(&fake);
        click(&mut w, "confirm-clear:accept-edits");
        assert_eq!(fake.calls(), ["clear_history_for accept-edits"]);
        w.frame(vec![]);
        assert!(reads(&fake) > before, "the panel read its calls again");
        assert!(shows(&w, "No calls yet."));
        assert!(shows(&w, "Audit · 0 calls"));
    }

    #[test]
    fn keep_clears_nothing() {
        let (mut w, fake) = row_with_controls();
        open(&mut w);
        open_audit(&mut w, &fake, 2);
        click(&mut w, "clear-audit:accept-edits");
        click(&mut w, "keep-clear:accept-edits");
        assert!(fake.calls().is_empty());
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("clear-audit:accept-edits")
        );
        assert!(shows(&w, "Audit · 2 calls"));
    }

    #[test]
    fn clear_audit_is_disabled_when_there_are_no_calls() {
        let (mut w, fake) = row_with_controls();
        open(&mut w);
        open_audit(&mut w, &fake, 0);
        click(&mut w, "clear-audit:accept-edits");
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("confirm-clear:accept-edits")
        );
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn a_failed_clear_shows_the_error_and_keeps_the_calls() {
        let (mut w, fake) = row_with_controls();
        *fake.clear_audit_result.lock().unwrap() = Some("disk full".into());
        open(&mut w);
        open_audit(&mut w, &fake, 2);
        click(&mut w, "clear-audit:accept-edits");
        click(&mut w, "confirm-clear:accept-edits");
        w.frame(vec![]);
        assert!(shows(&w, "disk full"));
        assert!(shows(&w, "Audit · 2 calls"));
    }

    #[test]
    fn a_press_on_clear_audit_whose_row_is_removed_does_not_act_on_the_next_row() {
        let (mut w, fake) = setup();
        fake.view.lock().unwrap().automations = vec![item("first"), item("second")];
        *fake.audit.lock().unwrap() = vec![call(1, None, None)];
        open(&mut w);
        click(&mut w, "audit:first");
        click(&mut w, "audit:second");
        let at = center(&w, "clear-audit:first");
        w.press(at);
        fake.view.lock().unwrap().automations = vec![item("second")];
        w.frame(vec![]);
        w.release(at);
        w.frame(vec![]);
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("confirm-clear:second"),
            "no prompt opened on the next row"
        );
        assert!(fake.calls().is_empty());
    }

    // ---------- groups ----------

    fn named(slug: &str, name: &str, agent: ToolKind, on: bool) -> AutomationItem {
        AutomationItem {
            name: name.into(),
            agent,
            enabled: on,
            ..item(slug)
        }
    }

    fn group(
        id: &str,
        title: &str,
        agent: ToolKind,
        members: &[&str],
    ) -> connecto_core::AutomationGroup {
        connecto_core::AutomationGroup {
            id: id.into(),
            title: title.into(),
            about: "Gets past the first screens.".into(),
            agent,
            members: members.iter().map(|m| (*m).to_owned()).collect(),
            custom: false,
        }
    }

    /// Two groups (Claude with three, Codex with one) and one automation of the user's own.
    fn grouped() -> (W, Arc<FakeAutomations>) {
        let (w, fake) = setup();
        {
            let mut view = fake.view.lock().unwrap();
            view.key = KeyStatus::Present;
            view.automations = vec![
                named(
                    "accept-claude-welcome",
                    "Accept the welcome",
                    ToolKind::Claude,
                    false,
                ),
                named(
                    "accept-claude-dark-theme",
                    "Pick the theme",
                    ToolKind::Claude,
                    true,
                ),
                named(
                    "accept-claude-trust-folder",
                    "Trust the folder",
                    ToolKind::Claude,
                    false,
                ),
                named(
                    "accept-codex-folder-trust",
                    "Accept the Codex folder prompt",
                    ToolKind::Codex,
                    false,
                ),
                named("mine", "My own automation", ToolKind::Claude, false),
            ];
            view.groups = vec![
                group(
                    "accept-defaults-claude",
                    "Accept defaults for Claude",
                    ToolKind::Claude,
                    &[
                        "accept-claude-welcome",
                        "accept-claude-dark-theme",
                        "accept-claude-trust-folder",
                    ],
                ),
                group(
                    "accept-defaults-codex",
                    "Accept defaults for Codex",
                    ToolKind::Codex,
                    &["accept-codex-folder-trust"],
                ),
            ];
        }
        (w, fake)
    }

    fn shows_text(w: &W, text: &str) -> bool {
        w.app
            .hits
            .automations
            .texts
            .iter()
            .any(|t| t.contains(text))
    }

    #[test]
    fn groups_are_listed_closed_with_how_many_are_on_and_the_users_own_stay_loose() {
        let (mut w, _fake) = grouped();
        open_collapsed(&mut w);
        assert!(shows_text(&w, "Accept defaults for Claude") && shows_text(&w, "1 of 3 on"));
        assert!(shows_text(&w, "Accept defaults for Codex") && shows_text(&w, "0 of 1 on"));
        assert!(
            w.app.hits.automations.rects.contains_key("row:mine"),
            "the user's own is a row"
        );
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("row:accept-claude-welcome"),
            "a member shows only once its group is opened"
        );
    }

    #[test]
    fn the_whole_header_line_opens_a_group_and_its_members_have_their_own_switches() {
        let (mut w, fake) = grouped();
        open_collapsed(&mut w);
        let rect = w.app.hits.automations.rects["group-row:accept-defaults-claude"];
        w.click(rect.min + egui::vec2(10.0, rect.height() / 2.0));
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("row:accept-claude-welcome")
        );
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("row:accept-claude-trust-folder")
        );
        assert!(shows_text(&w, "Gets past the first screens."));
        // One member's switch changes only that member.
        click(&mut w, "enabled:accept-claude-welcome");
        assert_eq!(fake.calls(), ["set_enabled accept-claude-welcome true"]);
    }

    #[test]
    fn the_group_switch_turns_all_of_its_members_on_or_off_without_touching_the_rest() {
        let (mut w, fake) = grouped();
        open_collapsed(&mut w);
        click(&mut w, "group-enabled:accept-defaults-claude");
        assert_eq!(
            fake.calls(),
            [
                "set_enabled accept-claude-welcome true",
                "set_enabled accept-claude-dark-theme true",
                "set_enabled accept-claude-trust-folder true",
            ],
            "partly on turns all on; the Codex group and the user's own are left alone"
        );
        // Now every member is on: the switch turns them all off.
        {
            let mut view = fake.view.lock().unwrap();
            for item in &mut view.automations {
                if item.slug.starts_with("accept-claude") {
                    item.enabled = true;
                }
            }
        }
        w.frame(vec![]);
        fake.calls.lock().unwrap().clear();
        click(&mut w, "group-enabled:accept-defaults-claude");
        assert_eq!(
            fake.calls(),
            [
                "set_enabled accept-claude-welcome false",
                "set_enabled accept-claude-dark-theme false",
                "set_enabled accept-claude-trust-folder false",
            ]
        );
    }

    #[test]
    fn the_agent_tabs_filter_groups_too() {
        let (mut w, _fake) = grouped();
        open_collapsed(&mut w);
        click(&mut w, "filter:Codex");
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("group-row:accept-defaults-codex")
        );
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("group-row:accept-defaults-claude")
        );
        assert!(!w.app.hits.automations.rects.contains_key("row:mine"));
        click(&mut w, "filter:Claude");
        assert!(
            w.app
                .hits
                .automations
                .rects
                .contains_key("group-row:accept-defaults-claude")
        );
        assert!(
            !w.app
                .hits
                .automations
                .rects
                .contains_key("group-row:accept-defaults-codex")
        );
    }

    #[test]
    fn a_group_with_a_member_that_cannot_run_switches_on_only_the_ones_that_can() {
        let (mut w, fake) = grouped();
        {
            let mut view = fake.view.lock().unwrap();
            let broken = view
                .automations
                .iter_mut()
                .find(|a| a.slug == "accept-claude-trust-folder")
                .unwrap();
            broken.errors = vec!["not valid".into()];
        }
        open_collapsed(&mut w);
        click(&mut w, "group-enabled:accept-defaults-claude");
        assert_eq!(
            fake.calls(),
            [
                "set_enabled accept-claude-welcome true",
                "set_enabled accept-claude-dark-theme true"
            ]
        );
    }

    // ---------- groups of the user's own ----------

    fn custom(id: &str, title: &str, members: &[&str]) -> connecto_core::AutomationGroup {
        connecto_core::AutomationGroup {
            id: id.into(),
            title: title.into(),
            about: String::new(),
            agent: ToolKind::Claude,
            members: members.iter().map(|m| (*m).to_owned()).collect(),
            custom: true,
        }
    }

    /// `grouped()` plus an empty group "Mine" (`g-empty`) and "Work" (`g-work`)
    /// holding `member`.
    fn with_custom() -> (W, Arc<FakeAutomations>) {
        let (w, fake) = grouped();
        {
            let mut view = fake.view.lock().unwrap();
            view.automations
                .push(named("member", "A member", ToolKind::Claude, false));
            view.groups.push(custom("g-empty", "Mine", &[]));
            view.groups.push(custom("g-work", "Work", &["member"]));
        }
        (w, fake)
    }

    fn has(w: &W, key: &str) -> bool {
        w.app.hits.automations.rects.contains_key(key)
    }

    /// Drag from the left of one mark to the middle of another.
    fn drag(w: &mut W, from: &str, to: &str) {
        let start = center(w, from) - egui::vec2(20.0, 0.0);
        let end = center(w, to);
        w.frame(vec![Event::PointerMoved(start)]);
        w.frame(vec![super::button(start, true)]);
        for step in 1..=4 {
            let at = start + (end - start) * (step as f32 / 4.0);
            w.frame(vec![Event::PointerMoved(at)]);
        }
        w.frame(vec![Event::PointerMoved(end)]);
        w.frame(vec![super::button(end, false)]);
        w.frame(vec![]);
    }

    #[test]
    fn new_group_asks_for_a_title_and_creates_on_create() {
        let (mut w, fake) = with_custom();
        open_collapsed(&mut w);
        assert!(!has(&w, "field:new-group"));
        click(&mut w, "new-group");
        assert!(has(&w, "field:new-group"));
        w.type_text("  Nightly ");
        click(&mut w, "create-group");
        assert_eq!(fake.calls(), ["create_group Nightly"]);
        assert!(!has(&w, "field:new-group"), "closed after it was made");
    }

    #[test]
    fn enter_creates_the_group_and_escape_cancels_the_entry() {
        let (mut w, fake) = with_custom();
        open_collapsed(&mut w);
        click(&mut w, "new-group");
        w.type_text("Nightly");
        w.key(Key::Enter, Modifiers::NONE);
        w.frame(vec![]);
        assert_eq!(fake.calls(), ["create_group Nightly"]);
        click(&mut w, "new-group");
        w.type_text("x");
        w.key(Key::Escape, Modifiers::NONE);
        w.frame(vec![]);
        assert!(!has(&w, "field:new-group"));
        assert!(w.app.automations_open(), "Esc cancelled the entry only");
        assert_eq!(fake.calls().len(), 1);
    }

    #[test]
    fn create_stays_off_for_a_blank_title_and_a_refusal_is_shown() {
        let (mut w, fake) = with_custom();
        open_collapsed(&mut w);
        click(&mut w, "new-group");
        w.type_text("   ");
        click(&mut w, "create-group");
        assert!(fake.calls().is_empty());
        click(&mut w, "field:new-group");
        w.type_text("work");
        *fake.group_result.lock().unwrap() = Some("A group named work exists.".into());
        click(&mut w, "create-group");
        assert!(shows_text(&w, "A group named work exists."));
        assert!(has(&w, "field:new-group"), "kept for another try");
        click(&mut w, "cancel-group");
        assert!(!has(&w, "field:new-group"));
        assert!(!shows_text(&w, "A group named work exists."));
    }

    #[test]
    fn a_group_of_the_users_shows_title_members_and_a_hint_when_empty() {
        let (mut w, _fake) = with_custom();
        open_collapsed(&mut w);
        assert!(has(&w, "group-row:g-empty") && has(&w, "group-row:g-work"));
        assert!(shows_text(&w, "Mine") && shows_text(&w, "0 of 0 on"));
        assert!(shows_text(&w, "Drag automations here"));
        assert!(!has(&w, "row:member"), "a closed group hides its members");
        let rect = w.app.hits.automations.rects["group-row:g-work"];
        w.click(rect.min + egui::vec2(10.0, rect.height() / 2.0));
        w.frame(vec![]);
        assert!(has(&w, "row:member"));
        assert!(has(&w, "rename-group:g-work") && has(&w, "delete-group:g-work"));
        assert!(
            !has(&w, "rename-group:accept-defaults-claude")
                && !has(&w, "delete-group:accept-defaults-claude"),
            "a shipped group has neither"
        );
    }

    #[test]
    fn a_group_is_renamed_in_place() {
        let (mut w, fake) = with_custom();
        open_collapsed(&mut w);
        click(&mut w, "rename-group:g-empty");
        assert!(has(&w, "field:rename-group:g-empty"));
        w.key(Key::A, Modifiers::COMMAND);
        w.type_text("Renamed");
        click(&mut w, "save-rename:g-empty");
        assert_eq!(fake.calls(), ["rename_group g-empty Renamed"]);
        assert!(!has(&w, "field:rename-group:g-empty"));
        // A refusal stays on screen with the entry open.
        click(&mut w, "rename-group:g-empty");
        *fake.group_result.lock().unwrap() = Some("Too long.".into());
        click(&mut w, "save-rename:g-empty");
        assert!(shows_text(&w, "Too long.") && has(&w, "field:rename-group:g-empty"));
        click(&mut w, "cancel-rename:g-empty");
        assert!(!has(&w, "field:rename-group:g-empty"));
    }

    #[test]
    fn deleting_a_group_asks_first_and_says_the_automations_are_kept() {
        let (mut w, fake) = with_custom();
        open_collapsed(&mut w);
        click(&mut w, "delete-group:g-empty");
        assert!(fake.calls().is_empty());
        assert!(shows_text(&w, "Its automations are kept."));
        click(&mut w, "keep-group:g-empty");
        assert!(fake.calls().is_empty() && !has(&w, "confirm-delete-group:g-empty"));
        click(&mut w, "delete-group:g-empty");
        click(&mut w, "confirm-delete-group:g-empty");
        assert_eq!(fake.calls(), ["delete_group g-empty"]);
        *fake.group_result.lock().unwrap() = Some("No such group.".into());
        click(&mut w, "delete-group:g-empty");
        click(&mut w, "confirm-delete-group:g-empty");
        assert!(shows_text(&w, "No such group."));
    }

    #[test]
    fn a_loose_automation_dragged_onto_a_group_of_the_users_moves_into_it() {
        let (mut w, fake) = with_custom();
        open_collapsed(&mut w);
        drag(&mut w, "row:mine", "group-row:g-empty");
        assert_eq!(fake.calls(), ["set_group mine g-empty"]);
    }

    #[test]
    fn shipped_groups_come_first_then_the_users_in_creation_order() {
        let (mut w, _fake) = with_custom();
        open_collapsed(&mut w);
        let y = |key: &str| w.app.hits.automations.rects[key].min.y;
        assert!(y("group-row:accept-defaults-claude") < y("group-row:g-empty"));
        assert!(y("group-row:accept-defaults-codex") < y("group-row:g-empty"));
        assert!(y("group-row:g-empty") < y("group-row:g-work"));
    }

    #[test]
    fn a_refused_move_stays_in_view_above_the_scrolling_list() {
        let (mut w, fake) = with_custom();
        *fake.group_result.lock().unwrap() = Some("Not allowed.".into());
        open_collapsed(&mut w);
        drag(&mut w, "row:mine", "group-row:g-empty");
        let tabs = w.app.hits.automations.rects["tab:automations"];
        let list = w.app.hits.automations.rects["group-row:accept-defaults-claude"];
        assert!(shows_text(&w, "Not allowed."));
        assert!(tabs.max.y < list.min.y, "the tabs are fixed above the list");
    }

    #[test]
    fn a_refused_move_is_shown_as_text() {
        let (mut w, fake) = with_custom();
        *fake.group_result.lock().unwrap() = Some("Not allowed.".into());
        open_collapsed(&mut w);
        drag(&mut w, "row:mine", "group-row:g-empty");
        assert!(shows_text(&w, "Not allowed."));
    }

    #[test]
    fn a_member_dragged_to_the_loose_list_leaves_its_group() {
        let (mut w, fake) = with_custom();
        open_collapsed(&mut w);
        let rect = w.app.hits.automations.rects["group-row:g-work"];
        w.click(rect.min + egui::vec2(10.0, rect.height() / 2.0));
        w.frame(vec![]);
        // Pick it up: the loose list then offers itself as the drop target.
        let start = center(&w, "row:member") - egui::vec2(20.0, 0.0);
        w.frame(vec![Event::PointerMoved(start)]);
        w.frame(vec![super::button(start, true)]);
        w.frame(vec![Event::PointerMoved(start + egui::vec2(0.0, 12.0))]);
        w.frame(vec![Event::PointerMoved(start + egui::vec2(0.0, 24.0))]);
        assert!(
            has(&w, "loose:Claude"),
            "the loose list is a target while dragging"
        );
        let end = center(&w, "loose:Claude");
        w.frame(vec![Event::PointerMoved(end)]);
        w.frame(vec![Event::PointerMoved(end)]);
        w.frame(vec![super::button(end, false)]);
        w.frame(vec![]);
        assert_eq!(fake.calls(), ["set_group member -"]);
    }

    #[test]
    fn a_shipped_group_takes_no_drops_and_its_members_cannot_be_dragged() {
        let (mut w, fake) = with_custom();
        open_collapsed(&mut w);
        drag(&mut w, "row:mine", "group-row:accept-defaults-claude");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
        let rect = w.app.hits.automations.rects["group-row:accept-defaults-claude"];
        w.click(rect.min + egui::vec2(10.0, rect.height() / 2.0));
        w.frame(vec![]);
        drag(&mut w, "row:accept-claude-welcome", "group-row:g-empty");
        assert!(fake.calls().is_empty(), "{:?}", fake.calls());
    }

    #[test]
    fn move_to_group_is_the_same_move_without_dragging() {
        let (mut w, fake) = with_custom();
        w.height = 2000.0;
        w.frame(vec![]);
        open(&mut w);
        assert!(
            !has(&w, "move:accept-claude-welcome"),
            "no Move to group for a shipped member"
        );
        click(&mut w, "move:mine");
        assert!(has(&w, "move-none:mine") && has(&w, "move-to:mine:g-work"));
        click(&mut w, "move-to:mine:g-work");
        assert_eq!(fake.calls(), ["set_group mine g-work"]);
        // A member is offered No group.
        let rect = w.app.hits.automations.rects["group-row:g-work"];
        w.click(rect.min + egui::vec2(10.0, rect.height() / 2.0));
        w.frame(vec![]);
        let rect = w.app.hits.automations.rects["row:member"];
        w.click(rect.min + egui::vec2(10.0, rect.height() / 2.0));
        w.frame(vec![]);
        click(&mut w, "move:member");
        click(&mut w, "move-none:member");
        assert_eq!(fake.calls()[1], "set_group member -");
    }
}

// ---------- the shared sheet frame ----------

mod sheets {
    use super::*;
    use connecto_core::{AutomationsControl, AutomationsView};

    /// An engine with nothing to show.
    struct NoControl;

    impl AutomationsControl for NoControl {
        fn view(&self) -> Arc<AutomationsView> {
            Arc::default()
        }
        fn set_api_key(&self, _key: &str) -> Result<(), String> {
            Ok(())
        }
        fn clear_api_key(&self) {}
        fn create(&self, _name: &str, _agent: ToolKind) -> Result<(), String> {
            Ok(())
        }
        fn open_session(&self, _slug: &str) -> Result<(), String> {
            Ok(())
        }
        fn set_enabled(&self, _slug: &str, _enabled: bool) {}
        fn delete(&self, _slug: &str) {}
        fn set_threshold(&self, _slug: &str, _threshold: f64) {}
        fn approve(&self, _pending: u64) {}
        fn dismiss(&self, _pending: u64) {}
        fn clear_history(&self) {}
    }

    #[derive(Clone, Copy, Debug)]
    enum Which {
        History,
        Utility,
        Automations,
    }

    const ALL: [Which; 3] = [Which::History, Which::Utility, Which::Automations];

    fn open_sheet(which: Which) -> W {
        let (_quill, _connecto, ws) = with_history();
        let fake: Arc<dyn AutomationsControl> = Arc::new(NoControl);
        let mut w = W::with_automations(&ws, Some(fake));
        match which {
            Which::History => open_history(&mut w),
            Which::Utility => open_utility(&mut w),
            Which::Automations => {
                w.app.queue_for_tests(crate::commands::Command::Automations);
                w.frame(vec![]);
                w.frame(vec![]);
            }
        }
        w
    }

    fn is_open(w: &W, which: Which) -> bool {
        match which {
            Which::History => w.app.history_open(),
            Which::Utility => w.app.utility_open(),
            Which::Automations => w.app.automations_open(),
        }
    }

    fn sheet_rect(w: &W) -> Rect {
        w.ctx
            .data(|data| data.get_temp::<Rect>(crate::sheet::sheet_rect_id()))
            .unwrap_or(Rect::NOTHING)
    }

    fn close_rect(w: &W) -> Rect {
        w.ctx
            .data(|data| data.get_temp::<Rect>(crate::sheet::close_button_id()))
            .unwrap_or(Rect::NOTHING)
    }

    #[test]
    fn every_sheet_fills_the_terminal_area_and_leaves_the_sidebar_alone() {
        for width in [1280.0, 640.0] {
            for which in ALL {
                let mut w = open_sheet(which);
                w.width = width;
                w.frame(vec![]);
                w.frame(vec![]);
                let rect = sheet_rect(&w);
                assert_eq!(rect, w.app.hits.main, "{which:?} at {width}");
                assert!(
                    rect.min.x >= w.app.hits.sidebar.max.x,
                    "{which:?} covers the sidebar"
                );
            }
        }
    }

    #[test]
    fn the_close_button_closes_every_sheet() {
        for which in ALL {
            let mut w = open_sheet(which);
            let close = close_rect(&w);
            assert!(close.is_positive(), "{which:?} has a close button");
            let main = w.app.hits.main;
            assert!(
                close.max.x <= main.max.x && close.min.y >= main.min.y,
                "{which:?}: {close:?} is in the top-right corner"
            );
            assert!(close.center().x > main.center().x);
            w.click(close.center());
            w.frame(vec![]);
            assert!(!is_open(&w, which), "{which:?} closes with the button");
        }
    }

    #[test]
    fn showing_a_terminal_from_the_sidebar_closes_every_sheet() {
        for which in ALL {
            let mut w = open_sheet(which);
            w.app.queue_for_tests(crate::commands::Command::Show {
                project: "quill".to_owned(),
                terminal: "codex-1".to_owned(),
            });
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(
                !is_open(&w, which),
                "{which:?} closes when a terminal shows"
            );
        }
    }

    #[test]
    fn esc_still_closes_every_sheet() {
        for which in ALL {
            let mut w = open_sheet(which);
            w.frame(esc());
            w.frame(vec![]);
            assert!(!is_open(&w, which), "{which:?} closes with Esc");
        }
    }

    #[test]
    fn the_history_transcript_closes_with_the_button_too() {
        let (_quill, _connecto, ws) = with_history();
        let mut w = W::new(&ws);
        open_history(&mut w);
        let view = w.app.hits.history.view_button.unwrap().center();
        w.click(view);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(
            sheet_rect(&w),
            w.app.hits.main,
            "the transcript fills the area"
        );
        let close = close_rect(&w);
        w.click(close.center());
        w.frame(vec![]);
        assert!(!w.app.history_open(), "the whole sheet closes");
    }
}

// ---------- skills, plugins and project settings ----------

mod packages_ui {
    use super::*;
    use connecto_core::{
        ConfigMode, HomeMigration, InstallState, InstalledPackage, PackageItem, PackagesControl,
        PackagesView, ProjectPackages, ProjectPackagesView, ProjectSettings, ProjectTools,
        RequirementState, RequirementView,
    };

    #[derive(Default)]
    struct FakePackages {
        view: Mutex<PackagesView>,
        calls: Mutex<Vec<String>>,
        saved: Mutex<Vec<(String, ProjectSettings)>>,
        refuse: Mutex<Option<String>>,
        /// Keep saved settings back until `publish`, as an asynchronous
        /// backend does.
        late: std::sync::atomic::AtomicBool,
        held: Mutex<Vec<(String, ProjectSettings)>>,
    }

    impl FakePackages {
        fn answer(&self, call: String) -> Result<(), String> {
            self.calls.lock().unwrap().push(call);
            self.refuse.lock().unwrap().clone().map_or(Ok(()), Err)
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn edit(&self, change: impl FnOnce(&mut PackagesView)) {
            change(&mut self.view.lock().unwrap());
        }

        /// Show the held settings in the view.
        fn publish(&self) {
            for (id, settings) in std::mem::take(&mut *self.held.lock().unwrap()) {
                self.edit(|view| {
                    if let Some(entry) = view.projects.iter_mut().find(|p| p.project_id == id) {
                        entry.settings = settings;
                    }
                });
            }
        }

        fn saved(&self) -> Vec<(String, ProjectSettings)> {
            self.saved.lock().unwrap().clone()
        }
    }

    impl PackagesControl for FakePackages {
        fn view(&self) -> Arc<PackagesView> {
            Arc::new(self.view.lock().unwrap().clone())
        }

        fn set_home(&self, path: PathBuf) -> Result<(), String> {
            self.answer(format!("set_home {}", path.display()))
        }

        fn set_default_package(&self, package: Option<&str>) -> Result<(), String> {
            self.answer(format!("set_default {package:?}"))
        }

        fn create_package(&self, name: &str, extends: Option<&str>) -> Result<(), String> {
            self.answer(format!("create {name} extends {extends:?}"))
        }

        fn edit_package(&self, id: &str) -> Result<(), String> {
            self.answer(format!("edit {id}"))
        }

        fn delete_package(&self, id: &str) -> Result<(), String> {
            self.answer(format!("delete {id}"))
        }

        fn export_package(&self, id: &str, dest: PathBuf) -> Result<(), String> {
            self.answer(format!("export {id} to {}", dest.display()))
        }

        fn import_package(&self, path: PathBuf) -> Result<(), String> {
            self.answer(format!("import {}", path.display()))
        }

        fn set_project_settings(
            &self,
            project_id: &str,
            settings: ProjectSettings,
        ) -> Result<(), String> {
            self.saved
                .lock()
                .unwrap()
                .push((project_id.to_owned(), settings.clone()));
            let result = self.answer(format!("save {project_id}"));
            if result.is_ok() && self.late.load(Ordering::Relaxed) {
                self.held
                    .lock()
                    .unwrap()
                    .push((project_id.to_owned(), settings));
            } else if result.is_ok() {
                self.edit(|view| {
                    if let Some(entry) = view
                        .projects
                        .iter_mut()
                        .find(|p| p.project_id == project_id)
                    {
                        entry.settings = settings;
                    }
                });
            }
            result
        }

        fn refresh_project(&self, project_id: &str) {
            let _ = self.answer(format!("refresh {project_id}"));
        }

        fn check_requirements(&self, package: &str) -> Result<(), String> {
            self.answer(format!("check {package}"))
        }

        fn install_requirement(
            &self,
            project_id: &str,
            package: &str,
            requirement: &str,
            command: &str,
        ) -> Result<(), String> {
            self.answer(format!(
                "install {project_id} {package} {requirement}: {command}"
            ))
        }
    }

    fn package(id: &str, name: &str, version: u32) -> PackageItem {
        PackageItem {
            id: id.into(),
            name: name.into(),
            description: format!("{name} description"),
            version,
            has_claude: true,
            ..PackageItem::default()
        }
    }

    fn fake() -> Arc<FakePackages> {
        let fake = FakePackages::default();
        fake.edit(|view| {
            view.home = Some(PathBuf::from("/home/connecto"));
            let mut builtin = package("connecto", "Connecto", 2);
            builtin.builtin = true;
            builtin.is_default = true;
            builtin.has_codex = true;
            let mut mine = package("mine", "Mine", 3);
            mine.extends = Some("connecto".into());
            let mut other = package("other", "Other", 1);
            other.errors = vec!["package.json is missing a name".into()];
            view.packages = vec![builtin, mine, other];
            view.default_package = Some("connecto".into());
            view.projects = ["quill", "connecto"]
                .iter()
                .map(|id| ProjectPackagesView {
                    project_id: (*id).into(),
                    ..ProjectPackagesView::default()
                })
                .collect();
        });
        Arc::new(fake)
    }

    fn window(fake: &Arc<FakePackages>) -> (Proj, W) {
        let (quill, _connecto, ws) = two_projects();
        let w = W::with_packages(&ws, Arc::clone(fake) as Arc<dyn PackagesControl>);
        (quill, w)
    }

    fn open(w: &mut W, command: crate::commands::Command) {
        w.app.queue_for_tests(command);
        w.frame(vec![]);
        w.frame(vec![]);
    }

    fn marks(w: &W, sheet: Sheet) -> &crate::forms::Marks {
        match sheet {
            Sheet::Skills => &w.app.hits.skills,
            Sheet::Settings => &w.app.hits.settings,
            Sheet::Project => &w.app.hits.project_settings,
        }
    }

    #[derive(Clone, Copy)]
    enum Sheet {
        Skills,
        Settings,
        Project,
    }

    fn center(w: &W, sheet: Sheet, key: &str) -> Pos2 {
        let found = marks(w, sheet).rects.get(key);
        assert!(
            found.is_some(),
            "{key} not drawn: {:?}",
            marks(w, sheet).rects.keys()
        );
        found.unwrap().center()
    }

    fn click(w: &mut W, sheet: Sheet, key: &str) {
        let at = center(w, sheet, key);
        w.click(at);
        w.frame(vec![]);
    }

    fn drawn(w: &W, sheet: Sheet, key: &str) -> bool {
        marks(w, sheet).rects.contains_key(key)
    }

    fn shows(w: &W, sheet: Sheet, text: &str) -> bool {
        marks(w, sheet).texts.iter().any(|line| line.contains(text))
    }

    fn open_skills(w: &mut W) {
        open(w, crate::commands::Command::SkillsPlugins);
        assert!(w.app.skills_open());
    }

    fn open_project_settings(w: &mut W, id: &str) {
        open(w, crate::commands::Command::ProjectSettingsAt(id.into()));
        assert_eq!(w.app.project_settings_of().as_deref(), Some(id));
    }

    // ----- the Skills & Plugins sheet -----

    #[test]
    fn the_menu_command_opens_the_sheet_and_lists_the_packages() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        for text in [
            "Connecto",
            "v2",
            "Built-in",
            "Default",
            "Mine",
            "v3",
            "extends connecto",
            "Claude",
            "Codex",
            "Mine description",
            "Error: package.json is missing a name",
        ] {
            assert!(shows(&w, Sheet::Skills, text), "{text}");
        }
        w.frame(esc());
        w.frame(vec![]);
        assert!(!w.app.skills_open());
    }

    #[test]
    fn the_menu_command_toggles_and_replaces_other_sheets() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_utility(&mut w);
        open_skills(&mut w);
        assert!(!w.app.utility_open());
        open(&mut w, crate::commands::Command::SkillsPlugins);
        assert!(!w.app.skills_open());
    }

    #[test]
    fn set_as_default_and_no_default_call_the_control() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "default:mine");
        click(&mut w, Sheet::Skills, "no-default:connecto");
        assert_eq!(
            fake.calls(),
            ["set_default Some(\"mine\")", "set_default None"]
        );
    }

    #[test]
    fn a_refused_request_shows_inline_and_keeps_the_sheet() {
        let fake = fake();
        *fake.refuse.lock().unwrap() = Some("unknown package".into());
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "default:mine");
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Skills, "unknown package"));
        assert!(w.app.skills_open());
    }

    #[test]
    fn creating_a_package_asks_a_name_and_an_extends_and_opens_its_session() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "new-package");
        click(&mut w, Sheet::Skills, "create");
        assert!(fake.calls().is_empty(), "a name is needed");
        let field = center(&w, Sheet::Skills, "field:name");
        w.click(field);
        w.type_text("Rust tools");
        w.frame(vec![]);
        click(&mut w, Sheet::Skills, "extends:connecto");
        click(&mut w, Sheet::Skills, "create");
        assert_eq!(
            fake.calls(),
            ["create Rust tools extends Some(\"connecto\")"]
        );
        assert!(!w.app.skills_open(), "the authoring terminal shows");
    }

    #[test]
    fn a_package_exports_to_the_chosen_file_and_a_file_imports() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        w.app.set_package_file_hooks(
            |name| Some(PathBuf::from(format!("/out/{name}"))),
            || Some(PathBuf::from("/in/team.connecto-package")),
        );
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "export:connecto");
        assert!(
            fake.calls().is_empty(),
            "the built-in package is not exported"
        );
        click(&mut w, Sheet::Skills, "export:mine");
        click(&mut w, Sheet::Skills, "import-package");
        assert_eq!(
            fake.calls(),
            [
                "export mine to /out/mine.connecto-package",
                "import /in/team.connecto-package"
            ]
        );
        assert!(w.app.skills_open(), "the sheet stays to show the result");
        fake.edit(|view| view.notice = Some("Imported Team as team".into()));
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Skills, "Imported Team as team"));
    }

    #[test]
    fn a_cancelled_panel_exports_and_imports_nothing() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        w.app.set_package_file_hooks(|_| None, || None);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "export:mine");
        click(&mut w, Sheet::Skills, "import-package");
        assert!(fake.calls().is_empty());
    }

    #[test]
    fn creating_without_extends_passes_none() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "new-package");
        let field = center(&w, Sheet::Skills, "field:name");
        w.click(field);
        w.type_text("Plain");
        w.frame(vec![]);
        click(&mut w, Sheet::Skills, "create");
        assert_eq!(fake.calls(), ["create Plain extends None"]);
    }

    #[test]
    fn a_refused_create_keeps_the_form_and_shows_the_reason() {
        let fake = fake();
        *fake.refuse.lock().unwrap() = Some("already exists".into());
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "new-package");
        let field = center(&w, Sheet::Skills, "field:name");
        w.click(field);
        w.type_text("Mine");
        w.frame(vec![]);
        click(&mut w, Sheet::Skills, "create");
        w.frame(vec![]);
        assert!(w.app.skills_open());
        assert!(shows(&w, Sheet::Skills, "already exists"));
    }

    #[test]
    fn delete_asks_inline_first_and_esc_cancels_it() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "delete:mine");
        assert!(fake.calls().is_empty(), "not yet");
        assert!(shows(&w, Sheet::Skills, "Delete Mine and its folder?"));
        w.frame(esc());
        w.frame(vec![]);
        assert!(w.app.skills_open(), "Esc cancels the question only");
        assert!(!shows(&w, Sheet::Skills, "Delete Mine and its folder?"));
        click(&mut w, Sheet::Skills, "delete:mine");
        click(&mut w, Sheet::Skills, "cancel-delete:mine");
        assert!(fake.calls().is_empty());
        click(&mut w, Sheet::Skills, "delete:mine");
        click(&mut w, Sheet::Skills, "confirm-delete:mine");
        assert_eq!(fake.calls(), ["delete mine"]);
    }

    #[test]
    fn the_built_in_package_cannot_be_edited_or_deleted() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "edit:connecto");
        click(&mut w, Sheet::Skills, "delete:connecto");
        assert!(fake.calls().is_empty());
        assert!(!shows(&w, Sheet::Skills, "Delete Connecto"));
        click(&mut w, Sheet::Skills, "edit:mine");
        assert_eq!(fake.calls(), ["edit mine"]);
        assert!(!w.app.skills_open(), "the editing session shows");
    }

    #[test]
    fn without_a_home_the_sheet_points_to_settings() {
        let fake = fake();
        fake.edit(|view| view.home = None);
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        assert!(shows(&w, Sheet::Skills, "Connecto Home is not set"));
        assert!(!drawn(&w, Sheet::Skills, "new-package"));
        click(&mut w, Sheet::Skills, "open-settings");
        assert!(!w.app.skills_open());
        assert!(w.app.settings_open());
        assert_eq!(
            w.app.settings_tab_for_tests(),
            crate::settings::SettingsTab::Home,
            "the folder is in Connecto Home"
        );
    }

    #[test]
    fn no_packages_and_a_last_error_are_said() {
        let fake = fake();
        fake.edit(|view| {
            view.packages.clear();
            view.last_error = Some("disk full".into());
        });
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        assert!(shows(&w, Sheet::Skills, "No packages yet"));
        assert!(shows(&w, Sheet::Skills, "disk full"));
    }

    // ----- the Settings sheet -----

    #[test]
    fn choosing_connecto_home_calls_set_home() {
        let fake = fake();
        fake.edit(|view| view.home = None);
        let (_quill, mut w) = window(&fake);
        open(&mut w, crate::commands::Command::Settings);
        assert!(w.app.settings_open());
        assert!(shows(&w, Sheet::Settings, "Folder: not set"));
        click(&mut w, Sheet::Settings, "choose-home");
        assert_eq!(fake.calls(), ["set_home /picked/folder"]);
        assert!(w.app.settings_open(), "the migration shows here");
    }

    #[test]
    fn a_cancelled_picker_and_a_refused_folder() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        w.app.set_test_hooks(|| None, fixed_clock);
        open(&mut w, crate::commands::Command::Settings);
        click(&mut w, Sheet::Settings, "choose-home");
        assert!(fake.calls().is_empty());
        w.app.set_test_hooks(picked, fixed_clock);
        *fake.refuse.lock().unwrap() = Some("not writable".into());
        click(&mut w, Sheet::Settings, "choose-home");
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Settings, "not writable"));
    }

    /// A new project has no terminal: Tools offers to start its agents, and
    /// the started one is shown. A project with terminals is not offered any.
    #[test]
    fn a_project_with_no_terminal_starts_its_agents_from_tools() {
        let fake = fake();
        let quill = project("quill", vec![spec("claude-1", Claude)]);
        let fresh = project("connecto", vec![]);
        let ws = workspace(&[&quill, &fresh]);
        let mut w = W::with_packages(&ws, Arc::clone(&fake) as Arc<dyn PackagesControl>);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        assert!(
            !drawn(&w, Sheet::Project, "start:claude"),
            "it has terminals"
        );
        open_project_settings(&mut w, "connecto");
        w.frame(vec![]);
        assert!(shows(
            &w,
            Sheet::Project,
            "This project has no terminal yet"
        ));
        click(&mut w, Sheet::Project, "start:claude");
        assert_eq!(
            fresh.terminals.adds.lock().unwrap().as_slice(),
            [(ToolKind::Claude, None)]
        );
        assert!(!w.app.project_settings_open(), "the new terminal shows");
    }

    /// A tool whose config is Isolated can have model providers of the
    /// project's own; System uses the app-wide ones.
    #[test]
    fn an_isolated_tool_offers_the_projects_own_model_providers() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        w.height = 2400.0;
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        assert!(shows(
            &w,
            Sheet::Project,
            "this project can have providers and keys of its own"
        ));
        assert!(!drawn(&w, Sheet::Project, "pi-providers-open"));
        assert!(!drawn(&w, Sheet::Project, "claude-providers-open"));
        click(&mut w, Sheet::Project, "Pi-config:Isolated (advanced)");
        click(&mut w, Sheet::Project, "Claude-config:Isolated (advanced)");
        w.frame(vec![]);
        assert!(drawn(&w, Sheet::Project, "pi-providers-open"));
        assert_eq!(
            drawn(&w, Sheet::Project, "claude-providers-open"),
            !cfg!(windows),
            "Claude's only where TogetherLink runs"
        );
        click(&mut w, Sheet::Project, "pi-providers-open");
        assert!(shows(
            &w,
            Sheet::Project,
            "PI MODEL PROVIDERS FOR THIS PROJECT"
        ));
        assert!(shows(
            &w,
            Sheet::Project,
            "Off: this project's terminals use the providers set in Settings > Agents"
        ));
        assert!(
            !drawn(&w, Sheet::Project, "pi-key-openrouter"),
            "nothing to set while off"
        );
        // Turning it on goes to the agents (none here: the refusal is said).
        click(&mut w, Sheet::Project, "providers-own");
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Project, "Agents are not available."));
        // Escape closes the dialog first, then the sheet.
        w.key(Key::Escape, Modifiers::NONE);
        assert!(!shows(&w, Sheet::Project, "PI MODEL PROVIDERS"));
        assert_eq!(w.app.project_settings_of().as_deref(), Some("quill"));
    }

    #[test]
    fn project_settings_reopens_on_the_tab_last_chosen() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        assert!(drawn(&w, Sheet::Project, "use-default"), "packages first");
        click(&mut w, Sheet::Project, "ptab-tools");
        assert!(drawn(&w, Sheet::Project, "tools:Claude"));
        w.key(Key::Escape, Modifiers::NONE);
        assert!(w.app.project_settings_of().is_none(), "closed");
        open_project_settings(&mut w, "quill");
        assert!(drawn(&w, Sheet::Project, "tools:Claude"), "Tools again");
        assert!(!drawn(&w, Sheet::Project, "use-default"));
    }

    #[test]
    fn settings_tabs_show_one_section_at_a_time() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open(&mut w, crate::commands::Command::Settings);
        for key in [
            "tab-home",
            "tab-agents",
            "tab-launch",
            "tab-secrets",
            "tab-remote",
        ] {
            assert!(drawn(&w, Sheet::Settings, key), "{key}");
        }
        assert!(drawn(&w, Sheet::Settings, "choose-home"), "Home is first");
        click(&mut w, Sheet::Settings, "tab-agents");
        assert!(!drawn(&w, Sheet::Settings, "choose-home"));
        assert!(drawn(&w, Sheet::Settings, "tab-agents"));
    }

    #[test]
    fn the_migration_status_is_shown() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open(&mut w, crate::commands::Command::Settings);
        assert!(shows(&w, Sheet::Settings, "Folder: /home/connecto"));
        for (state, text) in [
            (HomeMigration::Working, "Moving automations"),
            (HomeMigration::Done { moved: 3 }, "Moved 3 automations"),
            (HomeMigration::Error("busy".into()), "Moving failed: busy"),
        ] {
            fake.edit(|view| view.migration = state);
            w.frame(vec![]);
            assert!(shows(&w, Sheet::Settings, text), "{text}");
        }
        w.frame(esc());
        w.frame(vec![]);
        assert!(!w.app.settings_open());
    }

    // ----- the project gear and Project Settings -----

    #[test]
    fn the_gear_opens_project_settings_without_selecting_the_row() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        let row = w.rect(&Hit::Project("connecto".into()));
        w.frame(vec![Event::PointerMoved(row.center())]);
        w.frame(vec![]);
        let gear = w.rect(&Hit::Gear("connecto".into()));
        let clock = w.rect(&Hit::Clock("connecto".into()));
        assert!(gear.max.x <= clock.min.x, "left of the clock");
        w.click(gear.center());
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of().as_deref(), Some("connecto"));
        assert!(
            ws_activations(&w).is_empty(),
            "the row was not selected: {:?}",
            ws_activations(&w)
        );
    }

    fn ws_activations(w: &W) -> Vec<String> {
        w.ws.activations.lock().unwrap().clone()
    }

    #[test]
    fn the_gear_shows_on_the_active_row_and_not_on_an_idle_one() {
        let fake = fake();
        let (_quill, w) = window(&fake);
        assert!(w.has(&Hit::Gear("quill".into())));
        assert!(!w.has(&Hit::Gear("connecto".into())));
    }

    #[test]
    fn project_settings_show_the_default_package_and_the_added_ones() {
        let fake = fake();
        fake.edit(|view| {
            let project = &mut view.projects[0];
            project.settings.packages = vec!["mine".into(), "gone".into()];
            project.status = ProjectPackages {
                state: InstallState::Updated,
                packages: vec![InstalledPackage {
                    id: "mine".into(),
                    installed_version: Some(2),
                    available_version: Some(3),
                }],
                restart_needed: true,
                skipped_modified: vec![".claude/settings.json".into()],
                warnings: vec!["package gone is missing".into()],
                checked_ms: Some(fixed_clock().0 - 120_000),
            };
        });
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        for text in [
            "Connecto v2",
            "installed v2 · v3 available",
            "this package no longer exists",
        ] {
            assert!(shows(&w, Sheet::Project, text), "{text}");
        }
        click(&mut w, Sheet::Project, "ptab-status");
        for text in [
            "Updated",
            "checked 2 min ago",
            "package gone is missing",
            "Restart terminals to apply.",
            ".claude/settings.json",
        ] {
            assert!(shows(&w, Sheet::Project, text), "{text}");
        }
        click(&mut w, Sheet::Project, "ptab-packages");
        assert!(drawn(&w, Sheet::Project, "add:connecto"));
        assert!(drawn(&w, Sheet::Project, "add:other"));
        assert!(!drawn(&w, Sheet::Project, "add:mine"), "already added");
    }

    #[test]
    fn adding_removing_and_the_default_checkbox_save_the_full_settings() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "add:mine");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "use-default");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "remove:mine");
        let saved = fake.saved();
        assert_eq!(saved.len(), 3);
        assert_eq!(saved[0].0, "quill");
        assert_eq!(saved[0].1.packages, ["mine"]);
        assert!(saved[0].1.use_default_package);
        assert!(!saved[1].1.use_default_package);
        assert_eq!(saved[1].1.packages, ["mine"], "the rest is kept");
        assert!(saved[2].1.packages.is_empty());
    }

    /// Godot is named by the base package (`connecto`), the paid editor by
    /// `mine`, which extends it.
    fn needs_godot(fake: &FakePackages, state: RequirementState) {
        fake.edit(|view| {
            view.packages[0].requires = vec![RequirementView {
                id: "godot".into(),
                name: "Godot 4.7 or later".into(),
                why: "The game engine.".into(),
                state: state.clone(),
                install: Some("brew install --cask godot".into()),
                manual: "Download it from godotengine.org.".into(),
                url: Some("https://godotengine.org/download/".into()),
                licence: false,
            }];
            view.packages[1].requires = vec![RequirementView {
                id: "paid".into(),
                name: "Paid Editor".into(),
                state,
                manual: "Buy it and accept its licence yourself.".into(),
                licence: true,
                ..RequirementView::default()
            }];
        });
    }

    #[test]
    fn a_package_whose_tool_is_missing_opens_the_tools_dialog_instead_of_being_added() {
        let fake = fake();
        needs_godot(&fake, RequirementState::Missing);
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "add:mine");
        assert!(fake.saved().is_empty(), "not added while Godot is missing");
        for text in [
            "MINE NEEDS TOOLS",
            "not installed",
            "brew install --cask godot",
            "Connecto never accepts terms for you",
            "Buy it and accept its licence yourself.",
        ] {
            assert!(shows(&w, Sheet::Project, text), "{text}");
        }
        // The licence tool has no command to run, only its steps.
        assert!(!drawn(&w, Sheet::Project, "install:mine/paid"));
        // Install asks first, showing the command, and runs it only on a yes.
        click(&mut w, Sheet::Project, "install:connecto/godot");
        assert!(shows(&w, Sheet::Project, "Connecto will run this"));
        assert!(!fake.calls().iter().any(|c| c.starts_with("install")));
        // Opening the dialog looked for the tools of the package and its base.
        let calls = fake.calls();
        assert!(calls.contains(&"check mine".to_owned()), "{calls:?}");
        assert!(calls.contains(&"check connecto".to_owned()), "{calls:?}");
        click(&mut w, Sheet::Project, "install-run:connecto/godot");
        assert!(
            fake.calls()
                .contains(&"install quill connecto godot: brew install --cask godot".to_owned()),
            "the base package's tool, with the command the user saw"
        );
        // The sheet gives way to the install's terminal; adding again
        // brings the dialog back.
        assert_eq!(w.app.project_settings_of(), None);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "add:mine");
        let before = fake.calls().len();
        click(&mut w, Sheet::Project, "requirements-check");
        assert_eq!(
            fake.calls().len(),
            before + 2,
            "check again: mine and its base"
        );
        // Add stays off while a tool is missing; Cancel leaves it unadded.
        click(&mut w, Sheet::Project, "requirements-add");
        assert!(fake.saved().is_empty());
        click(&mut w, Sheet::Project, "requirements-cancel");
        assert!(!drawn(&w, Sheet::Project, "requirements-add"));
        assert!(fake.saved().is_empty(), "refused: the package stays off");
        // Once both are there, the dialog's Add saves the package.
        click(&mut w, Sheet::Project, "add:mine");
        needs_godot(
            &fake,
            RequirementState::Found {
                version: Some("4.7.2".into()),
            },
        );
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Project, "installed · 4.7.2"));
        click(&mut w, Sheet::Project, "requirements-add");
        assert_eq!(fake.saved().last().unwrap().1.packages, ["mine"]);
    }

    #[test]
    fn toggling_the_tools_hides_the_config_rows_and_saves() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        assert!(shows(&w, Sheet::Project, "Claude config"));
        assert!(shows(&w, Sheet::Project, "Codex config"));
        // A checkbox per tool: unchecking Claude keeps every other one.
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Claude");
        let no_claude = ProjectTools::All.with(ToolKind::Claude, false).unwrap();
        assert_eq!(fake.saved().last().unwrap().1.tools, no_claude);
        assert!(!shows(&w, Sheet::Project, "Claude config"));
        assert!(shows(&w, Sheet::Project, "Codex config"));
        // Down to Codex and Pi.
        for tool in ["Antigravity", "OpenCode"] {
            click(&mut w, Sheet::Project, "ptab-tools");
            click(&mut w, Sheet::Project, &format!("tools:{tool}"));
        }
        let codex_pi = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
        assert_eq!(fake.saved().last().unwrap().1.tools, codex_pi);
        assert!(shows(&w, Sheet::Project, "Codex config"));
        assert!(shows(&w, Sheet::Project, "Pi config"));
        assert!(!shows(&w, Sheet::Project, "OpenCode config"));
        // All checks every one again.
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:All");
        assert_eq!(fake.saved().last().unwrap().1.tools, ProjectTools::All);
        assert!(shows(&w, Sheet::Project, "Claude config"));
        assert!(shows(&w, Sheet::Project, "Codex config"));
        assert_eq!(fake.saved().len(), 4);
    }

    /// At least one tool stays checked: the last one cannot be unchecked,
    /// and All is off while every tool is checked.
    #[test]
    fn the_last_checked_tool_cannot_be_unchecked() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Pi);
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Pi");
        assert!(fake.saved().is_empty(), "Pi alone stays");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Codex");
        let codex_pi = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
        assert_eq!(fake.saved().last().unwrap().1.tools, codex_pi);
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:All");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:All");
        assert_eq!(fake.saved().len(), 2, "All again while all: nothing");
    }

    /// A host from before sets of tools saves one tool or all: its project
    /// is offered those, as choices, never a set it would save as all.
    #[test]
    fn a_host_from_before_sets_offers_one_tool_or_all() {
        let fake = fake();
        fake.edit(|view| view.tools_one_or_all = true);
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        assert!(shows(&w, Sheet::Project, "takes one tool or all of them"));
        click(&mut w, Sheet::Project, "tools:Codex");
        assert_eq!(fake.saved().last().unwrap().1.tools, ProjectTools::Codex);
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:All");
        assert_eq!(fake.saved().last().unwrap().1.tools, ProjectTools::All);
    }

    #[test]
    fn config_modes_save_per_tool() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        // A new project is on the system configuration; isolated is the other choice.
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "Codex-config:Isolated (advanced)");
        let saved = fake.saved();
        assert_eq!(saved[0].1.codex_config, ConfigMode::Isolated);
        assert_eq!(saved[0].1.claude_config, ConfigMode::System);
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "Claude-config:Isolated (advanced)");
        assert_eq!(fake.saved()[1].1.claude_config, ConfigMode::Isolated);
        // Choosing what is already chosen saves nothing.
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "Claude-config:Isolated (advanced)");
        assert_eq!(fake.saved().len(), 2);
    }

    #[test]
    fn a_refused_save_shows_inline_and_refresh_calls_the_control() {
        let fake = fake();
        *fake.refuse.lock().unwrap() = Some("unknown package".into());
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Codex");
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Project, "unknown package"));
        click(&mut w, Sheet::Project, "ptab-status");
        click(&mut w, Sheet::Project, "refresh");
        assert!(fake.calls().contains(&"refresh quill".to_owned()));
    }

    #[test]
    fn an_unknown_project_is_loading_and_a_working_install_disables_refresh() {
        let fake = fake();
        fake.edit(|view| view.projects.clear());
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        assert!(shows(&w, Sheet::Project, "Loading"));
        fake.edit(|view| {
            view.projects.push(ProjectPackagesView {
                project_id: "quill".into(),
                status: ProjectPackages {
                    state: InstallState::Working,
                    ..ProjectPackages::default()
                },
                ..ProjectPackagesView::default()
            });
        });
        w.frame(vec![]);
        click(&mut w, Sheet::Project, "ptab-status");
        assert!(shows(&w, Sheet::Project, "Working…"));
        click(&mut w, Sheet::Project, "ptab-status");
        click(&mut w, Sheet::Project, "refresh");
        assert!(!fake.calls().contains(&"refresh quill".to_owned()));
    }

    #[test]
    fn an_install_error_is_shown_and_esc_closes_the_sheet() {
        let fake = fake();
        fake.edit(|view| {
            view.projects[0].status.state = InstallState::Error("not writable".into());
        });
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-status");
        assert!(shows(&w, Sheet::Project, "Error: not writable"));
        w.frame(esc());
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of(), None);
    }

    // ----- a new project -----

    #[test]
    fn adding_a_project_opens_its_tools_settings() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        w.click_hit(&Hit::HeaderPlus);
        w.click_hit(&Hit::PlusOpenProject);
        w.frame(vec![]);
        assert_eq!(
            *w.ws.opened.lock().unwrap(),
            [PathBuf::from("/picked/folder")]
        );
        assert_eq!(w.app.project_settings_of(), None, "not there yet");
        let fresh = project("fresh", vec![]);
        fake.edit(|view| {
            view.projects.push(ProjectPackagesView {
                project_id: "fresh".into(),
                ..ProjectPackagesView::default()
            });
        });
        w.ws.edit(|view| view.projects.push(fresh.view()));
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of().as_deref(), Some("fresh"));
        assert!(
            drawn(&w, Sheet::Project, "tools:All"),
            "it opens on Tools, where its agents are chosen"
        );
    }

    #[test]
    fn opening_an_already_open_project_does_not_open_settings() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        w.click_hit(&Hit::HeaderPlus);
        w.click_hit(&Hit::PlusOpenProject);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of(), None);
    }

    // ----- the chooser -----

    #[test]
    fn the_chooser_offers_only_the_allowed_tools() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Codex);
        let (quill, mut w) = window(&fake);
        w.click_hit(&Hit::NewRow("quill".into()));
        w.frame(vec![]);
        // Codex and the shell.
        assert_eq!(w.app.hits.chooser.choices.len(), 2);
        // Enter takes the one offered, not Claude.
        w.frame(enter());
        assert_eq!(*quill.terminals.adds.lock().unwrap(), [(Codex, None)]);
    }

    #[test]
    fn both_tools_are_offered_by_default_and_a_click_adds_the_one_clicked() {
        let fake = fake();
        let (quill, mut w) = window(&fake);
        w.click_hit(&Hit::NewRow("quill".into()));
        w.frame(vec![]);
        assert_eq!(w.app.hits.chooser.choices.len(), 6);
        let codex = w.app.hits.chooser.choices[1].center();
        w.click(codex);
        assert_eq!(*quill.terminals.adds.lock().unwrap(), [(Codex, None)]);
    }

    #[test]
    fn the_chooser_offers_antigravity_after_pi_and_a_click_adds_it() {
        let fake = fake();
        let (quill, mut w) = window(&fake);
        w.click_hit(&Hit::NewRow("quill".into()));
        w.frame(vec![]);
        assert_eq!(w.app.hits.chooser.choices.len(), 6);
        let antigravity = w.app.hits.chooser.choices[3].center();
        w.click(antigravity);
        assert_eq!(
            *quill.terminals.adds.lock().unwrap(),
            [(connecto_core::ToolKind::Antigravity, None)]
        );
    }

    /// A host from before OpenCode would start Claude in its place: OpenCode
    /// is not offered for its project, the other agents are.
    #[test]
    fn the_chooser_leaves_out_an_agent_the_terminals_machine_cannot_start() {
        let fake = fake();
        let (quill, mut w) = window(&fake);
        quill
            .terminals
            .not_offered
            .lock()
            .unwrap()
            .push(connecto_core::ToolKind::OpenCode);
        w.click_hit(&Hit::NewRow("quill".into()));
        w.frame(vec![]);
        assert_eq!(w.app.hits.chooser.choices.len(), 5);
        // The fifth is now the shell.
        let shell = w.app.hits.chooser.choices[4].center();
        w.click(shell);
        assert_eq!(
            *quill.terminals.adds.lock().unwrap(),
            [(connecto_core::ToolKind::Shell, None)]
        );
    }

    #[test]
    fn the_chooser_offers_opencode_after_antigravity_and_a_click_adds_it() {
        let fake = fake();
        let (quill, mut w) = window(&fake);
        w.click_hit(&Hit::NewRow("quill".into()));
        w.frame(vec![]);
        assert_eq!(w.app.hits.chooser.choices.len(), 6);
        let opencode = w.app.hits.chooser.choices[4].center();
        w.click(opencode);
        assert_eq!(
            *quill.terminals.adds.lock().unwrap(),
            [(connecto_core::ToolKind::OpenCode, None)]
        );
    }

    #[test]
    fn cmd_alt_6_starts_an_opencode_terminal() {
        let m = Modifiers {
            mac_cmd: true,
            alt: true,
            ..Modifiers::NONE
        };
        assert_eq!(
            crate::commands::from_key(egui::Key::Num6, m),
            Some(crate::commands::Command::NewTool(
                connecto_core::ToolKind::OpenCode
            ))
        );
    }

    #[test]
    fn a_shortcut_for_a_tool_the_project_turned_off_is_refused() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Claude);
        let (quill, mut w) = window(&fake);
        w.app
            .queue_for_tests(crate::commands::Command::NewTool(Codex));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(quill.terminals.adds.lock().unwrap().is_empty());
        assert!(
            w.app
                .request_error_for_tests()
                .is_some_and(|text| text.contains("Codex is turned off"))
        );
        w.app
            .queue_for_tests(crate::commands::Command::NewTool(Claude));
        w.frame(vec![]);
        assert_eq!(*quill.terminals.adds.lock().unwrap(), [(Claude, None)]);
    }

    #[test]
    fn the_empty_project_chooser_offers_only_the_allowed_tools_too() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Claude);
        let (mut quill, _connecto, ws) = two_projects();
        quill.set(vec![]);
        let mut w = W::with_packages(&ws, Arc::clone(&fake) as Arc<dyn PackagesControl>);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.app.hits.inline.choices.len(), 2);
    }

    #[test]
    fn a_second_edit_before_the_view_updates_keeps_the_first() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Claude");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "add:mine");
        let saved = fake.saved();
        assert_eq!(saved.len(), 2);
        assert_eq!(
            saved[1].1.tools,
            ProjectTools::All.with(ToolKind::Claude, false).unwrap(),
            "first edit kept"
        );
        assert_eq!(saved[1].1.packages, ["mine"]);
        assert!(!shows(&w, Sheet::Project, "Claude config"), "shown at once");
        // The backend catches up; a third edit still builds on both.
        fake.publish();
        w.frame(vec![]);
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "use-default");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(
            last.tools,
            ProjectTools::All.with(ToolKind::Claude, false).unwrap()
        );
        assert_eq!(last.packages, ["mine"]);
        assert!(!last.use_default_package);
    }

    #[test]
    fn a_publish_later_than_any_timeout_still_keeps_the_first_edit() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Claude");
        // Well past the old 3 s limit; nothing in the view yet.
        std::thread::sleep(std::time::Duration::from_millis(3300));
        w.frame(vec![]);
        assert!(!shows(&w, Sheet::Project, "Claude config"), "still pending");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "add:mine");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(
            last.tools,
            ProjectTools::All.with(ToolKind::Claude, false).unwrap()
        );
        assert_eq!(last.packages, ["mine"]);
    }

    #[test]
    fn closing_and_reopening_the_sheet_keeps_pending_edits_for_the_next_one() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Codex");
        w.frame(esc());
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of(), None);
        open_project_settings(&mut w, "quill");
        assert!(!shows(&w, Sheet::Project, "Codex config"), "still pending");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "add:mine");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(
            last.tools,
            ProjectTools::All.with(ToolKind::Codex, false).unwrap()
        );
        assert_eq!(last.packages, ["mine"]);
        // Confirmed: later edits build on the view again.
        fake.publish();
        w.frame(vec![]);
        w.frame(vec![]);
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "remove:mine");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(
            last.tools,
            ProjectTools::All.with(ToolKind::Codex, false).unwrap()
        );
        assert!(last.packages.is_empty());
    }

    #[test]
    fn the_projects_own_install_error_after_the_change_drops_it() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Codex");
        assert!(!shows(&w, Sheet::Project, "Codex config"));
        fake.edit(|view| {
            view.projects[0].status.state = InstallState::Error("could not save".into());
        });
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            shows(&w, Sheet::Project, "Codex config"),
            "back to the view"
        );
    }

    #[test]
    fn an_unrelated_global_error_keeps_the_pending_settings() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Codex");
        fake.edit(|view| view.last_error = Some("package operation failed".into()));
        // Another project failing is not this change's failure either.
        fake.edit(|view| {
            view.projects[1].status.state = InstallState::Error("elsewhere".into());
        });
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!shows(&w, Sheet::Project, "Codex config"), "still pending");
        click(&mut w, Sheet::Project, "ptab-packages");
        click(&mut w, Sheet::Project, "add:mine");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(
            last.tools,
            ProjectTools::All.with(ToolKind::Codex, false).unwrap(),
            "built on the pending edit"
        );
        assert_eq!(last.packages, ["mine"]);
    }

    #[test]
    fn a_refused_edit_drops_what_was_pending() {
        let fake = fake();
        *fake.refuse.lock().unwrap() = Some("no".into());
        let (_quill, mut w) = window(&fake);
        open_project_settings(&mut w, "quill");
        click(&mut w, Sheet::Project, "ptab-tools");
        click(&mut w, Sheet::Project, "tools:Claude");
        w.frame(vec![]);
        assert!(
            shows(&w, Sheet::Project, "Codex config"),
            "back to the view"
        );
    }

    // ----- every start of a tool honours the restriction -----

    #[test]
    fn restart_is_refused_for_a_tool_turned_off_after_the_terminal_began() {
        let fake = fake();
        let (quill, mut w) = window(&fake);
        let at = crate::commands::Command::RestartAt {
            project: "quill".into(),
            terminal: "codex-1".into(),
        };
        w.app.queue_for_tests(at.clone());
        w.frame(vec![]);
        assert_eq!(*quill.terminals.restarts.lock().unwrap(), ["codex-1"]);
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Claude);
        w.app.queue_for_tests(at);
        w.frame(vec![]);
        assert_eq!(quill.terminals.restarts.lock().unwrap().len(), 1);
        assert!(
            w.app
                .request_error_for_tests()
                .is_some_and(|text| text.contains("Codex is turned off"))
        );
    }

    #[test]
    fn the_pane_offers_no_login_for_a_tool_turned_off_later() {
        let fake = fake();
        let quill = project("quill", vec![spec("claude", Claude)]);
        let ws = workspace(&[&quill]);
        *quill.slot("claude").control.stage.lock().unwrap() = PaneStage::NeedsLogin {
            auth: AuthState::LoggedOut,
        };
        let mut w = W::with_packages(&ws, Arc::clone(&fake) as Arc<dyn PackagesControl>);
        w.frame(vec![]);
        let key = ("quill".to_owned(), "claude".to_owned());
        assert_eq!(w.app.pane_hits(&key).len(), 1, "Log in while allowed");
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Codex);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.pane_hits(&key).is_empty(), "no Log in, no Confirm");
        assert_eq!(*quill.slot("claude").control.logins.lock().unwrap(), 0);
    }

    #[test]
    fn continue_from_history_is_refused_for_a_tool_turned_off() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Claude);
        let (quill, _connecto, ws) = with_history();
        let mut w = W::with_packages(&ws, Arc::clone(&fake) as Arc<dyn PackagesControl>);
        open_history(&mut w);
        let second = w.app.hits.history.rows[1].1.center();
        w.click(second);
        w.frame(vec![]);
        let button = w.app.hits.history.continue_button.unwrap().center();
        w.click(button);
        assert!(quill.terminals.continues.lock().unwrap().is_empty());
        assert!(
            w.app
                .request_error_for_tests()
                .is_some_and(|text| text.contains("Codex is turned off"))
        );
    }

    #[test]
    fn the_name_field_takes_focus_once_so_tab_can_leave_it() {
        let fake = fake();
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "new-package");
        w.frame(vec![]);
        w.type_text("a");
        let focused = |w: &W| w.ctx.memory(|m| m.focused());
        let first = focused(&w);
        assert!(first.is_some(), "the field has the keyboard");
        w.frame(vec![key_down(Key::Tab, Modifiers::NONE)]);
        w.frame(vec![]);
        assert_ne!(focused(&w), first, "Tab moved on");
    }

    #[test]
    fn errors_show_even_without_a_home() {
        let fake = fake();
        fake.edit(|view| {
            view.home = None;
            view.last_error = Some("could not read the folder".into());
        });
        let (_quill, mut w) = window(&fake);
        open_skills(&mut w);
        assert!(shows(&w, Sheet::Skills, "could not read the folder"));
    }
}

// ---------- remote hosts ----------

#[allow(clippy::panic)]
mod remote_ui {
    use connecto_core::{
        AuthorizedKeyRow, HostLink, HostRow, HostingStatus, PendingHost, RemoteControl,
        RemoteProject, RemoteView,
    };

    use super::*;
    use crate::app::{remote_host_id, remote_project_id};
    use crate::commands::Command;

    struct FakeRemote {
        view: Mutex<RemoteView>,
        /// The host's one terminal list, shown when it lists no projects.
        host: Mutex<Proj>,
        /// The projects each host lists, in order: (host, project, terminals).
        listed: Mutex<Vec<(String, RemoteProject, Proj)>>,
        calls: Mutex<Vec<String>>,
        refuse: Mutex<Option<String>>,
        /// Start the pairing without a phase, as a backend that has not yet
        /// reported one would.
        stay_idle: Mutex<bool>,
        /// Files the hosts offer, and what downloading one gives.
        offers: Mutex<Vec<connecto_core::RemoteOffer>>,
        /// Requests of the hosts' terminals for a secret.
        secret_asks: Mutex<Vec<connecto_core::RemoteSecretAsk>>,
        /// Each host project's secrets: (host, project, vault).
        vaults: Mutex<Vec<(String, String, Arc<FakeVault>)>>,
        /// The hosts' project groups; none: a host from before they were
        /// shared.
        groups: Mutex<Option<connecto_core::RemoteGroups>>,
    }

    impl FakeRemote {
        fn new() -> Arc<Self> {
            let host = project(
                "buildhost",
                vec![spec("7", Claude).named("web: claude-1"), spec("8", Codex)],
            );
            let view = RemoteView {
                hosts: vec![
                    HostRow {
                        name: "build box".into(),
                        fingerprint: "aaaa-bbbb".into(),
                        link: HostLink::Direct,
                        terminals: 2,
                    },
                    HostRow {
                        name: "old laptop".into(),
                        fingerprint: "cccc-dddd".into(),
                        link: HostLink::Disconnected,
                        terminals: 0,
                    },
                ],
                authorized_path: "/data/remote/authorized_keys".into(),
                my_fingerprint: "1111-2222-3333-4444-5555".into(),
                my_address: "connecto:abcd".into(),
                ..RemoteView::default()
            };
            let web = project(
                "p-web",
                vec![
                    spec("7", Claude).named("web: claude-1"),
                    spec("8", Codex).child_of("7"),
                ],
            );
            let api = project("p-api", vec![spec("9", Claude).named("api docs")]);
            let empty = project("p-empty", vec![]);
            let listed = vec![
                (
                    "build box".to_owned(),
                    RemoteProject {
                        id: "p-web".into(),
                        name: "web".into(),
                    },
                    web,
                ),
                (
                    "build box".to_owned(),
                    RemoteProject {
                        id: "p-api".into(),
                        name: "api".into(),
                    },
                    api,
                ),
                (
                    "build box".to_owned(),
                    RemoteProject {
                        id: "p-empty".into(),
                        name: "empty".into(),
                    },
                    empty,
                ),
            ];
            Arc::new(Self {
                view: Mutex::new(view),
                host: Mutex::new(host),
                listed: Mutex::new(listed),
                calls: Mutex::default(),
                refuse: Mutex::default(),
                stay_idle: Mutex::new(false),
                offers: Mutex::default(),
                secret_asks: Mutex::default(),
                vaults: Mutex::default(),
                groups: Mutex::default(),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn edit(&self, change: impl FnOnce(&mut RemoteView)) {
            change(&mut self.view.lock().unwrap());
        }

        /// Change one listed project's terminals, as a refresh from the host.
        fn edit_listed(&self, host: &str, project: &str, change: impl FnOnce(&mut Proj)) {
            let mut listed = self.listed.lock().unwrap();
            if let Some((_, _, proj)) = listed
                .iter_mut()
                .find(|(h, p, _)| h == host && p.id == project)
            {
                change(proj);
            }
        }

        /// Keep only the projects of `host` with these ids.
        fn keep_listed(&self, host: &str, ids: &[&str]) {
            self.listed
                .lock()
                .unwrap()
                .retain(|(h, p, _)| h != host || ids.contains(&p.id.as_str()));
        }

        /// Whether the fake host is connected: only a connected host lists.
        fn connected(&self, name: &str) -> bool {
            self.view
                .lock()
                .unwrap()
                .hosts
                .iter()
                .any(|h| h.name == name && matches!(h.link, HostLink::Direct))
        }

        fn note(&self, call: String) -> Result<(), String> {
            self.calls.lock().unwrap().push(call);
            self.refuse.lock().unwrap().clone().map_or(Ok(()), Err)
        }
    }

    impl RemoteControl for FakeRemote {
        fn project_groups(&self, name: &str) -> Option<connecto_core::RemoteGroups> {
            if !self.connected(name) {
                return None;
            }
            self.groups.lock().unwrap().clone()
        }
        fn secret_asks(&self) -> Vec<connecto_core::RemoteSecretAsk> {
            self.secret_asks.lock().unwrap().clone()
        }
        fn project_vault(
            &self,
            name: &str,
            project: &str,
            _shown_as: &str,
        ) -> Option<Arc<dyn connecto_core::VaultControl>> {
            if !self.connected(name) {
                return None;
            }
            self.vaults
                .lock()
                .unwrap()
                .iter()
                .find(|(h, p, _)| h == name && p == project)
                .map(|(_, _, v)| Arc::clone(v) as Arc<dyn connecto_core::VaultControl>)
        }
        fn file_offers(&self) -> Vec<connecto_core::RemoteOffer> {
            self.offers.lock().unwrap().clone()
        }
        fn download_offer(&self, name: &str, id: u64) -> Result<(String, Vec<u8>), String> {
            self.calls
                .lock()
                .unwrap()
                .push(format!("download {name} {id}"));
            Ok(("../report.html".into(), b"<html>".to_vec()))
        }
        fn view(&self) -> Arc<RemoteView> {
            Arc::new(self.view.lock().unwrap().clone())
        }
        fn set_hosting(&self, enabled: bool) {
            let _ = self.note(format!("set_hosting {enabled}"));
            self.view.lock().unwrap().hosting_enabled = enabled;
        }
        fn set_relay(&self, url: Option<String>, token: Option<String>) -> Result<(), String> {
            self.note(format!("set_relay {url:?} {token:?}"))
        }
        fn add_authorized_key(&self, line: &str) -> Result<(), String> {
            self.note(format!("add_key {line}"))
        }
        fn remove_authorized_key(&self, fingerprint: &str) -> Result<(), String> {
            self.note(format!("remove_key {fingerprint}"))
        }
        fn add_host(&self, name: &str, address: &str) -> Result<(), String> {
            self.note(format!("add_host {name} {address}"))?;
            self.view.lock().unwrap().pending = Some(PendingHost {
                name: name.to_owned(),
                fingerprint: "9999-8888-7777-6666-5555".into(),
                address: address.to_owned(),
            });
            Ok(())
        }
        fn start_mutual_pairing(&self) -> Result<(), String> {
            self.note("start_pairing".into())?;
            if !*self.stay_idle.lock().unwrap() {
                self.view.lock().unwrap().pairing = connecto_core::PairingPhase::ShowingCode {
                    my_code: "4828 1349".into(),
                    expires_in_secs: 118,
                };
            }
            Ok(())
        }
        fn cancel_pairing(&self) {
            let _ = self.note("cancel_pairing".into());
            self.view.lock().unwrap().pairing = connecto_core::PairingPhase::Idle;
        }
        fn submit_peer_code(&self, code: &str) -> Result<(), String> {
            self.note(format!("submit_peer_code {code}"))
        }
        fn finish_pairing(&self, name: &str) -> Result<(), String> {
            self.note(format!("finish_pairing {name}"))
        }
        fn disconnect_machine(&self, id: &str) -> Result<(), String> {
            self.note(format!("disconnect_machine {id}"))
        }
        fn confirm_host(&self) {
            let _ = self.note("confirm_host".into());
            let mut view = self.view.lock().unwrap();
            if let Some(pending) = view.pending.take() {
                view.hosts.push(HostRow {
                    name: pending.name,
                    fingerprint: pending.fingerprint,
                    link: HostLink::Connecting,
                    terminals: 0,
                });
            }
        }
        fn cancel_host(&self) {
            let _ = self.note("cancel_host".into());
            self.view.lock().unwrap().pending = None;
        }
        fn connect(&self, name: &str) {
            let _ = self.note(format!("connect {name}"));
        }
        fn disconnect(&self, name: &str) {
            let _ = self.note(format!("disconnect {name}"));
        }
        fn rename_host(&self, from: &str, to: &str) -> Result<(), String> {
            self.note(format!("rename_host {from} -> {to}"))?;
            for (host, _, _) in self.listed.lock().unwrap().iter_mut() {
                if host == from {
                    *host = to.to_owned();
                }
            }
            for host in &mut self.view.lock().unwrap().hosts {
                if host.name == from {
                    host.name = to.to_owned();
                }
            }
            Ok(())
        }
        fn forget_host(&self, name: &str) -> Result<(), String> {
            self.note(format!("forget_host {name}"))?;
            self.view.lock().unwrap().hosts.retain(|h| h.name != name);
            Ok(())
        }
        fn projects(&self, name: &str) -> Vec<RemoteProject> {
            if !self.connected(name) {
                return Vec::new();
            }
            self.listed
                .lock()
                .unwrap()
                .iter()
                .filter(|(h, _, _)| h == name)
                .map(|(_, project, _)| project.clone())
                .collect()
        }
        fn project_terminals(
            &self,
            name: &str,
            project: &str,
        ) -> Option<Arc<dyn TerminalsControl>> {
            if !self.connected(name) {
                return None;
            }
            self.listed
                .lock()
                .unwrap()
                .iter()
                .find(|(h, p, _)| h == name && p.id == project)
                .map(|(_, _, proj)| Arc::clone(&proj.terminals) as Arc<dyn TerminalsControl>)
        }
        fn terminals(&self, name: &str) -> Option<Arc<dyn TerminalsControl>> {
            self.view
                .lock()
                .unwrap()
                .hosts
                .iter()
                .any(|h| h.name == name)
                .then(|| {
                    Arc::clone(&self.host.lock().unwrap().terminals) as Arc<dyn TerminalsControl>
                })
        }
    }

    fn window() -> (Proj, Arc<FakeRemote>, W) {
        let (quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let w = W::with_remote(
            &ws,
            None,
            Arc::new(connecto_core::NoPackages),
            Arc::clone(&remote) as Arc<dyn RemoteControl>,
        );
        (quill, remote, w)
    }

    /// Open the host dropdown, unless it already is.
    fn open_hosts(w: &mut W) {
        if w.app.hits.host_rows.iter().any(|(n, _)| n == "localhost") {
            return;
        }
        let at = w
            .app
            .hits
            .host_rows
            .iter()
            .find(|(n, _)| n == "hosts")
            .map_or_else(
                || panic!("the host dropdown is not drawn"),
                |(_, r)| r.center(),
            );
        w.click(at);
        w.frame(vec![]);
    }

    fn row(w: &mut W, name: &str) -> Rect {
        open_hosts(w);
        w.app
            .hits
            .host_rows
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| *r)
            .unwrap_or_else(|| panic!("{name} not drawn: {:?}", w.app.hits.host_rows))
    }

    fn has_row(w: &mut W, name: &str) -> bool {
        open_hosts(w);
        w.app.hits.host_rows.iter().any(|(n, _)| n == name)
    }

    fn click_row(w: &mut W, name: &str) {
        let at = row(w, name).center();
        w.click(at);
        w.frame(vec![]);
    }

    fn center(w: &W, key: &str) -> Pos2 {
        w.app
            .hits
            .host_sheet
            .rects
            .get(key)
            .unwrap_or_else(|| panic!("{key} not drawn: {:?}", w.app.hits.host_sheet.rects.keys()))
            .center()
    }

    fn settings_center(w: &W, key: &str) -> Pos2 {
        w.app
            .hits
            .settings
            .rects
            .get(key)
            .unwrap_or_else(|| panic!("{key} not drawn: {:?}", w.app.hits.settings.rects.keys()))
            .center()
    }

    fn type_into(w: &mut W, at: Pos2, text: &str) {
        w.click(at);
        w.type_text(text);
        w.frame(vec![]);
    }

    #[test]
    fn the_sidebar_lists_localhost_then_each_saved_host() {
        let (_quill, _remote, mut w) = window();
        let dropdown = row(&mut w, "hosts");
        let project = w.rect(&Hit::Project("quill".into()));
        assert!(
            dropdown.max.y <= project.min.y,
            "hosts sit above the projects"
        );
        assert!(has_row(&mut w, "localhost"));
        assert!(has_row(&mut w, "build box"));
        assert!(has_row(&mut w, "old laptop"));
        assert!(has_row(&mut w, "add"));
        // localhost first, then the hosts in saved order, all above the projects.
        let names: Vec<_> = w
            .app
            .hits
            .host_rows
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(
            names,
            ["hosts", "localhost", "build box", "old laptop", "add"]
        );
    }

    /// A path a remote host's terminal printed is read through that host on
    /// a Cmd-click (nothing is looked up on this computer), and the preview
    /// offers to download it.
    #[test]
    fn a_path_in_a_remote_terminal_is_previewed_from_its_host() {
        let (_quill, remote, mut w) = window();
        click_row(&mut w, "build box");
        w.click_hit(&Hit::Terminal(rid("p-web"), "7".into()));
        w.frame(vec![]);
        let key = (rid("p-web"), "7".to_owned());
        assert_eq!(w.shown(), Some(key.clone()));
        let terminals = {
            let listed = remote.listed.lock().unwrap();
            fill(
                &listed[0].2,
                &mut w,
                (&key.0, &key.1),
                "see docs/plan.md:3 now",
            );
            Arc::clone(&listed[0].2.terminals)
        };
        let cell = w.app.cell_for_tests();
        let at = cell_origin(&w) + Vec2::new(6.5 * cell.width, cell.height * 0.5);
        let held = Modifiers {
            command: true,
            mac_cmd: cfg!(target_os = "macos"),
            ctrl: !cfg!(target_os = "macos"),
            ..Modifiers::NONE
        };
        let press = |pressed| Event::PointerButton {
            pos: at,
            button: PointerButton::Primary,
            pressed,
            modifiers: held,
        };
        w.frame(vec![Event::ModifiersChanged(held), Event::PointerMoved(at)]);
        w.frame(vec![press(true)]);
        w.frame(vec![press(false)]);
        let mut seen = None;
        for _ in 0..200 {
            w.frame(vec![]);
            seen = w.app.preview_for_tests();
            if seen.as_ref().is_some_and(|(_, downloadable)| *downloadable) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
        assert_eq!(
            *terminals.fetches.lock().unwrap(),
            [("7".to_owned(), "docs/plan.md".to_owned())]
        );
        assert_eq!(seen, Some(("/host/web/docs/plan.md".to_owned(), true)));
    }

    /// Frames until the remote terminal `7` of p-web was typed into, or the
    /// pane said why not.
    fn settle_upload(w: &mut W, remote: &FakeRemote, key: &(String, String)) {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(10);
        loop {
            w.frame(vec![]);
            let typed = !remote.listed.lock().unwrap()[0]
                .2
                .slot("7")
                .typed()
                .is_empty();
            if typed || w.app.pane_notice(key).is_some() {
                return;
            }
            assert!(std::time::Instant::now() < deadline, "never settled");
            std::thread::sleep(std::time::Duration::from_millis(5));
        }
    }

    fn open_remote_terminal(w: &mut W) -> (String, String) {
        click_row(w, "build box");
        w.click_hit(&Hit::Terminal(rid("p-web"), "7".into()));
        w.frame(vec![]);
        let key = (rid("p-web"), "7".to_owned());
        assert_eq!(w.shown(), Some(key.clone()));
        key
    }

    /// The host cannot read this computer's paths: dropped files go to it
    /// first, and the paths it saved them at are typed together, in drop
    /// order, once all arrived.
    #[test]
    fn files_dropped_on_a_remote_terminal_are_sent_to_its_host_and_its_paths_typed() {
        let (_quill, remote, mut w) = window();
        let key = open_remote_terminal(&mut w);
        let dir = tempfile::tempdir().unwrap();
        let (a, b) = (dir.path().join("b.txt"), dir.path().join("a.png"));
        std::fs::write(&a, b"first").unwrap();
        std::fs::write(&b, b"second").unwrap();
        let content = w.app.hits.content;
        w.drop_files(
            &[a.to_str().unwrap(), b.to_str().unwrap()],
            content.center(),
        );
        settle_upload(&mut w, &remote, &key);
        let listed = remote.listed.lock().unwrap();
        let typed = String::from_utf8(listed[0].2.slot("7").typed()).unwrap();
        assert_eq!(typed, "/host/uploads/b.txt /host/uploads/a.png ");
        assert_eq!(
            *listed[0].2.terminals.uploads.lock().unwrap(),
            [
                ("7".to_owned(), "b.txt".to_owned(), b"first".to_vec()),
                ("7".to_owned(), "a.png".to_owned(), b"second".to_vec())
            ]
        );
    }

    /// Files copied in Finder and pasted on a remote terminal: macOS only.
    #[cfg(target_os = "macos")]
    mod file_paste {
        use super::*;

        /// Files copied in Finder, as it leaves the pasteboard.
        fn copy_in_finder(w: &mut W, paths: &[std::path::PathBuf]) {
            let copied = crate::clipboard_files::finder_copy(paths);
            w.app.set_clipboard_files(Arc::new(move || copied.clone()));
        }

        fn plain(key: Key) -> Event {
            key_down(key, Modifiers::NONE)
        }

        /// Files copied in Finder and pasted on a remote terminal are asked
        /// about first, nothing typed or sent. Enter sends exactly the files
        /// listed, even once the clipboard holds others, to the host as dropped
        /// files are, and the host's paths are typed.
        #[test]
        fn files_copied_and_pasted_on_a_remote_terminal_are_sent_to_its_host_once_confirmed() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let dir = tempfile::tempdir().unwrap();
            let (a, b) = (dir.path().join("b.txt"), dir.path().join("a.png"));
            std::fs::write(&a, b"first").unwrap();
            std::fs::write(&b, b"second").unwrap();
            copy_in_finder(&mut w, &[a, b]);
            w.frame(vec![Event::Paste("b.txt\na.png".into())]);
            w.frame(vec![]);
            let question = w.app.pane_upload_question(&key).unwrap();
            assert!(
                question.starts_with("Upload 2 files to the host? b.txt, a.png"),
                "{question}"
            );
            {
                let listed = remote.listed.lock().unwrap();
                assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
                assert!(listed[0].2.slot("7").typed().is_empty());
            }
            let other = dir.path().join("other.txt");
            std::fs::write(&other, b"other").unwrap();
            copy_in_finder(&mut w, &[other]);
            w.frame(vec![plain(Key::Enter)]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            settle_upload(&mut w, &remote, &key);
            let listed = remote.listed.lock().unwrap();
            let typed = String::from_utf8(listed[0].2.slot("7").typed()).unwrap();
            assert_eq!(typed, "/host/uploads/b.txt /host/uploads/a.png ");
            assert_eq!(
                *listed[0].2.terminals.uploads.lock().unwrap(),
                [
                    ("7".to_owned(), "b.txt".to_owned(), b"first".to_vec()),
                    ("7".to_owned(), "a.png".to_owned(), b"second".to_vec())
                ]
            );
        }

        /// More files pasted than a host takes at once are refused before any is
        /// read, as a drop of them is, and nothing is typed.
        #[test]
        fn too_many_files_pasted_on_a_remote_terminal_are_refused_and_nothing_typed() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("x");
            std::fs::write(&file, b"x").unwrap();
            let many = vec![file; connecto_core::terminals::MAX_UPLOAD_FILES + 1];
            copy_in_finder(&mut w, &many);
            w.frame(vec![Event::Paste("x".into())]);
            w.frame(vec![]);
            let notice = w.app.pane_notice(&key).unwrap();
            assert!(notice.contains("At most"), "{notice}");
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            assert!(listed[0].2.slot("7").typed().is_empty());
        }

        /// Files pasted and asked about, with one file to send.
        fn ask_about_one_file(w: &mut W, key: &(String, String)) -> tempfile::TempDir {
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("b.txt");
            std::fs::write(&file, b"b").unwrap();
            copy_in_finder(w, &[file]);
            w.frame(vec![Event::Paste("b.txt".into())]);
            w.frame(vec![]);
            assert!(w.app.pane_upload_question(key).is_some());
            dir
        }

        /// No file is looked at before the upload: one gone by the time Enter
        /// sends it is reported by the upload, as for a drop, and nothing typed.
        #[test]
        fn a_pasted_file_gone_before_enter_is_reported_by_the_upload() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let dir = ask_about_one_file(&mut w, &key);
            std::fs::remove_file(dir.path().join("b.txt")).unwrap();
            w.frame(vec![plain(Key::Enter)]);
            settle_upload(&mut w, &remote, &key);
            let notice = w.app.pane_notice(&key).unwrap();
            assert!(notice.contains("b.txt could not be read"), "{notice}");
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            assert!(listed[0].2.slot("7").typed().is_empty());
        }

        /// Esc at the question pastes the text egui pasted, as without files.
        #[test]
        fn esc_at_the_upload_question_pastes_the_text_instead() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let _dir = ask_about_one_file(&mut w, &key);
            w.frame(vec![plain(Key::Escape)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            assert_eq!(listed[0].2.slot("7").typed(), b"b.txt");
        }

        /// Any other key at the question cancels it, the safest answer: nothing
        /// is sent, and neither the paste nor that key is typed.
        #[test]
        fn another_key_at_the_upload_question_cancels_it_and_types_nothing() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let _dir = ask_about_one_file(&mut w, &key);
            w.frame(vec![Event::Text("y".into())]);
            w.frame(vec![plain(Key::Enter)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let notice = w.app.pane_notice(&key).unwrap();
            assert!(notice.contains("cancelled"), "{notice}");
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            // Only the later Enter, the question gone, reached the terminal.
            assert_eq!(listed[0].2.slot("7").typed(), b"\r");
        }

        /// The question goes with its terminal off show (another one clicked
        /// in the sidebar; a shortcut would only cancel it): Enter in the
        /// next one sends nothing.
        #[test]
        fn the_upload_question_is_cancelled_when_its_terminal_goes_off_show() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let _dir = ask_about_one_file(&mut w, &key);
            w.click_hit(&Hit::Terminal(rid("p-web"), "8".into()));
            w.frame(vec![]);
            assert_eq!(w.shown(), Some((rid("p-web"), "8".to_owned())));
            w.frame(vec![plain(Key::Enter)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            assert!(listed[0].2.slot("7").typed().is_empty());
        }

        /// A text paste on a remote terminal stays text, even one that looks like
        /// a path, and so does one with a URL on the pasteboard that is not a
        /// file of this computer: only those are asked about.
        #[test]
        fn a_text_paste_on_a_remote_terminal_is_typed_and_sends_nothing() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            w.frame(vec![Event::Paste("/tmp/looks like a/path.txt".into())]);
            w.frame(vec![]);
            w.app.set_clipboard_files(Arc::new(|| {
                vec![crate::clipboard_files::CopiedFile {
                    url: "https://example.org/tmp/x.txt".into(),
                    path: Some("/tmp/x.txt".into()),
                }]
            }));
            w.frame(vec![Event::Paste(" x.txt".into())]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            assert_eq!(
                listed[0].2.slot("7").typed(),
                b"/tmp/looks like a/path.txt x.txt"
            );
        }

        /// A paste that arrives as the terminal is clicked, when no terminal had
        /// the keyboard, is its text: no question.
        #[test]
        fn a_paste_before_the_terminal_had_the_keyboard_is_its_text() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("b.txt");
            std::fs::write(&file, b"b").unwrap();
            copy_in_finder(&mut w, &[file]);
            w.ctx.memory_mut(|memory| memory.stop_text_input());
            w.frame(vec![]);
            let at = w.app.hits.content.center();
            w.press(at);
            w.frame(vec![button(at, false), Event::Paste("b.txt".into())]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            assert_eq!(listed[0].2.slot("7").typed(), b"b.txt");
        }

        /// Nothing sent, and nothing typed into the remote terminal.
        fn nothing_sent_or_typed(remote: &FakeRemote) {
            let listed = remote.listed.lock().unwrap();
            let uploads = listed[0].2.terminals.uploads.lock().unwrap();
            assert!(uploads.is_empty(), "{uploads:?}");
            let typed = listed[0].2.slot("7").typed();
            assert!(typed.is_empty(), "{:?}", String::from_utf8_lossy(&typed));
        }

        /// An Enter that came with the paste, before the question was drawn,
        /// never sends: it cancels.
        #[test]
        fn an_enter_in_the_pastes_own_frame_does_not_send() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("b.txt");
            std::fs::write(&file, b"b").unwrap();
            copy_in_finder(&mut w, &[file]);
            w.frame(vec![Event::Paste("b.txt".into()), plain(Key::Enter)]);
            for _ in 0..3 {
                w.frame(vec![]);
            }
            assert_eq!(w.app.pane_upload_question(&key), None);
            let notice = w.app.pane_notice(&key).unwrap_or_default();
            assert!(notice.contains("cancelled"), "{notice}");
            nothing_sent_or_typed(&remote);
        }

        /// The question's first drawing, in a terminal that never showed one,
        /// is egui's invisible sizing pass: an Enter in the very next frame
        /// came before it was ever on screen, and sends nothing.
        #[test]
        fn an_enter_before_the_question_was_ever_on_screen_does_not_send() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("b.txt");
            std::fs::write(&file, b"b").unwrap();
            copy_in_finder(&mut w, &[file]);
            w.frame(vec![Event::Paste("b.txt".into())]);
            w.frame(vec![plain(Key::Enter)]);
            for _ in 0..3 {
                w.frame(vec![]);
            }
            assert_eq!(w.app.pane_upload_question(&key), None);
            nothing_sent_or_typed(&remote);
        }

        /// An Esc that came with the paste, before the choice was ever shown,
        /// types nothing: it cancels.
        #[test]
        fn an_esc_in_the_pastes_own_frame_types_nothing() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("b.txt");
            std::fs::write(&file, b"b").unwrap();
            copy_in_finder(&mut w, &[file]);
            w.frame(vec![Event::Paste("b.txt".into()), plain(Key::Escape)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let notice = w.app.pane_notice(&key).unwrap_or_default();
            assert!(notice.contains("cancelled"), "{notice}");
            nothing_sent_or_typed(&remote);
        }

        /// A printable key cancels the question, and neither its key nor the
        /// text it types reaches the terminal; so for composed (IME) text.
        #[test]
        fn a_keystroke_or_composed_text_at_the_question_cancels_it_whole() {
            for keystroke in [
                vec![plain(Key::Y), Event::Text("y".into())],
                vec![Event::Ime(egui::ImeEvent::Commit("字".into()))],
            ] {
                let (_quill, remote, mut w) = window();
                let key = open_remote_terminal(&mut w);
                let _dir = ask_about_one_file(&mut w, &key);
                w.frame(keystroke.clone());
                w.frame(vec![]);
                assert_eq!(w.app.pane_upload_question(&key), None, "{keystroke:?}");
                nothing_sent_or_typed(&remote);
            }
        }

        /// A shortcut pressed at the question cancels it and does nothing else.
        #[test]
        fn a_shortcut_at_the_question_cancels_it_and_is_not_run() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let _dir = ask_about_one_file(&mut w, &key);
            w.frame(vec![cmd_key(Key::Backslash)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            assert!(!w.ws.view.lock().unwrap().sidebar_collapsed);
            w.frame(vec![plain(Key::Enter)]);
            w.frame(vec![]);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
        }

        /// A paste in the same frame as a shortcut, which runs first, is its
        /// text: whether it came before or after the shortcut is not known.
        #[test]
        fn a_paste_with_a_shortcut_in_its_frame_is_its_text() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let dir = tempfile::tempdir().unwrap();
            let file = dir.path().join("b.txt");
            std::fs::write(&file, b"b").unwrap();
            copy_in_finder(&mut w, &[file]);
            w.frame(vec![Event::Paste("b.txt".into()), cmd_key(Key::Backslash)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            assert_eq!(listed[0].2.slot("7").typed(), b"b.txt");
        }

        /// The window losing focus cancels the question.
        #[test]
        fn the_window_losing_focus_cancels_the_question() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let _dir = ask_about_one_file(&mut w, &key);
            w.frame(vec![Event::WindowFocused(false)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            w.frame(vec![Event::WindowFocused(true)]);
            w.frame(vec![plain(Key::Enter)]);
            w.frame(vec![]);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
        }

        /// A Finder file reference URL could be named only by the file system:
        /// such a paste is its text, not a question that cannot say what it
        /// would send.
        #[test]
        fn a_paste_of_a_file_reference_url_is_its_text() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            w.app.set_clipboard_files(Arc::new(|| {
                vec![crate::clipboard_files::CopiedFile {
                    url: "file:///.file/id=6571367.2773272/".into(),
                    path: None,
                }]
            }));
            w.frame(vec![Event::Paste("b.txt".into())]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
            assert_eq!(listed[0].2.slot("7").typed(), b"b.txt");
        }

        /// A dialog opening over the question (here a secret request that
        /// arrives) ends it: the Enter after it sends nothing.
        #[test]
        fn a_dialog_opening_over_the_question_cancels_it() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let _dir = ask_about_one_file(&mut w, &key);
            remote.secret_asks.lock().unwrap().push(remote_ask(4));
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(
                !w.app.hits.secret_ask.rects.is_empty(),
                "the secret request is not on show"
            );
            w.frame(vec![plain(Key::Enter)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
        }

        /// A dialog that arrives in the very frame an Enter is pressed ends
        /// the question too: the Enter sends nothing.
        #[test]
        fn an_enter_as_a_dialog_arrives_does_not_send() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let _dir = ask_about_one_file(&mut w, &key);
            remote.secret_asks.lock().unwrap().push(remote_ask(4));
            w.frame(vec![plain(Key::Enter)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
        }

        /// The terminal losing the keyboard (no click, no other key) ends the
        /// question: the Enter after it sends nothing.
        #[test]
        fn the_terminal_losing_the_keyboard_cancels_the_question() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let _dir = ask_about_one_file(&mut w, &key);
            w.ctx.memory_mut(|memory| memory.stop_text_input());
            w.frame(vec![plain(Key::Enter)]);
            w.frame(vec![]);
            assert_eq!(w.app.pane_upload_question(&key), None);
            let listed = remote.listed.lock().unwrap();
            assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
        }

        /// The terminal reattached while the question was open (as after a
        /// restart on its host) is not the one the paste was for: Esc types
        /// nothing into it, and Enter sends nothing.
        #[test]
        fn a_reattached_terminal_gets_nothing_from_the_question() {
            for answer in [Key::Escape, Key::Enter] {
                let (_quill, remote, mut w) = window();
                let key = open_remote_terminal(&mut w);
                let _dir = ask_about_one_file(&mut w, &key);
                let slot = Arc::clone(remote.listed.lock().unwrap()[0].2.slots.get("7").unwrap());
                slot.session.generation.fetch_add(1, Ordering::SeqCst);
                w.frame(vec![plain(answer)]);
                w.frame(vec![]);
                assert_eq!(w.app.pane_upload_question(&key), None, "{answer:?}");
                nothing_sent_or_typed(&remote);
            }
        }

        /// A click on the question reaches no program under it, even one
        /// that asked for the mouse.
        #[test]
        fn a_click_on_the_question_does_not_reach_the_program() {
            let (_quill, remote, mut w) = window();
            let key = open_remote_terminal(&mut w);
            let slot = Arc::clone(remote.listed.lock().unwrap()[0].2.slots.get("7").unwrap());
            slot.session.mouse.store(true, Ordering::SeqCst);
            w.frame(vec![]);
            w.frame(vec![]);
            let _dir = ask_about_one_file(&mut w, &key);
            let (strip, _) = w.app.pane_mut(&key).unwrap().drew_strip.clone().unwrap();
            w.click(strip.center());
            w.frame(vec![]);
            nothing_sent_or_typed(&remote);
        }

        /// In the narrowest window the question and its keys are each laid
        /// out whole inside the terminal, the keys on a line of their own.
        #[test]
        fn the_question_fits_a_narrow_window_on_two_lines() {
            let (_quill, _remote, mut w) = window();
            w.width = 640.0;
            let key = open_remote_terminal(&mut w);
            let dir = tempfile::tempdir().unwrap();
            let paths: Vec<_> = (0..connecto_core::terminals::MAX_UPLOAD_FILES)
                .map(|n| {
                    let file = dir.path().join(format!("quarterly-report-final-{n}.txt"));
                    std::fs::write(&file, b"x").unwrap();
                    file
                })
                .collect();
            copy_in_finder(&mut w, &paths);
            w.frame(vec![Event::Paste("x".into())]);
            w.frame(vec![]);
            let content = w.app.hits.content;
            assert!(
                w.app.pane_upload_question(&key).is_some(),
                "no question: {:?}",
                w.app.pane_notice(&key)
            );
            let (strip, lines) = w.app.pane_mut(&key).unwrap().drew_strip.clone().unwrap();
            assert!(content.contains_rect(strip), "{strip:?} in {content:?}");
            assert_eq!(lines.len(), 2);
            for line in &lines {
                assert!(strip.contains_rect(*line), "{line:?} in {strip:?}");
            }
            assert!(lines[0].max.y <= lines[1].min.y, "{lines:?}");
        }
    }

    /// A terminal reattached while its files were on the way (as after a
    /// restart on its host) is not the one they were dropped on: nothing is
    /// typed into it.
    #[test]
    fn files_that_arrive_after_the_remote_terminal_reattached_are_not_typed() {
        let (_quill, remote, mut w) = window();
        let key = open_remote_terminal(&mut w);
        let slot = Arc::clone(remote.listed.lock().unwrap()[0].2.slots.get("7").unwrap());
        let bump = Arc::clone(&slot);
        *remote.listed.lock().unwrap()[0]
            .2
            .terminals
            .on_upload
            .lock()
            .unwrap() = Some(Box::new(move || {
            bump.session.generation.fetch_add(1, Ordering::SeqCst);
        }));
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, b"x").unwrap();
        let content = w.app.hits.content;
        w.drop_files(&[file.to_str().unwrap()], content.center());
        settle_upload(&mut w, &remote, &key);
        let notice = w.app.pane_notice(&key).unwrap();
        assert!(notice.contains("nothing was typed"), "{notice}");
        assert!(slot.typed().is_empty());
    }

    #[test]
    fn a_folder_dropped_on_a_remote_terminal_is_refused_and_nothing_typed() {
        let (_quill, remote, mut w) = window();
        let key = open_remote_terminal(&mut w);
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, b"x").unwrap();
        let content = w.app.hits.content;
        w.drop_files(
            &[file.to_str().unwrap(), dir.path().to_str().unwrap()],
            content.center(),
        );
        settle_upload(&mut w, &remote, &key);
        let notice = w.app.pane_notice(&key).unwrap();
        assert!(notice.contains("folder"), "{notice}");
        let listed = remote.listed.lock().unwrap();
        assert!(listed[0].2.slot("7").typed().is_empty());
        assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn a_host_refusing_a_file_says_why_and_types_nothing() {
        let (_quill, remote, mut w) = window();
        let key = open_remote_terminal(&mut w);
        *remote.listed.lock().unwrap()[0]
            .2
            .terminals
            .refuse
            .lock()
            .unwrap() = Some("Update Connecto on the host to send files to its terminals.".into());
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("notes.txt");
        std::fs::write(&file, b"x").unwrap();
        let content = w.app.hits.content;
        w.drop_files(&[file.to_str().unwrap()], content.center());
        settle_upload(&mut w, &remote, &key);
        let notice = w.app.pane_notice(&key).unwrap();
        assert!(notice.contains("Update Connecto on the host"), "{notice}");
        assert!(
            remote.listed.lock().unwrap()[0]
                .2
                .slot("7")
                .typed()
                .is_empty()
        );
    }

    #[test]
    fn too_many_files_dropped_on_a_remote_terminal_are_refused_up_front() {
        let (_quill, remote, mut w) = window();
        let key = open_remote_terminal(&mut w);
        let content = w.app.hits.content;
        let many = vec!["/tmp/x"; connecto_core::terminals::MAX_UPLOAD_FILES + 1];
        w.drop_files(&many, content.center());
        w.frame(vec![]);
        let notice = w.app.pane_notice(&key).unwrap();
        assert!(notice.contains("At most"), "{notice}");
        let listed = remote.listed.lock().unwrap();
        assert!(listed[0].2.terminals.uploads.lock().unwrap().is_empty());
    }

    #[test]
    fn an_image_pasted_on_a_remote_terminal_is_sent_as_a_png_and_its_host_path_typed() {
        let (_quill, remote, mut w) = window();
        let key = open_remote_terminal(&mut w);
        let image = egui::ColorImage::new([2, 2], vec![egui::Color32::BLUE; 4]);
        w.frame(vec![Event::PasteImage(Arc::new(image))]);
        settle_upload(&mut w, &remote, &key);
        let listed = remote.listed.lock().unwrap();
        let uploads = listed[0].2.terminals.uploads.lock().unwrap();
        assert_eq!(uploads.len(), 1);
        let (terminal, name, bytes) = &uploads[0];
        assert_eq!(terminal, "7");
        assert!(
            name.starts_with("pasted-") && name.ends_with(".png"),
            "{name}"
        );
        assert!(bytes.starts_with(b"\x89PNG"));
        let typed = String::from_utf8(listed[0].2.slot("7").typed()).unwrap();
        assert_eq!(typed, format!("/host/uploads/{name} "));
    }

    #[test]
    fn selecting_a_host_connects_it_and_swaps_the_terminal_list() {
        let (_quill, remote, mut w) = window();
        assert!(w.has(&Hit::Project("quill".into())));
        click_row(&mut w, "build box");
        assert!(remote.calls().contains(&"connect build box".to_owned()));
        // The tree now lists that host's terminals, and none of the projects.
        assert!(!w.has(&Hit::Project("quill".into())));
        assert!(w.has(&Hit::Project(rid("p-web"))));
        assert!(w.has(&Hit::Project(rid("p-api"))));
        assert!(w.has(&Hit::Terminal(rid("p-web"), "7".into())));
        assert!(w.has(&Hit::Terminal(rid("p-web"), "8".into())));
        // Back to this machine.
        click_row(&mut w, "localhost");
        assert!(w.has(&Hit::Project("quill".into())));
        assert!(!w.has(&Hit::Project(rid("p-web"))));
    }

    /// A window over the two local projects and a remote, and the fake
    /// workspace, so a test can see whether a folder was opened.
    fn remote_window() -> (Arc<FakeWorkspace>, Arc<FakeRemote>, W) {
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let w = W::with_remote(
            &ws,
            None,
            Arc::new(connecto_core::NoPackages),
            Arc::clone(&remote) as Arc<dyn RemoteControl>,
        );
        (ws, remote, w)
    }

    #[test]
    fn creating_a_project_on_a_remote_machine_says_it_is_not_built_yet() {
        let (ws, _remote, mut w) = remote_window();
        click_row(&mut w, "build box");
        // The header plus, and the command the menu and the shortcuts send.
        w.click_hit(&Hit::HeaderPlus);
        w.click_hit(&Hit::PlusOpenProject);
        w.frame(vec![]);
        assert!(w.app.notice_open(), "the notice is shown");
        let texts = w.app.hits.notice.texts.clone();
        assert!(
            texts
                .iter()
                .any(|t| t == "Project creation on remote machines is still in development"),
            "{texts:?}"
        );
        assert!(ws.opened.lock().unwrap().is_empty(), "no folder picker");
        // Escape and OK both dismiss it, and neither opens a folder.
        w.frame(esc());
        w.frame(vec![]);
        assert!(!w.app.notice_open());
        w.app.queue_for_tests(Command::OpenProject);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            w.app.notice_open(),
            "the menu and shortcut path asks the same"
        );
        let ok = w.app.hits.notice.rects["notice-ok"].center();
        w.click(ok);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.notice_open());
        assert!(ws.opened.lock().unwrap().is_empty(), "still no folder");
    }

    #[test]
    fn creating_a_project_on_this_machine_still_opens_the_folder_picker() {
        let (ws, _remote, mut w) = remote_window();
        click_row(&mut w, "build box");
        click_row(&mut w, "localhost");
        w.click_hit(&Hit::HeaderPlus);
        w.click_hit(&Hit::PlusOpenProject);
        w.frame(vec![]);
        assert!(!w.app.notice_open(), "no notice on this machine");
        assert_eq!(
            *ws.opened.lock().unwrap(),
            [PathBuf::from("/picked/folder")]
        );
    }

    #[test]
    fn a_disconnected_host_says_so_instead_of_listing_nothing() {
        let (_quill, remote, mut w) = window();
        remote.host.lock().unwrap().set(vec![]);
        click_row(&mut w, "old laptop");
        assert!(w.has(&Hit::Project(remote_host_id("old laptop"))));
        assert!(!w.has(&Hit::Terminal(remote_host_id("old laptop"), "7".into())));
    }

    #[test]
    fn rename_and_forget_are_in_the_row_menu_commands() {
        let (_quill, remote, mut w) = window();
        click_row(&mut w, "build box");
        w.app
            .queue_for_tests(Command::RenameHostAt("build box".into()));
        w.frame(vec![]);
        w.frame(vec![]);
        let field = center(&w, "host-rename");
        w.click(field);
        w.frame(vec![Event::Key {
            key: Key::A,
            physical_key: None,
            pressed: true,
            repeat: false,
            modifiers: Modifiers::COMMAND,
        }]);
        w.type_text("ci box");
        w.frame(vec![]);
        let save = center(&w, "host-save");
        w.click(save);
        w.frame(vec![]);
        assert_eq!(
            remote.calls().last().unwrap(),
            "rename_host build box -> ci box"
        );
        // The selection follows the new name.
        assert!(!w.app.modal_open_for_tests());
        assert!(has_row(&mut w, "ci box"));
        assert!(w.has(&Hit::Project(remote_project_id("ci box", "p-web"))));

        w.app
            .queue_for_tests(Command::AskForgetHost("ci box".into()));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.confirm_open(), "forgetting asks first");
        let calls = remote.calls().len();
        let remove = w.app.hits.confirm.rects["confirm-remove"].center();
        w.click(remove);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(remote.calls().last().unwrap(), "forget_host ci box");
        assert_eq!(remote.calls().len(), calls + 1);
        assert!(!w.app.confirm_open());
        // The host that was shown is forgotten: back to this machine.
        assert!(w.has(&Hit::Project("quill".into())));
    }

    /// The id of one of the build box's projects, as the window draws it.
    fn rid(project: &str) -> String {
        remote_project_id("build box", project)
    }

    /// The project rows drawn in the sidebar, top to bottom.
    fn project_rows(w: &W) -> Vec<String> {
        let mut rows: Vec<(f32, String)> = w
            .app
            .hits
            .tree
            .iter()
            .filter_map(|(hit, rect)| match hit {
                Hit::Project(id) => Some((rect.min.y, id.clone())),
                _ => None,
            })
            .collect();
        rows.sort_by(|a, b| a.0.total_cmp(&b.0));
        rows.into_iter().map(|(_, id)| id).collect()
    }

    fn activate_project(w: &mut W, id: &str) {
        w.click_hit(&Hit::Project(id.to_owned()));
        w.frame(vec![]);
    }

    #[test]
    fn a_host_shows_each_of_its_projects_in_its_order_with_unique_ids() {
        let (_quill, _remote, mut w) = window();
        click_row(&mut w, "build box");
        assert_eq!(
            project_rows(&w),
            [rid("p-web"), rid("p-api"), rid("p-empty")]
        );
        assert!(!w.has(&Hit::Project("quill".into())));
        // An id never equals a local project's, another host's, or the host's
        // own fallback id, and a name containing a colon cannot make two.
        assert_ne!(remote_host_id("build box"), rid("p-web"));
        assert_ne!(remote_project_id("ci box", "p-web"), rid("p-web"));
        assert_ne!(remote_project_id("a:b", "c"), remote_project_id("a", "b:c"));
    }

    #[test]
    fn a_child_is_drawn_under_its_parent_in_a_remote_project() {
        let (_quill, remote, mut w) = window();
        // The child is listed before its parent, and still sits under it.
        remote.edit_listed("build box", "p-web", |proj| {
            proj.set(vec![
                spec("8", Codex).child_of("7"),
                spec("7", Claude).named("web: claude-1"),
            ]);
        });
        click_row(&mut w, "build box");
        assert_eq!(w.terminal_rows(&rid("p-web")), ["7", "8"]);
        let parent = w.rect(&Hit::Terminal(rid("p-web"), "7".into())).min.x;
        let child = w.rect(&Hit::Terminal(rid("p-web"), "8".into())).min.x;
        assert!(child > parent, "a child is indented under its parent");
    }

    #[test]
    fn a_host_that_lists_no_projects_shows_its_one_terminal_list() {
        let (_quill, remote, mut w) = window();
        remote.listed.lock().unwrap().clear();
        click_row(&mut w, "build box");
        assert_eq!(project_rows(&w), [remote_host_id("build box")]);
        assert!(w.has(&Hit::Terminal(remote_host_id("build box"), "7".into())));
        assert!(w.has(&Hit::Terminal(remote_host_id("build box"), "8".into())));
    }

    #[test]
    fn an_empty_remote_project_is_shown_with_its_new_terminal_row() {
        let (_quill, remote, mut w) = window();
        click_row(&mut w, "build box");
        activate_project(&mut w, &rid("p-empty"));
        assert!(w.has(&Hit::Project(rid("p-empty"))));
        assert!(w.terminal_rows(&rid("p-empty")).is_empty());
        // No connection note hides the list: the New terminal row is there.
        assert!(w.has(&Hit::NewRow(rid("p-empty"))));
        assert!(w.shown().is_none(), "nothing is selected");
        // Its row adds a terminal to that project.
        w.app.queue_for_tests(Command::NewTool(Claude));
        w.frame(vec![]);
        w.frame(vec![]);
        let listed = remote.listed.lock().unwrap();
        let Some((_, _, empty)) = listed.iter().find(|(_, p, _)| p.id == "p-empty") else {
            panic!("p-empty is listed");
        };
        assert_eq!(empty.terminals.adds.lock().unwrap().len(), 1);
    }

    /// While the host's Tools setting is on its way, the chooser shows its
    /// agents as loading and does not start one (by click or Enter); a shell
    /// starts. Once it is here, the agents can be chosen.
    #[test]
    fn a_remote_choosers_agents_wait_for_the_hosts_settings() {
        use std::sync::atomic::Ordering;
        let (_quill, remote, mut w) = window();
        click_row(&mut w, "build box");
        activate_project(&mut w, &rid("p-empty"));
        let terminals = {
            let listed = remote.listed.lock().unwrap();
            let Some((_, _, empty)) = listed.iter().find(|(_, p, _)| p.id == "p-empty") else {
                panic!("p-empty is listed");
            };
            Arc::clone(&empty.terminals)
        };
        terminals.agents_loading.store(true, Ordering::SeqCst);
        // An empty project shows the chooser in place.
        w.frame(vec![]);
        assert_eq!(w.app.hits.inline.choices.len(), 6);
        let claude = w.app.hits.inline.choices[0].center();
        w.click(claude);
        w.frame(enter());
        assert!(terminals.adds.lock().unwrap().is_empty(), "no agent yet");
        let shell = w.app.hits.inline.choices[5].center();
        w.click(shell);
        assert_eq!(*terminals.adds.lock().unwrap(), [(ToolKind::Shell, None)]);
        terminals.agents_loading.store(false, Ordering::SeqCst);
        w.frame(vec![]);
        let claude = w.app.hits.inline.choices[0].center();
        w.click(claude);
        assert_eq!(terminals.adds.lock().unwrap().last(), Some(&(Claude, None)));
    }

    #[test]
    fn each_host_keeps_its_own_project_when_you_switch_away_and_back() {
        let (_quill, remote, mut w) = window();
        remote.edit(|v| {
            v.hosts.push(HostRow {
                name: "ci box".into(),
                fingerprint: "eeee-ffff".into(),
                link: HostLink::Direct,
                terminals: 1,
            });
        });
        remote.listed.lock().unwrap().push((
            "ci box".to_owned(),
            RemoteProject {
                id: "p-ci".into(),
                name: "ci".into(),
            },
            project("p-ci", vec![spec("5", Claude)]),
        ));
        click_row(&mut w, "build box");
        activate_project(&mut w, &rid("p-api"));
        // Host B: its own project is chosen, which must not move A's choice.
        click_row(&mut w, "ci box");
        let ci = remote_project_id("ci box", "p-ci");
        assert_eq!(w.shown(), Some((ci.clone(), "5".into())));
        click_row(&mut w, "build box");
        assert_eq!(w.shown(), Some((rid("p-api"), "9".into())));
        click_row(&mut w, "ci box");
        assert_eq!(w.shown(), Some((ci, "5".into())));
    }

    /// A remote project's settings as its host keeps them: one project, under
    /// the id the window knows it by, and a record of what was saved.
    #[derive(Default)]
    struct HostSettings {
        view: Mutex<connecto_core::PackagesView>,
        saved: Mutex<Vec<(String, connecto_core::ProjectSettings)>>,
        /// The host has said no to the last change.
        refused: std::sync::atomic::AtomicBool,
    }

    impl connecto_core::PackagesControl for HostSettings {
        fn view(&self) -> Arc<connecto_core::PackagesView> {
            Arc::new(self.view.lock().unwrap().clone())
        }
        fn set_project_settings(
            &self,
            project_id: &str,
            settings: connecto_core::ProjectSettings,
        ) -> Result<(), String> {
            self.saved
                .lock()
                .unwrap()
                .push((project_id.to_owned(), settings));
            Ok(())
        }
        fn latest_change_refused(&self, _project_id: &str) -> bool {
            self.refused.load(std::sync::atomic::Ordering::SeqCst)
        }
    }

    fn remote_proj(remote: &FakeRemote, project: &str) -> Arc<FakeTerminals> {
        remote
            .listed
            .lock()
            .unwrap()
            .iter()
            .find(|(h, p, _)| h == "build box" && p.id == project)
            .map(|(_, _, proj)| Arc::clone(&proj.terminals))
            .unwrap()
    }

    #[test]
    fn a_remote_project_offers_its_hosts_history_and_settings_but_no_names_or_closing() {
        let (ws, remote, mut w) = remote_window();
        let web = remote_proj(&remote, "p-web");
        let host_settings = Arc::new(HostSettings::default());
        host_settings.view.lock().unwrap().projects = vec![connecto_core::ProjectPackagesView {
            project_id: rid("p-web"),
            ..Default::default()
        }];
        *web.remote_settings.lock().unwrap() = Some((
            Arc::clone(&host_settings) as Arc<dyn connecto_core::PackagesControl>,
            Arc::new(connecto_core::NoTokens) as Arc<dyn connecto_core::TokenControl>,
        ));
        web.edit(|view| {
            view.history = Arc::new(vec![HistoryEntry {
                id: "h9".into(),
                terminal_id: "4".into(),
                name: Some("old build".into()),
                tool: Claude,
                parent: None,
                closed_at_ms: 0,
                task: None,
                last_reply: None,
                resumable: true,
            }]);
        });
        click_row(&mut w, "build box");
        activate_project(&mut w, &rid("p-web"));
        assert!(w.has(&Hit::Gear(rid("p-web"))));
        assert!(w.has(&Hit::Clock(rid("p-web"))));

        // History: the host's closed terminals, kept fresh while open, and a
        // Continue that goes to the host.
        w.app.queue_for_tests(Command::HistoryOf(rid("p-web")));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.history_open());
        assert!(!w.app.notice_open());
        assert!(web.watches.load(std::sync::atomic::Ordering::SeqCst) > 0);
        assert_eq!(w.app.hits.history.rows.len(), 1);
        let button = w.app.hits.history.continue_button.unwrap().center();
        w.click(button);
        assert_eq!(*web.continues.lock().unwrap(), ["h9"]);

        // Project Settings: the host's, and a change goes to the host.
        w.app
            .queue_for_tests(Command::ProjectSettingsAt(rid("p-web")));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.project_settings_open());
        let at = w.app.hits.project_settings.rects["use-default"].center();
        w.click(at);
        w.frame(vec![]);
        let saved = host_settings.saved.lock().unwrap().clone();
        assert_eq!(saved.len(), 1, "saved on the host");
        assert!(!saved[0].1.use_default_package);
        w.frame(esc());
        w.frame(vec![]);

        // Names and closing stay the host's: asked anyway, each says so.
        for command in [
            Command::RenameProjectAt(rid("p-web")),
            Command::CloseProjectAt(rid("p-web")),
        ] {
            w.app.queue_for_tests(command);
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(w.app.notice_open(), "the refusal is said");
            w.frame(esc());
            w.frame(vec![]);
            assert!(!w.app.renaming_for_tests());
        }
        assert!(ws.closed.lock().unwrap().is_empty(), "nothing closed");
        assert!(ws.renamed.lock().unwrap().is_empty(), "nothing renamed");
    }

    /// A host that takes a change before its view shows it: the next edit
    /// builds on the change, not on the host's old view; once the host
    /// refuses it, the host's settings show again.
    #[test]
    fn a_remote_change_stands_until_the_host_shows_it_or_refuses_it() {
        let (_ws, remote, mut w) = remote_window();
        let web = remote_proj(&remote, "p-web");
        let host_settings = Arc::new(HostSettings::default());
        host_settings.view.lock().unwrap().projects = vec![connecto_core::ProjectPackagesView {
            project_id: rid("p-web"),
            ..Default::default()
        }];
        *web.remote_settings.lock().unwrap() = Some((
            Arc::clone(&host_settings) as Arc<dyn connecto_core::PackagesControl>,
            Arc::new(connecto_core::NoTokens) as Arc<dyn connecto_core::TokenControl>,
        ));
        click_row(&mut w, "build box");
        w.app
            .queue_for_tests(Command::ProjectSettingsAt(rid("p-web")));
        w.frame(vec![]);
        w.frame(vec![]);
        let click = |w: &mut W| {
            let at = w.app.hits.project_settings.rects["use-default"].center();
            w.click(at);
            w.frame(vec![]);
        };
        click(&mut w);
        click(&mut w);
        let saved: Vec<bool> = host_settings
            .saved
            .lock()
            .unwrap()
            .iter()
            .map(|(_, s)| s.use_default_package)
            .collect();
        assert_eq!(saved, [false, true], "the second edit built on the first");

        host_settings
            .refused
            .store(true, std::sync::atomic::Ordering::SeqCst);
        w.frame(vec![]);
        click(&mut w);
        let last = host_settings
            .saved
            .lock()
            .unwrap()
            .last()
            .unwrap()
            .1
            .clone();
        assert!(
            !last.use_default_package,
            "built on the host's settings again"
        );
    }

    /// A remote project's operations as its host keeps them, and what was
    /// asked of them.
    #[derive(Default)]
    struct HostOps {
        submitted: Mutex<Vec<(String, connecto_core::ops::OpsAction)>>,
        /// What the host shows, when not its one operation.
        shown: Mutex<Option<connecto_core::ops::OpsView>>,
    }

    impl connecto_core::ops::OpsControl for HostOps {
        fn view(&self, project: &str) -> connecto_core::ops::OpsView {
            use connecto_core::ops::{OpKind, OpRow, OpStanding, OpStats, OpsView};
            if let Some(view) = self.shown.lock().unwrap().clone() {
                return view;
            }
            OpsView {
                project: project.to_owned(),
                available: true,
                ops: vec![OpRow {
                    slug: "tests".into(),
                    name: "Host tests".into(),
                    description: String::new(),
                    kind: Some(OpKind::Script),
                    danger: None,
                    asks_first: false,
                    standing: OpStanding::Proposed,
                    intercept: false,
                    phrases: Vec::new(),
                    args: Vec::new(),
                    body: "cargo test\n".into(),
                    spec: String::new(),
                    hash: "h-host".into(),
                    timeout_s: 600,
                    stats: OpStats::default(),
                }],
                ..OpsView::default()
            }
        }

        fn submit(&self, project: String, action: connecto_core::ops::OpsAction) {
            self.submitted.lock().unwrap().push((project, action));
        }
    }

    /// A window on the Operations tab of a remote project whose host's
    /// operations are `host_ops`.
    fn remote_ops_tab(host_ops: &Arc<HostOps>) -> W {
        let (_ws, remote, mut w) = remote_window();
        let web = remote_proj(&remote, "p-web");
        let host_settings = Arc::new(HostSettings::default());
        host_settings.view.lock().unwrap().projects = vec![connecto_core::ProjectPackagesView {
            project_id: rid("p-web"),
            ..Default::default()
        }];
        *web.remote_settings.lock().unwrap() = Some((
            host_settings as Arc<dyn connecto_core::PackagesControl>,
            Arc::new(connecto_core::NoTokens) as Arc<dyn connecto_core::TokenControl>,
        ));
        *web.remote_ops.lock().unwrap() =
            Some(Arc::clone(host_ops) as Arc<dyn connecto_core::ops::OpsControl>);
        click_row(&mut w, "build box");
        w.app
            .queue_for_tests(Command::ProjectSettingsAt(rid("p-web")));
        w.frame(vec![]);
        w.frame(vec![]);
        let tab = w.app.hits.project_settings.rects["ptab-operations"].center();
        w.click(tab);
        w.frame(vec![]);
        w
    }

    /// A host too old to share its operations: the tab says so, and offers
    /// nothing that would send it an action.
    #[test]
    fn a_remote_project_whose_host_cannot_share_operations_offers_no_controls() {
        let host_ops = Arc::new(HostOps::default());
        let why = "The Connecto on that machine is too old to share this project's operations.";
        *host_ops.shown.lock().unwrap() = Some(connecto_core::ops::OpsView {
            project: rid("p-web"),
            unavailable: Some(why.into()),
            ..connecto_core::ops::OpsView::default()
        });
        let w = remote_ops_tab(&host_ops);
        let hits = &w.app.hits.project_settings;
        assert!(hits.texts.iter().any(|t| t == why), "{:?}", hits.texts);
        let controls: Vec<&String> = hits
            .rects
            .keys()
            .filter(|k| k.starts_with("ops-"))
            .collect();
        assert!(controls.is_empty(), "{controls:?}");
        assert!(host_ops.submitted.lock().unwrap().is_empty());
    }

    /// Counts a host sent at their largest add up without overflowing.
    #[test]
    fn a_remote_projects_largest_run_counts_add_up() {
        let host_ops = Arc::new(HostOps::default());
        let mut view = connecto_core::ops::OpsControl::view(&*host_ops, &rid("p-web"));
        let mut op = view.ops[0].clone();
        op.stats.runs = u32::MAX;
        op.stats.by_agent = u32::MAX;
        view.ops = vec![
            op.clone(),
            connecto_core::ops::OpRow {
                slug: "more".into(),
                ..op
            },
        ];
        *host_ops.shown.lock().unwrap() = Some(view);
        let w = remote_ops_tab(&host_ops);
        let total = 2 * u64::from(u32::MAX);
        let texts = &w.app.hits.project_settings.texts;
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with(&format!("So far: {total} runs ({total} by agents)"))),
            "{texts:?}"
        );
    }

    /// A remote project's Operations tab is its host's: the host's list,
    /// and an approval of the hash shown goes to the host.
    #[test]
    fn a_remote_projects_operations_are_its_hosts_to_view_and_approve() {
        let (_ws, remote, mut w) = remote_window();
        let web = remote_proj(&remote, "p-web");
        let host_settings = Arc::new(HostSettings::default());
        host_settings.view.lock().unwrap().projects = vec![connecto_core::ProjectPackagesView {
            project_id: rid("p-web"),
            ..Default::default()
        }];
        *web.remote_settings.lock().unwrap() = Some((
            host_settings as Arc<dyn connecto_core::PackagesControl>,
            Arc::new(connecto_core::NoTokens) as Arc<dyn connecto_core::TokenControl>,
        ));
        let host_ops = Arc::new(HostOps::default());
        *web.remote_ops.lock().unwrap() =
            Some(Arc::clone(&host_ops) as Arc<dyn connecto_core::ops::OpsControl>);
        click_row(&mut w, "build box");
        w.app
            .queue_for_tests(Command::ProjectSettingsAt(rid("p-web")));
        w.frame(vec![]);
        w.frame(vec![]);
        let tab = w.app.hits.project_settings.rects["ptab-operations"].center();
        w.click(tab);
        w.frame(vec![]);
        let texts = w.app.hits.project_settings.texts.clone();
        assert!(texts.iter().any(|t| t == "Host tests"), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t.contains("kept on the machine")),
            "{texts:?}"
        );
        let approve = w.app.hits.project_settings.rects["ops-approve:tests"].center();
        w.click(approve);
        w.frame(vec![]);
        assert_eq!(
            *host_ops.submitted.lock().unwrap(),
            [(
                rid("p-web"),
                connecto_core::ops::OpsAction::Approve {
                    slug: "tests".into(),
                    hash: "h-host".into(),
                }
            )]
        );
    }

    /// A project whose host cannot be asked for its settings (it offers no
    /// remote settings) says so instead of showing this machine's.
    #[test]
    fn a_remote_project_without_host_settings_says_so() {
        let (_ws, _remote, mut w) = remote_window();
        click_row(&mut w, "build box");
        w.app
            .queue_for_tests(Command::ProjectSettingsAt(rid("p-web")));
        w.frame(vec![]);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.project_settings_open());
        assert!(w.app.notice_open());
    }

    /// A file a host offers shows in a strip; Download saves it under a
    /// plain name in the folder downloads go to, then the strip says where,
    /// and Close takes it away.
    #[test]
    fn an_offered_file_downloads_into_the_folder_and_says_where() {
        let (_ws, remote, mut w) = remote_window();
        let dir = tempfile::tempdir().unwrap();
        w.app
            .set_download_folder_for_tests(dir.path().to_path_buf());
        *remote.offers.lock().unwrap() = vec![connecto_core::RemoteOffer {
            host: "build box".into(),
            id: 7,
            project: "web".into(),
            terminal: "claude-1".into(),
            name: "report.html".into(),
            bytes: 6,
        }];
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .offer_strip
                .texts
                .iter()
                .any(|t| t.contains("offers report.html")),
            "{:?}",
            w.app.hits.offer_strip.texts
        );
        let at = w.app.hits.offer_strip.rects["offer-download"].center();
        w.click(at);
        let saved = dir.path().join("report.html");
        for _ in 0..200 {
            w.frame(vec![]);
            if w.app.hits.offer_strip.rects.contains_key("offer-reveal") {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(10));
        }
        assert_eq!(std::fs::read(&saved).unwrap(), b"<html>");
        assert!(
            remote
                .calls
                .lock()
                .unwrap()
                .contains(&"download build box 7".to_owned())
        );
        assert!(
            w.app
                .hits
                .offer_strip
                .texts
                .iter()
                .any(|t| t.contains("Saved report.html"))
        );
        let close = w.app.hits.offer_strip.rects["offer-close"].center();
        w.click(close);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.hits.offer_strip.rects.is_empty(), "closed");
    }

    #[test]
    fn a_new_terminal_under_a_remote_project_goes_to_that_project() {
        let (_quill, remote, mut w) = window();
        click_row(&mut w, "build box");
        activate_project(&mut w, &rid("p-api"));
        w.app.queue_for_tests(Command::NewTool(Claude));
        w.frame(vec![]);
        w.frame(vec![]);
        let listed = remote.listed.lock().unwrap();
        let adds = |id: &str| {
            listed
                .iter()
                .find(|(h, p, _)| h == "build box" && p.id == id)
                .map(|(_, _, proj)| proj.terminals.adds.lock().unwrap().len())
        };
        assert_eq!(adds("p-api"), Some(1));
        assert_eq!(adds("p-web"), Some(0));
    }

    /// Terminal > New <CLI> (and its shortcut) on a remote project follows
    /// what the host's Tools setting is known to allow: nothing while it is
    /// loading, never an agent it leaves out; an allowed one and a shell go.
    #[test]
    fn a_remote_new_tool_command_follows_the_hosts_selection() {
        use std::sync::atomic::Ordering;
        let (_quill, remote, mut w) = window();
        click_row(&mut w, "build box");
        activate_project(&mut w, &rid("p-api"));
        let terminals = {
            let listed = remote.listed.lock().unwrap();
            let Some((_, _, api)) = listed.iter().find(|(_, p, _)| p.id == "p-api") else {
                panic!("p-api is listed");
            };
            Arc::clone(&api.terminals)
        };
        let new = |w: &mut W, tool| {
            w.app.queue_for_tests(Command::NewTool(tool));
            w.frame(vec![]);
            w.frame(vec![]);
        };
        terminals.agents_loading.store(true, Ordering::SeqCst);
        new(&mut w, Codex);
        assert!(terminals.adds.lock().unwrap().is_empty(), "still loading");
        assert!(
            w.app
                .request_error_for_tests()
                .is_some_and(|e| e.contains("still loading"))
        );
        terminals.agents_loading.store(false, Ordering::SeqCst);
        terminals.not_offered.lock().unwrap().push(Claude);
        new(&mut w, Claude);
        assert!(terminals.adds.lock().unwrap().is_empty(), "left out there");
        assert!(
            w.app
                .request_error_for_tests()
                .is_some_and(|e| e.contains("turned off"))
        );
        new(&mut w, Codex);
        new(&mut w, ToolKind::Shell);
        assert_eq!(
            *terminals.adds.lock().unwrap(),
            [(Codex, None), (ToolKind::Shell, None)]
        );
    }

    /// A remote project's Pi starts on its host, with the host's providers:
    /// this machine having no Pi of its own does not stop it.
    #[test]
    fn pi_starts_on_a_remote_project_when_this_machine_has_no_pi() {
        struct PiOff;
        impl connecto_core::AgentsControl for PiOff {
            fn view(&self) -> Arc<connecto_core::AgentsView> {
                Arc::default()
            }
            fn inactive(&self) -> Vec<ToolKind> {
                vec![ToolKind::Pi]
            }
        }
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                agents: Arc::new(PiOff),
                ..Diagnostics::default()
            },
        );
        click_row(&mut w, "build box");
        activate_project(&mut w, &rid("p-api"));
        w.app.queue_for_tests(Command::NewTool(ToolKind::Pi));
        w.frame(vec![]);
        w.frame(vec![]);
        let listed = remote.listed.lock().unwrap();
        let (_, _, api) = listed
            .iter()
            .find(|(h, p, _)| h == "build box" && p.id == "p-api")
            .unwrap();
        assert_eq!(
            api.terminals
                .adds
                .lock()
                .unwrap()
                .iter()
                .map(|(tool, _)| *tool)
                .collect::<Vec<_>>(),
            [ToolKind::Pi]
        );
    }

    #[test]
    fn a_selected_remote_terminal_survives_a_refresh_and_falls_back_when_it_vanishes() {
        let (_quill, remote, mut w) = window();
        click_row(&mut w, "build box");
        w.click_hit(&Hit::Terminal(rid("p-web"), "8".into()));
        w.frame(vec![]);
        assert_eq!(w.shown(), Some((rid("p-web"), "8".into())));
        // The list refreshes with a new terminal: the choice stays.
        remote.edit_listed("build box", "p-web", |proj| {
            proj.set(vec![
                spec("7", Claude).named("web: claude-1"),
                spec("8", Codex).child_of("7"),
                spec("10", Claude),
            ]);
        });
        w.frame(vec![]);
        assert_eq!(w.shown(), Some((rid("p-web"), "8".into())));
        // The chosen terminal closes: the first in the list takes its place.
        remote.edit_listed("build box", "p-web", |proj| {
            proj.set(vec![spec("7", Claude).named("web: claude-1")]);
        });
        w.frame(vec![]);
        assert_eq!(w.shown(), Some((rid("p-web"), "7".into())));
        // Its project goes too: the one that is left is shown, with its first.
        remote.keep_listed("build box", &["p-api"]);
        w.frame(vec![]);
        assert_eq!(w.shown(), Some((rid("p-api"), "9".into())));
        assert_eq!(project_rows(&w), [rid("p-api")]);
    }

    #[test]
    fn coming_back_to_a_host_keeps_its_project_and_terminal() {
        let (_quill, _remote, mut w) = window();
        click_row(&mut w, "build box");
        w.click_hit(&Hit::Terminal(rid("p-web"), "8".into()));
        w.frame(vec![]);
        activate_project(&mut w, &rid("p-api"));
        click_row(&mut w, "localhost");
        click_row(&mut w, "build box");
        assert_eq!(w.shown(), Some((rid("p-api"), "9".into())));
        activate_project(&mut w, &rid("p-web"));
        assert_eq!(w.shown(), Some((rid("p-web"), "8".into())));
    }

    #[test]
    fn a_remote_project_folds_in_the_window_and_this_machines_projects_keep_theirs() {
        let (ws, _remote, mut w) = remote_window();
        click_row(&mut w, "build box");
        w.click_hit(&Hit::Twisty(rid("p-web")));
        w.frame(vec![]);
        assert!(w.terminal_rows(&rid("p-web")).is_empty());
        w.click_hit(&Hit::Twisty(rid("p-web")));
        w.frame(vec![]);
        assert_eq!(w.terminal_rows(&rid("p-web")), ["7", "8"]);
        assert!(
            ws.view.lock().unwrap().projects.iter().all(|p| p.expanded),
            "the workspace was not asked to fold a remote project"
        );
    }

    /// Show the Settings section whose tab is `key`.
    fn show_tab(w: &mut W, key: &str) {
        let at = settings_center(w, key);
        w.click(at);
        w.frame(vec![]);
    }

    #[test]
    fn remote_access_is_a_tab_with_the_hosting_toggle_and_no_advanced_keys() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        remote.edit(|v| {
            v.authorized = vec![AuthorizedKeyRow {
                fingerprint: "SHA256:abc".into(),
                algorithm: "ssh-ed25519".into(),
                comment: "laptop".into(),
                paired: false,
            }];
        });
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            !w.app
                .hits
                .settings
                .texts
                .iter()
                .any(|t| t.contains("REMOTE ACCESS")),
            "one section at a time"
        );
        show_tab(&mut w, "tab-remote");
        let texts = &w.app.hits.settings.texts;
        assert!(texts.iter().any(|t| t.contains("REMOTE ACCESS")));
        assert!(
            texts.iter().any(|t| t.contains("1111-2222-3333-4444-5555")),
            "{texts:?}"
        );
        // The keys file, the raw key list and the paste field are gone.
        assert!(
            !texts.iter().any(|t| t.contains("ADVANCED")
                || t.contains("authorized_keys")
                || t.contains("laptop")),
            "{texts:?}"
        );
        assert!(!w.app.hits.settings.rects.contains_key("key-line"));
        assert!(!w.app.hits.settings.rects.contains_key("add-key"));
        // It starts off, and one click turns it on.
        assert!(!remote.view().hosting_enabled);
        let toggle = settings_center(&w, "hosting-toggle");
        w.click(toggle);
        w.frame(vec![]);
        assert_eq!(remote.calls(), ["set_hosting true"]);
        remote.edit(|v| v.hosting = HostingStatus::Listening);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .settings
                .texts
                .iter()
                .any(|t| t.contains("Listening"))
        );
    }

    /// Secrets are in the file unless the person chooses the system store,
    /// which one click saves (for the next launch). Only where there is one.
    #[test]
    fn the_system_store_for_secrets_is_an_opt_in_on_the_secrets_tab() {
        let (quill, _connecto, ws) = two_projects();
        let _ = quill;
        let remote = FakeRemote::new();
        let mut w = W::with_remote(
            &ws,
            None,
            Arc::new(connecto_core::NoPackages),
            Arc::clone(&remote) as Arc<dyn RemoteControl>,
        );
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-secrets");
        let Some(label) = crate::platform::SYSTEM_STORE_CHOICE else {
            assert!(
                !w.app
                    .hits
                    .settings
                    .rects
                    .contains_key("secrets-system-store")
            );
            return;
        };
        let texts = &w.app.hits.settings.texts;
        assert!(texts.iter().any(|t| t.contains("SECRETS")), "{texts:?}");
        assert!(!label.is_empty());
        assert!(!ws.secrets_in_system_store());
        let toggle = settings_center(&w, "secrets-system-store");
        w.click(toggle);
        w.frame(vec![]);
        assert!(ws.secrets_in_system_store(), "saved for the next launch");
    }

    /// A vault that keeps what it is given, values included, so a test can
    /// check where they went.
    #[derive(Default)]
    struct FakeVault {
        puts: Mutex<Vec<(connecto_core::ProviderScope, String, String)>>,
        view: Mutex<Arc<connecto_core::VaultView>>,
        answers: Mutex<Vec<(u64, connecto_core::AskAnswer)>>,
        /// Keys made and kept where this vault is, by name.
        kept_keys: Mutex<Vec<String>>,
        /// What the host says each answer came to, in turn: the next one
        /// is taken by each answer.
        outcomes: Mutex<Vec<connecto_core::AnswerState>>,
        state: Mutex<Option<connecto_core::AnswerState>>,
        /// The changes the window took, up to this number.
        acked: Mutex<Option<u64>>,
        /// Where the list was asked to be shown from, in turn.
        shown_from: Mutex<Vec<(connecto_core::ProviderScope, Option<String>)>>,
        /// Key pairs made for the window to hold.
        made_here: Mutex<usize>,
    }

    impl connecto_core::VaultControl for FakeVault {
        fn view(&self) -> Arc<connecto_core::VaultView> {
            Arc::clone(&self.view.lock().unwrap())
        }
        fn put(
            &self,
            scope: &connecto_core::ProviderScope,
            name: &str,
            description: &str,
            value: &str,
        ) -> Result<(), String> {
            self.puts
                .lock()
                .unwrap()
                .push((scope.clone(), name.to_owned(), value.to_owned()));
            let mut view = (**self.view.lock().unwrap()).clone();
            view.entries.push(connecto_core::SecretEntry {
                name: name.to_owned(),
                scope: scope.clone(),
                description: description.to_owned(),
                bindings: Vec::new(),
                updated_unix: 0,
                last_used_unix: None,
                public_key: None,
            });
            *self.view.lock().unwrap() = Arc::new(view);
            Ok(())
        }
        fn set_bindings(
            &self,
            _: &connecto_core::ProviderScope,
            _: &str,
            _: Vec<connecto_core::Binding>,
        ) -> Result<(), String> {
            Ok(())
        }
        fn delete(&self, _: &connecto_core::ProviderScope, _: &str) -> Result<(), String> {
            Ok(())
        }
        fn answer_state(&self, _ask: u64) -> Option<connecto_core::AnswerState> {
            self.state.lock().unwrap().clone()
        }
        fn show_secrets_from(&self, scope: &connecto_core::ProviderScope, from: Option<String>) {
            self.shown_from.lock().unwrap().push((scope.clone(), from));
        }
        fn ack_remote_changes(&self, upto: u64) {
            *self.acked.lock().unwrap() = Some(upto);
        }
        fn answer(&self, ask: u64, answer: connecto_core::AskAnswer) -> Result<(), String> {
            self.answers.lock().unwrap().push((ask, answer));
            let mut outcomes = self.outcomes.lock().unwrap();
            if !outcomes.is_empty() {
                // Another machine's vault: the request stays until taken.
                *self.state.lock().unwrap() = Some(outcomes.remove(0));
                return Ok(());
            }
            let mut view = (**self.view.lock().unwrap()).clone();
            view.asks.retain(|a| a.id != ask);
            *self.view.lock().unwrap() = Arc::new(view);
            Ok(())
        }
        fn put_generated_key(
            &self,
            _scope: &connecto_core::ProviderScope,
            name: &str,
            _description: &str,
        ) -> Result<(), String> {
            self.kept_keys.lock().unwrap().push(name.to_owned());
            Ok(())
        }
        fn generate_ssh_key(&self, comment: &str) -> Result<connecto_core::GeneratedKey, String> {
            *self.made_here.lock().unwrap() += 1;
            Ok(connecto_core::GeneratedKey {
                private_openssh:
                    "-----BEGIN OPENSSH PRIVATE KEY-----\nfake\n-----END OPENSSH PRIVATE KEY-----\n"
                        .into(),
                public_openssh: format!("ssh-ed25519 AAAAfake {comment}"),
            })
        }
    }

    #[test]
    fn a_terminals_request_opens_a_dialog_that_generates_and_approves_a_key() {
        let (quill, _connecto, ws) = two_projects();
        let vault = Arc::new(FakeVault::default());
        *vault.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            asks: vec![connecto_core::SecretAsk {
                id: 7,
                terminal: "claude-1".into(),
                project: Some(quill.id.to_owned()),
                name: "DEPLOY_KEY".into(),
                kind: connecto_core::AskKind::SshKey,
                reason: "to reach the staging server".into(),
                proposed: vec![connecto_core::Binding::Ssh],
                proposed_omitted: false,
            }],
            ..connecto_core::VaultView::default()
        });
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        let texts = &w.app.hits.secret_ask.texts;
        assert!(
            texts
                .iter()
                .any(|t| t.contains("asks for an SSH key named DEPLOY_KEY")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("to reach the staging server"))
        );
        let at = |w: &W, key: &str| {
            w.app
                .hits
                .secret_ask
                .rects
                .get(key)
                .unwrap_or_else(|| panic!("{key}: {:?}", w.app.hits.secret_ask.rects.keys()))
                .center()
        };
        let generate = at(&w, "vault-ask-generate");
        w.click(generate);
        w.frame(vec![]);
        let texts = &w.app.hits.secret_ask.texts;
        assert!(
            texts.iter().any(|t| t.starts_with("Public key")),
            "the person sees the key pair: {texts:?}"
        );
        let approve = at(&w, "vault-ask-approve");
        w.click(approve);
        w.frame(vec![]);
        w.frame(vec![]);
        let answers = vault.answers.lock().unwrap();
        assert_eq!(answers.len(), 1);
        let (id, answer) = &answers[0];
        assert_eq!(*id, 7);
        let connecto_core::AskAnswer::Provide {
            scope,
            value,
            bindings,
        } = answer
        else {
            panic!("approved: {answer:?}");
        };
        assert_eq!(
            scope,
            &connecto_core::ProviderScope::Project(quill.id.to_owned())
        );
        assert!(value.starts_with("-----BEGIN OPENSSH PRIVATE KEY-----"));
        assert_eq!(bindings, &[connecto_core::Binding::Ssh]);
        assert!(
            w.app.hits.secret_ask.texts.is_empty(),
            "the dialog closes once answered"
        );
    }

    fn token_ask(id: u64, terminal: &str, project: &str, name: &str) -> connecto_core::SecretAsk {
        connecto_core::SecretAsk {
            id,
            terminal: terminal.into(),
            project: Some(project.to_owned()),
            name: name.into(),
            kind: connecto_core::AskKind::Token,
            reason: "to call the API".into(),
            proposed: Vec::new(),
            proposed_omitted: false,
        }
    }

    fn vault_asking(asks: Vec<connecto_core::SecretAsk>) -> Arc<FakeVault> {
        let vault = Arc::new(FakeVault::default());
        *vault.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            asks,
            ..connecto_core::VaultView::default()
        });
        vault
    }

    /// Everything typed into any of the project's terminals.
    fn all_typed(project: &Proj) -> usize {
        project.slots.values().map(|slot| slot.typed().len()).sum()
    }

    fn pending_at(w: &W, key: &str) -> Pos2 {
        w.app
            .hits
            .pending
            .rects
            .get(key)
            .unwrap_or_else(|| panic!("{key}: {:?}", w.app.hits.pending.rects.keys()))
            .center()
    }

    /// A secret request can be minimized to a card in the corner: the
    /// terminal under it then takes keys and drops, which it does not while
    /// the request is open; the card says who asks and for what, and opens
    /// the request again to be answered.
    #[test]
    fn a_minimized_secret_request_frees_the_terminal_and_reopens_from_its_card() {
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![token_ask(7, "claude-1", quill.id, "STRIPE_KEY")]);
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 1600.0;
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            w.app.holds_input_for_tests(),
            "the open request holds the window"
        );
        let content = w.app.hits.content;
        // Open: nothing typed or dropped reaches a terminal.
        w.type_text("x");
        w.drop_files(&["/tmp/a.png"], content.center());
        w.frame(vec![]);
        assert_eq!(all_typed(&quill), 0, "typed under the open request");

        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        assert!(w.app.hits.secret_ask.texts.is_empty(), "the dialog is gone");
        assert!(!w.app.holds_input_for_tests());
        let texts = &w.app.hits.pending.texts;
        assert!(texts.iter().any(|t| t == "SECRET REQUEST"), "{texts:?}");
        assert!(
            texts.iter().any(|t| t == "claude-1 \u{b7} quill"),
            "who asks: {texts:?}"
        );
        assert!(
            texts.iter().any(|t| t == "Asks for STRIPE_KEY"),
            "{texts:?}"
        );
        // Minimized: the terminal takes keys and drops.
        let typed = all_typed(&quill);
        w.frame(vec![]);
        w.type_text("y");
        assert_eq!(all_typed(&quill), typed + 1, "a key reached the terminal");
        w.drop_files(&["/tmp/a.png"], content.center());
        assert!(all_typed(&quill) > typed + 1, "a drop reached the terminal");

        // The card opens it again; open, it holds the keys again.
        let card = pending_at(&w, "pending-mini:secret:7");
        w.click(card);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .secret_ask
                .texts
                .iter()
                .any(|t| t.contains("named STRIPE_KEY")),
            "{:?}",
            w.app.hits.secret_ask.texts
        );
        assert!(w.app.hits.pending.texts.is_empty(), "no card while open");
        let before = all_typed(&quill);
        w.ctx.memory_mut(|memory| memory.stop_text_input());
        w.type_text("z");
        assert_eq!(
            all_typed(&quill),
            before,
            "typed under the reopened request"
        );
        // And it can be completed.
        let value = w.app.hits.secret_ask.rects["vault-ask-value"].center();
        w.click(value);
        w.type_text("sk-live-1234");
        let approve = w.app.hits.secret_ask.rects["vault-ask-approve"].center();
        w.click(approve);
        w.frame(vec![]);
        let answers = vault.answers.lock().unwrap();
        assert_eq!(answers.len(), 1);
        assert_eq!(answers[0].0, 7);
        assert!(matches!(
            &answers[0].1,
            connecto_core::AskAnswer::Provide { value, .. } if value == "sk-live-1234"
        ));
        assert!(w.app.hits.secret_ask.texts.is_empty());
        assert!(w.app.hits.pending.texts.is_empty());
        assert_eq!(
            all_typed(&quill),
            before,
            "the value never reached a terminal"
        );
    }

    /// Esc minimizes the open request too, and what was typed in it is kept
    /// for when it opens again.
    #[test]
    fn escape_minimizes_a_request_and_keeps_what_was_typed() {
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![token_ask(7, "claude-1", quill.id, "STRIPE_KEY")]);
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 1600.0;
        w.frame(vec![]);
        let value = w.app.hits.secret_ask.rects["vault-ask-value"].center();
        w.click(value);
        w.type_text("half-typed");
        w.key(Key::Escape, Modifiers::NONE);
        w.frame(vec![]);
        assert!(w.app.hits.secret_ask.texts.is_empty(), "Esc minimized it");
        let card = pending_at(&w, "pending-mini:secret:7");
        w.click(card);
        w.frame(vec![]);
        let approve = w.app.hits.secret_ask.rects["vault-ask-approve"].center();
        w.click(approve);
        w.frame(vec![]);
        let answers = vault.answers.lock().unwrap();
        assert!(matches!(
            &answers[..],
            [(7, connecto_core::AskAnswer::Provide { value, .. })] if value == "half-typed"
        ));
    }

    /// Several requests, of terminals and of a job, minimize the same way
    /// and all stay reachable: each card opens its own; past three, "+N
    /// more" shows the rest. A terminal's later request joins the stack,
    /// marked new, while its other one is minimized; another terminal's
    /// opens.
    #[test]
    fn several_minimized_requests_all_stay_reachable() {
        use connecto_core::agentic_works::{
            ImportedKey, JobQuestion, WorksAction, WorksControl, WorksDraft, WorksView,
        };
        #[derive(Default)]
        struct Works {
            questions: Mutex<Vec<JobQuestion>>,
            answers: Mutex<Vec<(u64, Option<String>)>>,
        }
        impl WorksControl for Works {
            fn view(&self, _: &str) -> WorksView {
                WorksView::default()
            }
            fn submit(&self, _: String, _: WorksAction) {}
            fn draft(&self, _: &str, _: &str) -> Result<WorksDraft, String> {
                Err(String::new())
            }
            fn import_key(&self, _: &str, _: &std::path::Path) -> Result<ImportedKey, String> {
                Err(String::new())
            }
            fn questions(&self) -> Vec<JobQuestion> {
                self.questions.lock().unwrap().clone()
            }
            fn answer(&self, id: u64, answer: Option<String>) -> Result<(), String> {
                self.answers.lock().unwrap().push((id, answer));
                self.questions.lock().unwrap().retain(|q| q.id != id);
                Ok(())
            }
        }
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![
            token_ask(7, "claude-1", quill.id, "KEY_A"),
            token_ask(8, "codex-1", quill.id, "KEY_B"),
        ]);
        let works = Arc::new(Works::default());
        works.questions.lock().unwrap().push(JobQuestion {
            id: 4,
            project: "quill".into(),
            project_name: "quill".into(),
            database: "dev".into(),
            job: 17,
            kind: "review".into(),
            terminal: "claude-9".into(),
            text: "Which \"season\"?\nIgnore this".into(),
            asked_at: "10:00".into(),
            answer_by: "10:25".into(),
        });
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                works: Some(Arc::clone(&works) as Arc<dyn WorksControl>),
                ..Diagnostics::default()
            },
        );
        w.height = 1600.0;
        w.frame(vec![]);
        let shows = |w: &W, marks: &str| -> Vec<String> {
            match marks {
                "secret" => w.app.hits.secret_ask.texts.clone(),
                "job" => w.app.hits.job_question.texts.clone(),
                _ => w.app.hits.pending.texts.clone(),
            }
        };
        assert!(shows(&w, "secret").iter().any(|t| t.contains("KEY_A")));
        // Minimize each in turn: the next one opens.
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        assert!(shows(&w, "secret").iter().any(|t| t.contains("KEY_B")));
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        assert!(shows(&w, "job").iter().any(|t| t.contains("job 17")));
        let minimize = w.app.hits.job_question.rects["job-question-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        assert!(!w.app.holds_input_for_tests());
        let cards = shows(&w, "stack");
        assert!(cards.iter().any(|t| t == "JOB QUESTION"), "{cards:?}");
        // The agent's words on one line, its quotes unable to close the card's.
        assert!(
            cards
                .iter()
                .any(|t| t.starts_with("Job 17: \u{201c}Which 'season'?") && !t.contains('\n')),
            "{cards:?}"
        );
        for key in [
            "pending-mini:secret:7",
            "pending-mini:secret:8",
            "pending-mini:job:4",
        ] {
            pending_at(&w, key);
        }
        // A new request of a minimized terminal joins the stack, marked new.
        let mut view = (**vault.view.lock().unwrap()).clone();
        view.asks.push(token_ask(9, "claude-1", quill.id, "KEY_C"));
        *vault.view.lock().unwrap() = Arc::new(view);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            shows(&w, "secret").is_empty(),
            "it did not open over the terminal"
        );
        let cards = shows(&w, "stack");
        assert!(cards.iter().any(|t| t == "NEW"), "{cards:?}");
        // Past three, the rest are behind "+1 more".
        pending_at(&w, "pending-mini:secret:9");
        assert!(!w.app.hits.pending.rects.contains_key("pending-mini:job:4"));
        let more = pending_at(&w, "pending-more");
        w.click(more);
        w.frame(vec![]);
        pending_at(&w, "pending-mini:job:4");
        // Each card opens its own request: the middle one here.
        let card = pending_at(&w, "pending-mini:secret:8");
        w.click(card);
        w.frame(vec![]);
        assert!(shows(&w, "secret").iter().any(|t| t.contains("KEY_B")));
        let decline = w.app.hits.secret_ask.rects["vault-ask-decline"].center();
        w.click(decline);
        w.frame(vec![]);
        assert_eq!(vault.answers.lock().unwrap()[0].0, 8);
        // The others stay minimized; nothing opens on its own.
        assert!(shows(&w, "secret").is_empty());
        assert!(
            !w.app
                .hits
                .pending
                .rects
                .contains_key("pending-mini:secret:8")
        );
        let card = pending_at(&w, "pending-mini:job:4");
        w.click(card);
        w.frame(vec![]);
        let none = w.app.hits.job_question.rects["job-question-none"].center();
        w.click(none);
        w.frame(vec![]);
        assert_eq!(*works.answers.lock().unwrap(), [(4, None)]);
        pending_at(&w, "pending-mini:secret:7");
        pending_at(&w, "pending-mini:secret:9");
        // Another terminal's new request opens at once.
        let mut view = (**vault.view.lock().unwrap()).clone();
        view.asks.push(token_ask(10, "codex-2", quill.id, "KEY_D"));
        *vault.view.lock().unwrap() = Arc::new(view);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(shows(&w, "secret").iter().any(|t| t.contains("KEY_D")));
    }

    /// The mouse reports a program in the terminal asked for.
    fn mouse_reports(project: &Proj, terminal: &str) -> usize {
        let typed = project.slot(terminal).typed();
        typed.windows(3).filter(|w| *w == b"\x1b[<").count()
    }

    fn with_vault(ws: &Arc<FakeWorkspace>, vault: &Arc<FakeVault>) -> W {
        W::with_diagnostics(
            ws,
            Diagnostics {
                workspace: Some(Arc::clone(ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        )
    }

    /// A click on the open request (Minimize) or on a minimized card never
    /// reaches a program under it that asked for the mouse; a click on the
    /// terminal itself does.
    #[test]
    fn clicks_on_a_request_or_its_card_never_reach_the_program_under_them() {
        let (quill, _connecto, ws) = two_projects();
        quill
            .slot("claude-1")
            .session
            .mouse
            .store(true, Ordering::Relaxed);
        let vault = vault_asking(vec![token_ask(7, "claude-1", quill.id, "STRIPE_KEY")]);
        let mut w = with_vault(&ws, &vault);
        w.height = 900.0;
        w.frame(vec![]);
        w.frame(vec![]);
        let content = w.app.hits.content;
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        assert!(content.contains(minimize), "Minimize is over the terminal");
        w.click(minimize);
        w.frame(vec![]);
        assert_eq!(mouse_reports(&quill, "claude-1"), 0, "Minimize's click");
        let card = pending_at(&w, "pending-mini:secret:7");
        assert!(content.contains(card), "the card is over the terminal");
        w.click(card);
        w.frame(vec![]);
        assert!(!w.app.hits.secret_ask.texts.is_empty(), "reopened");
        assert_eq!(mouse_reports(&quill, "claude-1"), 0, "the card's click");
        // Open, a click anywhere over the window is the dialog's.
        w.click(content.left_bottom() + Vec2::new(40.0, -40.0));
        assert_eq!(
            mouse_reports(&quill, "claude-1"),
            0,
            "a click under the dialog"
        );
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        w.click(content.left_bottom() + Vec2::new(40.0, -40.0));
        assert!(
            mouse_reports(&quill, "claude-1") > 0,
            "a click on the terminal reaches the program"
        );
    }

    /// With nothing over the terminal, a drag that starts on it is the
    /// program's to its release, through the sidebar too.
    #[test]
    fn a_drag_from_the_terminal_into_the_sidebar_stays_the_programs() {
        let (quill, _connecto, ws) = two_projects();
        quill
            .slot("claude-1")
            .session
            .mouse
            .store(true, Ordering::Relaxed);
        let mut w = W::new(&ws);
        w.frame(vec![]);
        w.frame(vec![]);
        let content = w.app.hits.content;
        let sidebar = w.app.hits.sidebar;
        let start = content.center();
        let outside = sidebar.center();
        w.frame(vec![Event::PointerMoved(start)]);
        w.frame(vec![button(start, true)]);
        w.frame(vec![Event::PointerMoved(outside)]);
        w.frame(vec![button(outside, false)]);
        w.frame(vec![]);
        let typed = String::from_utf8_lossy(&quill.slot("claude-1").typed()).into_owned();
        let reports: Vec<&str> = typed.split("\x1b[<").filter(|r| !r.is_empty()).collect();
        assert_eq!(reports.len(), 3, "press, drag, release: {typed:?}");
        assert!(
            reports[0].starts_with("0;") && reports[0].ends_with('M'),
            "{typed:?}"
        );
        assert!(reports[1].starts_with("32;"), "the drag outside: {typed:?}");
        assert!(reports[2].ends_with('m'), "the release: {typed:?}");
    }

    /// Each job question keeps its own answer while it waits: one typed,
    /// minimized, then left for another question, is there when it opens
    /// again; once the question ends, its answer goes with it.
    #[test]
    fn each_job_question_keeps_its_own_draft() {
        use connecto_core::agentic_works::{
            ImportedKey, JobQuestion, WorksAction, WorksControl, WorksDraft, WorksView,
        };
        #[derive(Default)]
        struct Works {
            questions: Mutex<Vec<JobQuestion>>,
            answers: Mutex<Vec<(u64, Option<String>)>>,
        }
        impl WorksControl for Works {
            fn view(&self, _: &str) -> WorksView {
                WorksView::default()
            }
            fn submit(&self, _: String, _: WorksAction) {}
            fn draft(&self, _: &str, _: &str) -> Result<WorksDraft, String> {
                Err(String::new())
            }
            fn import_key(&self, _: &str, _: &std::path::Path) -> Result<ImportedKey, String> {
                Err(String::new())
            }
            fn questions(&self) -> Vec<JobQuestion> {
                self.questions.lock().unwrap().clone()
            }
            fn answer(&self, id: u64, answer: Option<String>) -> Result<(), String> {
                self.answers.lock().unwrap().push((id, answer));
                self.questions.lock().unwrap().retain(|q| q.id != id);
                Ok(())
            }
        }
        let question = |id: u64, terminal: &str| JobQuestion {
            id,
            project: "quill".into(),
            project_name: "quill".into(),
            database: "dev".into(),
            job: i64::try_from(id).unwrap(),
            kind: "review".into(),
            terminal: terminal.into(),
            text: format!("Question {id}?"),
            asked_at: "10:00".into(),
            answer_by: "10:25".into(),
        };
        let (_quill, _connecto, ws) = two_projects();
        let works = Arc::new(Works::default());
        *works.questions.lock().unwrap() = vec![question(4, "claude-9"), question(5, "codex-8")];
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                works: Some(Arc::clone(&works) as Arc<dyn WorksControl>),
                ..Diagnostics::default()
            },
        );
        w.height = 1600.0;
        w.frame(vec![]);
        let at = |w: &W, key: &str| w.app.hits.job_question.rects[key].center();
        let showing = |w: &W, text: &str| {
            w.app
                .hits
                .job_question
                .texts
                .iter()
                .any(|t| t.contains(text))
        };
        assert!(showing(&w, "Question 4?"));
        let answer = at(&w, "job-question-answer");
        w.click(answer);
        w.type_text("in spring");
        w.frame(vec![]);
        let minimize = at(&w, "job-question-minimize");
        w.click(minimize);
        w.frame(vec![]);
        assert!(showing(&w, "Question 5?"), "the other question opens");
        let answer = at(&w, "job-question-answer");
        w.click(answer);
        w.type_text("never");
        w.frame(vec![]);
        let minimize = at(&w, "job-question-minimize");
        w.click(minimize);
        w.frame(vec![]);
        let card = pending_at(&w, "pending-mini:job:4");
        w.click(card);
        w.frame(vec![]);
        let send = at(&w, "job-question-send");
        w.click(send);
        w.frame(vec![]);
        assert_eq!(
            *works.answers.lock().unwrap(),
            [(4, Some("in spring".to_owned()))],
            "the first question's own answer"
        );
        // Question 5 ends unanswered (its time is over), and a new question
        // comes with its id: it starts empty.
        works.questions.lock().unwrap().clear();
        w.frame(vec![]);
        w.frame(vec![]);
        works.questions.lock().unwrap().push(question(5, "codex-1"));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(showing(&w, "Question 5?"));
        let send = at(&w, "job-question-send");
        w.click(send);
        w.frame(vec![]);
        assert_eq!(
            works.answers.lock().unwrap().len(),
            1,
            "nothing to send: the draft went"
        );
    }

    /// A minimized request that ends while another dialog covers the window
    /// lets go of the value typed for it and its field's history at once,
    /// not when that dialog closes.
    #[test]
    fn a_request_that_ends_under_a_dialog_lets_go_of_its_value_at_once() {
        const VALUE: &str = "covered_value_789";
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![token_ask(7, "claude-1", quill.id, "STRIPE_KEY")]);
        let mut w = with_vault(&ws, &vault);
        w.height = 1600.0;
        w.frame(vec![]);
        let value = w.app.hits.secret_ask.rects["vault-ask-value"].center();
        w.click(value);
        w.type_text(VALUE);
        w.frame(vec![]);
        seed_value_history(&w, "ask-value", VALUE);
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        assert!(w.app.secret_draft_for_tests(), "kept while minimized");
        // A notice covers the window; meanwhile the request expires.
        let mut view = (**vault.view.lock().unwrap()).clone();
        view.remote_changes = vec![(1, "A paired machine added the secret GH of quill.".into())];
        *vault.view.lock().unwrap() = Arc::new(view);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.hits.notice.texts.is_empty(), "the notice covers it");
        let mut view = (**vault.view.lock().unwrap()).clone();
        view.asks.clear();
        *vault.view.lock().unwrap() = Arc::new(view);
        w.frame(vec![]);
        assert!(!w.app.hits.notice.texts.is_empty(), "still covered");
        assert!(!w.app.secret_draft_for_tests(), "the value was let go of");
        assert!(!value_history_holds(&w, VALUE), "and the field's history");
    }

    /// Once a reopened request is answered, declined or expires and only
    /// cards remain, typing reaches the terminal again.
    #[test]
    fn the_terminal_takes_the_keyboard_back_when_a_reopened_request_ends() {
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![
            token_ask(7, "claude-1", quill.id, "KEY_A"),
            token_ask(8, "codex-1", quill.id, "KEY_B"),
            token_ask(9, "codex-2", quill.id, "KEY_C"),
        ]);
        let mut w = with_vault(&ws, &vault);
        w.height = 1600.0;
        w.frame(vec![]);
        for _ in 0..3 {
            let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
            w.click(minimize);
            w.frame(vec![]);
        }
        // Declined.
        let card = pending_at(&w, "pending-mini:secret:7");
        w.click(card);
        w.frame(vec![]);
        let decline = w.app.hits.secret_ask.rects["vault-ask-decline"].center();
        w.click(decline);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.hits.secret_ask.texts.is_empty());
        let before = all_typed(&quill);
        w.type_text("x");
        assert_eq!(all_typed(&quill), before + 1, "typed after a decline");
        // Expired while open.
        let card = pending_at(&w, "pending-mini:secret:8");
        w.click(card);
        w.frame(vec![]);
        assert!(!w.app.hits.secret_ask.texts.is_empty());
        let mut view = (**vault.view.lock().unwrap()).clone();
        view.asks.retain(|a| a.id != 8);
        *vault.view.lock().unwrap() = Arc::new(view);
        w.frame(vec![]);
        w.frame(vec![]);
        pending_at(&w, "pending-mini:secret:9");
        let before = all_typed(&quill);
        w.type_text("y");
        assert_eq!(all_typed(&quill), before + 1, "typed after it expired");
    }

    /// Unfolding or folding the stack leaves the keyboard with the terminal.
    #[test]
    fn more_and_fewer_leave_the_keyboard_with_the_terminal() {
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![
            token_ask(7, "claude-1", quill.id, "KEY_A"),
            token_ask(8, "codex-1", quill.id, "KEY_B"),
            token_ask(9, "codex-2", quill.id, "KEY_C"),
            token_ask(10, "claude-3", quill.id, "KEY_D"),
        ]);
        let mut w = with_vault(&ws, &vault);
        w.height = 1600.0;
        w.frame(vec![]);
        for _ in 0..4 {
            let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
            w.click(minimize);
            w.frame(vec![]);
        }
        for (round, key) in ["x", "y"].iter().enumerate() {
            let more = pending_at(&w, "pending-more");
            w.click(more);
            w.frame(vec![]);
            let before = all_typed(&quill);
            w.type_text(key);
            assert_eq!(
                all_typed(&quill),
                before + 1,
                "round {round}: typed after the toggle"
            );
        }
    }

    /// One Esc closes the notice on top; the request it uncovers stays open
    /// to be read, not minimized unseen.
    #[test]
    fn escape_closes_only_the_dialog_on_top() {
        let (quill, _connecto, ws) = two_projects();
        let vault = Arc::new(FakeVault::default());
        *vault.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            asks: vec![token_ask(7, "claude-1", quill.id, "STRIPE_KEY")],
            remote_changes: vec![(1, "A paired machine added the secret GH of quill.".into())],
            ..connecto_core::VaultView::default()
        });
        let mut w = with_vault(&ws, &vault);
        w.height = 1600.0;
        w.frame(vec![]);
        assert!(!w.app.hits.notice.texts.is_empty(), "the notice is on top");
        assert!(w.app.hits.secret_ask.texts.is_empty());
        w.frame(esc());
        w.frame(vec![]);
        assert!(w.app.hits.notice.texts.is_empty(), "Esc closed the notice");
        assert!(
            w.app
                .hits
                .secret_ask
                .texts
                .iter()
                .any(|t| t.contains("STRIPE_KEY")),
            "the request is open, not minimized: {:?}",
            w.app.hits.pending.texts
        );
        // The next Esc is the request's.
        w.frame(esc());
        w.frame(vec![]);
        assert!(w.app.hits.secret_ask.texts.is_empty());
        pending_at(&w, "pending-mini:secret:7");
    }

    /// A request minimized while Settings > Secrets is used does not keep
    /// the value typed there in the field's history once Settings closes.
    #[test]
    fn a_minimized_request_does_not_keep_settings_value_history() {
        const VALUE: &str = "settings_value_456";
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![token_ask(7, "claude-1", quill.id, "STRIPE_KEY")]);
        let mut w = with_vault(&ws, &vault);
        w.height = 1600.0;
        w.frame(vec![]);
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.settings_open());
        seed_value_history(&w, "value", VALUE);
        w.frame(esc());
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.settings_open());
        pending_at(&w, "pending-mini:secret:7");
        assert!(!value_history_holds(&w, VALUE), "Settings closed");
    }

    /// An answer sent to a paired machine still shows as sent when its
    /// request opens again after another was open meanwhile; a refusal
    /// that came meanwhile is said then.
    #[test]
    fn a_sent_answer_still_shows_when_its_request_opens_again() {
        use connecto_core::AnswerState;
        let (quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let theirs = Arc::new(FakeVault::default());
        *theirs.outcomes.lock().unwrap() = vec![AnswerState::Waiting];
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        remote.secret_asks.lock().unwrap().push(remote_ask(4));
        let local = vault_asking(Vec::new());
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&local) as Arc<dyn connecto_core::VaultControl>,
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        let value = w.app.hits.secret_ask.rects["vault-ask-value"].center();
        w.click(value);
        w.type_text("a-long-value-1234");
        w.frame(vec![]);
        let approve = w.app.hits.secret_ask.rects["vault-ask-approve"].center();
        w.click(approve);
        w.frame(vec![]);
        let sent = |w: &W| {
            w.app
                .hits
                .secret_ask
                .texts
                .iter()
                .any(|t| t.contains("waiting for it to answer"))
        };
        assert!(sent(&w), "{:?}", w.app.hits.secret_ask.texts);
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        // Another request opens meanwhile, and is minimized in turn.
        let mut view = (**local.view.lock().unwrap()).clone();
        view.asks.push(token_ask(7, "claude-1", quill.id, "KEY_A"));
        *local.view.lock().unwrap() = Arc::new(view);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .secret_ask
                .texts
                .iter()
                .any(|t| t.contains("KEY_A"))
        );
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        let card = pending_at(&w, "pending-mini:secret:build box:4");
        w.click(card);
        w.frame(vec![]);
        assert!(sent(&w), "still sent: {:?}", w.app.hits.secret_ask.texts);
        // Refused while another request was open: said once it opens.
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        let card = pending_at(&w, "pending-mini:secret:7");
        w.click(card);
        w.frame(vec![]);
        *theirs.state.lock().unwrap() =
            Some(AnswerState::Refused("The token is too short.".into()));
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        w.frame(vec![]);
        let card = pending_at(&w, "pending-mini:secret:build box:4");
        w.click(card);
        w.frame(vec![]);
        let texts = &w.app.hits.secret_ask.texts;
        assert!(
            texts.iter().any(|t| t == "The token is too short."),
            "{texts:?}"
        );
        assert!(!sent(&w), "open to answer again: {texts:?}");
    }

    /// In the smallest window, every minimized request stays reachable:
    /// "+N more" unfolds a list that scrolls to the last one.
    #[test]
    fn every_minimized_request_is_reachable_in_the_smallest_window() {
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![
            token_ask(7, "claude-1", quill.id, "KEY_A"),
            token_ask(8, "codex-1", quill.id, "KEY_B"),
            token_ask(9, "codex-2", quill.id, "KEY_C"),
            token_ask(10, "claude-3", quill.id, "KEY_D"),
        ]);
        let mut w = with_vault(&ws, &vault);
        w.height = 300.0;
        w.frame(vec![]);
        for _ in 0..4 {
            let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
            w.click(minimize);
            w.frame(vec![]);
        }
        assert!(w.app.hits.secret_ask.texts.is_empty());
        let more = pending_at(&w, "pending-more");
        w.click(more);
        w.frame(vec![]);
        let window = Rect::from_min_size(Pos2::ZERO, Vec2::new(w.width, w.height));
        let rect_of = |w: &W, key: &str| w.app.hits.pending.rects[key];
        // The list ends above the button; the last card is reachable once it
        // is whole above it.
        let list_bottom = rect_of(&w, "pending-more").min.y;
        let over = Pos2::new(
            rect_of(&w, "pending-more").center().x - 60.0,
            list_bottom - 20.0,
        );
        for _ in 0..40 {
            let last = rect_of(&w, "pending-mini:secret:10");
            if last.max.y <= list_bottom && window.contains_rect(last) {
                break;
            }
            let wheel = Event::MouseWheel {
                unit: egui::MouseWheelUnit::Point,
                delta: Vec2::new(0.0, -40.0),
                phase: egui::TouchPhase::Move,
                source: egui::MouseWheelSource::Unknown,
                modifiers: Modifiers::NONE,
            };
            w.frame(vec![Event::PointerMoved(over), wheel]);
            w.frame(vec![]);
        }
        let last = rect_of(&w, "pending-mini:secret:10");
        assert!(
            last.max.y <= list_bottom && window.contains_rect(last),
            "the last card scrolled into view: {last:?}, the list ends at {list_bottom}"
        );
        let last = last.center();
        w.click(last);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .secret_ask
                .texts
                .iter()
                .any(|t| t.contains("KEY_D")),
            "{:?}",
            w.app.hits.secret_ask.texts
        );
    }

    /// Minimizing a request while another opens in its place does not keep
    /// the window drawing frame after frame.
    #[test]
    fn minimizing_into_another_request_leaves_the_window_idle() {
        let (quill, _connecto, ws) = two_projects();
        let vault = vault_asking(vec![
            token_ask(7, "claude-1", quill.id, "KEY_A"),
            token_ask(8, "codex-1", quill.id, "KEY_B"),
        ]);
        let mut w = with_vault(&ws, &vault);
        w.height = 1600.0;
        for _ in 0..4 {
            w.frame(vec![]);
        }
        let minimize = w.app.hits.secret_ask.rects["vault-ask-minimize"].center();
        w.click(minimize);
        // The next request fades in over a few frames; then nothing moves.
        for _ in 0..12 {
            w.frame(vec![]);
        }
        assert!(
            w.app
                .hits
                .secret_ask
                .texts
                .iter()
                .any(|t| t.contains("KEY_B"))
        );
        assert!(
            w.repaint_delay > std::time::Duration::from_millis(100),
            "drawing again in {:?}",
            w.repaint_delay
        );
    }

    /// The request says plainly who asks (project, terminal, kind of agent),
    /// why in its words, and where the secret would be sent.
    #[test]
    fn a_secret_request_shows_who_asks_why_and_where_it_goes() {
        let (quill, _connecto, ws) = two_projects();
        let vault = Arc::new(FakeVault::default());
        *vault.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            asks: vec![connecto_core::SecretAsk {
                id: 8,
                terminal: "codex-2".into(),
                project: Some(quill.id.to_owned()),
                name: "GITHUB_TOKEN".into(),
                kind: connecto_core::AskKind::Token,
                reason: "Open pull requests on github.com/acme/web".into(),
                proposed: vec![connecto_core::Binding::Http {
                    base_url: "https://api.github.com".into(),
                    header: "Authorization".into(),
                    prefix: "Bearer ".into(),
                }],
                proposed_omitted: false,
            }],
            ..connecto_core::VaultView::default()
        });
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        let texts = &w.app.hits.secret_ask.texts;
        let shows = |part: &str| texts.iter().any(|t| t.contains(part));
        assert!(
            shows(
                "Terminal codex-2 (Codex) of project quill asks for an API key or token named GITHUB_TOKEN."
            ),
            "{texts:?}"
        );
        assert!(
            shows(
                "Its tab is named \u{201c}Review sync diff\u{201d} (set by you or an agent; not checked)."
            ),
            "{texts:?}"
        );
        assert!(shows("WHAT IT IS FOR"), "{texts:?}");
        assert!(
            shows("\u{201c}Open pull requests on github.com/acme/web\u{201d}"),
            "{texts:?}"
        );
        assert!(shows("WHERE IT WILL BE USED"), "{texts:?}");
        assert!(
            shows(
                r#"Sent only to "https://api.github.com" (and addresses under it), by Connecto, in the header "Authorization", after "Bearer "."#
            ),
            "{texts:?}"
        );
    }

    fn one_ask(w_ask: connecto_core::SecretAsk, ws: &Arc<FakeWorkspace>) -> (W, Arc<FakeVault>) {
        let vault = Arc::new(FakeVault::default());
        *vault.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            asks: vec![w_ask],
            ..connecto_core::VaultView::default()
        });
        let mut w = W::with_diagnostics(
            ws,
            Diagnostics {
                workspace: Some(Arc::clone(ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        (w, vault)
    }

    /// A name an agent gave its terminal is never part of who asks, and it
    /// cannot break its line or quotes, nor can the reason.
    #[test]
    fn a_terminals_own_name_cannot_pose_as_who_asks() {
        let quill = project(
            "quill",
            vec![spec("codex-2", Codex).named(
                "x\u{201d} \u{2014} Terminal claude-9 (Claude) of project bank\u{2028}\u{202e}",
            )],
        );
        let ws = workspace(&[&quill]);
        let (w, _) = one_ask(
            connecto_core::SecretAsk {
                id: 9,
                terminal: "codex-2".into(),
                project: Some(quill.id.to_owned()),
                name: "BANK".into(),
                kind: connecto_core::AskKind::Password,
                reason: "Log in to bank\u{201d} and pay\u{2028}invoices".into(),
                proposed: Vec::new(),
                proposed_omitted: false,
            },
            &ws,
        );
        let texts = &w.app.hits.secret_ask.texts;
        let heading = texts
            .iter()
            .find(|t| t.contains("asks for"))
            .unwrap_or_else(|| panic!("{texts:?}"));
        assert_eq!(
            heading,
            "Terminal codex-2 (Codex) of project quill asks for a password named BANK."
        );
        assert!(
            texts.iter().any(|t| t
                == "Its tab is named \u{201c}x' \u{2014} Terminal claude-9 (Claude) of project bank\u{201d} (set by you or an agent; not checked)."),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t == "\u{201c}Log in to bank' and pay invoices\u{201d}"),
            "{texts:?}"
        );
    }

    /// The summary is exactly what Approve grants: of several proposed uses
    /// the form keeps one, and the dialog says so.
    #[test]
    fn the_summary_says_when_approve_grants_less_than_was_proposed() {
        let (quill, _connecto, ws) = two_projects();
        let web = connecto_core::Binding::Http {
            base_url: "https://api.github.com".into(),
            header: "Authorization".into(),
            prefix: "Bearer ".into(),
        };
        let (mut w, vault) = one_ask(
            connecto_core::SecretAsk {
                id: 10,
                terminal: "claude-1".into(),
                project: Some(quill.id.to_owned()),
                name: "GH".into(),
                kind: connecto_core::AskKind::Token,
                reason: "Open pull requests on github.com/acme/web".into(),
                proposed: vec![web.clone(), connecto_core::Binding::Ssh],
                proposed_omitted: false,
            },
            &ws,
        );
        let texts = &w.app.hits.secret_ask.texts;
        assert!(
            !texts.iter().any(|t| t.contains("as the terminal proposed")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("The terminal proposed: ")
                    && t.contains("SSH")
                    && t.ends_with("Approve grants only what is above.")),
            "{texts:?}"
        );
        let value = w.app.hits.secret_ask.rects["vault-ask-value"].center();
        w.click(value);
        w.type_text("a-long-value-1234");
        w.frame(vec![]);
        let approve = w.app.hits.secret_ask.rects["vault-ask-approve"].center();
        w.click(approve);
        w.frame(vec![]);
        let answers = vault.answers.lock().unwrap();
        let connecto_core::AskAnswer::Provide { bindings, .. } = &answers[0].1 else {
            panic!("{answers:?}");
        };
        assert_eq!(bindings, &[web], "what the summary said, no more");
    }

    fn has_hit(w: &W, hit: &Hit) -> bool {
        w.app
            .hits
            .tree
            .iter()
            .any(|(candidate, _)| candidate == hit)
    }

    /// A host's project groups show as the host has them: its groups in its
    /// order, each over its projects, and the ungrouped ones after. They fold
    /// here, and are changed only on the host.
    #[test]
    fn a_hosts_project_groups_show_as_the_host_has_them() {
        let (_quill, remote, mut w) = window();
        *remote.groups.lock().unwrap() = Some(connecto_core::RemoteGroups {
            groups: vec![
                connecto_core::ProjectGroup {
                    id: "group-2".into(),
                    name: "HARBOR".into(),
                },
                connecto_core::ProjectGroup {
                    id: "group-1".into(),
                    name: "Empty".into(),
                },
            ],
            of: vec![("p-api".into(), "group-2".into())],
        });
        w.height = 2400.0;
        click_row(&mut w, "build box");
        w.frame(vec![]);
        let harbor = Hit::Group(crate::app::remote_group_id("build box", "group-2"));
        let empty = Hit::Group(crate::app::remote_group_id("build box", "group-1"));
        let api = w.rect(&Hit::Project(crate::app::remote_project_id(
            "build box",
            "p-api",
        )));
        let web = w.rect(&Hit::Project(crate::app::remote_project_id(
            "build box",
            "p-web",
        )));
        let harbor_at = w.rect(&harbor);
        let empty_at = w.rect(&empty);
        assert!(harbor_at.min.y < empty_at.min.y, "in the host's order");
        assert!(
            harbor_at.min.y < api.min.y && api.min.y < empty_at.min.y,
            "under its group"
        );
        assert!(empty_at.min.y < web.min.y, "the ungrouped after the groups");
        // Folded here.
        w.click(harbor_at.center());
        w.frame(vec![]);
        assert!(!has_hit(
            &w,
            &Hit::Project(crate::app::remote_project_id("build box", "p-api"))
        ));
        // Never changed from here: nothing reaches this machine's groups.
        assert!(remote.calls().iter().all(|c| !c.contains("group")));
    }

    /// A host from before groups were shared lists its projects flat, as
    /// before.
    #[test]
    fn a_host_without_groups_lists_its_projects_flat() {
        let (_quill, _remote, mut w) = window();
        w.height = 2400.0;
        click_row(&mut w, "build box");
        w.frame(vec![]);
        assert!(
            !w.app
                .hits
                .tree
                .iter()
                .any(|(hit, _)| matches!(hit, Hit::Group(_) | Hit::Ungrouped))
        );
        w.rect(&Hit::Project(crate::app::remote_project_id(
            "build box",
            "p-api",
        )));
        w.rect(&Hit::Project(crate::app::remote_project_id(
            "build box",
            "p-web",
        )));
    }

    /// A paired host's project's secrets show in Settings > Secrets under
    /// the project's name on that machine, with an SSH key's public half; a
    /// secret added there goes to that machine, never to this one's vault,
    /// and a key for it is made there.
    #[test]
    fn a_hosts_project_secrets_are_listed_and_kept_on_that_machine_never_here() {
        const VALUE: &str = "remote_settings_value_123";
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let local = Arc::new(FakeVault::default());
        let theirs = Arc::new(FakeVault::default());
        let id = crate::app::remote_project_id("build box", "p-web");
        let scope = connecto_core::ProviderScope::Project(id.clone());
        *theirs.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            entries: vec![connecto_core::SecretEntry {
                name: "DEPLOY_KEY".into(),
                scope: scope.clone(),
                description: String::new(),
                bindings: vec![connecto_core::Binding::Ssh],
                updated_unix: 0,
                last_used_unix: None,
                public_key: Some("ssh-ed25519 AAAAremote connecto DEPLOY_KEY".into()),
            }],
            ..connecto_core::VaultView::default()
        });
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&local) as Arc<dyn connecto_core::VaultControl>,
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 3200.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-secrets");
        let texts = &w.app.hits.settings.texts;
        assert!(texts.iter().any(|t| t == "DEPLOY_KEY"), "{texts:?}");
        assert!(texts.iter().any(|t| t == "web on build box"), "{texts:?}");
        assert!(
            texts
                .iter()
                .any(|t| t.starts_with("ssh-ed25519 AAAAremote")),
            "{texts:?}"
        );

        let choose = settings_center(&w, &format!("vault-scope:{id}"));
        w.click(choose);
        w.frame(vec![]);
        let name = settings_center(&w, "vault-name");
        type_into(&mut w, name, "API_TOKEN");
        let value = settings_center(&w, "vault-value");
        type_into(&mut w, value, VALUE);
        let save = settings_center(&w, "vault-save");
        w.click(save);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(
            *theirs.puts.lock().unwrap(),
            [(scope.clone(), "API_TOKEN".to_owned(), VALUE.to_owned())]
        );
        assert!(local.puts.lock().unwrap().is_empty(), "never kept here");

        let name = settings_center(&w, "vault-name");
        type_into(&mut w, name, "SERVER_KEY");
        let generate = settings_center(&w, "vault-generate");
        w.click(generate);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(*theirs.kept_keys.lock().unwrap(), ["SERVER_KEY"]);
        assert_eq!(*local.made_here.lock().unwrap(), 0, "no key made here");
        assert_eq!(*theirs.made_here.lock().unwrap(), 0);
        assert!(local.kept_keys.lock().unwrap().is_empty());
        let texts = &w.app.hits.settings.texts;
        assert!(
            texts.iter().all(|t| !t.contains("PRIVATE KEY")),
            "{texts:?}"
        );
    }

    /// A paired host's terminal asks for a secret: the dialog opens here
    /// too, keeps it in that project only, and an SSH key for it is made on
    /// that machine. Sent, it waits; once the request leaves (whoever
    /// answered it), the dialog closes.
    #[test]
    fn a_hosts_terminal_request_opens_here_and_its_key_is_made_there() {
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let local = Arc::new(FakeVault::default());
        let theirs = Arc::new(FakeVault::default());
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        remote
            .secret_asks
            .lock()
            .unwrap()
            .push(connecto_core::RemoteSecretAsk {
                host: "build box".into(),
                project: "p-web".into(),
                project_name: "web".into(),
                ask: connecto_core::SecretAsk {
                    id: 3,
                    terminal: "claude-1".into(),
                    project: Some("p-web".into()),
                    name: "DEPLOY_KEY".into(),
                    kind: connecto_core::AskKind::SshKey,
                    reason: "to push the release".into(),
                    proposed: vec![connecto_core::Binding::Ssh],
                    proposed_omitted: false,
                },
            });
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&local) as Arc<dyn connecto_core::VaultControl>,
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        let hits = &w.app.hits.secret_ask;
        assert!(
            hits.texts
                .iter()
                .any(|t| t
                    .contains("Terminal claude-1 (Claude) of project web on build box asks for an SSH key named DEPLOY_KEY")),
            "{:?}",
            hits.texts
        );
        assert!(
            hits.texts
                .iter()
                .any(|t| t.contains("\u{201c}to push the release\u{201d}")),
            "{:?}",
            hits.texts
        );
        assert!(
            hits.texts.iter().any(|t| t.contains("runs on build box")),
            "{:?}",
            hits.texts
        );
        assert!(
            !hits.rects.contains_key("vault-ask-scope:app"),
            "never app-wide: {:?}",
            hits.rects.keys()
        );
        let generate = hits.rects["vault-ask-generate"].center();
        w.click(generate);
        w.frame(vec![]);
        w.frame(vec![]);
        let id = crate::app::remote_project_id("build box", "p-web");
        assert_eq!(
            *theirs.answers.lock().unwrap(),
            [(
                3,
                connecto_core::AskAnswer::GenerateSshKey {
                    scope: connecto_core::ProviderScope::Project(id),
                }
            )]
        );
        assert!(local.answers.lock().unwrap().is_empty());
        assert_eq!(*local.made_here.lock().unwrap(), 0, "no key made here");
        let texts = &w.app.hits.secret_ask.texts;
        assert!(
            texts.iter().any(|t| t.contains("waiting for it to answer")),
            "{texts:?}"
        );
        // The host has it: the request leaves, and so does the dialog.
        remote.secret_asks.lock().unwrap().clear();
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.hits.secret_ask.texts.is_empty());
    }

    /// Generating a key on the host grants SSH use only, so it is offered
    /// only while the use chosen (and summarised) is SSH.
    #[test]
    fn a_hosts_key_is_generated_only_for_the_ssh_use_shown() {
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let theirs = Arc::new(FakeVault::default());
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        let mut ask = remote_ask(5);
        ask.ask.kind = connecto_core::AskKind::SshKey;
        ask.ask.proposed = vec![connecto_core::Binding::Ssh];
        remote.secret_asks.lock().unwrap().push(ask);
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .secret_ask
                .rects
                .contains_key("vault-ask-generate")
        );
        let web = w.app.hits.secret_ask.rects["vault-kind:vault-ask:Web requests"].center();
        w.click(web);
        w.frame(vec![]);
        let hits = &w.app.hits.secret_ask;
        assert!(
            !hits.rects.contains_key("vault-ask-generate"),
            "a generated key would be SSH, not the web use shown"
        );
        assert!(
            hits.texts
                .iter()
                .any(|t| t.contains("choose \u{201c}SSH key\u{201d} above")),
            "{:?}",
            hits.texts
        );
        assert!(theirs.answers.lock().unwrap().is_empty());
    }

    fn remote_ask(id: u64) -> connecto_core::RemoteSecretAsk {
        connecto_core::RemoteSecretAsk {
            host: "build box".into(),
            project: "p-web".into(),
            project_name: "web".into(),
            ask: connecto_core::SecretAsk {
                id,
                terminal: "claude-1".into(),
                project: Some("p-web".into()),
                name: "TOKEN".into(),
                kind: connecto_core::AskKind::Token,
                reason: "to deploy".into(),
                proposed: Vec::new(),
                proposed_omitted: false,
            },
        }
    }

    /// An answer sent to another machine is followed by its own outcome:
    /// the same refusal twice is told twice and the dialog opens again each
    /// time, and a corrected answer is taken.
    #[test]
    fn each_answer_sent_to_a_host_is_told_by_its_own_outcome() {
        use connecto_core::AnswerState;
        const REFUSED: &str = "The address needs https://.";
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let theirs = Arc::new(FakeVault::default());
        *theirs.outcomes.lock().unwrap() = vec![
            AnswerState::Refused(REFUSED.into()),
            AnswerState::Refused(REFUSED.into()),
            AnswerState::Taken,
        ];
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        remote.secret_asks.lock().unwrap().push(remote_ask(4));
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        for round in 0..3 {
            let value = w.app.hits.secret_ask.rects["vault-ask-value"].center();
            w.click(value);
            w.type_text("a-long-value-1234");
            w.frame(vec![]);
            let approve = w.app.hits.secret_ask.rects["vault-ask-approve"].center();
            w.click(approve);
            w.frame(vec![]);
            w.frame(vec![]);
            assert_eq!(
                theirs.answers.lock().unwrap().len(),
                round + 1,
                "round {round}"
            );
            if round < 2 {
                let texts = &w.app.hits.secret_ask.texts;
                assert!(
                    texts.iter().any(|t| t == REFUSED),
                    "round {round}: {texts:?}"
                );
                assert!(
                    !texts.iter().any(|t| t.contains("waiting for it to answer")),
                    "the dialog is open again: {texts:?}"
                );
            }
        }
        let texts = &w.app.hits.secret_ask.texts;
        assert!(
            texts.iter().any(|t| t.contains("waiting for it to answer")),
            "taken, it waits for the request to go: {texts:?}"
        );
    }

    /// A key made here for this machine's secret is let go of when the
    /// person picks another machine's project; with that machine gone,
    /// no key is made, here or there.
    #[test]
    fn a_key_is_never_made_here_for_another_machines_project() {
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let local = Arc::new(FakeVault::default());
        let theirs = Arc::new(FakeVault::default());
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&local) as Arc<dyn connecto_core::VaultControl>,
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 3200.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-secrets");
        let name = settings_center(&w, "vault-name");
        type_into(&mut w, name, "SERVER_KEY");
        let generate = settings_center(&w, "vault-generate");
        w.click(generate);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(*local.made_here.lock().unwrap(), 1);
        assert!(
            w.app
                .hits
                .settings
                .texts
                .iter()
                .any(|t| t.contains("Private key"))
        );

        let id = crate::app::remote_project_id("build box", "p-web");
        let choose = settings_center(&w, &format!("vault-scope:{id}"));
        w.click(choose);
        w.frame(vec![]);
        w.frame(vec![]);
        let texts = &w.app.hits.settings.texts;
        assert!(
            !texts.iter().any(|t| t.contains("Private key")),
            "the key made here is let go of: {texts:?}"
        );
        let save = settings_center(&w, "vault-save");
        w.click(save);
        w.frame(vec![]);
        assert!(theirs.puts.lock().unwrap().is_empty(), "no value to send");

        // The machine goes away with its project still chosen.
        remote.edit(|view| {
            for host in &mut view.hosts {
                host.link = HostLink::Disconnected;
            }
        });
        w.frame(vec![]);
        w.frame(vec![]);
        let texts = &w.app.hits.settings.texts;
        assert!(
            texts
                .iter()
                .any(|t| t == "That machine is not connected now."),
            "{texts:?}"
        );
        let generate = settings_center(&w, "vault-generate");
        w.click(generate);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(*local.made_here.lock().unwrap(), 1, "none made here");
        assert!(theirs.kept_keys.lock().unwrap().is_empty());
        let texts = &w.app.hits.settings.texts;
        assert!(
            !texts.iter().any(|t| t.contains("Private key")),
            "{texts:?}"
        );
    }

    /// The forms and actions that hold a value never print it.
    #[test]
    fn a_value_in_a_form_or_action_never_prints() {
        const VALUE: &str = "sk-live-form-value-123";
        let key = connecto_core::GeneratedKey {
            private_openssh: VALUE.into(),
            public_openssh: "ssh-ed25519 AAAA".into(),
        };
        let vault_form = crate::vault_ui::VaultForm {
            value: VALUE.into(),
            generated: Some(key.clone()),
            ..crate::vault_ui::VaultForm::default()
        };
        let mut ask_form = crate::vault_ui::AskForm::new(&remote_ask(1).ask, Some("build box"));
        ask_form.value = VALUE.into();
        ask_form.generated = Some(key.clone());
        let answer = connecto_core::AskAnswer::Provide {
            scope: connecto_core::ProviderScope::Global,
            value: VALUE.into(),
            bindings: Vec::new(),
        };
        for printed in [
            format!("{vault_form:?}"),
            format!("{ask_form:?}"),
            format!("{key:?}"),
            format!("{answer:?}"),
            format!(
                "{:?}",
                crate::vault_ui::AskAction::Answer {
                    id: 1,
                    answer: answer.clone()
                }
            ),
            format!(
                "{:?}",
                crate::vault_ui::VaultAction::Put {
                    scope: connecto_core::ProviderScope::Global,
                    name: "GH".into(),
                    description: String::new(),
                    value: VALUE.into(),
                }
            ),
        ] {
            assert!(!printed.contains(VALUE), "{printed}");
        }
    }

    /// Changes from paired machines, more than a notice once held, are all
    /// told, and the vault is told the window has them.
    #[test]
    fn every_change_from_a_paired_machine_is_told_and_then_let_go_of() {
        let (_quill, _connecto, ws) = two_projects();
        let vault = Arc::new(FakeVault::default());
        *vault.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            remote_changes: (1..=60).map(|n| (n, format!("change {n}."))).collect(),
            ..connecto_core::VaultView::default()
        });
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.frame(vec![]);
        w.frame(vec![]);
        let text = w.app.hits.notice.texts.join("\n");
        for n in 1..=60 {
            assert!(text.contains(&format!("change {n}.")), "change {n}: {text}");
        }
        assert!(
            w.app.hits.notice.rects.contains_key("notice-ok"),
            "and the notice can be closed"
        );
        assert_eq!(*vault.acked.lock().unwrap(), Some(60));
    }

    /// What egui keeps of the value fields besides their text: the history
    /// that undo brings back.
    fn value_history_holds(w: &W, secret: &str) -> bool {
        ["value", "ask-value"]
            .iter()
            .map(|id| egui::Id::unique(("vault-field", *id)))
            .chain([egui::Id::unique("vault-ask-key")])
            .any(|id| {
                egui::TextEdit::load_state(&w.ctx, id).is_some_and(|state| {
                    let current = (egui::text::CCursorRange::default(), String::new());
                    let mut undoer = state.undoer();
                    undoer.has_undo(&current)
                        && undoer
                            .undo(&current)
                            .is_some_and(|(_, text)| text.contains(secret))
                })
            })
    }

    fn seed_value_history(w: &W, field: &str, secret: &str) {
        let id = if field == "vault-ask-key" {
            egui::Id::unique("vault-ask-key")
        } else {
            egui::Id::unique(("vault-field", field))
        };
        let mut state = egui::TextEdit::load_state(&w.ctx, id).unwrap_or_default();
        let mut undoer = state.undoer();
        undoer.add_undo(&(egui::text::CCursorRange::default(), secret.to_owned()));
        state.set_undoer(undoer);
        egui::TextEdit::store_state(&w.ctx, id, state);
        assert!(value_history_holds(w, secret));
    }

    /// A value typed in Settings > Secrets leaves nothing in the field's
    /// history once saved, once the place changes, or once Settings closes.
    #[test]
    fn a_value_typed_in_settings_leaves_no_history() {
        const VALUE: &str = "history_value_123";
        let (_quill, _connecto, ws) = two_projects();
        let vault = Arc::new(FakeVault::default());
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-secrets");
        let name = settings_center(&w, "vault-name");
        type_into(&mut w, name, "GH");
        let value = settings_center(&w, "vault-value");
        type_into(&mut w, value, VALUE);
        seed_value_history(&w, "value", VALUE);
        let save = settings_center(&w, "vault-save");
        w.click(save);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(vault.puts.lock().unwrap().len(), 1);
        assert!(!value_history_holds(&w, VALUE), "saved");

        seed_value_history(&w, "value", VALUE);
        let other = w
            .app
            .hits
            .settings
            .rects
            .keys()
            .find(|k| k.starts_with("vault-scope:") && *k != "vault-scope:app")
            .unwrap()
            .clone();
        let at = settings_center(&w, &other);
        w.click(at);
        w.frame(vec![]);
        assert!(!value_history_holds(&w, VALUE), "the place changed");

        seed_value_history(&w, "value", VALUE);
        w.frame(esc());
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!value_history_holds(&w, VALUE), "Settings closed");
    }

    /// A value typed for a request leaves nothing in the field's history
    /// once answered, nor when another request takes its place.
    #[test]
    fn a_value_typed_for_a_request_leaves_no_history() {
        const VALUE: &str = "request_value_123";
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let theirs = Arc::new(FakeVault::default());
        *theirs.outcomes.lock().unwrap() = vec![connecto_core::AnswerState::Taken];
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        remote.secret_asks.lock().unwrap().push(remote_ask(4));
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        let value = w.app.hits.secret_ask.rects["vault-ask-value"].center();
        w.click(value);
        w.type_text(VALUE);
        w.frame(vec![]);
        seed_value_history(&w, "ask-value", VALUE);
        let approve = w.app.hits.secret_ask.rects["vault-ask-approve"].center();
        w.click(approve);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(theirs.answers.lock().unwrap().len(), 1);
        assert!(!value_history_holds(&w, VALUE), "answered");

        // Another request takes its place.
        seed_value_history(&w, "ask-value", VALUE);
        *remote.secret_asks.lock().unwrap() = vec![remote_ask(9)];
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!value_history_holds(&w, VALUE), "another request");
    }

    /// "Replace value…" on another machine's secret lets go of a key made
    /// here, as choosing that machine's project does.
    #[test]
    fn replacing_another_machines_secret_lets_go_of_a_key_made_here() {
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let local = Arc::new(FakeVault::default());
        let theirs = Arc::new(FakeVault::default());
        let id = crate::app::remote_project_id("build box", "p-web");
        *theirs.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            entries: vec![connecto_core::SecretEntry {
                name: "API".into(),
                scope: connecto_core::ProviderScope::Project(id.clone()),
                description: String::new(),
                bindings: Vec::new(),
                updated_unix: 0,
                last_used_unix: None,
                public_key: None,
            }],
            ..connecto_core::VaultView::default()
        });
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&local) as Arc<dyn connecto_core::VaultControl>,
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 3200.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-secrets");
        let generate = settings_center(&w, "vault-generate");
        w.click(generate);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .settings
                .texts
                .iter()
                .any(|t| t.contains("Private key"))
        );
        let replace = settings_center(&w, &format!("vault-replace:{id}:API"));
        w.click(replace);
        w.frame(vec![]);
        w.frame(vec![]);
        let texts = &w.app.hits.settings.texts;
        assert!(
            !texts.iter().any(|t| t.contains("Private key")),
            "{texts:?}"
        );
        assert!(
            !texts.iter().any(|t| t.contains("OPENSSH PRIVATE KEY")),
            "{texts:?}"
        );
    }

    /// A host project keeping more secrets than are read at once: Settings
    /// goes on to the next ones, and back to the first.
    #[test]
    fn settings_goes_on_to_the_secrets_after_those_shown() {
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let theirs = Arc::new(FakeVault::default());
        let id = crate::app::remote_project_id("build box", "p-web");
        let scope = connecto_core::ProviderScope::Project(id.clone());
        *theirs.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            pages: vec![connecto_core::SecretsPage {
                scope: scope.clone(),
                from: Some("K10".into()),
                next: Some("K20".into()),
            }],
            ..connecto_core::VaultView::default()
        });
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 3200.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-secrets");
        let texts = &w.app.hits.settings.texts;
        assert!(
            texts
                .iter()
                .any(|t| t == "web on build box: the secrets after K10."),
            "{texts:?}"
        );
        let next = settings_center(&w, &format!("vault-next:{id}"));
        w.click(next);
        w.frame(vec![]);
        let first = settings_center(&w, &format!("vault-first:{id}"));
        w.click(first);
        w.frame(vec![]);
        assert_eq!(
            *theirs.shown_from.lock().unwrap(),
            [(scope.clone(), Some("K20".to_owned())), (scope, None)]
        );
    }

    /// A host's request whose proposed use could not be sent says so, and
    /// is approved only once the person has chosen a use.
    #[test]
    fn a_request_without_its_proposed_use_waits_for_a_chosen_one() {
        let (_quill, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let theirs = Arc::new(FakeVault::default());
        *theirs.outcomes.lock().unwrap() = vec![connecto_core::AnswerState::Taken];
        remote.vaults.lock().unwrap().push((
            "build box".into(),
            "p-web".into(),
            Arc::clone(&theirs),
        ));
        let mut ask = remote_ask(4);
        ask.ask.proposed_omitted = true;
        remote.secret_asks.lock().unwrap().push(ask);
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .secret_ask
                .texts
                .iter()
                .any(|t| t.contains("proposed a use too long to send")),
            "{:?}",
            w.app.hits.secret_ask.texts
        );
        let value = w.app.hits.secret_ask.rects["vault-ask-value"].center();
        w.click(value);
        w.type_text("a-long-value-1234");
        w.frame(vec![]);
        let approve = w.app.hits.secret_ask.rects["vault-ask-approve"].center();
        w.click(approve);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            theirs.answers.lock().unwrap().is_empty(),
            "no use chosen yet"
        );
        let web = w.app.hits.secret_ask.rects["vault-kind:vault-ask:Web requests"].center();
        w.click(web);
        w.frame(vec![]);
        let approve = w.app.hits.secret_ask.rects["vault-ask-approve"].center();
        w.click(approve);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(
            theirs.answers.lock().unwrap().len(),
            1,
            "approved once chosen"
        );
    }

    /// A change a paired machine made to secrets here is told to the
    /// person, once.
    #[test]
    fn a_change_from_a_paired_machine_is_told_here_once() {
        let (_quill, _connecto, ws) = two_projects();
        let vault = Arc::new(FakeVault::default());
        *vault.view.lock().unwrap() = Arc::new(connecto_core::VaultView {
            remote_changes: vec![(1, "A paired machine added the secret GH of quill.".into())],
            ..connecto_core::VaultView::default()
        });
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.notice_open());
        let texts = w.app.hits.notice.texts.clone();
        assert!(
            texts
                .iter()
                .any(|t| t.contains("added the secret GH of quill")),
            "{texts:?}"
        );
        let ok = w.app.hits.notice.rects["notice-ok"].center();
        w.click(ok);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.notice_open(), "told once");
    }

    #[test]
    fn a_secret_added_in_settings_reaches_the_vault_and_is_never_shown() {
        const VALUE: &str = "ghp_settings_value_123";
        let (_quill, _connecto, ws) = two_projects();
        let vault = Arc::new(FakeVault::default());
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                vault: Arc::clone(&vault) as Arc<dyn connecto_core::VaultControl>,
                ..Diagnostics::default()
            },
        );
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-secrets");
        let texts = &w.app.hits.settings.texts;
        assert!(texts.iter().any(|t| t == "No secrets yet."), "{texts:?}");
        let name = settings_center(&w, "vault-name");
        type_into(&mut w, name, "GITHUB_TOKEN");
        let value = settings_center(&w, "vault-value");
        type_into(&mut w, value, VALUE);
        w.frame(vec![]);
        let save = settings_center(&w, "vault-save");
        w.click(save);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(
            *vault.puts.lock().unwrap(),
            [(
                connecto_core::ProviderScope::Global,
                "GITHUB_TOKEN".to_owned(),
                VALUE.to_owned()
            )]
        );
        let texts = &w.app.hits.settings.texts;
        assert!(texts.iter().any(|t| t == "GITHUB_TOKEN"), "{texts:?}");
        assert!(
            texts
                .iter()
                .any(|t| t.contains("Terminals cannot use it yet")),
            "{texts:?}"
        );
        assert!(texts.iter().all(|t| !t.contains(VALUE)));
    }

    #[test]
    fn an_unreadable_relay_token_is_not_said_to_be_absent() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        remote.view.lock().unwrap().relay_token_unreadable = true;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-remote");
        let texts = &w.app.hits.settings.texts;
        let unreadable = format!(
            "Access token could not be read from the {}",
            crate::platform::KEY_STORE
        );
        assert!(texts.contains(&unreadable), "{texts:?}");
        assert!(!texts.iter().any(|t| t == "No access token"), "{texts:?}");
    }

    #[test]
    fn a_refused_relay_is_said_under_its_buttons() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        *remote.refuse.lock().unwrap() = Some("that is not a relay address".into());
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-remote");
        let url = settings_center(&w, "relay-url");
        type_into(&mut w, url, "https://relay.example");
        let save = settings_center(&w, "relay-save");
        w.click(save);
        w.frame(vec![]);
        w.frame(vec![]);
        let texts = &w.app.hits.settings.texts;
        let heading = texts.iter().position(|t| t == "RELAY").unwrap();
        let error = texts
            .iter()
            .position(|t| t.contains("not a relay address"))
            .unwrap();
        assert!(error > heading, "{texts:?}");
    }

    #[test]
    fn settings_choose_how_each_agent_is_launched_and_warn_when_it_may_stop_to_ask() {
        use connecto_core::{AgentMode, LaunchModes};
        let (_quill, _remote, mut w) = window();
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-launch");
        // The default is today's behaviour, and says nothing alarming.
        assert_eq!(w.ws.launch_modes(), LaunchModes::default());
        let texts = &w.app.hits.settings.texts;
        assert!(
            texts
                .iter()
                .any(|t| t.contains("HOW CODING CLIS ARE LAUNCHED"))
        );
        for key in [
            "mode:claude:bypass",
            "mode:claude:auto",
            "mode:claude:prompt",
            "mode:codex:bypass",
            "mode:codex:auto",
            "mode:codex:prompt",
        ] {
            assert!(w.app.hits.settings.rects.contains_key(key), "{key}");
        }
        assert!(!texts.iter().any(|t| t.contains("can stop and wait")));
        // Choosing for one tool changes that tool only, and the warning shows.
        let auto = settings_center(&w, "mode:claude:auto");
        w.click(auto);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(
            w.ws.launch_modes(),
            LaunchModes {
                claude: AgentMode::Auto,
                codex: AgentMode::Bypass,
                pi: AgentMode::Bypass,
                antigravity: AgentMode::Bypass,
                opencode: AgentMode::Bypass,
            }
        );
        assert!(
            w.app
                .hits
                .settings
                .texts
                .iter()
                .any(|t| t.contains("can stop and wait"))
        );
        let prompt = settings_center(&w, "mode:codex:prompt");
        w.click(prompt);
        w.frame(vec![]);
        assert_eq!(
            w.ws.launch_modes(),
            LaunchModes {
                claude: AgentMode::Auto,
                codex: AgentMode::Prompt,
                pi: AgentMode::Bypass,
                antigravity: AgentMode::Bypass,
                opencode: AgentMode::Bypass,
            }
        );
    }

    fn set_phase(remote: &FakeRemote, phase: connecto_core::PairingPhase) {
        remote.edit(|v| v.pairing = phase);
    }

    #[test]
    fn a_refused_command_does_not_hide_the_backend_error() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        *remote.refuse.lock().unwrap() = Some("boom".into());
        w.app.queue_for_tests(Command::AddHost);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(dialog_shows(&w, "boom"));
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(dialog_shows(&w, "Finish the open dialog or field first."));
        assert!(dialog_shows(&w, "boom"), "the backend's error stays");
        assert!(w.app.hits.pairing.rects.contains_key("pairing-retry"));
        assert!(
            w.app.pairing_open(),
            "an Idle dialog with an error stays open"
        );
    }

    #[test]
    fn idle_closes_only_after_a_phase_was_seen_for_this_attempt() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        *remote.stay_idle.lock().unwrap() = true;
        w.app.queue_for_tests(Command::AddHost);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.pairing_open(), "Idle before any phase is not the end");
        assert!(dialog_shows(&w, "No pairing is under way."));
        set_phase(
            &remote,
            connecto_core::PairingPhase::ShowingCode {
                my_code: "4828 1349".into(),
                expires_in_secs: 100,
            },
        );
        w.frame(vec![]);
        set_phase(&remote, connecto_core::PairingPhase::Idle);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.pairing_open(), "Idle after a phase is the end");
    }

    #[test]
    fn keeping_cannot_be_dismissed_or_cancelled() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        set_phase(&remote, connecto_core::PairingPhase::Keeping);
        w.frame(vec![]);
        assert!(dialog_shows(&w, "Finishing…"));
        assert!(dialog_shows(&w, "this takes a few seconds"));
        let before = remote.calls().len();
        w.frame(esc());
        w.frame(vec![]);
        assert!(
            w.app.pairing_open(),
            "Escape does not close it while it finishes"
        );
        // The close and advanced controls are disabled: a click does nothing.
        click_pairing(&mut w, "pairing-close");
        click_pairing(&mut w, "pairing-advanced");
        assert!(w.app.pairing_open());
        assert_eq!(remote.calls().len(), before, "no cancel was sent");
        assert!(matches!(
            remote.view().pairing,
            connecto_core::PairingPhase::Keeping
        ));
    }

    #[test]
    fn confirming_offers_cancel_and_says_what_it_does() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        set_phase(&remote, connecto_core::PairingPhase::Confirming);
        w.frame(vec![]);
        assert!(dialog_shows(
            &w,
            "Cancelling now undoes the pairing on both machines."
        ));
        click_pairing(&mut w, "pairing-close");
        assert_eq!(remote.calls().last().unwrap(), "cancel_pairing");
        assert!(!w.app.pairing_open());
    }

    #[test]
    fn copy_code_copies_the_digits_only() {
        let (_quill, _remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        click_pairing(&mut w, "pairing-copy");
        assert_eq!(w.copied.last().map(String::as_str), Some("48281349"));
    }

    fn calls_starting_with(remote: &FakeRemote, prefix: &str) -> usize {
        remote
            .calls()
            .iter()
            .filter(|c| c.starts_with(prefix))
            .count()
    }

    #[test]
    fn no_sheet_opens_over_the_pairing_dialog() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::AddHost);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.pairing_open());
        w.app.queue_for_tests(Command::Settings);
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.settings_open(), "a sheet would cover the dialog");
        assert!(w.app.pairing_open());
        assert!(dialog_shows(&w, "Finish the open dialog or field first."));
        assert_eq!(calls_starting_with(&remote, "start_pairing"), 1);
    }

    #[test]
    fn try_again_starts_from_nothing_carried_over() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        let field = pairing_center(&w, "pairing-code");
        type_into(&mut w, field, "48281349");
        set_phase(
            &remote,
            connecto_core::PairingPhase::NameNeeded {
                suggested_name: "Old".into(),
            },
        );
        w.frame(vec![]);
        assert_eq!(w.app.pairing_name_for_tests().as_deref(), Some("Old"));
        set_phase(
            &remote,
            connecto_core::PairingPhase::Failed {
                message: "relay down".into(),
            },
        );
        w.frame(vec![]);
        click_pairing(&mut w, "pairing-retry");
        assert_eq!(w.app.pairing_digits_for_tests().as_deref(), Some(""));
        assert_eq!(w.app.pairing_name_for_tests(), None);
        set_phase(
            &remote,
            connecto_core::PairingPhase::NameNeeded {
                suggested_name: "New".into(),
            },
        );
        w.frame(vec![]);
        assert_eq!(w.app.pairing_name_for_tests().as_deref(), Some("New"));
    }

    #[test]
    fn a_refused_start_shows_idle_with_the_error_and_no_spinner() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        *remote.refuse.lock().unwrap() = Some("remote sessions are not available".into());
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        assert!(w.app.pairing_open(), "the refusal is shown, not hidden");
        assert!(dialog_shows(&w, "remote sessions are not available"));
        assert!(
            !dialog_shows(&w, "Preparing"),
            "no progress for a pairing that never began"
        );
        assert!(w.app.hits.pairing.rects.contains_key("pairing-retry"));
    }

    #[test]
    fn an_idle_pairing_that_was_accepted_closes_the_dialog() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        assert!(w.app.pairing_open());
        set_phase(&remote, connecto_core::PairingPhase::Idle);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.pairing_open(), "nothing would ever move it on");
    }

    #[test]
    fn typing_on_after_all_digits_does_not_resubmit_the_code() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        let field = pairing_center(&w, "pairing-code");
        type_into(&mut w, field, "12345678");
        assert_eq!(calls_starting_with(&remote, "submit_peer_code"), 1);
        type_into(&mut w, field, "7");
        assert_eq!(
            calls_starting_with(&remote, "submit_peer_code"),
            1,
            "the same code was not resent"
        );
    }

    #[test]
    fn the_advanced_way_closes_settings_before_the_address_form() {
        let (_quill, _remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        click_pairing(&mut w, "pairing-advanced");
        assert!(w.app.modal_open_for_tests(), "the address form is open");
        assert!(
            !w.app.settings_open(),
            "one Escape must not close two sheets"
        );
    }

    #[test]
    fn the_dialog_takes_the_keyboard_when_its_field_appears() {
        let (_quill, _remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        assert!(
            w.ctx.memory(|m| m.focused()).is_some(),
            "the code field has the focus"
        );
    }

    /// Open the pairing dialog from Settings > Remote Access.
    fn pair_from_settings(w: &mut W) {
        show_tab(w, "tab-remote");
        let at = settings_center(w, "pair-start");
        w.click(at);
        w.frame(vec![]);
        w.frame(vec![]);
    }

    fn dialog_shows(w: &W, text: &str) -> bool {
        w.app.hits.pairing.texts.iter().any(|t| t.contains(text))
    }

    fn pairing_center(w: &W, key: &str) -> Pos2 {
        w.app
            .hits
            .pairing
            .rects
            .get(key)
            .unwrap_or_else(|| panic!("{key} not drawn: {:?}", w.app.hits.pairing.rects.keys()))
            .center()
    }

    fn click_pairing(w: &mut W, key: &str) {
        let at = pairing_center(w, key);
        w.click(at);
        w.frame(vec![]);
        w.frame(vec![]);
    }

    fn machine(
        id: &str,
        name: &str,
        inbound: HostLink,
        outbound: HostLink,
    ) -> connecto_core::MachineRow {
        connecto_core::MachineRow {
            id: id.into(),
            name: name.into(),
            paired_at: "2026-11-10".into(),
            inbound,
            outbound,
        }
    }

    #[test]
    fn pair_a_device_opens_the_dialog_over_open_settings() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        assert!(w.app.pairing_open());
        assert!(
            w.app.settings_open(),
            "the dialog is over Settings, not in place of it"
        );
        assert_eq!(remote.calls().last().unwrap(), "start_pairing");
        assert!(dialog_shows(&w, "4828 1349"));
    }

    #[test]
    fn each_pairing_phase_is_drawn_with_its_own_text_and_action() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        use connecto_core::PairingPhase as P;

        set_phase(&remote, P::Preparing);
        w.frame(vec![]);
        assert!(dialog_shows(&w, "Preparing"));
        assert!(dialog_shows(&w, "20 seconds"), "the wait is explained");

        set_phase(
            &remote,
            P::ShowingCode {
                my_code: "4828 1349".into(),
                expires_in_secs: 118,
            },
        );
        w.frame(vec![]);
        assert!(dialog_shows(&w, "4828 1349"));
        assert!(dialog_shows(&w, "Valid for 1:58"));
        // Eight digits typed or pasted with a space submit once.
        let field = pairing_center(&w, "pairing-code");
        type_into(&mut w, field, "4828 1349");
        w.frame(vec![]);
        assert_eq!(remote.calls().last().unwrap(), "submit_peer_code 48281349");

        set_phase(
            &remote,
            P::WaitingForPeer {
                my_code: "4828 1349".into(),
                expires_in_secs: 100,
            },
        );
        w.frame(vec![]);
        assert!(dialog_shows(
            &w,
            "Waiting for the other machine to enter this code"
        ));
        assert!(!w.app.hits.pairing.rects.contains_key("pairing-code"));

        set_phase(&remote, P::Connecting);
        w.frame(vec![]);
        assert!(dialog_shows(&w, "Connecting"));

        set_phase(
            &remote,
            P::NameNeeded {
                suggested_name: "Studio Mac".into(),
            },
        );
        w.frame(vec![]);
        assert!(dialog_shows(&w, "NAME THE OTHER MACHINE"));
        click_pairing(&mut w, "pairing-save-name");
        assert_eq!(remote.calls().last().unwrap(), "finish_pairing Studio Mac");

        set_phase(&remote, P::Confirming);
        w.frame(vec![]);
        assert!(dialog_shows(&w, "Your name for it is saved"));

        set_phase(
            &remote,
            P::Done {
                machine_id: "abc".into(),
            },
        );
        w.frame(vec![]);
        assert!(dialog_shows(&w, "Paired. The machine is in the list now."));

        set_phase(
            &remote,
            P::Failed {
                message: "the relay was unreachable".into(),
            },
        );
        w.frame(vec![]);
        assert!(dialog_shows(&w, "the relay was unreachable"));
        let before = remote.calls().len();
        click_pairing(&mut w, "pairing-retry");
        assert_eq!(remote.calls().len(), before + 1);
        assert_eq!(remote.calls().last().unwrap(), "start_pairing");
    }

    #[test]
    fn closing_the_dialog_or_escape_stops_the_pairing_and_leaves_settings() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        click_pairing(&mut w, "pairing-close");
        assert_eq!(remote.calls().last().unwrap(), "cancel_pairing");
        assert!(!w.app.pairing_open());
        pair_from_settings(&mut w);
        w.frame(esc());
        w.frame(vec![]);
        assert_eq!(remote.calls().last().unwrap(), "cancel_pairing");
        assert!(!w.app.pairing_open());
        assert!(w.app.settings_open(), "Escape closed only the dialog");
    }

    #[test]
    fn errors_from_the_pairing_calls_are_shown_in_the_dialog() {
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        *remote.refuse.lock().unwrap() = Some("remote sessions are not available".into());
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        pair_from_settings(&mut w);
        assert!(
            w.app.pairing_open(),
            "the button explains why, and the dialog says it"
        );
        assert!(dialog_shows(&w, "remote sessions are not available"));
        assert!(
            !w.app
                .hits
                .settings
                .texts
                .iter()
                .any(|t| t.contains("not available"))
        );
        // A refused code keeps the dialog and the field.
        *remote.refuse.lock().unwrap() = None;
        set_phase(
            &remote,
            connecto_core::PairingPhase::ShowingCode {
                my_code: "4828 1349".into(),
                expires_in_secs: 60,
            },
        );
        w.frame(vec![]);
        *remote.refuse.lock().unwrap() =
            Some("Enter the eight digits shown on the other machine.".into());
        let field = pairing_center(&w, "pairing-code");
        type_into(&mut w, field, "1234 5678");
        assert!(w.app.pairing_open());
        assert!(dialog_shows(
            &w,
            "Enter the eight digits shown on the other machine."
        ));
        // A refused name keeps the dialog open with the reason beside it.
        *remote.refuse.lock().unwrap() =
            Some("there is already a machine called Studio Mac".into());
        set_phase(
            &remote,
            connecto_core::PairingPhase::NameNeeded {
                suggested_name: "Studio Mac".into(),
            },
        );
        w.frame(vec![]);
        click_pairing(&mut w, "pairing-save-name");
        assert!(w.app.pairing_open());
        assert!(dialog_shows(
            &w,
            "there is already a machine called Studio Mac"
        ));
    }

    #[test]
    fn the_machines_list_shows_the_empty_state_and_each_connection_in_words() {
        use connecto_core::HostLink;
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-remote");
        assert!(
            w.app
                .hits
                .settings
                .texts
                .iter()
                .any(|t| t == "No machines paired yet.")
        );
        remote.edit(|v| {
            v.machines = vec![
                machine(
                    "1111",
                    "Studio Mac",
                    HostLink::Direct,
                    HostLink::Disconnected,
                ),
                machine(
                    "legacy:SHA256:x",
                    "Old laptop",
                    HostLink::Disconnected,
                    HostLink::Relayed,
                ),
                machine(
                    "3333",
                    "Idle box",
                    HostLink::Disconnected,
                    HostLink::Disconnected,
                ),
            ];
        });
        w.frame(vec![]);
        let texts = w.app.hits.settings.texts.clone();
        assert!(texts.iter().any(|t| t == "Studio Mac"));
        assert!(
            texts
                .iter()
                .any(|t| t.contains("connected from it (direct)")),
            "{texts:?}"
        );
        assert!(
            texts
                .iter()
                .any(|t| t.contains("connected to it (through the relay)"))
        );
        assert!(
            texts
                .iter()
                .filter(|t| t.as_str() == "Not connected")
                .count()
                == 1
        );
        assert!(!texts.iter().any(|t| t == "No machines paired yet."));
        assert!(
            w.app
                .hits
                .settings
                .rects
                .contains_key("disconnect-machine:legacy:SHA256:x")
        );
    }

    #[test]
    fn disconnect_asks_first_and_then_removes_the_pairing() {
        use connecto_core::HostLink;
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        remote.edit(|v| {
            v.machines = vec![machine(
                "1111",
                "Studio Mac",
                HostLink::Direct,
                HostLink::Disconnected,
            )];
        });
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-remote");
        let at = settings_center(&w, "disconnect-machine:1111");
        w.click(at);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.confirm_open());
        assert!(
            remote.calls().is_empty(),
            "nothing is removed before the confirm"
        );
        let texts = w.app.hits.confirm.texts.clone();
        assert!(texts.iter().any(|t| t == "Remove Studio Mac?"), "{texts:?}");
        assert!(texts.iter().any(
            |t| t.contains("ends any connection") && t.contains("pair the two machines again")
        ));
        // Keep leaves it alone.
        let keep = w.app.hits.confirm.rects["confirm-keep"].center();
        w.click(keep);
        w.frame(vec![]);
        assert!(!w.app.confirm_open());
        assert!(remote.calls().is_empty());
        // Again, and confirm: the machine is removed.
        let at = settings_center(&w, "disconnect-machine:1111");
        w.click(at);
        w.frame(vec![]);
        w.frame(vec![]);
        let remove = w.app.hits.confirm.rects["confirm-remove"].center();
        w.click(remove);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(remote.calls(), ["disconnect_machine 1111"]);
        assert!(!w.app.confirm_open());
    }

    #[test]
    fn a_refused_removal_stays_open_and_says_why() {
        use connecto_core::HostLink;
        let (_quill, remote, mut w) = window();
        w.height = 2400.0;
        remote.edit(|v| {
            v.machines = vec![machine(
                "1111",
                "Studio Mac",
                HostLink::Disconnected,
                HostLink::Disconnected,
            )];
        });
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
        show_tab(&mut w, "tab-remote");
        let at = settings_center(&w, "disconnect-machine:1111");
        w.click(at);
        w.frame(vec![]);
        *remote.refuse.lock().unwrap() = Some("could not write the key file".into());
        let remove = w.app.hits.confirm.rects["confirm-remove"].center();
        w.click(remove);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.confirm_open());
        assert!(
            w.app
                .hits
                .confirm
                .texts
                .iter()
                .any(|t| t.contains("could not write the key file"))
        );
    }

    #[test]
    fn the_sidebar_add_host_opens_the_pairing_dialog() {
        let (_quill, remote, mut w) = window();
        open_hosts(&mut w);
        let add = w
            .app
            .hits
            .host_rows
            .iter()
            .find(|(name, _)| name == "add")
            .map(|(_, rect)| rect.center())
            .unwrap();
        w.click(add);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.pairing_open());
        assert_eq!(remote.calls().last().unwrap(), "start_pairing");
    }

    #[test]
    fn the_advanced_way_adds_by_address_from_the_dialog() {
        let (_quill, remote, mut w) = window();
        w.app.queue_for_tests(Command::AddHost);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.pairing_open());
        click_pairing(&mut w, "pairing-advanced");
        assert!(!w.app.pairing_open());
        assert_eq!(remote.calls().last().unwrap(), "cancel_pairing");
        assert!(w.app.modal_open_for_tests(), "the address form is open");
        let name = center(&w, "host-name");
        type_into(&mut w, name, "ci box");
        let address = center(&w, "host-address");
        type_into(&mut w, address, "connecto:abc");
        let cont = center(&w, "host-continue");
        w.click(cont);
        w.frame(vec![]);
        assert_eq!(
            remote.calls().last().unwrap(),
            "add_host ci box connecto:abc"
        );
    }

    mod agents_settings {
        use super::*;
        use connecto_core::{AgentBusy, AgentRow, AgentsControl, AgentsView};

        struct FakeAgents {
            view: Mutex<AgentsView>,
            calls: Mutex<Vec<String>>,
            off: Mutex<Vec<ToolKind>>,
            /// How often which agents are off was read.
            off_reads: std::sync::atomic::AtomicUsize,
        }

        impl FakeAgents {
            fn new(rows: Vec<AgentRow>) -> Arc<Self> {
                Arc::new(Self {
                    view: Mutex::new(AgentsView {
                        rows,
                        ..AgentsView::default()
                    }),
                    calls: Mutex::default(),
                    off: Mutex::default(),
                    off_reads: std::sync::atomic::AtomicUsize::new(0),
                })
            }
            fn calls(&self) -> Vec<String> {
                self.calls.lock().unwrap().clone()
            }
        }

        impl AgentsControl for FakeAgents {
            fn view(&self) -> Arc<AgentsView> {
                Arc::new(self.view.lock().unwrap().clone())
            }
            fn activate(&self, tool: ToolKind) {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("activate {}", tool.name()));
            }
            fn deactivate(&self, tool: ToolKind) {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("deactivate {}", tool.name()));
                self.off.lock().unwrap().push(tool);
            }
            fn refresh(&self) {
                self.calls.lock().unwrap().push("refresh".into());
            }
            fn install_node(&self, tool: ToolKind) {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("install_node {}", tool.name()));
            }
            fn decline_node(&self, tool: ToolKind) {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("decline_node {}", tool.name()));
            }
            fn signin_started(&self, tool: ToolKind) {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("signin_started {}", tool.name()));
                for row in &mut self.view.lock().unwrap().rows {
                    if row.tool == tool {
                        row.signin_requested = false;
                    }
                }
            }
            fn inactive(&self) -> Vec<ToolKind> {
                self.off_reads
                    .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                self.off.lock().unwrap().clone()
            }
            fn set_pi_key(
                &self,
                _scope: &connecto_core::ProviderScope,
                provider: connecto_core::PiProvider,
                key: Option<String>,
            ) -> Result<(), String> {
                self.calls.lock().unwrap().push(format!(
                    "pi_key {} {}",
                    provider.id(),
                    key.as_deref().unwrap_or("<removed>")
                ));
                let keys = &mut self.view.lock().unwrap().pi.keys_set;
                keys.retain(|p| *p != provider);
                if key.is_some() {
                    keys.push(provider);
                }
                Ok(())
            }
            fn set_pi_default_model(
                &self,
                _scope: &connecto_core::ProviderScope,
                model: Option<String>,
            ) -> Result<(), String> {
                self.calls.lock().unwrap().push(format!(
                    "pi_model {}",
                    model.as_deref().unwrap_or("<pi's choice>")
                ));
                self.view.lock().unwrap().pi.default_model = model;
                Ok(())
            }
            fn set_claude_together_key(
                &self,
                scope: &connecto_core::ProviderScope,
                key: Option<String>,
            ) -> Result<(), String> {
                self.calls.lock().unwrap().push(format!(
                    "claude_key {:?} {}",
                    scope,
                    key.as_deref().unwrap_or("<removed>")
                ));
                let claude = &mut self.view.lock().unwrap().claude;
                claude.together_key = key.is_some();
                if key.is_some() && claude.togetherlink == connecto_core::TogetherLinkState::Missing
                {
                    claude.install_offered = true;
                }
                Ok(())
            }
            fn install_togetherlink(&self) {
                self.calls
                    .lock()
                    .unwrap()
                    .push("install_togetherlink".into());
                let claude = &mut self.view.lock().unwrap().claude;
                claude.install_offered = false;
                claude.togetherlink =
                    connecto_core::TogetherLinkState::Installing("Installing TogetherLink…".into());
            }
            fn decline_togetherlink(&self) {
                self.calls
                    .lock()
                    .unwrap()
                    .push("decline_togetherlink".into());
                self.view.lock().unwrap().claude.install_offered = false;
            }
            fn set_pi_lmstudio_url(
                &self,
                _scope: &connecto_core::ProviderScope,
                url: String,
            ) -> Result<(), String> {
                self.calls
                    .lock()
                    .unwrap()
                    .push(format!("pi_lmstudio {url}"));
                if !url.starts_with("http") {
                    return Err("That is not a server address.".into());
                }
                self.view.lock().unwrap().pi.lmstudio_url = url;
                Ok(())
            }
        }

        fn center(w: &W, key: &str) -> Pos2 {
            w.app
                .hits
                .settings
                .rects
                .get(key)
                .unwrap_or_else(|| panic!("{key} not drawn"))
                .center()
        }

        fn replace_text(w: &mut W, key: &str, text: &str) {
            let at = center(w, key);
            w.click(at);
            // Select what the field holds, so the typing replaces it.
            w.frame(vec![Event::Key {
                key: egui::Key::A,
                physical_key: Some(egui::Key::A),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::COMMAND,
            }]);
            w.type_text(text);
            w.frame(vec![]);
        }

        #[test]
        fn pis_providers_are_set_up_in_a_dialog_that_says_whether_the_server_answers() {
            let fake = FakeAgents::new(vec![
                row(ToolKind::Claude, "Claude Code", true),
                row(ToolKind::Pi, "Pi", true),
            ]);
            // Nothing answers and there is no key: Pi is to be set up.
            fake.view.lock().unwrap().pi.lmstudio = connecto_core::PiLocalState::NotAnswering;
            let mut w = window(&fake);
            w.height = 2400.0;
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(texts(&w).contains("No model provider that works yet"));
            assert!(!texts(&w).contains("PI MODEL PROVIDERS"));
            assert!(
                !w.app.hits.settings.rects.contains_key("agent-activate:pi"),
                "nothing to activate before a provider works"
            );
            click(&mut w, "pi-setup-open");
            assert!(texts(&w).contains("PI MODEL PROVIDERS"));
            assert!(texts(&w).contains("Nothing answers at http://localhost:1234"));
            assert!(texts(&w).contains("Pi is not offered for new terminals until"));

            // The local server's address: typed over the default one, saved,
            // and what the check found is said under it.
            replace_text(&mut w, "pi-lmstudio-url", "http://localhost:1235");
            click(&mut w, "pi-lmstudio-save");
            assert_eq!(fake.calls(), ["pi_lmstudio http://localhost:1235"]);
            fake.view.lock().unwrap().pi.lmstudio =
                connecto_core::PiLocalState::Working { models: 2 };
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(texts(&w).contains(
                "Working: the server at http://localhost:1235 answers with 2 chat models."
            ));
            assert!(!texts(&w).contains("Pi is not offered for new terminals until"));
            click(&mut w, "pi-lmstudio-check");
            assert_eq!(fake.calls()[1], "refresh");

            // A key: saved, never shown, and removable.
            replace_text(&mut w, "pi-key-openrouter", "sk-or-test");
            click(&mut w, "pi-save-openrouter");
            assert_eq!(fake.calls()[2], "pi_key openrouter sk-or-test");
            w.frame(vec![]);
            assert!(texts(&w).contains("Key saved"));
            assert!(!texts(&w).contains("sk-or-test"));
            click(&mut w, "pi-remove-openrouter");
            assert_eq!(fake.calls()[3], "pi_key openrouter <removed>");

            // A refused address is said in the dialog.
            replace_text(&mut w, "pi-lmstudio-url", "nonsense");
            click(&mut w, "pi-lmstudio-save");
            w.frame(vec![]);
            assert!(texts(&w).contains("That is not a server address."));

            // The model for new terminals: found by typing, among Pi's.
            assert!(texts(&w).contains("Pi lists no model yet"));
            fake.view.lock().unwrap().pi.models = vec![
                "lmstudio/google/gemma-4-e2b".to_owned(),
                "openrouter/openai/gpt-5".to_owned(),
            ];
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(texts(&w).contains("Pi chooses."));
            replace_text(&mut w, "pi-model-filter", "gemma");
            assert!(texts(&w).contains("1 of 2 models shown."));
            click(&mut w, "pi-model:lmstudio/google/gemma-4-e2b");
            assert_eq!(
                fake.calls().last().unwrap(),
                "pi_model lmstudio/google/gemma-4-e2b"
            );
            click(&mut w, "pi-model-clear");
            assert_eq!(fake.calls().last().unwrap(), "pi_model <pi's choice>");

            // Done closes the dialog, not Settings; the row says what works.
            click(&mut w, "pi-setup-done");
            w.frame(vec![]);
            assert!(!texts(&w).contains("PI MODEL PROVIDERS"));
            assert!(w.app.settings_open());
            assert!(texts(&w).contains("Providers: LM Studio (2 local)"));
        }

        /// Claude's providers: a key saved while TogetherLink is missing
        /// brings the offer to install it; the key is never shown.
        #[cfg(not(windows))]
        #[test]
        fn claudes_providers_offer_togetherlink_once_a_key_is_saved() {
            let fake = FakeAgents::new(vec![
                row(ToolKind::Claude, "Claude Code", true),
                row(ToolKind::Pi, "Pi", true),
            ]);
            fake.view.lock().unwrap().claude.togetherlink =
                connecto_core::TogetherLinkState::Missing;
            let mut w = window(&fake);
            w.height = 2400.0;
            w.frame(vec![]);
            click(&mut w, "claude-setup-open");
            assert!(texts(&w).contains("CLAUDE MODEL PROVIDERS"));
            assert!(texts(&w).contains("Not installed. It is needed once a key is saved."));
            assert!(!texts(&w).contains("Install TogetherLink now?"));
            replace_text(&mut w, "claude-together-key", "tgp_v1_typed");
            click(&mut w, "claude-together-save");
            assert_eq!(fake.calls(), ["claude_key Global tgp_v1_typed"]);
            w.frame(vec![]);
            assert!(texts(&w).contains("Key saved"));
            assert!(!texts(&w).contains("tgp_v1_typed"));
            assert!(texts(&w).contains("Install TogetherLink now?"));
            assert!(texts(&w).contains("cannot start with this key"));
            // Not now, then Install: the progress is said.
            click(&mut w, "togetherlink-decline");
            assert_eq!(fake.calls()[1], "decline_togetherlink");
            w.frame(vec![]);
            assert!(!texts(&w).contains("Install TogetherLink now?"));
            click(&mut w, "togetherlink-install");
            assert_eq!(fake.calls()[2], "install_togetherlink");
            w.frame(vec![]);
            assert!(texts(&w).contains("Installing TogetherLink…"));
            // Done: the card says what Claude reaches.
            fake.view.lock().unwrap().claude.togetherlink =
                connecto_core::TogetherLinkState::Installed;
            click(&mut w, "claude-setup-done");
            w.frame(vec![]);
            assert!(!texts(&w).contains("CLAUDE MODEL PROVIDERS"));
            assert!(texts(&w).contains("Model provider: Together AI, through TogetherLink"));
        }

        /// TogetherLink does not run on Windows: Claude has no providers to set.
        #[cfg(windows)]
        #[test]
        fn claudes_providers_are_not_offered_on_windows() {
            let fake = FakeAgents::new(vec![row(ToolKind::Claude, "Claude Code", true)]);
            fake.view.lock().unwrap().claude.together_key = true;
            let w = window(&fake);
            assert!(!w.app.hits.settings.rects.contains_key("claude-setup-open"));
            assert!(!texts(&w).contains("Together AI"));
        }

        fn cli_window(fake: &Arc<FakeAgents>) -> (Proj, Arc<FakeWorkspace>, W) {
            cli_window_with(fake, Diagnostics::default())
        }

        fn cli_window_with(
            fake: &Arc<FakeAgents>,
            more: Diagnostics,
        ) -> (Proj, Arc<FakeWorkspace>, W) {
            let (quill, _connecto, ws) = two_projects();
            ws.activate("quill");
            let mut w = W::with_diagnostics(
                &ws,
                Diagnostics {
                    workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                    agents: Arc::clone(fake) as Arc<dyn AgentsControl>,
                    ..more
                },
            );
            w.height = 2400.0;
            w.app.queue_for_tests(crate::commands::Command::SettingsAt(
                crate::settings::SettingsTab::Cli,
            ));
            w.frame(vec![]);
            w.frame(vec![]);
            (quill, ws, w)
        }

        /// Without Claude Code installed, CLI improvements offers no Claude
        /// item at all, and says what it needs.
        #[test]
        fn claude_insights_is_not_offered_without_claude() {
            let fake = FakeAgents::new(vec![
                row(ToolKind::Claude, "Claude Code", false),
                row(ToolKind::Codex, "Codex", true),
            ]);
            let (quill, _ws, w) = cli_window(&fake);
            let hits = &w.app.hits.settings;
            assert!(hits.rects.contains_key("tab-cli"), "the section is there");
            assert!(!hits.rects.contains_key("claude-insights"));
            assert!(
                !hits
                    .texts
                    .iter()
                    .any(|t| t.contains("Claude auto apply insights")),
                "{:?}",
                hits.texts
            );
            assert!(hits.texts.iter().any(|t| t.contains("Install Claude Code")));
            assert!(quill.terminals.prompted.lock().unwrap().is_empty());
        }

        /// With Claude Code installed, the item opens a Claude terminal on
        /// Opus in the active project, with the insights task as its first
        /// message, shows it, and closes Settings.
        #[test]
        fn claude_insights_opens_a_claude_terminal_on_opus_with_the_task() {
            use connecto_core::cli_improvements::{
                INSIGHTS_MODEL, INSIGHTS_TERMINAL, insights_prompt,
            };
            let fake = FakeAgents::new(vec![row(ToolKind::Claude, "Claude Code", true)]);
            let (quill, ws, mut w) = cli_window(&fake);
            let texts = &w.app.hits.settings.texts;
            assert!(
                texts.iter().any(|t| t == "The terminal opens in quill."),
                "{texts:?}"
            );
            let at = w.app.hits.settings.rects["claude-insights"].center();
            w.click(at);
            w.frame(vec![]);
            wait_for_start(&quill, &mut w);
            assert_eq!(
                *quill.terminals.prompted.lock().unwrap(),
                [(
                    ToolKind::Claude,
                    INSIGHTS_TERMINAL.to_owned(),
                    insights_prompt(),
                    Some(INSIGHTS_MODEL.to_owned()),
                )]
            );
            let view = ws.view();
            let project = view.projects.iter().find(|p| p.id == "quill").unwrap();
            assert_eq!(
                project.active_terminal.as_deref(),
                Some("prompted-1"),
                "shown"
            );
            assert!(!w.app.settings_open(), "Settings closed on it");
        }

        /// Render until Settings closes (the start is in), or a deadline.
        fn wait_for_start(quill: &Proj, w: &mut W) {
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            while w.app.settings_open() {
                assert!(
                    std::time::Instant::now() < deadline,
                    "never started: {:?}",
                    quill.terminals.prompted.lock().unwrap().len()
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
                w.frame(vec![]);
            }
            w.frame(vec![]);
        }

        /// Which agents are off is read for CLI improvements only while
        /// its section is on show.
        #[test]
        fn the_insights_target_is_read_only_with_its_section_on_show() {
            use std::sync::atomic::Ordering;
            let fake = FakeAgents::new(vec![row(ToolKind::Claude, "Claude Code", true)]);
            let (_quill, _connecto, ws) = two_projects();
            ws.activate("quill");
            let mut w = W::with_diagnostics(
                &ws,
                Diagnostics {
                    workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                    agents: Arc::clone(&fake) as Arc<dyn AgentsControl>,
                    ..Diagnostics::default()
                },
            );
            for _ in 0..5 {
                w.frame(vec![]);
            }
            let closed = fake.off_reads.load(Ordering::SeqCst);
            w.app.queue_for_tests(crate::commands::Command::SettingsAt(
                crate::settings::SettingsTab::Launch,
            ));
            for _ in 0..5 {
                w.frame(vec![]);
            }
            let other_tab = fake.off_reads.load(Ordering::SeqCst);
            w.app.queue_for_tests(crate::commands::Command::SettingsAt(
                crate::settings::SettingsTab::Cli,
            ));
            for _ in 0..5 {
                w.frame(vec![]);
            }
            let shown = fake.off_reads.load(Ordering::SeqCst);
            assert_eq!(closed, 0, "not with Settings closed");
            assert_eq!(other_tab, closed, "nor on another tab");
            assert!(shown > other_tab, "only with its section on show");
        }

        fn cli_texts(w: &W) -> Vec<String> {
            w.app.hits.settings.texts.clone()
        }

        /// Click the item; give a start that would come time to come.
        fn click_insights(w: &mut W) {
            let at = w.app.hits.settings.rects["claude-insights"].center();
            w.click(at);
            w.frame(vec![]);
            std::thread::sleep(std::time::Duration::from_millis(100));
            w.frame(vec![]);
        }

        /// With another terminal on show, the new one is what the window
        /// shows once it starts.
        #[test]
        fn the_insights_terminal_is_shown_over_the_one_selected() {
            let fake = FakeAgents::new(vec![row(ToolKind::Claude, "Claude Code", true)]);
            let (quill, _connecto, ws) = two_projects();
            let mut w = W::with_diagnostics(
                &ws,
                Diagnostics {
                    workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                    agents: Arc::clone(&fake) as Arc<dyn AgentsControl>,
                    ..Diagnostics::default()
                },
            );
            w.height = 2400.0;
            w.frame(vec![]);
            w.click_hit(&Hit::Terminal("quill".into(), "codex-1".into()));
            w.frame(vec![]);
            assert_eq!(
                w.app.shown_for_tests(),
                Some(("quill".to_owned(), "codex-1".to_owned()))
            );
            w.app.queue_for_tests(crate::commands::Command::SettingsAt(
                crate::settings::SettingsTab::Cli,
            ));
            w.frame(vec![]);
            w.frame(vec![]);
            let at = w.app.hits.settings.rects["claude-insights"].center();
            w.click(at);
            w.frame(vec![]);
            wait_for_start(&quill, &mut w);
            assert_eq!(
                w.app.shown_for_tests(),
                Some(("quill".to_owned(), "prompted-1".to_owned()))
            );
        }

        /// A paired machine's project on show: nothing starts, never in this
        /// machine's last project instead, and the section says why.
        #[test]
        fn insights_is_refused_while_a_paired_machines_project_is_shown() {
            let fake = FakeAgents::new(vec![row(ToolKind::Claude, "Claude Code", true)]);
            let remote = FakeRemote::new();
            let (quill, _ws, mut w) = cli_window_with(
                &fake,
                Diagnostics {
                    remote: Arc::clone(&remote) as Arc<dyn RemoteControl>,
                    ..Diagnostics::default()
                },
            );
            w.frame(esc());
            w.frame(vec![]);
            click_row(&mut w, "build box");
            w.app.queue_for_tests(crate::commands::Command::SettingsAt(
                crate::settings::SettingsTab::Cli,
            ));
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(
                cli_texts(&w)
                    .iter()
                    .any(|t| t.contains("A paired machine's projects are shown")),
                "{:?}",
                cli_texts(&w)
            );
            click_insights(&mut w);
            assert!(quill.terminals.prompted.lock().unwrap().is_empty());
        }

        /// Claude deactivated, or turned off in the project: nothing starts,
        /// and the section says why.
        #[test]
        fn insights_is_refused_where_claude_may_not_start() {
            struct NoClaudeHere;
            impl connecto_core::PackagesControl for NoClaudeHere {
                fn view(&self) -> Arc<connecto_core::PackagesView> {
                    Arc::default()
                }
                fn allowed_tools(&self, _project: &str) -> Vec<ToolKind> {
                    vec![ToolKind::Codex, ToolKind::Shell]
                }
            }
            // Deactivated in Settings > Agents.
            let fake = FakeAgents::new(vec![row(ToolKind::Claude, "Claude Code", true)]);
            fake.off.lock().unwrap().push(ToolKind::Claude);
            let (quill, _ws, mut w) = cli_window(&fake);
            assert!(
                cli_texts(&w)
                    .iter()
                    .any(|t| t.contains("Claude Code is deactivated")),
                "{:?}",
                cli_texts(&w)
            );
            click_insights(&mut w);
            assert!(quill.terminals.prompted.lock().unwrap().is_empty());

            // Turned off in the project's settings.
            let fake = FakeAgents::new(vec![row(ToolKind::Claude, "Claude Code", true)]);
            let (quill, _ws, mut w) = cli_window_with(
                &fake,
                Diagnostics {
                    packages: Arc::new(NoClaudeHere),
                    ..Diagnostics::default()
                },
            );
            assert!(
                cli_texts(&w)
                    .iter()
                    .any(|t| t.contains("Claude Code is turned off in the settings of quill")),
                "{:?}",
                cli_texts(&w)
            );
            click_insights(&mut w);
            assert!(quill.terminals.prompted.lock().unwrap().is_empty());
        }

        /// A refused start is said in the section, which stays open.
        #[test]
        fn a_refused_claude_insights_start_is_said() {
            let fake = FakeAgents::new(vec![row(ToolKind::Claude, "Claude Code", true)]);
            let (quill, _ws, mut w) = cli_window(&fake);
            *quill.terminals.refuse.lock().unwrap() = Some("the terminal list is full".into());
            let at = w.app.hits.settings.rects["claude-insights"].center();
            w.click(at);
            let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
            loop {
                w.frame(vec![]);
                if w.app
                    .hits
                    .settings
                    .texts
                    .iter()
                    .any(|t| t.contains("could not be started: the terminal list is full"))
                {
                    break;
                }
                assert!(
                    std::time::Instant::now() < deadline,
                    "{:?}",
                    w.app.hits.settings.texts
                );
                std::thread::sleep(std::time::Duration::from_millis(10));
            }
            assert!(w.app.settings_open());
        }

        fn row(tool: ToolKind, name: &str, installed: bool) -> AgentRow {
            AgentRow {
                tool,
                name: name.into(),
                source: "the vendor's installer at example.com".into(),
                installed,
                version: installed.then(|| "1.2.3 (Test)".to_owned()),
                signed_in: installed.then_some(true),
                active: true,
                busy: None,
                error: None,
                signin_requested: false,
                node_prompt: None,
            }
        }

        fn window(fake: &Arc<FakeAgents>) -> W {
            let (_a, _b, ws) = two_projects();
            let mut w = W::with_diagnostics(
                &ws,
                Diagnostics {
                    workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                    agents: Arc::clone(fake) as Arc<dyn AgentsControl>,
                    ..Diagnostics::default()
                },
            );
            w.app.queue_for_tests(crate::commands::Command::SettingsAt(
                crate::settings::SettingsTab::Agents,
            ));
            w.frame(vec![]);
            w.frame(vec![]);
            w
        }

        fn texts(w: &W) -> String {
            w.app.hits.settings.texts.join("\n")
        }

        fn click(w: &mut W, key: &str) {
            let at = w
                .app
                .hits
                .settings
                .rects
                .get(key)
                .unwrap_or_else(|| {
                    panic!("{key} not drawn: {:?}", w.app.hits.settings.rects.keys())
                })
                .center();
            w.click(at);
            w.frame(vec![]);
        }

        #[test]
        fn every_agent_is_listed_with_its_state_and_a_missing_one_can_be_installed() {
            let fake = FakeAgents::new(vec![
                row(ToolKind::Claude, "Claude Code", true),
                row(ToolKind::Codex, "Codex", false),
            ]);
            let mut w = window(&fake);
            let t = texts(&w);
            assert!(t.contains("AGENTS"), "{t}");
            assert!(
                t.contains("Claude Code") && t.contains("Installed: 1.2.3 (Test)"),
                "{t}"
            );
            assert!(
                t.contains(&format!("Signed in on {}", crate::platform::THIS_COMPUTER)),
                "{t}"
            );
            assert!(t.contains("Codex") && t.contains("Not installed"), "{t}");
            assert!(
                t.contains("Installs from the vendor's installer at example.com."),
                "{t}"
            );
            assert!(
                w.app
                    .hits
                    .settings
                    .rects
                    .contains_key("agent-deactivate:claude")
            );
            click(&mut w, "agent-activate:codex");
            assert_eq!(fake.calls(), ["activate codex"]);
        }

        /// Pi waiting on Node.js: the question is shown with its two answers,
        /// in place of Install and activate.
        #[test]
        fn pi_asks_whether_to_install_nodejs_and_passes_the_answer_on() {
            let mut pi = row(ToolKind::Pi, "Pi", false);
            pi.node_prompt =
                Some("Pi runs on Node.js 22.19 or newer. Install it and go on with Pi?".into());
            let fake = FakeAgents::new(vec![pi]);
            let mut w = window(&fake);
            w.height = 2400.0;
            w.frame(vec![]);
            assert!(texts(&w).contains("Install it and go on with Pi?"));
            assert!(!w.app.hits.settings.rects.contains_key("agent-activate:pi"));
            click(&mut w, "agent-node-decline:pi");
            click(&mut w, "agent-node-install:pi");
            assert_eq!(fake.calls(), ["decline_node pi", "install_node pi"]);
        }

        #[test]
        fn deactivate_hides_the_agent_from_the_new_terminal_choices_and_a_shortcut_says_why() {
            let fake = FakeAgents::new(vec![
                row(ToolKind::Claude, "Claude Code", true),
                row(ToolKind::Codex, "Codex", true),
            ]);
            let mut w = window(&fake);
            click(&mut w, "agent-deactivate:claude");
            assert_eq!(fake.calls(), ["deactivate claude"]);
            // Close Settings, open the chooser: Claude is not offered (Codex,
            // Pi, Antigravity, OpenCode and the shell are).
            w.app.queue_for_tests(crate::commands::Command::Settings);
            w.frame(vec![]);
            w.click_hit(&Hit::NewRow("quill".into()));
            w.frame(vec![]);
            assert_eq!(w.app.hits.chooser.choices.len(), 5);
            w.frame(vec![Event::Key {
                key: egui::Key::Escape,
                physical_key: Some(egui::Key::Escape),
                pressed: true,
                repeat: false,
                modifiers: egui::Modifiers::NONE,
            }]);
            w.app
                .queue_for_tests(crate::commands::Command::NewTool(ToolKind::Claude));
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(
                w.app
                    .request_error_for_tests()
                    .is_some_and(|text| text.contains("Claude is deactivated")
                        && text.contains("Settings > Agents")),
                "{:?}",
                w.app.request_error_for_tests()
            );
        }

        #[test]
        fn progress_and_failure_are_shown_in_plain_words_and_the_button_waits() {
            let mut busy = row(ToolKind::Codex, "Codex", false);
            busy.busy = Some(AgentBusy::Installing(
                "Downloading the installer from chatgpt.com…".into(),
            ));
            let mut failed = row(ToolKind::Claude, "Claude Code", false);
            failed.error = Some("Connecto could not reach claude.ai. Is this Mac online?".into());
            let fake = FakeAgents::new(vec![failed, busy]);
            let w = window(&fake);
            let t = texts(&w);
            assert!(
                t.contains("Downloading the installer from chatgpt.com…"),
                "{t}"
            );
            assert!(t.contains("Is this Mac online?"), "{t}");
            assert!(
                w.app
                    .hits
                    .settings
                    .rects
                    .contains_key("agent-activate:claude")
            );
        }

        #[test]
        fn sign_in_closes_settings_so_the_login_terminal_is_seen() {
            let mut agy = row(ToolKind::Antigravity, "Antigravity CLI", true);
            agy.signed_in = None;
            let fake = FakeAgents::new(vec![agy]);
            let automations = Arc::new(crate::tests::automations_sheet::FakeAutomations::default());
            let (_a, _b, ws) = two_projects();
            let mut w = W::with_diagnostics(
                &ws,
                Diagnostics {
                    workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                    agents: Arc::clone(&fake) as Arc<dyn AgentsControl>,
                    automations: Some(
                        Arc::clone(&automations) as Arc<dyn connecto_core::AutomationsControl>
                    ),
                    ..Diagnostics::default()
                },
            );
            w.app.queue_for_tests(crate::commands::Command::SettingsAt(
                crate::settings::SettingsTab::Agents,
            ));
            w.frame(vec![]);
            w.frame(vec![]);
            click(&mut w, "agent-signin:antigravity");
            assert_eq!(automations.calls(), ["open_login antigravity"]);
            assert!(!w.app.settings_open());
        }

        #[test]
        fn a_finished_install_takes_the_person_to_sign_in_once() {
            let mut fresh = row(ToolKind::Claude, "Claude Code", true);
            fresh.signed_in = Some(false);
            fresh.signin_requested = true;
            let fake = FakeAgents::new(vec![fresh]);
            let mut w = window(&fake);
            // No automations folder to open the sign-in in: said in Settings, and not asked again.
            assert!(
                fake.calls()
                    .iter()
                    .filter(|c| c.starts_with("signin_started"))
                    .count()
                    == 1,
                "{:?}",
                fake.calls()
            );
            assert!(texts(&w).contains("Not signed in"));
            assert!(
                texts(&w).contains("Sign-in is not available here."),
                "{}",
                texts(&w)
            );
            w.frame(vec![]);
            assert_eq!(
                fake.calls()
                    .iter()
                    .filter(|c| c.starts_with("signin_started"))
                    .count(),
                1
            );
            assert!(
                w.app
                    .hits
                    .settings
                    .rects
                    .contains_key("agent-signin:claude")
            );
        }
    }
}

// ---------- the Settings sections, and commands over an open sheet ----------

#[allow(clippy::panic)]
mod settings_sections {
    use super::*;
    use crate::commands::Command;
    use crate::settings::SettingsTab;
    use connecto_core::ToolKind;

    fn open_settings(w: &mut W) {
        w.app.queue_for_tests(Command::Settings);
        w.frame(vec![]);
        w.frame(vec![]);
    }

    fn shows(w: &W, text: &str) -> bool {
        w.app.hits.settings.texts.iter().any(|t| t.contains(text))
    }

    fn tab(w: &mut W, key: &str) {
        let at = w
            .app
            .hits
            .settings
            .rects
            .get(key)
            .unwrap_or_else(|| panic!("{key} not drawn"))
            .center();
        w.click(at);
        w.frame(vec![]);
    }

    #[test]
    fn settings_shows_one_section_under_four_tabs() {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        open_settings(&mut w);
        assert!(shows(&w, "CONNECTO HOME") && !shows(&w, "REMOTE ACCESS"));
        assert!(!shows(&w, "UTILITY MODEL"), "that is under Agents");
        tab(&mut w, "tab-agents");
        assert_eq!(w.app.settings_tab_for_tests(), SettingsTab::Agents);
        assert!(shows(&w, "UTILITY MODEL"));
        assert!(w.app.hits.settings.rects.contains_key("open-utility"));
        assert!(!shows(&w, "CONNECTO HOME"));
        tab(&mut w, "tab-launch");
        assert!(shows(&w, "HOW CODING CLIS ARE LAUNCHED"));
        assert!(!shows(&w, "AGENTS"));
        tab(&mut w, "tab-remote");
        assert!(shows(&w, "REMOTE ACCESS"));
        assert!(!shows(&w, "HOW CODING CLIS ARE LAUNCHED"));
    }

    #[test]
    fn settings_opens_on_the_section_it_was_left_on() {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        open_settings(&mut w);
        tab(&mut w, "tab-launch");
        w.frame(esc());
        w.frame(vec![]);
        assert!(!w.app.settings_open());
        open_settings(&mut w);
        assert_eq!(w.app.settings_tab_for_tests(), SettingsTab::Launch);
        assert!(shows(&w, "HOW CODING CLIS ARE LAUNCHED"));
    }

    #[test]
    fn a_deep_link_opens_its_section_whether_or_not_settings_is_open() {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        w.app
            .queue_for_tests(Command::SettingsAt(SettingsTab::Agents));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.settings_open());
        assert_eq!(w.app.settings_tab_for_tests(), SettingsTab::Agents);
        assert!(shows(&w, "UTILITY MODEL"));
        tab(&mut w, "tab-remote");
        w.app
            .queue_for_tests(Command::SettingsAt(SettingsTab::Launch));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.settings_open());
        assert_eq!(w.app.settings_tab_for_tests(), SettingsTab::Launch);
        assert!(shows(&w, "HOW CODING CLIS ARE LAUNCHED"));
    }

    #[test]
    fn the_tab_row_stays_inside_a_narrow_window() {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        w.width = 640.0;
        open_settings(&mut w);
        let main = w.app.hits.main;
        for key in ["tab-home", "tab-agents", "tab-launch", "tab-remote"] {
            let rect = *w
                .app
                .hits
                .settings
                .rects
                .get(key)
                .unwrap_or_else(|| panic!("{key} not drawn"));
            assert!(
                rect.min.x >= main.min.x && rect.max.x <= main.max.x,
                "{key} {rect:?} outside {main:?}"
            );
        }
    }

    #[test]
    fn the_utility_link_opens_utility_sessions_in_place_of_settings() {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        open_settings(&mut w);
        tab(&mut w, "tab-agents");
        let at = w.app.hits.settings.rects["open-utility"].center();
        w.click(at);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.utility_open());
        assert!(!w.app.settings_open());
    }

    #[test]
    fn a_new_terminal_from_the_sidebar_closes_settings_and_opens_the_picker() {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        open_settings(&mut w);
        w.click_hit(&Hit::NewRow("quill".into()));
        w.frame(vec![]);
        assert!(w.app.chooser_open());
        assert!(!w.app.settings_open());
    }

    #[test]
    fn a_menu_new_shell_runs_over_an_open_sheet() {
        let (quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        open_settings(&mut w);
        w.app.queue_for_tests(Command::NewTool(ToolKind::Shell));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.settings_open());
        assert_eq!(
            *quill.terminals.adds.lock().unwrap(),
            [(ToolKind::Shell, None)]
        );
    }

    #[test]
    fn terminal_and_project_commands_run_over_an_open_sheet_and_close_it() {
        let commands = [
            Command::NewTerminal,
            Command::NewTerminalIn("quill".into()),
            Command::NewTool(ToolKind::Claude),
            Command::GotoProject(1),
            Command::NextTerminal,
            Command::PreviousTerminal,
            Command::NextNeedingMe,
            Command::RestartTerminal,
            Command::CloseTerminal,
            Command::CloseFinishedChildren,
            Command::RenameTerminal,
            Command::RenameProject,
        ];
        for command in commands {
            let (_quill, _connecto, ws) = two_projects();
            let mut w = W::new(&ws);
            open_settings(&mut w);
            let name = format!("{command:?}");
            w.app.queue_for_tests(command);
            w.frame(vec![]);
            w.frame(vec![]);
            assert!(!w.app.settings_open(), "{name} left Settings open");
        }
    }

    #[test]
    fn a_name_being_typed_refuses_a_terminal_command_and_says_why() {
        let (quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        rename_start(&mut w);
        w.app.queue_for_tests(Command::NewTerminal);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.chooser_open());
        assert_eq!(
            w.app.request_error_for_tests().as_deref(),
            Some("Finish the open dialog or field first.")
        );
        assert!(quill.terminals.adds.lock().unwrap().is_empty());
    }
}

// ---------- updates ----------

#[allow(clippy::panic)]
mod updates {
    use super::*;
    use crate::commands::Command;
    use connecto_core::{UpdateControl, UpdateStatus, UpdateView};

    /// An updater with a release ready, that records what it is asked.
    #[derive(Default)]
    struct FakeUpdates {
        status: Mutex<UpdateStatus>,
        installs: AtomicUsize,
        checks: AtomicUsize,
    }

    impl UpdateControl for FakeUpdates {
        fn view(&self) -> Arc<UpdateView> {
            Arc::new(UpdateView {
                current: "0.1.8".into(),
                status: self.status.lock().unwrap().clone(),
                auto_check: true,
                asked: false,
                notice: None,
                ..UpdateView::default()
            })
        }
        fn check_now(&self) {
            self.checks.fetch_add(1, Ordering::SeqCst);
        }
        fn set_auto_check(&self, _on: bool) {}
        fn install_and_restart(&self) -> Result<(), String> {
            self.installs.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn window(fake: &Arc<FakeUpdates>) -> (Proj, W) {
        let (orbit, _connecto, ws) = two_projects();
        let w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                updates: Arc::clone(fake) as Arc<dyn UpdateControl>,
                ..Diagnostics::default()
            },
        );
        (orbit, w)
    }

    fn strip_center(w: &W, key: &str) -> Pos2 {
        w.app
            .hits
            .update_strip
            .rects
            .get(key)
            .unwrap_or_else(|| {
                panic!(
                    "{key} not drawn: {:?}",
                    w.app.hits.update_strip.rects.keys()
                )
            })
            .center()
    }

    #[test]
    fn a_ready_update_restarts_at_once_when_no_agent_is_in_a_turn() {
        let fake = Arc::new(FakeUpdates::default());
        *fake.status.lock().unwrap() = UpdateStatus::Ready {
            version: "0.1.9".into(),
        };
        let (_orbit, mut w) = window(&fake);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            w.app
                .hits
                .update_strip
                .texts
                .iter()
                .any(|t| t.contains("0.1.9")),
            "{:?}",
            w.app.hits.update_strip.texts
        );
        let at = strip_center(&w, "update-install");
        w.click(at);
        w.frame(vec![]);
        assert_eq!(fake.installs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn a_ready_update_waits_for_running_turns_then_restarts() {
        let fake = Arc::new(FakeUpdates::default());
        *fake.status.lock().unwrap() = UpdateStatus::Ready {
            version: "0.1.9".into(),
        };
        let (mut orbit, mut w) = window(&fake);
        orbit.set(vec![spec("claude", Claude).state(TermActivity::Working)]);
        w.frame(vec![]);
        w.frame(vec![]);
        let at = strip_center(&w, "update-install");
        w.click(at);
        w.frame(vec![]);
        assert_eq!(fake.installs.load(Ordering::SeqCst), 0, "a turn is running");
        assert!(
            w.app
                .hits
                .update_strip
                .texts
                .iter()
                .any(|t| t.contains("1 agent turn")),
            "{:?}",
            w.app.hits.update_strip.texts
        );
        orbit.set(vec![spec("claude", Claude)]);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(
            fake.installs.load(Ordering::SeqCst),
            1,
            "restarted once the turn ended"
        );
    }

    #[test]
    fn background_work_at_an_idle_prompt_does_not_hold_the_restart() {
        let fake = Arc::new(FakeUpdates::default());
        *fake.status.lock().unwrap() = UpdateStatus::Ready {
            version: "0.1.9".into(),
        };
        let (mut orbit, mut w) = window(&fake);
        // The dot shows Working, but the agent is at its prompt with a
        // background subagent or shell: no turn is running.
        orbit.set(vec![spec("claude", Claude).background()]);
        w.frame(vec![]);
        w.frame(vec![]);
        let at = strip_center(&w, "update-install");
        w.click(at);
        w.frame(vec![]);
        assert_eq!(fake.installs.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn the_strip_sits_right_of_the_sidebar_clear_of_the_window_buttons() {
        let fake = Arc::new(FakeUpdates::default());
        *fake.status.lock().unwrap() = UpdateStatus::Ready {
            version: "0.1.9".into(),
        };
        let (_orbit, mut w) = window(&fake);
        w.frame(vec![]);
        w.frame(vec![]);
        let sidebar = w.app.hits.sidebar;
        for key in ["update-install", "update-later"] {
            let rect = w.app.hits.update_strip.rects[key];
            assert!(
                rect.min.x >= sidebar.max.x,
                "{key} at {rect:?}, sidebar {sidebar:?}"
            );
        }
        // And the terminal area starts below it.
        assert!(w.app.hits.main.min.y >= w.app.hits.update_strip.rects["update-install"].max.y);
    }

    #[test]
    fn later_hides_the_offer_and_check_for_updates_asks_the_updater() {
        let fake = Arc::new(FakeUpdates::default());
        *fake.status.lock().unwrap() = UpdateStatus::Ready {
            version: "0.1.9".into(),
        };
        let (_orbit, mut w) = window(&fake);
        w.frame(vec![]);
        w.frame(vec![]);
        let at = strip_center(&w, "update-later");
        w.click(at);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.hits.update_strip.rects.contains_key("update-install"));
        w.app.queue_for_tests(Command::CheckForUpdates);
        w.frame(vec![]);
        assert_eq!(fake.checks.load(Ordering::SeqCst), 1);
        assert_eq!(fake.installs.load(Ordering::SeqCst), 0);
    }
}

// ---------- project groups in the sidebar ----------

mod project_groups {
    use super::*;

    fn group_of(ws: &FakeWorkspace, project: &str) -> Option<String> {
        ws.view()
            .projects
            .iter()
            .find(|p| p.id == project)
            .and_then(|p| p.group.clone())
    }

    /// Two projects and a group "Clients" holding none of them yet.
    fn grouped() -> (Arc<FakeWorkspace>, W) {
        let (_quill, _connecto, ws) = two_projects();
        ws.create_group("Clients").unwrap();
        let mut w = W::new(&ws);
        w.frame(vec![]);
        (ws, w)
    }

    #[test]
    fn the_plus_menu_names_a_new_group_in_the_sidebar() {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        assert!(!w.has(&Hit::GroupFilter), "no filter without groups");
        w.click_hit(&Hit::HeaderPlus);
        w.click_hit(&Hit::PlusNewGroup);
        w.frame(vec![]);
        w.type_text("Clients");
        w.key(Key::Enter, Modifiers::NONE);
        w.frame(vec![]);
        let view = ws.view();
        assert_eq!(view.groups.len(), 1);
        assert_eq!(view.groups[0].name, "Clients");
        assert!(ws.opened.lock().unwrap().is_empty(), "no folder asked for");
        w.frame(vec![]);
        assert!(w.has(&Hit::Group("group-1".into())));
        assert!(w.has(&Hit::GroupFilter));
    }

    /// Every text the last pass painted.
    fn painted(w: &mut W) -> Vec<String> {
        fn collect(shape: &egui::Shape, texts: &mut Vec<String>) {
            match shape {
                egui::Shape::Text(text) => texts.push(text.galley.text().to_owned()),
                egui::Shape::Vec(shapes) => shapes.iter().for_each(|s| collect(s, texts)),
                _ => {}
            }
        }
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(
                Pos2::ZERO,
                Vec2::new(w.width, w.height),
            )),
            ..RawInput::default()
        };
        let app = &mut w.app;
        let output = w.ctx.run_ui(input, |ui| app.draw(ui));
        let mut texts = Vec::new();
        for clipped in &output.shapes {
            collect(&clipped.shape, &mut texts);
        }
        texts
    }

    #[test]
    fn projects_in_no_group_are_listed_without_a_header() {
        let (ws, mut w) = grouped();
        ws.set_project_group("connecto", Some("group-1")).unwrap();
        w.frame(vec![]);
        assert!(w.has(&Hit::Group("group-1".into())));
        assert!(w.has(&Hit::Project("quill".into())));
        assert!(!w.has(&Hit::Ungrouped));
        let texts = painted(&mut w);
        assert!(texts.iter().any(|t| t == "CLIENTS"), "{texts:?}");
        assert!(
            !texts.iter().any(|t| t.eq_ignore_ascii_case("no group")),
            "{texts:?}"
        );
        // Not even when no project is in a group.
        ws.set_project_group("connecto", None).unwrap();
        w.frame(vec![]);
        assert!(w.has(&Hit::Project("quill".into())) && w.has(&Hit::Project("connecto".into())));
        assert!(!w.has(&Hit::Ungrouped));
        let texts = painted(&mut w);
        assert!(
            !texts.iter().any(|t| t.eq_ignore_ascii_case("no group")),
            "{texts:?}"
        );
    }

    #[test]
    fn a_project_dragged_onto_a_group_joins_it_and_back_out() {
        let (ws, mut w) = grouped();
        // Neither is in a group: no header above them.
        assert!(!w.has(&Hit::Ungrouped));
        let from = w.rect(&Hit::Project("connecto".into())).center();
        let to = w.rect(&Hit::Group("group-1".into())).center();
        drag(&mut w, from, to);
        w.frame(vec![]);
        assert_eq!(group_of(&ws, "connecto").as_deref(), Some("group-1"));
        // Dropped on a project of the group: joins the same group.
        let from = w.rect(&Hit::Project("quill".into())).center();
        let to = w.rect(&Hit::Project("connecto".into())).center();
        drag(&mut w, from, to);
        w.frame(vec![]);
        assert_eq!(group_of(&ws, "quill").as_deref(), Some("group-1"));
        // While one in a group is dragged, a drop zone takes it out.
        let from = w.rect(&Hit::Project("quill".into())).center();
        w.frame(vec![Event::PointerMoved(from)]);
        w.frame(vec![button(from, true)]);
        w.frame(vec![Event::PointerMoved(from + Vec2::new(0.0, 30.0))]);
        w.frame(vec![Event::PointerMoved(from + Vec2::new(0.0, 30.0))]);
        let out = w.rect(&Hit::Ungrouped).center();
        w.frame(vec![Event::PointerMoved(out)]);
        w.frame(vec![Event::PointerMoved(out)]);
        w.frame(vec![button(out, false)]);
        w.frame(vec![]);
        assert_eq!(group_of(&ws, "quill"), None);
    }

    #[test]
    fn the_filter_lists_one_group_or_the_projects_in_none() {
        let (ws, mut w) = grouped();
        ws.set_project_group("connecto", Some("group-1")).unwrap();
        w.frame(vec![]);
        w.click_hit(&Hit::GroupFilter);
        w.click_hit(&Hit::GroupFilterItem("group-1".into()));
        w.frame(vec![]);
        assert!(w.has(&Hit::Project("connecto".into())));
        assert!(!w.has(&Hit::Project("quill".into())));
        assert!(
            !w.has(&Hit::Group("group-1".into())),
            "no headers when filtered"
        );
        w.click_hit(&Hit::GroupFilter);
        w.click_hit(&Hit::GroupFilterItem("ungrouped".into()));
        w.frame(vec![]);
        assert!(w.has(&Hit::Project("quill".into())));
        assert!(!w.has(&Hit::Project("connecto".into())));
        // A deleted group's filter falls back to every project.
        w.click_hit(&Hit::GroupFilter);
        w.click_hit(&Hit::GroupFilterItem("group-1".into()));
        ws.delete_group("group-1").unwrap();
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.has(&Hit::Project("quill".into())) && w.has(&Hit::Project("connecto".into())));
        assert!(!w.has(&Hit::GroupFilter), "no groups, no filter");
    }

    #[test]
    fn a_project_in_a_group_sits_to_the_right_of_one_in_none() {
        let (ws, mut w) = grouped();
        ws.set_project_group("connecto", Some("group-1")).unwrap();
        w.frame(vec![]);
        let grouped = w.rect(&Hit::Project("connecto".into()));
        let loose = w.rect(&Hit::Project("quill".into()));
        assert!(
            grouped.min.x > loose.min.x + 8.0,
            "{grouped:?} vs {loose:?}"
        );
        // Filtered to the group, there is no header to sit under.
        w.click_hit(&Hit::GroupFilter);
        w.click_hit(&Hit::GroupFilterItem("group-1".into()));
        w.frame(vec![]);
        let alone = w.rect(&Hit::Project("connecto".into()));
        assert!(
            (alone.min.x - loose.min.x).abs() < 1.0,
            "{alone:?} vs {loose:?}"
        );
    }

    #[test]
    fn a_click_on_a_group_folds_its_projects() {
        let (ws, mut w) = grouped();
        ws.set_project_group("connecto", Some("group-1")).unwrap();
        w.frame(vec![]);
        assert!(w.has(&Hit::Project("connecto".into())));
        w.click_hit(&Hit::Group("group-1".into()));
        w.frame(vec![]);
        assert!(!w.has(&Hit::Project("connecto".into())));
        assert!(w.has(&Hit::Project("quill".into())));
    }
}

// ----- the project's operations (ADR 0031) -----

mod operations {
    use super::*;
    use connecto_core::ops::{
        Danger, OpKind, OpRow, OpStanding, OpStats, OpsAction, OpsControl, OpsView,
    };

    #[derive(Default)]
    struct FakeOps {
        views: Mutex<HashMap<String, OpsView>>,
        submitted: Mutex<Vec<(String, OpsAction)>>,
    }

    impl OpsControl for FakeOps {
        fn view(&self, project: &str) -> OpsView {
            self.views
                .lock()
                .unwrap()
                .get(project)
                .cloned()
                .unwrap_or_default()
        }

        fn submit(&self, project: String, action: OpsAction) {
            self.submitted.lock().unwrap().push((project, action));
        }
    }

    fn op(slug: &str, standing: OpStanding) -> OpRow {
        OpRow {
            slug: slug.into(),
            name: slug.to_uppercase(),
            description: "Does it.".into(),
            kind: Some(OpKind::Script),
            danger: Some(Danger::None),
            asks_first: false,
            standing,
            intercept: false,
            phrases: vec![slug.into()],
            args: Vec::new(),
            body: "echo hi\n".into(),
            spec: String::new(),
            hash: format!("h-{slug}"),
            timeout_s: 600,
            stats: OpStats::default(),
        }
    }

    fn window(ops: &Arc<FakeOps>) -> W {
        let (_quill, _connecto, ws) = two_projects();
        let mut w = W::with_diagnostics(
            &ws,
            Diagnostics {
                workspace: Some(Arc::clone(&ws) as Arc<dyn WorkspaceControl>),
                ops: Arc::clone(ops) as Arc<dyn OpsControl>,
                ..Diagnostics::default()
            },
        );
        w.frame(vec![]);
        w.frame(vec![]);
        w
    }

    #[test]
    fn the_bolt_shows_for_a_project_with_operations_and_opens_its_dialog() {
        let ops = Arc::new(FakeOps::default());
        ops.views.lock().unwrap().insert(
            "quill".into(),
            OpsView {
                project: "quill".into(),
                available: true,
                ops: vec![
                    op("tests", OpStanding::Approved),
                    op("deploy", OpStanding::Proposed),
                ],
                ..OpsView::default()
            },
        );
        let mut w = window(&ops);
        assert!(w.has(&Hit::Ops("quill".into())), "the active row has it");
        let bolt = w.rect(&Hit::Ops("quill".into()));
        let gear = w.rect(&Hit::Gear("quill".into()));
        assert!(bolt.max.x <= gear.min.x, "left of the gear");
        // A project with no operations has none, even when hovered.
        let row = w.rect(&Hit::Project("connecto".into()));
        w.frame(vec![Event::PointerMoved(row.center())]);
        w.frame(vec![]);
        assert!(w.has(&Hit::Gear("connecto".into())));
        assert!(!w.has(&Hit::Ops("connecto".into())));
        w.click(bolt.center());
        w.frame(vec![]);
        assert_eq!(w.app.ops_sheet_of().as_deref(), Some("quill"));
        assert!(
            ops.submitted.lock().unwrap().is_empty(),
            "opening does nothing"
        );
    }
}
