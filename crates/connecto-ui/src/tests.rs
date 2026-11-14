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
    ProjectView, RecentProject, ResizeOutcome, SaveState, SessionStatus, SnapshotOutcome,
    TermActivity, TermSnapshot, TerminalEntry, TerminalSession, TerminalsControl, TerminalsView,
    ToolKind, Transcript, UtilityEffort, UtilityModel, UtilityRun, UtilitySettings, UtilityStatus,
    WorkspaceControl, WorkspaceView,
};
use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2, ViewportCommand};

use crate::app::viewport_for_tests;
use crate::app::{App, Diagnostics, PaneConfig};
use crate::pane::PANE_PADDING;
use crate::theme;
use crate::tree::{Hit, LIGHTS_BAND, RAIL_WIDTH, SIDEBAR_WIDTH};

#[cfg(not(target_os = "macos"))]
mod in_window_menu;

/// The in-window menu bar's height; macOS has a native menu and none.
#[cfg(target_os = "macos")]
const MENU_BAR: f32 = 0.0;
#[cfg(not(target_os = "macos"))]
const MENU_BAR: f32 = crate::menu::BAR_HEIGHT;

// ---------- fakes ----------

#[derive(Default)]
struct FakeSession {
    input: Mutex<Vec<u8>>,
    sizes: Mutex<Vec<GridSize>>,
    polls: AtomicUsize,
    refuse: bool,
}

impl TerminalSession for FakeSession {
    fn poll_snapshot(&self, _out: &mut TermSnapshot) -> SnapshotOutcome {
        self.polls.fetch_add(1, Ordering::Relaxed);
        SnapshotOutcome::Unchanged
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

    fn scroll(&self, _lines: i32) -> bool {
        true
    }

    fn status(&self) -> SessionStatus {
        SessionStatus::Running
    }
}

struct FakeControl {
    stage: Mutex<PaneStage>,
    logins: Mutex<u32>,
    starts: Mutex<u32>,
    live: AtomicBool,
    warnings: Mutex<Arc<Vec<String>>>,
}

impl Default for FakeControl {
    fn default() -> Self {
        Self {
            stage: Mutex::new(PaneStage::Running),
            logins: Mutex::new(0),
            starts: Mutex::new(0),
            live: AtomicBool::new(true),
            warnings: Mutex::default(),
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
    continues: Mutex<Vec<String>>,
    transcripts: Mutex<Vec<String>>,
    titlings: Mutex<Vec<String>>,
    runs: Mutex<Vec<UtilityRun>>,
    utility_panes: Mutex<HashMap<u64, TerminalEntry>>,
    refuse: Mutex<Option<String>>,
}

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

    fn add(&self, tool: ToolKind, name: Option<String>) -> Result<(), String> {
        self.adds.lock().unwrap().push((tool, name));
        self.answer()
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

    fn rename_project(&self, project: &str, name: Option<String>) -> Result<(), String> {
        self.renamed
            .lock()
            .unwrap()
            .push((project.to_owned(), name));
        self.answer()
    }

    fn move_project(&self, _project: &str, _index: usize) {}

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
    }
}

impl Spec {
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
                    finished_turns: spec.turns,
                    session: Arc::clone(&slot.session) as Arc<dyn TerminalSession>,
                    control: Some(Arc::clone(&slot.control) as Arc<dyn PaneControl>),
                    desired_running: true,
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
            continues: Mutex::default(),
            transcripts: Mutex::default(),
            titlings: Mutex::default(),
            runs: Mutex::default(),
            utility_panes: Mutex::default(),
            refuse: Mutex::default(),
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
        let _ = self.ctx.run_ui(input, |ui| app.draw(ui));
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

/// Two projects: orbit has a parent with two children, and a second terminal;
/// connecto has one.
fn two_projects() -> (Proj, Proj, Arc<FakeWorkspace>) {
    let orbit = project(
        "orbit",
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
    let ws = workspace(&[&orbit, &connecto]);
    (orbit, connecto, ws)
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
    let orbit = project(
        "orbit",
        vec![
            spec("claude-1", Claude).named("Ledger retries"),
            spec("codex-1", Codex),
            spec("codex-2", Codex).child_of("claude-1"),
            spec("claude-3", Claude)
                .child_of("claude-1")
                .asked_by("codex-2"),
        ],
    );
    let ws = workspace(&[&orbit]);
    let w = W::new(&ws);
    // The list has codex-1 before the children; the tree puts them under claude-1.
    assert_eq!(
        w.terminal_rows("orbit"),
        ["claude-1", "codex-2", "claude-3", "codex-1"]
    );
    let parent = w.rect(&Hit::Terminal("orbit".into(), "claude-1".into()));
    let child = w.rect(&Hit::Terminal("orbit".into(), "codex-2".into()));
    let grandchild = w.rect(&Hit::Terminal("orbit".into(), "claude-3".into()));
    assert!(child.min.x > parent.min.x, "children are indented");
    assert!(
        (grandchild.min.x - child.min.x).abs() < 0.5,
        "same level as a child"
    );
}

#[test]
fn an_orphan_child_is_drawn_at_top_level() {
    let orbit = project(
        "orbit",
        vec![
            spec("codex-2", Codex).child_of("gone"),
            spec("claude", Claude),
        ],
    );
    let ws = workspace(&[&orbit]);
    let w = W::new(&ws);
    assert_eq!(w.terminal_rows("orbit"), ["codex-2", "claude"]);
    let a = w.rect(&Hit::Terminal("orbit".into(), "codex-2".into()));
    let b = w.rect(&Hit::Terminal("orbit".into(), "claude".into()));
    assert!((a.min.x - b.min.x).abs() < 0.5);
}

#[test]
fn a_collapsed_project_hides_its_terminals_and_shows_a_count() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert!(w.has(&Hit::Terminal("orbit".into(), "claude-1".into())));
    w.click_hit(&Hit::Twisty("orbit".into()));
    w.frame(vec![]);
    assert!(!w.has(&Hit::Terminal("orbit".into(), "claude-1".into())));
    assert!(!ws.view.lock().unwrap().projects[0].expanded);
    assert!(!orbit.typed_anywhere());
}

#[test]
fn a_parent_can_fold_its_children() {
    let (_orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::Kids("orbit".into(), "claude-1".into()));
    w.frame(vec![]);
    assert_eq!(w.terminal_rows("orbit"), ["claude-1", "codex-1"]);
    w.click_hit(&Hit::Kids("orbit".into(), "claude-1".into()));
    w.frame(vec![]);
    assert_eq!(w.terminal_rows("orbit").len(), 4);
}

#[test]
fn selecting_a_project_shows_its_remembered_terminal_and_sends_nothing_anywhere() {
    let (orbit, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert_eq!(w.shown(), Some(("orbit".into(), "claude-1".into())));
    // Only the selected project lists its terminals.
    assert!(!w.has(&Hit::Terminal("connecto".into(), "claude".into())));
    w.click_hit(&Hit::Project("connecto".into()));
    assert_eq!(w.shown(), Some(("connecto".into(), "claude".into())));
    assert_eq!(*ws.activations.lock().unwrap(), ["connecto"]);
    assert!(w.has(&Hit::Terminal("connecto".into(), "claude".into())));
    assert!(!w.has(&Hit::Terminal("orbit".into(), "claude-1".into())));
    assert!(!orbit.typed_anywhere() && !connecto.typed_anywhere());
    for project in [&orbit, &connecto] {
        let terminals = &project.terminals;
        assert!(terminals.adds.lock().unwrap().is_empty());
        assert!(terminals.closes.lock().unwrap().is_empty());
        assert!(terminals.restarts.lock().unwrap().is_empty());
    }
    // Typing now reaches the terminal on show and no other.
    w.frame(vec![]);
    w.type_text("x");
    assert_eq!(connecto.slot("claude").typed(), b"x");
    assert!(!orbit.typed_anywhere());
}

#[test]
fn dropping_files_on_the_terminal_inserts_their_escaped_paths_and_runs_nothing() {
    let (orbit, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let content = w.app.hits.content;
    // Over the sidebar: not the terminal's.
    w.drop_files(&["/tmp/x.png"], w.app.hits.sidebar.center());
    assert!(!orbit.typed_anywhere());
    w.drop_files(&["/tmp/a b.png", "/tmp/c.txt"], content.center());
    #[cfg(not(windows))]
    let expected: &[u8] = b"/tmp/a\\ b.png /tmp/c.txt ";
    // PowerShell quoting (platform::quote_dropped_path).
    #[cfg(windows)]
    let expected: &[u8] = b"'/tmp/a b.png' /tmp/c.txt ";
    assert_eq!(orbit.slot("claude-1").typed(), expected);
    assert!(!connecto.typed_anywhere());
}

#[test]
fn only_the_selected_project_lists_terminals_and_each_remembers_its_last_one() {
    let (orbit, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert_eq!(w.terminal_rows("orbit").len(), 4);
    assert!(!w.has(&Hit::Terminal("connecto".into(), "claude".into())));
    w.click_hit(&Hit::Terminal("orbit".into(), "codex-1".into()));
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("orbit".into(), "codex-1".into())));
    w.click_hit(&Hit::Project("connecto".into()));
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("connecto".into(), "claude".into())));
    assert!(!w.has(&Hit::Terminal("orbit".into(), "codex-1".into())));
    w.click_hit(&Hit::Project("orbit".into()));
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("orbit".into(), "codex-1".into())));
    assert!(!w.has(&Hit::Terminal("connecto".into(), "claude".into())));
    assert!(!orbit.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn background_projects_keep_their_panes_but_only_the_shown_terminal_is_polled() {
    let (orbit, connecto, ws) = two_projects();
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
    assert!(orbit.slot("claude-1").session.polls.load(Ordering::Relaxed) > 0);
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
    let (orbit, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    for _ in 0..3 {
        w.click_hit(&Hit::Project("connecto".into()));
        w.click_hit(&Hit::Project("orbit".into()));
    }
    for slot in orbit.slots.values().chain(connecto.slots.values()) {
        assert_eq!(*slot.control.starts.lock().unwrap(), 0);
        assert_eq!(*slot.control.logins.lock().unwrap(), 0);
        assert!(slot.typed().is_empty());
    }
    assert_eq!(ws.stopped.load(Ordering::Relaxed), 0);
    assert!(ws.closed.lock().unwrap().is_empty());
}

#[test]
fn a_project_shows_its_most_urgent_dot_and_the_unseen_ring_clears_when_shown() {
    let orbit = project("orbit", vec![spec("claude", Claude)]);
    let mut bg = project("bg", vec![spec("a", Claude), spec("b", Codex)]);
    let ws = workspace(&[&orbit, &bg]);
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
    let (mut orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let mut specs = vec![
        spec("claude-1", Claude).named("Ledger retries"),
        spec("codex-1", Codex),
        spec("codex-9", Codex).child_of("claude-1"),
    ];
    specs.push(spec("claude-9", Claude));
    orbit.set(specs);
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("orbit".into(), "claude-1".into())));
    w.type_text("k");
    assert_eq!(orbit.slot("claude-1").typed(), b"k");
    assert!(orbit.slot("codex-9").typed().is_empty());
}

#[test]
fn closing_the_shown_terminal_shows_its_neighbour() {
    let (mut orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::Terminal("orbit".into(), "codex-1".into()));
    orbit.set(vec![
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
    let typed: usize = orbit.slots.values().map(|slot| slot.typed().len()).sum();
    assert_eq!(
        typed, 1,
        "the new terminal on show gets the key, and one only"
    );
}

#[test]
fn close_on_a_child_calls_close() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    // The close button shows on hover.
    let child = w.rect(&Hit::Terminal("orbit".into(), "claude-3".into()));
    w.frame(vec![Event::PointerMoved(child.center())]);
    w.frame(vec![]);
    w.click_hit(&Hit::Close("orbit".into(), "claude-3".into()));
    assert_eq!(*orbit.terminals.closes.lock().unwrap(), ["claude-3"]);
    assert_eq!(
        w.shown(),
        Some(("orbit".into(), "claude-1".into())),
        "a close button does not also show its row"
    );
}

#[test]
fn a_refused_request_shows_its_reason_under_the_header() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    *orbit.terminals.refuse.lock().unwrap() = Some("could not save".to_owned());
    let child = w.rect(&Hit::Terminal("orbit".into(), "claude-3".into()));
    w.frame(vec![Event::PointerMoved(child.center())]);
    w.frame(vec![]);
    let before = w.app.hits.notice_height;
    w.click_hit(&Hit::Close("orbit".into(), "claude-3".into()));
    w.frame(vec![]);
    assert!(w.app.hits.notice_height > before);
    assert_eq!(
        w.app.request_error_for_tests().as_deref(),
        Some("could not save")
    );
}

// ---------- the header and renaming ----------

fn rename_start(w: &mut W) -> Rect {
    let row = w.rect(&Hit::Terminal("orbit".into(), "claude-1".into()));
    w.click(row.center());
    w.click(row.center());
    w.frame(vec![]);
    w.frame(vec![]);
    row
}

#[test]
fn a_double_click_on_the_name_renames_in_place_and_enter_saves() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    rename_start(&mut w);
    assert!(w.app.is_renaming());
    // The old name is selected; typing replaces it.
    w.type_text("Ledger v2");
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(
        *orbit.terminals.renames.lock().unwrap(),
        [("claude-1".to_owned(), Some("Ledger v2".to_owned()))]
    );
    assert!(!w.app.is_renaming());
    assert!(
        !orbit.typed_anywhere(),
        "the typed name did not reach the terminal"
    );
}

#[test]
fn escape_cancels_a_rename_and_an_empty_name_clears_it() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    rename_start(&mut w);
    w.type_text("x");
    w.frame(esc());
    w.frame(vec![]);
    assert!(orbit.terminals.renames.lock().unwrap().is_empty());
    assert!(!w.app.is_renaming());
    assert!(
        !orbit.typed_anywhere(),
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
        *orbit.terminals.renames.lock().unwrap(),
        [("claude-1".to_owned(), None)]
    );
}

#[test]
fn a_project_row_renames_the_same_way() {
    let (_orbit, _connecto, ws) = two_projects();
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
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    *orbit.terminals.refuse.lock().unwrap() = Some("name is too long".to_owned());
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
    let (_orbit, _connecto, ws) = two_projects();
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
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::T)]);
    w.frame(vec![]);
    assert!(w.app.chooser_open());
    w.type_text("Write tests");
    w.frame(vec![key_down(Key::ArrowDown, Modifiers::NONE)]);
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(
        *orbit.terminals.adds.lock().unwrap(),
        [(Codex, Some("Write tests".to_owned()))]
    );
    assert!(!w.app.chooser_open());
    assert!(
        !orbit.typed_anywhere(),
        "neither the name nor Enter reached a terminal"
    );
}

#[test]
fn a_click_on_a_tool_adds_it_and_the_new_terminal_comes_to_the_front() {
    let (mut orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::NewRow("orbit".into()));
    w.frame(vec![]);
    assert!(w.app.chooser_open());
    let claude = w.app.hits.chooser.choices[0].center();
    w.click(claude);
    assert_eq!(*orbit.terminals.adds.lock().unwrap(), [(Claude, None)]);
    // The registry adds it; the window shows the terminal the user asked for.
    let mut specs = vec![
        spec("claude-1", Claude),
        spec("codex-2", Codex).child_of("claude-1"),
        spec("claude-3", Claude).child_of("claude-1"),
        spec("codex-1", Codex),
    ];
    specs.push(spec("claude-4", Claude));
    orbit.set(specs);
    w.frame(vec![]);
    assert_eq!(w.shown(), Some(("orbit".into(), "claude-4".into())));
}

#[test]
fn escape_closes_the_chooser_without_adding() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::T)]);
    w.frame(vec![]);
    w.frame(esc());
    w.frame(vec![]);
    assert!(!w.app.chooser_open());
    assert!(orbit.terminals.adds.lock().unwrap().is_empty());
    assert!(!orbit.typed_anywhere());
}

#[test]
fn new_claude_and_new_codex_add_at_once_without_a_chooser() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![key_down(Key::Num1, Modifiers { alt: true, ..cmd() })]);
    w.frame(vec![key_down(Key::Num2, Modifiers { alt: true, ..cmd() })]);
    assert_eq!(
        *orbit.terminals.adds.lock().unwrap(),
        [(Claude, None), (Codex, None)]
    );
    assert!(!orbit.typed_anywhere());
}

#[test]
fn the_chooser_says_what_it_knows_of_each_tools_login() {
    let (orbit, _connecto, ws) = two_projects();
    *orbit.slot("codex-1").control.stage.lock().unwrap() = PaneStage::NeedsLogin {
        auth: AuthState::LoggedOut,
    };
    let view = orbit.terminals.view();
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
    let (orbit, connecto, ws) = two_projects();
    orbit.terminals.edit(|view| {
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
    (orbit, connecto, ws)
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
    let (_orbit, _connecto, ws) = with_history();
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
    let (orbit, _connecto, ws) = with_history();
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
        !orbit.typed_anywhere(),
        "search text never reaches a terminal"
    );
}

#[test]
fn continue_resumes_the_selected_entry_and_a_transcript_only_entry_offers_no_continue() {
    let (orbit, _connecto, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    // The first row is selected; click the second to select it.
    let second = w.app.hits.history.rows[1].1.center();
    w.click(second);
    w.frame(vec![]);
    let button = w.app.hits.history.continue_button.unwrap().center();
    w.click(button);
    assert_eq!(*orbit.terminals.continues.lock().unwrap(), ["h2"]);
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
    let (orbit, _connecto, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    let view = w.app.hits.history.view_button.unwrap().center();
    w.click(view);
    w.frame(vec![]);
    assert_eq!(*orbit.terminals.transcripts.lock().unwrap(), ["h1"]);
    // The registry loads it off-thread.
    orbit.terminals.edit(|v| {
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
    assert!(!orbit.typed_anywhere());
    // The text is read-only: typing changes nothing and Esc goes back.
    w.type_text("zzz");
    w.frame(esc());
    w.frame(vec![]);
    assert!(w.app.history_open());
    assert!(w.app.hits.history.transcript.is_none());
}

#[test]
fn escape_closes_the_sheet_and_gives_the_keyboard_back_without_a_stray_key() {
    let (orbit, _connecto, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    w.frame(esc());
    w.frame(vec![]);
    assert!(!w.app.history_open());
    w.frame(vec![]);
    w.type_text("k");
    assert_eq!(orbit.slot("claude-1").typed(), b"k", "no Esc byte first");
}

#[test]
fn empty_history_says_so() {
    let (_orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_history(&mut w);
    assert!(listed(&w).is_empty());
}

// ---------- the rail ----------

#[test]
fn the_sidebar_collapses_to_a_rail_of_tiles_with_dots_and_back() {
    let (_orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    assert!((w.app.hits.sidebar.width() - SIDEBAR_WIDTH).abs() < 0.5);
    w.frame(vec![cmd_key(Key::Backslash)]);
    w.frame(vec![]);
    assert!(ws.view.lock().unwrap().sidebar_collapsed);
    assert!((w.app.hits.sidebar.width() - RAIL_WIDTH).abs() < 0.5);
    assert!(w.has(&Hit::Tile("orbit".into())) && w.has(&Hit::Tile("connecto".into())));
    assert!(!w.has(&Hit::Terminal("orbit".into(), "claude-1".into())));
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
    let (_orbit, _connecto, ws) = two_projects();
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
    let (_orbit, _connecto, ws) = two_projects();
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
fn a_plain_click_on_the_plus_asks_for_a_folder_and_opens_it() {
    let (_orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.click_hit(&Hit::HeaderPlus);
    w.frame(vec![]);
    assert_eq!(
        *ws.opened.lock().unwrap(),
        [PathBuf::from("/picked/folder")]
    );
}

#[test]
fn nothing_in_the_terminal_area_starts_a_window_drag() {
    let (_orbit, _connecto, ws) = two_projects();
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
    let (_orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::Backslash)]);
    w.frame(vec![]);
    let strip = w.rect(&Hit::Header);
    let tile = w.rect(&Hit::Tile("orbit".into()));
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
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::Backslash)]);
    w.frame(vec![]);
    w.click_hit(&Hit::Tile("orbit".into()));
    w.frame(vec![cmd_key(Key::R)]);
    w.frame(vec![]);
    w.frame(vec![]);
    assert!(!ws.view.lock().unwrap().sidebar_collapsed);
    assert!(w.app.is_renaming());
    w.type_text("N");
    w.frame(enter());
    w.frame(vec![]);
    assert_eq!(orbit.terminals.renames.lock().unwrap().len(), 1);
    assert!(!orbit.typed_anywhere());
}

#[test]
fn a_rename_nothing_can_draw_is_dropped_rather_than_holding_the_keyboard() {
    let (orbit, _connecto, ws) = two_projects();
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
    assert_eq!(orbit.slot("claude-1").typed(), b"k");
}

// ---------- menu shortcuts ----------

#[test]
fn no_menu_shortcut_reaches_a_terminal() {
    let (orbit, connecto, ws) = two_projects();
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
            !orbit.typed_anywhere() && !connecto.typed_anywhere(),
            "{key:?} {modifiers:?} reached a terminal: {:?}",
            orbit.slot("claude-1").typed()
        );
    }
}

#[test]
fn with_a_native_menu_the_chord_is_dropped_and_not_acted_on_twice() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.app.set_menu_owns_shortcuts(true);
    w.frame(vec![cmd_key(Key::T), key_down(Key::Tab, Modifiers::CTRL)]);
    w.frame(vec![]);
    assert!(!w.app.chooser_open(), "the menu acts, not the key");
    assert!(!orbit.typed_anywhere());
    // The menu's click arrives as a command.
    w.app.queue_for_tests(crate::commands::Command::NewTerminal);
    w.frame(vec![]);
    w.frame(vec![]);
    assert!(w.app.chooser_open());
}

#[test]
fn control_tab_cycles_terminals_and_command_digits_switch_projects() {
    let (_orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![key_down(Key::Tab, Modifiers::CTRL)]);
    assert_eq!(w.shown(), Some(("orbit".into(), "codex-2".into())));
    w.frame(vec![key_down(
        Key::Tab,
        Modifiers {
            shift: true,
            ctrl: true,
            ..Modifiers::NONE
        },
    )]);
    assert_eq!(w.shown(), Some(("orbit".into(), "claude-1".into())));
    w.frame(vec![cmd_key(Key::Num2)]);
    assert_eq!(w.shown(), Some(("connecto".into(), "claude".into())));
    w.frame(vec![cmd_key(Key::Num1)]);
    assert_eq!(w.shown().map(|(p, _)| p).as_deref(), Some("orbit"));
}

#[test]
fn next_needing_me_finds_a_waiting_terminal_in_any_project() {
    let orbit = project("orbit", vec![spec("claude", Claude)]);
    let connecto = project(
        "connecto",
        vec![
            spec("a", Claude),
            spec("b", Codex).state(TermActivity::NeedsUser),
        ],
    );
    let ws = workspace(&[&orbit, &connecto]);
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::J)]);
    assert_eq!(w.shown(), Some(("connecto".into(), "b".into())));
    // Nothing else waits: it stays.
    w.frame(vec![cmd_key(Key::J)]);
    assert_eq!(w.shown(), Some(("connecto".into(), "b".into())));
    assert!(!orbit.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn close_terminal_close_project_pause_and_stop_everything_reach_the_workspace() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    w.frame(vec![cmd_key(Key::W)]);
    assert_eq!(*orbit.terminals.closes.lock().unwrap(), ["claude-1"]);
    w.frame(vec![key_down(
        Key::W,
        Modifiers {
            shift: true,
            ..cmd()
        },
    )]);
    assert_eq!(*ws.closed.lock().unwrap(), ["orbit"]);
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
    let orbit = project(
        "orbit",
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
    let ws = workspace(&[&orbit]);
    let mut w = W::new(&ws);
    w.frame(vec![key_down(
        Key::K,
        Modifiers {
            shift: true,
            ..cmd()
        },
    )]);
    assert_eq!(
        *orbit.terminals.closes.lock().unwrap(),
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
    let orbit = project(
        "orbit",
        vec![spec("claude", Claude).state(TermActivity::NotStarted)],
    );
    waiting(&orbit);
    let ws = workspace(&[&orbit]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    w.frame(vec![]);
    assert_eq!(*orbit.terminals.restarts.lock().unwrap(), ["claude"]);

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
    let orbit = project(
        "orbit",
        vec![spec("claude", Claude).state(TermActivity::Exited)],
    );
    let ws = workspace(&[&orbit]);
    let mut w = W::new(&ws);
    let key = ("orbit".to_owned(), "claude".to_owned());
    let restart = w.app.pane_hits(&key)[0];
    assert!(
        restart.min.y > w.app.hits.content.center().y,
        "inside the pane, not a header"
    );
    w.click(restart.center());
    assert_eq!(*orbit.terminals.restarts.lock().unwrap(), ["claude"]);
    assert!(!orbit.typed_anywhere());
}

#[test]
fn a_failed_terminal_shows_its_reason_with_log_in_and_restart_and_no_start() {
    let orbit = project(
        "orbit",
        vec![spec("claude", Claude).state(TermActivity::Failed)],
    );
    *orbit.slot("claude").control.stage.lock().unwrap() = PaneStage::Failed {
        reason: "no".into(),
    };
    let ws = workspace(&[&orbit]);
    let mut w = W::new(&ws);
    w.frame(vec![]);
    let key = ("orbit".to_owned(), "claude".to_owned());
    assert_eq!(w.app.pane_hits(&key).len(), 2, "Log in and Restart");
    let restart = w.app.pane_hits(&key)[1].center();
    w.click(restart);
    assert_eq!(*orbit.terminals.restarts.lock().unwrap(), ["claude"]);
    assert_eq!(*orbit.slot("claude").control.starts.lock().unwrap(), 0);
}

// ---------- the pulse ----------

#[test]
fn the_window_repaints_for_the_pulse_only_while_something_works() {
    let mut orbit = project("orbit", vec![spec("claude", Claude)]);
    let mut bg = project("bg", vec![spec("a", Codex)]);
    let ws = workspace(&[&orbit, &bg]);
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
    orbit.set(vec![spec("claude", Claude).state(TermActivity::Working)]);
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
                path: "/work/orbit".into(),
                name: "orbit".into(),
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
    assert_eq!(w.app.hits.inline.choices.len(), 3);
    let shell = w.app.hits.inline.choices[2].center();
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
    assert_eq!(w.app.hits.inline.choices.len(), 3);
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
        }];
        view.active = Some("gone".into());
    });
    let mut w = W::new(&ws);
    w.frame(vec![]);
    assert!(w.app.hits.open_button.is_none());
}

#[test]
fn a_standing_warning_and_the_save_state_show_under_the_header() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let plain = w.app.hits.notice_height;
    orbit.terminals.edit(|view| {
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
    let (orbit, connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    for hit in [
        Hit::Project("connecto".into()),
        Hit::Twisty("orbit".into()),
        Hit::Terminal("orbit".into(), "codex-1".into()),
        Hit::Twisty("orbit".into()),
        Hit::Twisty("orbit".into()),
        Hit::Pause,
        Hit::Pause,
        Hit::Collapse,
        Hit::Tile("orbit".into()),
        Hit::Collapse,
    ] {
        w.click_hit(&hit);
        w.frame(vec![]);
    }
    assert!(!orbit.typed_anywhere() && !connecto.typed_anywhere());
}

/// Whatever sidebar control has the keyboard, Enter and Space stay away from
/// the terminal.
#[test]
fn no_activation_key_leaks_into_the_terminal_from_any_control() {
    for key in [Key::Enter, Key::Space] {
        let (orbit, connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        let ids: Vec<egui::Id> = vec![
            egui::Id::unique(("tree-terminal", "orbit", "codex-1")),
            egui::Id::unique(("tree-project", "connecto")),
            egui::Id::unique(("tree-new", "orbit")),
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
                !orbit.typed_anywhere() && !connecto.typed_anywhere(),
                "{id:?} with {key:?} leaked"
            );
        }
    }
}

// ---------- the panes: input, selection, login ----------

fn one_terminal() -> (Proj, W) {
    let orbit = project("orbit", vec![spec("claude", Claude), spec("codex", Codex)]);
    let ws = workspace(&[&orbit]);
    let w = W::new(&ws);
    (orbit, w)
}

fn fill(w: &mut W, key: (&str, &str), text: &str) {
    let cols = 100_u16;
    let mut cells = vec![connecto_core::Cell::default(); usize::from(cols) * 40];
    for (cell, ch) in cells.iter_mut().zip(text.chars()) {
        cell.ch = ch;
    }
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
    let (orbit, mut w) = one_terminal();
    fill(&mut w, ("orbit", "claude"), "https://example.test/code");
    let cell = w.app.cell_for_tests();
    let at = |col: f32| cell_origin(&w) + Vec2::new((col + 0.5) * cell.width, cell.height * 0.5);
    let (from, to) = (at(8.0), at(19.0));
    drag(&mut w, from, to);
    assert!(w.copied.is_empty(), "a drag alone must not copy");
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied, ["example.test"]);
    assert!(!orbit.typed_anywhere());
}

#[cfg(target_os = "macos")]
#[test]
fn cmd_c_without_a_selection_copies_nothing_and_sends_nothing() {
    let (orbit, mut w) = one_terminal();
    fill(&mut w, ("orbit", "claude"), "secret on screen");
    w.frame(vec![Event::Copy]);
    assert!(w.copied.is_empty());
    assert!(!orbit.typed_anywhere());
}

/// Off macOS Ctrl+C arrives as Copy: with nothing selected it interrupts,
/// and after a copy the selection is gone so the next one interrupts too.
#[cfg(not(target_os = "macos"))]
#[test]
fn ctrl_c_without_a_selection_interrupts_and_copies_nothing() {
    let (orbit, mut w) = one_terminal();
    fill(&mut w, ("orbit", "claude"), "secret on screen");
    w.frame(vec![Event::Copy]);
    assert!(w.copied.is_empty());
    assert_eq!(orbit.slot("claude").typed(), [3]);
    w.frame(vec![Event::Cut]);
    assert_eq!(orbit.slot("claude").typed(), [3, 0x18]);
}

#[cfg(not(target_os = "macos"))]
#[test]
fn ctrl_c_on_a_selection_copies_it_once_then_interrupts() {
    let (orbit, mut w) = one_terminal();
    fill(&mut w, ("orbit", "claude"), "first line");
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
    assert!(!orbit.typed_anywhere());
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied.len(), 1);
    assert_eq!(orbit.slot("claude").typed(), [3]);
}

#[test]
fn typing_clears_the_selection() {
    let (_orbit, mut w) = one_terminal();
    fill(&mut w, ("orbit", "claude"), "text");
    let start = cell_origin(&w) + Vec2::new(2.0, 2.0);
    drag(&mut w, start, start + Vec2::new(60.0, 0.0));
    let key = ("orbit".to_owned(), "claude".to_owned());
    assert!(w.app.pane_mut(&key).unwrap().selection.is_some());
    w.frame(vec![Event::Text("a".into())]);
    assert!(w.app.pane_mut(&key).unwrap().selection.is_none());
}

#[test]
fn cmd_a_selects_the_screen_cmd_c_copies_it_and_nothing_reaches_the_pty() {
    let (orbit, mut w) = one_terminal();
    fill(&mut w, ("orbit", "claude"), "first line");
    w.frame(vec![cmd_key(Key::A)]);
    let key = ("orbit".to_owned(), "claude".to_owned());
    let selection = w.app.pane_mut(&key).unwrap().selection.unwrap();
    assert_eq!(selection.anchor, (0, 0));
    assert_eq!(selection.head, (39, 99));
    w.frame(vec![Event::Copy]);
    assert_eq!(w.copied.len(), 1);
    assert!(w.copied[0].starts_with("first line"));
    assert!(!orbit.typed_anywhere());
    w.frame(vec![Event::Text("x".into())]);
    assert!(w.app.pane_mut(&key).unwrap().selection.is_none());
}

#[test]
fn cmd_a_on_an_unfocused_terminal_selects_nothing() {
    let (orbit, mut w) = one_terminal();
    fill(&mut w, ("orbit", "claude"), "first line");
    w.ctx.memory_mut(|memory| memory.stop_text_input());
    w.frame(vec![cmd_key(Key::A)]);
    let key = ("orbit".to_owned(), "claude".to_owned());
    assert!(w.app.pane_mut(&key).unwrap().selection.is_none());
    assert!(!orbit.typed_anywhere());
}

#[test]
fn cmd_v_reaches_only_the_shown_terminal() {
    let (orbit, mut w) = one_terminal();
    w.frame(vec![Event::Paste("code-123".into())]);
    assert_eq!(orbit.slot("claude").typed(), b"code-123");
    assert!(orbit.slot("codex").typed().is_empty());
}

#[test]
fn the_shown_terminal_starts_focused_and_alone_receives_keys() {
    let (orbit, mut w) = one_terminal();
    w.frame(vec![Event::Text("a".into())]);
    assert_eq!(orbit.slot("claude").typed(), b"a");
    assert!(orbit.slot("codex").typed().is_empty());
}

#[test]
fn clicking_a_terminal_row_moves_keys_to_it_and_only_it() {
    let (orbit, mut w) = one_terminal();
    w.click_hit(&Hit::Terminal("orbit".into(), "codex".into()));
    w.frame(vec![Event::Text("b".into())]);
    assert_eq!(orbit.slot("codex").typed(), b"b");
    assert!(orbit.slot("claude").typed().is_empty());
    w.click_hit(&Hit::Terminal("orbit".into(), "claude".into()));
    w.frame(vec![Event::Text("c".into())]);
    assert_eq!(orbit.slot("claude").typed(), b"c");
    assert_eq!(orbit.slot("codex").typed(), b"b");
}

#[test]
fn typing_on_the_frame_after_a_click_reaches_the_new_terminal() {
    let (orbit, mut w) = one_terminal();
    let at = w
        .rect(&Hit::Terminal("orbit".into(), "codex".into()))
        .center();
    w.press(at);
    w.frame(vec![button(at, false)]);
    w.frame(vec![Event::Text("k".into())]);
    assert_eq!(orbit.slot("codex").typed(), b"k");
    w.frame(vec![Event::Paste("code-9".into())]);
    assert_eq!(orbit.slot("codex").typed(), b"kcode-9");
}

#[test]
fn control_c_goes_to_the_shown_terminal() {
    let (orbit, mut w) = one_terminal();
    w.click_hit(&Hit::Terminal("orbit".into(), "codex".into()));
    w.key(Key::C, Modifiers::CTRL);
    assert_eq!(orbit.slot("codex").typed(), [3]);
    assert!(orbit.slot("claude").typed().is_empty());
}

#[test]
fn navigation_keys_reach_the_child_and_do_not_move_focus() {
    let (orbit, mut w) = one_terminal();
    w.key(Key::Tab, Modifiers::NONE);
    w.key(Key::Escape, Modifiers::NONE);
    w.key(Key::ArrowUp, Modifiers::NONE);
    w.frame(vec![Event::Text("z".into())]);
    assert_eq!(orbit.slot("claude").typed(), b"\t\x1b\x1b[Az");
}

#[test]
fn a_row_chosen_by_enter_does_not_also_type_it() {
    let (orbit, mut w) = one_terminal();
    let id = egui::Id::unique(("tree-terminal", "orbit", "codex"));
    w.ctx.memory_mut(|memory| memory.request_focus(id));
    w.frame(vec![]);
    let mut events = enter();
    events.push(Event::Text("ls".into()));
    events.push(key_event(Key::Enter, true));
    events.push(key_event(Key::Enter, false));
    w.frame(events);
    assert_eq!(w.shown(), Some(("orbit".into(), "codex".into())));
    assert_eq!(orbit.slot("codex").typed(), b"ls\r");
}

#[test]
fn a_session_refusing_input_shows_a_notice_and_spares_the_others() {
    let orbit = project("orbit", vec![spec("claude", Claude), spec("codex", Codex)]);
    // Rebuild the second terminal with a refusing session.
    let refusing = Arc::new(FakeSession {
        refuse: true,
        ..FakeSession::default()
    });
    orbit.terminals.edit(|view| {
        view.terminals[1].session = Arc::clone(&refusing) as Arc<dyn TerminalSession>;
    });
    let ws = workspace(&[&orbit]);
    let mut w = W::new(&ws);
    w.click_hit(&Hit::Terminal("orbit".into(), "codex".into()));
    w.frame(vec![Event::Text("x".into())]);
    let codex = ("orbit".to_owned(), "codex".to_owned());
    assert!(w.app.pane_mut(&codex).unwrap().notice.is_some());
    assert!(
        w.app
            .pane_mut(&("orbit".to_owned(), "claude".to_owned()))
            .unwrap()
            .notice
            .is_none()
    );
    w.click_hit(&Hit::Terminal("orbit".into(), "claude".into()));
    w.frame(vec![Event::Text("y".into())]);
    assert_eq!(orbit.slot("claude").typed(), b"y");
}

#[test]
fn a_window_resize_reaches_the_shown_session_only() {
    let (orbit, mut w) = one_terminal();
    assert_eq!(orbit.slot("claude").session.sizes.lock().unwrap().len(), 1);
    w.width = 900.0;
    w.frame(vec![]);
    w.frame(vec![]);
    let sizes = orbit.slot("claude").session.sizes.lock().unwrap().clone();
    assert_eq!(sizes.len(), 2);
    assert!(sizes[1].cols < sizes[0].cols);
    assert!(
        orbit.slot("codex").session.sizes.lock().unwrap().is_empty(),
        "hidden: no work"
    );
    w.click_hit(&Hit::Terminal("orbit".into(), "codex".into()));
    w.frame(vec![]);
    assert_eq!(orbit.slot("codex").session.sizes.lock().unwrap().len(), 1);
}

fn managed(stage: PaneStage) -> (Proj, W) {
    let (orbit, mut w) = one_terminal();
    *orbit.slot("claude").control.stage.lock().unwrap() = stage;
    w.frame(vec![]);
    w.frame(vec![]);
    (orbit, w)
}

fn login_button(w: &W) -> Pos2 {
    let key = ("orbit".to_owned(), "claude".to_owned());
    w.app.pane_hits(&key)[0].center()
}

fn second_button(w: &W) -> Pos2 {
    let key = ("orbit".to_owned(), "claude".to_owned());
    w.app.pane_hits(&key)[1].center()
}

#[test]
fn a_pane_that_needs_login_offers_it_and_only_a_click_requests_it() {
    let (orbit, mut w) = managed(PaneStage::NeedsLogin {
        auth: AuthState::LoggedOut,
    });
    w.frame(vec![Event::Text("x".into())]);
    let control = &orbit.slot("claude").control;
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
    let (orbit, mut w) = managed(PaneStage::Stopped);
    let at = login_button(&w);
    w.click(at);
    w.frame(vec![]);
    let cancel = second_button(&w);
    w.click(cancel);
    let control = &orbit.slot("claude").control;
    assert_eq!(*control.logins.lock().unwrap(), 0);
    assert_eq!(*control.starts.lock().unwrap(), 0);
    w.frame(vec![]);
    assert!(
        !w.app
            .pane_mut(&("orbit".to_owned(), "claude".to_owned()))
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
        let (orbit, w) = managed(stage);
        let hits = w.app.pane_hits(&("orbit".to_owned(), "claude".to_owned()));
        assert_eq!(hits.len(), 1, "only Log in");
        assert_eq!(*orbit.slot("claude").control.starts.lock().unwrap(), 0);
    }
}

#[test]
fn a_terminal_shows_only_its_state_on_the_second_line() {
    let orbit = project(
        "orbit",
        vec![spec("claude", Claude).state(TermActivity::Failed)],
    );
    let entry = orbit.terminals.view().terminals[0].clone();
    let dot = crate::dots::Dot::of(entry.state, false);
    assert_eq!(crate::tree::meta_line(&entry, dot), "failed");
    let named = TerminalEntry {
        name: Some("Ledger".into()),
        ..entry
    };
    assert_eq!(crate::tree::meta_line(&named, dot), "failed");
}

#[test]
fn text_typed_in_the_frame_a_rename_ends_does_not_reach_the_terminal() {
    let (orbit, _m, ws) = two_projects();
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
        !orbit.typed_anywhere(),
        "{:?}",
        orbit.slot("claude-1").typed()
    );
    // A project rename too.
    let (orbit, connecto, ws) = two_projects();
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
    assert!(!orbit.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn continue_by_space_types_no_space_into_the_terminal() {
    let (orbit, _m, ws) = with_history();
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
    assert_eq!(*orbit.terminals.continues.lock().unwrap(), ["h1"]);
    assert!(
        !orbit.typed_anywhere(),
        "{:?}",
        orbit.slot("claude-1").typed()
    );
}

#[test]
fn enter_on_view_transcript_only_views_and_does_not_also_continue() {
    let (orbit, _m, ws) = with_history();
    let mut w = W::new(&ws);
    open_history(&mut w);
    let id = w.app.hits.history.view_id.unwrap();
    w.ctx.memory_mut(|memory| memory.request_focus(id));
    w.frame(vec![]);
    w.frame(enter());
    w.frame(vec![]);
    assert!(orbit.terminals.continues.lock().unwrap().is_empty());
    assert_eq!(*orbit.terminals.transcripts.lock().unwrap(), ["h1"]);
    assert!(w.app.history_open());
    assert!(!orbit.typed_anywhere());
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
    let (orbit, connecto, ws) = two_projects();
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
    assert!(!orbit.typed_anywhere() && !connecto.typed_anywhere());
}

#[test]
fn a_preparing_pane_offers_no_commands() {
    let (orbit, w) = managed(PaneStage::Preparing {
        step: "Checking login".to_owned(),
    });
    assert!(
        w.app
            .pane_hits(&("orbit".to_owned(), "claude".to_owned()))
            .is_empty()
    );
    assert_eq!(*orbit.slot("claude").control.logins.lock().unwrap(), 0);
}

#[test]
fn standing_warnings_take_room_from_the_grid_and_stay_in_every_stage() {
    for stage in [PaneStage::LoggingIn, PaneStage::Running, PaneStage::Stopped] {
        let (orbit, mut w) = managed(stage);
        let key = ("orbit".to_owned(), "claude".to_owned());
        assert_eq!(w.app.pane_mut(&key).unwrap().warning_lines(), 0);
        let rows = w
            .app
            .pane_mut(&key)
            .unwrap()
            .applied_size
            .map(|size| size.rows);
        *orbit.slot("claude").control.warnings.lock().unwrap() =
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
fn an_unknown_login_state_is_not_shown_as_logged_out() {
    let (_orbit, mut w) = managed(PaneStage::NeedsLogin {
        auth: AuthState::Unknown,
    });
    let key = ("orbit".to_owned(), "claude".to_owned());
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
        project: "orbit".into(),
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
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    hover_row(&mut w, "orbit", "claude-3");
    w.click_hit(&Hit::Robot("orbit".into(), "claude-3".into()));
    assert_eq!(*orbit.terminals.titlings.lock().unwrap(), ["claude-3"]);
    assert_eq!(w.shown(), Some(("orbit".into(), "claude-1".into())));
}

#[test]
fn robot_shows_exactly_when_the_close_button_does_and_sits_left_of_it() {
    let (_orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    let robot = Hit::Robot("orbit".into(), "claude-3".into());
    let close = Hit::Close("orbit".into(), "claude-3".into());
    assert_eq!(w.has(&robot), w.has(&close));
    assert!(!w.has(&robot), "a quiet row shows neither");
    hover_row(&mut w, "orbit", "claude-3");
    assert!(w.has(&robot) && w.has(&close));
    assert!(w.rect(&robot).max.x <= w.rect(&close).min.x + 0.5);
    // The row on show has both without a hover.
    let shown = ("orbit".to_owned(), "claude-1".to_owned());
    w.frame(vec![Event::PointerMoved(Pos2::new(900.0, 400.0))]);
    w.frame(vec![]);
    assert!(w.has(&Hit::Robot(shown.0.clone(), shown.1.clone())));
    assert!(w.has(&Hit::Close(shown.0, shown.1)));
}

#[test]
fn a_running_titling_pulses_the_robot_and_ignores_clicks() {
    let (orbit, _connecto, ws) = two_projects();
    orbit
        .terminals
        .runs
        .lock()
        .unwrap()
        .push(run(1, "claude-3", UtilityStatus::Running));
    let mut w = W::new(&ws);
    let robot = Hit::Robot("orbit".into(), "claude-3".into());
    let close = Hit::Close("orbit".into(), "claude-3".into());
    assert!(!w.has(&robot), "busy does not show it when the X is hidden");
    hover_row(&mut w, "orbit", "claude-3");
    assert!(w.has(&robot) && w.has(&close));
    assert!(w.rect(&robot).max.x <= w.rect(&close).min.x + 0.5);
    w.click_hit(&robot);
    assert!(orbit.terminals.titlings.lock().unwrap().is_empty());
    assert_eq!(w.shown(), Some(("orbit".into(), "claude-1".into())));
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
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    *orbit.terminals.refuse.lock().unwrap() = Some("already running".to_owned());
    hover_row(&mut w, "orbit", "claude-3");
    w.click_hit(&Hit::Robot("orbit".into(), "claude-3".into()));
    w.frame(vec![]);
    assert_eq!(
        w.app.request_error_for_tests().as_deref(),
        Some("already running")
    );
}

#[test]
fn the_menu_command_opens_and_closes_the_utility_sheet() {
    let (_orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    open_utility(&mut w);
    assert!(w.app.hits.utility.empty, "no runs shows the empty state");
    w.frame(esc());
    w.frame(vec![]);
    assert!(!w.app.utility_open());
}

#[test]
fn the_sheet_lists_the_projects_runs() {
    let (orbit, _connecto, ws) = two_projects();
    *orbit.terminals.runs.lock().unwrap() = vec![
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
    let (_orbit, _connecto, ws) = two_projects();
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

fn give_pane(orbit: &Proj, run: u64) -> Arc<Slot> {
    let slot = Arc::new(Slot::default());
    orbit.terminals.utility_panes.lock().unwrap().insert(
        run,
        TerminalEntry {
            id: format!("utility-{run}"),
            name: Some(format!("Utility: run {run}")),
            parent: None,
            asked_by: None,
            tool: ToolKind::Claude,
            state: TermActivity::Idle,
            finished_turns: 0,
            session: Arc::clone(&slot.session) as Arc<dyn TerminalSession>,
            control: Some(Arc::clone(&slot.control) as Arc<dyn PaneControl>),
            desired_running: false,
            activity: 0,
        },
    );
    slot
}

fn utility_runs_fixture(orbit: &Proj) -> Arc<Slot> {
    *orbit.terminals.runs.lock().unwrap() = vec![
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
    give_pane(orbit, 2)
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
    let (orbit, _connecto, ws) = two_projects();
    let slot = utility_runs_fixture(&orbit);
    let mut w = W::new(&ws);
    open_utility(&mut w);
    click_run(&mut w, 2);
    assert!(!w.app.utility_open(), "the sheet closes");
    assert_eq!(
        w.app.utility_view_for_tests(),
        Some(("orbit".into(), "utility-2".into()))
    );
    assert!(
        w.app.hits.utility_banner.is_some(),
        "labelled as a utility session"
    );
    w.type_text("y");
    assert_eq!(slot.typed(), b"y");
    assert!(!orbit.typed_anywhere(), "no tree terminal got the key");
}

#[test]
fn a_run_whose_session_is_closed_does_not_open() {
    let (orbit, _connecto, ws) = two_projects();
    let _ = utility_runs_fixture(&orbit);
    let mut w = W::new(&ws);
    open_utility(&mut w);
    click_run(&mut w, 1);
    assert!(w.app.utility_open(), "the sheet stays");
    assert_eq!(w.app.utility_view_for_tests(), None);
}

#[test]
fn selecting_a_tree_terminal_goes_back_from_a_utility_session() {
    let (orbit, _connecto, ws) = two_projects();
    let slot = utility_runs_fixture(&orbit);
    let mut w = W::new(&ws);
    open_utility(&mut w);
    click_run(&mut w, 2);
    assert!(w.app.utility_view_for_tests().is_some());
    w.click_hit(&Hit::Terminal("orbit".into(), "claude-3".into()));
    w.frame(vec![]);
    assert_eq!(w.app.utility_view_for_tests(), None);
    assert!(
        w.app.hits.utility_banner.is_none()
            || w.shown() == Some(("orbit".into(), "claude-3".into()))
    );
    assert_eq!(w.shown(), Some(("orbit".into(), "claude-3".into())));
    w.type_text("k");
    assert_eq!(orbit.slot("claude-3").typed(), b"k");
    assert!(slot.typed().is_empty());
}

#[test]
fn a_session_the_backend_closed_falls_back_to_the_terminal() {
    let (orbit, _connecto, ws) = two_projects();
    let _ = utility_runs_fixture(&orbit);
    let mut w = W::new(&ws);
    open_utility(&mut w);
    click_run(&mut w, 2);
    orbit.terminals.utility_panes.lock().unwrap().clear();
    w.frame(vec![]);
    w.frame(vec![]);
    assert_eq!(w.app.utility_view_for_tests(), None);
}

#[test]
fn rapid_robot_clicks_start_one_titling_even_before_the_run_shows() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    hover_row(&mut w, "orbit", "claude-3");
    let robot = Hit::Robot("orbit".into(), "claude-3".into());
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
    assert_eq!(*orbit.terminals.titlings.lock().unwrap(), ["claude-3"]);
    // Once a newer run has finished, the robot works again.
    orbit.terminals.runs.lock().unwrap().push(run(
        1,
        "claude-3",
        UtilityStatus::Done { title: "T".into() },
    ));
    w.frame(vec![]);
    w.frame(vec![]);
    w.click(at);
    assert_eq!(
        *orbit.terminals.titlings.lock().unwrap(),
        ["claude-3", "claude-3"]
    );
}

#[test]
fn a_refused_titling_does_not_leave_the_robot_busy() {
    let (orbit, _connecto, ws) = two_projects();
    let mut w = W::new(&ws);
    hover_row(&mut w, "orbit", "claude-3");
    *orbit.terminals.refuse.lock().unwrap() = Some("no".to_owned());
    let robot = Hit::Robot("orbit".into(), "claude-3".into());
    w.click_hit(&robot);
    *orbit.terminals.refuse.lock().unwrap() = None;
    hover_row(&mut w, "orbit", "claude-3");
    w.click_hit(&robot);
    assert_eq!(orbit.terminals.titlings.lock().unwrap().len(), 2);
}

// ---------- automations ----------

mod automations_sheet {
    use super::*;
    use connecto_core::{
        AutomationItem, AutomationOutcome, AutomationsControl, AutomationsView, HistoryRecord,
        KeyStatus, PendingConfirmation,
    };

    #[derive(Default)]
    struct FakeAutomations {
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
        fn calls(&self) -> Vec<String> {
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
            project_id: "orbit".into(),
            project: "orbit".into(),
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
        let (_orbit, _connecto, ws) = two_projects();
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
                project: "orbit".into(),
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
        let (_orbit, _connecto, ws) = two_projects();
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
            let (_orbit, _connecto, ws) = two_projects();
            let fake = Arc::new(FakeAutomations::default());
            if !create {
                // With a row listed, the form is below the fold of the window.
                fake.view.lock().unwrap().automations = vec![item("accept-edits")];
            }
            fake.view.lock().unwrap().root = Some(PathBuf::from("/automations"));
            *fake.navigate.lock().unwrap() =
                Some((Arc::clone(&ws), "orbit".into(), "claude-3".into()));
            let mut w =
                W::with_automations(&ws, Some(Arc::clone(&fake) as Arc<dyn AutomationsControl>));
            // The user picked another terminal of the project first.
            w.click_hit(&Hit::Terminal("orbit".into(), "codex-1".into()));
            assert_eq!(w.shown(), Some(("orbit".into(), "codex-1".into())));
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
                Some(("orbit".into(), "claude-3".into())),
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
        let (_orbit, _connecto, ws) = two_projects();
        let fake = Arc::new(FakeAutomations::default());
        *fake.navigate.lock().unwrap() = Some((Arc::clone(&ws), "orbit".into(), "claude-3".into()));
        fake.view.lock().unwrap().root = Some(PathBuf::from("/automations"));
        let mut w =
            W::with_automations(&ws, Some(Arc::clone(&fake) as Arc<dyn AutomationsControl>));
        w.click_hit(&Hit::Terminal("orbit".into(), "codex-1".into()));
        open_setup(&mut w);
        click(&mut w, "login-claude");
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.shown(), Some(("orbit".into(), "claude-3".into())));
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
        let (_orbit, _connecto, ws) = with_history();
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
                project: "orbit".to_owned(),
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
        let (_orbit, _connecto, ws) = with_history();
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
            view.projects = ["orbit", "connecto"]
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
        let (orbit, _connecto, ws) = two_projects();
        let w = W::with_packages(&ws, Arc::clone(fake) as Arc<dyn PackagesControl>);
        (orbit, w)
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
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
        open_utility(&mut w);
        open_skills(&mut w);
        assert!(!w.app.utility_open());
        open(&mut w, crate::commands::Command::SkillsPlugins);
        assert!(!w.app.skills_open());
    }

    #[test]
    fn set_as_default_and_no_default_call_the_control() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
        open_skills(&mut w);
        click(&mut w, Sheet::Skills, "default:mine");
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Skills, "unknown package"));
        assert!(w.app.skills_open());
    }

    #[test]
    fn creating_a_package_asks_a_name_and_an_extends_and_opens_its_session() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
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
    fn creating_without_extends_passes_none() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
        open_skills(&mut w);
        assert!(shows(&w, Sheet::Skills, "No packages yet"));
        assert!(shows(&w, Sheet::Skills, "disk full"));
    }

    // ----- the Settings sheet -----

    #[test]
    fn choosing_connecto_home_calls_set_home() {
        let fake = fake();
        fake.edit(|view| view.home = None);
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
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

    #[test]
    fn the_migration_status_is_shown() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, w) = window(&fake);
        assert!(w.has(&Hit::Gear("orbit".into())));
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
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        for text in [
            "Connecto v2",
            "installed v2 · v3 available",
            "this package no longer exists",
            "Updated",
            "checked 2 min ago",
            "Restart terminals to apply.",
            ".claude/settings.json",
            "package gone is missing",
        ] {
            assert!(shows(&w, Sheet::Project, text), "{text}");
        }
        assert!(drawn(&w, Sheet::Project, "add:connecto"));
        assert!(drawn(&w, Sheet::Project, "add:other"));
        assert!(!drawn(&w, Sheet::Project, "add:mine"), "already added");
    }

    #[test]
    fn adding_removing_and_the_default_checkbox_save_the_full_settings() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        click(&mut w, Sheet::Project, "add:mine");
        click(&mut w, Sheet::Project, "use-default");
        click(&mut w, Sheet::Project, "remove:mine");
        let saved = fake.saved();
        assert_eq!(saved.len(), 3);
        assert_eq!(saved[0].0, "orbit");
        assert_eq!(saved[0].1.packages, ["mine"]);
        assert!(saved[0].1.use_default_package);
        assert!(!saved[1].1.use_default_package);
        assert_eq!(saved[1].1.packages, ["mine"], "the rest is kept");
        assert!(saved[2].1.packages.is_empty());
    }

    #[test]
    fn toggling_the_tools_hides_the_config_rows_and_saves() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        assert!(shows(&w, Sheet::Project, "Claude config"));
        assert!(shows(&w, Sheet::Project, "Codex config"));
        click(&mut w, Sheet::Project, "tools:Claude");
        assert_eq!(fake.saved().last().unwrap().1.tools, ProjectTools::Claude);
        assert!(shows(&w, Sheet::Project, "Claude config"));
        assert!(!shows(&w, Sheet::Project, "Codex config"));
        click(&mut w, Sheet::Project, "tools:Codex");
        assert!(!shows(&w, Sheet::Project, "Claude config"));
        assert!(shows(&w, Sheet::Project, "Codex config"));
        click(&mut w, Sheet::Project, "tools:Both");
        assert!(shows(&w, Sheet::Project, "Claude config"));
        assert!(shows(&w, Sheet::Project, "Codex config"));
        assert_eq!(fake.saved().len(), 3);
    }

    #[test]
    fn config_modes_save_per_tool() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        // A new project is on the system configuration; isolated is the other choice.
        click(&mut w, Sheet::Project, "Codex-config:Isolated (advanced)");
        let saved = fake.saved();
        assert_eq!(saved[0].1.codex_config, ConfigMode::Isolated);
        assert_eq!(saved[0].1.claude_config, ConfigMode::System);
        click(&mut w, Sheet::Project, "Claude-config:Isolated (advanced)");
        assert_eq!(fake.saved()[1].1.claude_config, ConfigMode::Isolated);
        // Choosing what is already chosen saves nothing.
        click(&mut w, Sheet::Project, "Claude-config:Isolated (advanced)");
        assert_eq!(fake.saved().len(), 2);
    }

    #[test]
    fn a_refused_save_shows_inline_and_refresh_calls_the_control() {
        let fake = fake();
        *fake.refuse.lock().unwrap() = Some("unknown package".into());
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        click(&mut w, Sheet::Project, "tools:Codex");
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Project, "unknown package"));
        click(&mut w, Sheet::Project, "refresh");
        assert!(fake.calls().contains(&"refresh orbit".to_owned()));
    }

    #[test]
    fn an_unknown_project_is_loading_and_a_working_install_disables_refresh() {
        let fake = fake();
        fake.edit(|view| view.projects.clear());
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        assert!(shows(&w, Sheet::Project, "Loading"));
        fake.edit(|view| {
            view.projects.push(ProjectPackagesView {
                project_id: "orbit".into(),
                status: ProjectPackages {
                    state: InstallState::Working,
                    ..ProjectPackages::default()
                },
                ..ProjectPackagesView::default()
            });
        });
        w.frame(vec![]);
        assert!(shows(&w, Sheet::Project, "Working…"));
        click(&mut w, Sheet::Project, "refresh");
        assert!(!fake.calls().contains(&"refresh orbit".to_owned()));
    }

    #[test]
    fn an_install_error_is_shown_and_esc_closes_the_sheet() {
        let fake = fake();
        fake.edit(|view| {
            view.projects[0].status.state = InstallState::Error("not writable".into());
        });
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        assert!(shows(&w, Sheet::Project, "Error: not writable"));
        w.frame(esc());
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of(), None);
    }

    // ----- a new project -----

    #[test]
    fn adding_a_project_opens_its_settings() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
        w.click_hit(&Hit::HeaderPlus);
        w.frame(vec![]);
        assert_eq!(
            *w.ws.opened.lock().unwrap(),
            [PathBuf::from("/picked/folder")]
        );
        assert_eq!(w.app.project_settings_of(), None, "not there yet");
        let fresh = project("fresh", vec![]);
        w.ws.edit(|view| view.projects.push(fresh.view()));
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of().as_deref(), Some("fresh"));
    }

    #[test]
    fn opening_an_already_open_project_does_not_open_settings() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
        w.click_hit(&Hit::HeaderPlus);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of(), None);
    }

    // ----- the chooser -----

    #[test]
    fn the_chooser_offers_only_the_allowed_tools() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Codex);
        let (orbit, mut w) = window(&fake);
        w.click_hit(&Hit::NewRow("orbit".into()));
        w.frame(vec![]);
        assert_eq!(w.app.hits.chooser.choices.len(), 2);
        // Enter takes the one offered, not Claude.
        w.frame(enter());
        assert_eq!(*orbit.terminals.adds.lock().unwrap(), [(Codex, None)]);
    }

    #[test]
    fn both_tools_are_offered_by_default_and_a_click_adds_the_one_clicked() {
        let fake = fake();
        let (orbit, mut w) = window(&fake);
        w.click_hit(&Hit::NewRow("orbit".into()));
        w.frame(vec![]);
        assert_eq!(w.app.hits.chooser.choices.len(), 3);
        let codex = w.app.hits.chooser.choices[1].center();
        w.click(codex);
        assert_eq!(*orbit.terminals.adds.lock().unwrap(), [(Codex, None)]);
    }

    #[test]
    fn a_shortcut_for_a_tool_the_project_turned_off_is_refused() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Claude);
        let (orbit, mut w) = window(&fake);
        w.app
            .queue_for_tests(crate::commands::Command::NewTool(Codex));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(orbit.terminals.adds.lock().unwrap().is_empty());
        assert!(
            w.app
                .request_error_for_tests()
                .is_some_and(|text| text.contains("Codex is turned off"))
        );
        w.app
            .queue_for_tests(crate::commands::Command::NewTool(Claude));
        w.frame(vec![]);
        assert_eq!(*orbit.terminals.adds.lock().unwrap(), [(Claude, None)]);
    }

    #[test]
    fn the_empty_project_chooser_offers_only_the_allowed_tools_too() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Claude);
        let (mut orbit, _connecto, ws) = two_projects();
        orbit.set(vec![]);
        let mut w = W::with_packages(&ws, Arc::clone(&fake) as Arc<dyn PackagesControl>);
        w.frame(vec![]);
        w.frame(vec![]);
        assert_eq!(w.app.hits.inline.choices.len(), 2);
    }

    #[test]
    fn a_second_edit_before_the_view_updates_keeps_the_first() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        click(&mut w, Sheet::Project, "tools:Claude");
        click(&mut w, Sheet::Project, "add:mine");
        let saved = fake.saved();
        assert_eq!(saved.len(), 2);
        assert_eq!(saved[1].1.tools, ProjectTools::Claude, "first edit kept");
        assert_eq!(saved[1].1.packages, ["mine"]);
        assert!(!shows(&w, Sheet::Project, "Codex config"), "shown at once");
        // The backend catches up; a third edit still builds on both.
        fake.publish();
        w.frame(vec![]);
        click(&mut w, Sheet::Project, "use-default");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(last.tools, ProjectTools::Claude);
        assert_eq!(last.packages, ["mine"]);
        assert!(!last.use_default_package);
    }

    #[test]
    fn a_publish_later_than_any_timeout_still_keeps_the_first_edit() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        click(&mut w, Sheet::Project, "tools:Claude");
        // Well past the old 3 s limit; nothing in the view yet.
        std::thread::sleep(std::time::Duration::from_millis(3300));
        w.frame(vec![]);
        assert!(!shows(&w, Sheet::Project, "Codex config"), "still pending");
        click(&mut w, Sheet::Project, "add:mine");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(last.tools, ProjectTools::Claude);
        assert_eq!(last.packages, ["mine"]);
    }

    #[test]
    fn closing_and_reopening_the_sheet_keeps_pending_edits_for_the_next_one() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        click(&mut w, Sheet::Project, "tools:Codex");
        w.frame(esc());
        w.frame(vec![]);
        assert_eq!(w.app.project_settings_of(), None);
        open_project_settings(&mut w, "orbit");
        assert!(!shows(&w, Sheet::Project, "Claude config"), "still pending");
        click(&mut w, Sheet::Project, "add:mine");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(last.tools, ProjectTools::Codex);
        assert_eq!(last.packages, ["mine"]);
        // Confirmed: later edits build on the view again.
        fake.publish();
        w.frame(vec![]);
        w.frame(vec![]);
        click(&mut w, Sheet::Project, "remove:mine");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(last.tools, ProjectTools::Codex);
        assert!(last.packages.is_empty());
    }

    #[test]
    fn the_projects_own_install_error_after_the_change_drops_it() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        click(&mut w, Sheet::Project, "tools:Codex");
        assert!(!shows(&w, Sheet::Project, "Claude config"));
        fake.edit(|view| {
            view.projects[0].status.state = InstallState::Error("could not save".into());
        });
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(
            shows(&w, Sheet::Project, "Claude config"),
            "back to the view"
        );
    }

    #[test]
    fn an_unrelated_global_error_keeps_the_pending_settings() {
        let fake = fake();
        fake.late.store(true, Ordering::Relaxed);
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
        click(&mut w, Sheet::Project, "tools:Codex");
        fake.edit(|view| view.last_error = Some("package operation failed".into()));
        // Another project failing is not this change's failure either.
        fake.edit(|view| {
            view.projects[1].status.state = InstallState::Error("elsewhere".into());
        });
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!shows(&w, Sheet::Project, "Claude config"), "still pending");
        click(&mut w, Sheet::Project, "add:mine");
        let last = fake.saved().pop().unwrap().1;
        assert_eq!(last.tools, ProjectTools::Codex, "built on the pending edit");
        assert_eq!(last.packages, ["mine"]);
    }

    #[test]
    fn a_refused_edit_drops_what_was_pending() {
        let fake = fake();
        *fake.refuse.lock().unwrap() = Some("no".into());
        let (_orbit, mut w) = window(&fake);
        open_project_settings(&mut w, "orbit");
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
        let (orbit, mut w) = window(&fake);
        let at = crate::commands::Command::RestartAt {
            project: "orbit".into(),
            terminal: "codex-1".into(),
        };
        w.app.queue_for_tests(at.clone());
        w.frame(vec![]);
        assert_eq!(*orbit.terminals.restarts.lock().unwrap(), ["codex-1"]);
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Claude);
        w.app.queue_for_tests(at);
        w.frame(vec![]);
        assert_eq!(orbit.terminals.restarts.lock().unwrap().len(), 1);
        assert!(
            w.app
                .request_error_for_tests()
                .is_some_and(|text| text.contains("Codex is turned off"))
        );
    }

    #[test]
    fn the_pane_offers_no_login_for_a_tool_turned_off_later() {
        let fake = fake();
        let orbit = project("orbit", vec![spec("claude", Claude)]);
        let ws = workspace(&[&orbit]);
        *orbit.slot("claude").control.stage.lock().unwrap() = PaneStage::NeedsLogin {
            auth: AuthState::LoggedOut,
        };
        let mut w = W::with_packages(&ws, Arc::clone(&fake) as Arc<dyn PackagesControl>);
        w.frame(vec![]);
        let key = ("orbit".to_owned(), "claude".to_owned());
        assert_eq!(w.app.pane_hits(&key).len(), 1, "Log in while allowed");
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Codex);
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(w.app.pane_hits(&key).is_empty(), "no Log in, no Confirm");
        assert_eq!(*orbit.slot("claude").control.logins.lock().unwrap(), 0);
    }

    #[test]
    fn continue_from_history_is_refused_for_a_tool_turned_off() {
        let fake = fake();
        fake.edit(|view| view.projects[0].settings.tools = ProjectTools::Claude);
        let (orbit, _connecto, ws) = with_history();
        let mut w = W::with_packages(&ws, Arc::clone(&fake) as Arc<dyn PackagesControl>);
        open_history(&mut w);
        let second = w.app.hits.history.rows[1].1.center();
        w.click(second);
        w.frame(vec![]);
        let button = w.app.hits.history.continue_button.unwrap().center();
        w.click(button);
        assert!(orbit.terminals.continues.lock().unwrap().is_empty());
        assert!(
            w.app
                .request_error_for_tests()
                .is_some_and(|text| text.contains("Codex is turned off"))
        );
    }

    #[test]
    fn the_name_field_takes_focus_once_so_tab_can_leave_it() {
        let fake = fake();
        let (_orbit, mut w) = window(&fake);
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
        let (_orbit, mut w) = window(&fake);
        open_skills(&mut w);
        assert!(shows(&w, Sheet::Skills, "could not read the folder"));
    }
}

// ---------- remote hosts ----------

#[allow(clippy::panic)]
mod remote_ui {
    use connecto_core::{
        AuthorizedKeyRow, HostLink, HostRow, HostingStatus, PendingHost, RemoteControl, RemoteView,
    };

    use super::*;
    use crate::commands::Command;

    struct FakeRemote {
        view: Mutex<RemoteView>,
        host: Mutex<Proj>,
        calls: Mutex<Vec<String>>,
        refuse: Mutex<Option<String>>,
        /// Start the pairing without a phase, as a backend that has not yet
        /// reported one would.
        stay_idle: Mutex<bool>,
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
            Arc::new(Self {
                view: Mutex::new(view),
                host: Mutex::new(host),
                calls: Mutex::default(),
                refuse: Mutex::default(),
                stay_idle: Mutex::new(false),
            })
        }

        fn calls(&self) -> Vec<String> {
            self.calls.lock().unwrap().clone()
        }

        fn edit(&self, change: impl FnOnce(&mut RemoteView)) {
            change(&mut self.view.lock().unwrap());
        }

        fn note(&self, call: String) -> Result<(), String> {
            self.calls.lock().unwrap().push(call);
            self.refuse.lock().unwrap().clone().map_or(Ok(()), Err)
        }
    }

    impl RemoteControl for FakeRemote {
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
        let (orbit, _connecto, ws) = two_projects();
        let remote = FakeRemote::new();
        let w = W::with_remote(
            &ws,
            None,
            Arc::new(connecto_core::NoPackages),
            Arc::clone(&remote) as Arc<dyn RemoteControl>,
        );
        (orbit, remote, w)
    }

    fn row(w: &W, name: &str) -> Rect {
        w.app
            .hits
            .host_rows
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, r)| *r)
            .unwrap_or_else(|| panic!("{name} not drawn: {:?}", w.app.hits.host_rows))
    }

    fn has_row(w: &W, name: &str) -> bool {
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
        let (_orbit, _remote, w) = window();
        assert!(has_row(&w, "localhost"));
        assert!(has_row(&w, "build box"));
        assert!(has_row(&w, "old laptop"));
        assert!(has_row(&w, "add"));
        // localhost first, then the hosts in saved order, all above the projects.
        let names: Vec<_> = w
            .app
            .hits
            .host_rows
            .iter()
            .map(|(n, _)| n.as_str())
            .collect();
        assert_eq!(names, ["add", "localhost", "build box", "old laptop"]);
        let project = w.rect(&Hit::Project("orbit".into()));
        assert!(
            row(&w, "old laptop").max.y <= project.min.y,
            "hosts sit above the projects"
        );
    }

    #[test]
    fn selecting_a_host_connects_it_and_swaps_the_terminal_list() {
        let (_orbit, remote, mut w) = window();
        assert!(w.has(&Hit::Project("orbit".into())));
        click_row(&mut w, "build box");
        assert!(remote.calls().contains(&"connect build box".to_owned()));
        // The tree now lists that host's terminals, and none of the projects.
        assert!(!w.has(&Hit::Project("orbit".into())));
        assert!(w.has(&Hit::Project("remote:build box".into())));
        assert!(w.has(&Hit::Terminal("remote:build box".into(), "7".into())));
        assert!(w.has(&Hit::Terminal("remote:build box".into(), "8".into())));
        // Back to this machine.
        click_row(&mut w, "localhost");
        assert!(w.has(&Hit::Project("orbit".into())));
        assert!(!w.has(&Hit::Project("remote:build box".into())));
    }

    #[test]
    fn a_disconnected_host_says_so_instead_of_listing_nothing() {
        let (_orbit, remote, mut w) = window();
        remote.host.lock().unwrap().set(vec![]);
        click_row(&mut w, "old laptop");
        assert!(w.has(&Hit::Project("remote:old laptop".into())));
        assert!(!w.has(&Hit::Terminal("remote:old laptop".into(), "7".into())));
    }

    #[test]
    fn rename_and_forget_are_in_the_row_menu_commands() {
        let (_orbit, remote, mut w) = window();
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
        assert!(has_row(&w, "ci box"));
        assert!(w.has(&Hit::Project("remote:ci box".into())));

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
        assert!(w.has(&Hit::Project("orbit".into())));
    }

    /// Show the Settings section whose tab is `key`.
    fn show_tab(w: &mut W, key: &str) {
        let at = settings_center(w, key);
        w.click(at);
        w.frame(vec![]);
    }

    #[test]
    fn remote_access_is_a_tab_with_the_hosting_toggle_and_no_advanced_keys() {
        let (_orbit, remote, mut w) = window();
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
    fn the_system_store_for_secrets_is_an_opt_in_on_the_remote_tab() {
        let (orbit, _connecto, ws) = two_projects();
        let _ = orbit;
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
        show_tab(&mut w, "tab-remote");
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

    #[test]
    fn an_unreadable_relay_token_is_not_said_to_be_absent() {
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, _remote, mut w) = window();
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
                codex: AgentMode::Bypass
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
                codex: AgentMode::Prompt
            }
        );
    }

    fn set_phase(remote: &FakeRemote, phase: connecto_core::PairingPhase) {
        remote.edit(|v| v.pairing = phase);
    }

    #[test]
    fn a_refused_command_does_not_hide_the_backend_error() {
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, _remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, _remote, mut w) = window();
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
        let (_orbit, _remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        let (_orbit, remote, mut w) = window();
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
        }

        impl FakeAgents {
            fn new(rows: Vec<AgentRow>) -> Arc<Self> {
                Arc::new(Self {
                    view: Mutex::new(AgentsView { rows }),
                    calls: Mutex::default(),
                    off: Mutex::default(),
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
                self.off.lock().unwrap().clone()
            }
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

        #[test]
        fn deactivate_hides_the_agent_from_the_new_terminal_choices_and_a_shortcut_says_why() {
            let fake = FakeAgents::new(vec![
                row(ToolKind::Claude, "Claude Code", true),
                row(ToolKind::Codex, "Codex", true),
            ]);
            let mut w = window(&fake);
            click(&mut w, "agent-deactivate:claude");
            assert_eq!(fake.calls(), ["deactivate claude"]);
            // Close Settings, open the chooser: Claude is not offered (Codex and the shell are).
            w.app.queue_for_tests(crate::commands::Command::Settings);
            w.frame(vec![]);
            w.click_hit(&Hit::NewRow("orbit".into()));
            w.frame(vec![]);
            assert_eq!(w.app.hits.chooser.choices.len(), 2);
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
        let (_orbit, _connecto, ws) = two_projects();
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
        let (_orbit, _connecto, ws) = two_projects();
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
        let (_orbit, _connecto, ws) = two_projects();
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
        let (_orbit, _connecto, ws) = two_projects();
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
        let (_orbit, _connecto, ws) = two_projects();
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
        let (_orbit, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        open_settings(&mut w);
        w.click_hit(&Hit::NewRow("orbit".into()));
        w.frame(vec![]);
        assert!(w.app.chooser_open());
        assert!(!w.app.settings_open());
    }

    #[test]
    fn a_menu_new_shell_runs_over_an_open_sheet() {
        let (orbit, _connecto, ws) = two_projects();
        let mut w = W::new(&ws);
        open_settings(&mut w);
        w.app.queue_for_tests(Command::NewTool(ToolKind::Shell));
        w.frame(vec![]);
        w.frame(vec![]);
        assert!(!w.app.settings_open());
        assert_eq!(
            *orbit.terminals.adds.lock().unwrap(),
            [(ToolKind::Shell, None)]
        );
    }

    #[test]
    fn terminal_and_project_commands_run_over_an_open_sheet_and_close_it() {
        let commands = [
            Command::NewTerminal,
            Command::NewTerminalIn("orbit".into()),
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
            let (_orbit, _connecto, ws) = two_projects();
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
        let (orbit, _connecto, ws) = two_projects();
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
        assert!(orbit.terminals.adds.lock().unwrap().is_empty());
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
