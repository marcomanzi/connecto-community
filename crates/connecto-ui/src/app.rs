//! The application window: the sidebar tree, the terminal header and the
//! terminal on show.
//!
//! The window keeps a pane for every terminal of every open project, so a
//! session in a background project keeps its size and selection. Only the
//! terminal on show is polled, resized and typed into, and a click on the
//! sidebar changes what is shown and nothing else (project-tree-ui R2).

use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{LazyLock, OnceLock};
use std::time::{Duration, Instant};

use connecto_core::{
    AutomationsControl, NoPackages, PackagesControl, ProjectView, RemoteControl, SaveState,
    TermActivity, TerminalEntry, TerminalsControl, TerminalsView, ToolKind, UtilityRun,
    UtilityStatus, WorkspaceControl, WorkspaceView,
};
use eframe::Frame;
use egui::{Align2, Color32, Event, FontId, Id, Rect, Sense, Stroke, Ui, pos2, vec2};

use crate::automations::{
    self, AutomationsAction, AutomationsInput, AutomationsOut, AutomationsSheet,
};
use crate::chooser::{self, Chooser, ChooserEnd, ChooserHits, ChooserInfo};
use crate::commands::{self, Command};
use crate::confirm::{self, Confirm, ConfirmAction, Target};
use crate::dots;
use crate::fonts;
use crate::forms::Out;
use crate::header::{self, HEADER_HEIGHT, HeadInput, HeadOut};
use crate::history::{self, History, HistoryAction, HistoryInput, HistoryOut, Scope};
use crate::hosts::{self, HostAction, HostSheet};
use crate::menu::{InWindowBar, NativeMenu};
use crate::metrics::{FrameMetrics, MetricsSink, PaneCounters};
use crate::pairing::{self, PairingAction, PairingSheet};
use crate::pane::{Pane, drop_activation};
use crate::picker;
use crate::project_settings::{
    self, ProjectSettingsAction, ProjectSettingsInput, ProjectSettingsSheet,
};
use crate::rename::{Rename, RenameEnd, RenameTarget};
use crate::render::CellMetrics;
use crate::settings::{self, SettingsAction, SettingsSheet, SettingsTab};
#[cfg(feature = "frame-capture")]
use crate::shots::ScreenshotSink;
use crate::skills::{self, SkillsAction, SkillsSheet};
use crate::theme;
use crate::tree::{self, RAIL_WIDTH, SIDEBAR_WIDTH, TreeInput, TreeOut};
use crate::utility::{self, UtilityAction, UtilityInput, UtilityOut, UtilitySheet};
use crate::watchdog::{self, WindowWatch};

pub use crate::pane::PaneConfig;

const NOTICE_LINE: f32 = 2.0;
const ICON_PNG: &[u8] = include_bytes!("../../../assets/brand/icon-256.png");
const UNSAVED_TEXT: &str = "The terminal list is not saved yet. A retry runs every 30 s. After a restart, a terminal may come back running when you stopped it, or stopped when it was running.";
const READ_ONLY_TEXT: &str = "The terminal list could not be read, so changes are not saved and terminals cannot be added or removed.";
/// How long a terminal the user just asked for is looked for, to show it.
const NEW_TERMINAL_WINDOW: Duration = Duration::from_secs(5);
#[cfg(feature = "test-endpoints")]
const BANNER_HEIGHT: f32 = 22.0;

/// Base of the wake clock. Zero means "no wake pending", so times start at 1.
static CLOCK_BASE: LazyLock<Instant> = LazyLock::new(Instant::now);

pub(crate) fn clock_us() -> u64 {
    u64::try_from(CLOCK_BASE.elapsed().as_micros())
        .unwrap_or(u64::MAX)
        .max(1)
}

/// Lets session threads ask for a frame before the window exists.
#[derive(Default)]
pub struct WakeSlot {
    ctx: OnceLock<egui::Context>,
    /// When the oldest unanswered request was made, or zero.
    pending_since_us: AtomicU64,
}

impl WakeSlot {
    /// Request a repaint. Safe from any thread and never blocks.
    pub fn wake(&self) {
        let _ = self.pending_since_us.compare_exchange(
            0,
            clock_us(),
            Ordering::Relaxed,
            Ordering::Relaxed,
        );
        if let Some(ctx) = self.ctx.get() {
            ctx.request_repaint();
        }
    }

    /// Close the window, as if the user had. Safe from any thread.
    pub fn request_close(&self) {
        if let Some(ctx) = self.ctx.get() {
            ctx.send_viewport_cmd(egui::ViewportCommand::Close);
            ctx.request_repaint();
        }
    }

    /// How long the oldest request has waited, and clear it. A new frame is
    /// answering it now.
    fn take_latency(&self) -> Option<Duration> {
        let since = self.pending_since_us.swap(0, Ordering::Relaxed);
        (since != 0).then(|| Duration::from_micros(clock_us().saturating_sub(since)))
    }

    fn is_pending(&self) -> bool {
        self.pending_since_us() != 0
    }

    /// When the oldest unanswered request was made, or zero.
    pub(crate) fn pending_since_us(&self) -> u64 {
        self.pending_since_us.load(Ordering::Relaxed)
    }
}

/// What the workspace and every project's terminal list say right now. Taken
/// once a frame so the whole frame draws one state.
pub(crate) struct Snapshot {
    pub(crate) ws: Arc<WorkspaceView>,
    /// One per project, in the same order.
    pub(crate) views: Vec<Option<Arc<TerminalsView>>>,
    /// Each project's utility runs, newest first, in the same order.
    pub(crate) utility: Vec<Arc<Vec<UtilityRun>>>,
}

impl Snapshot {
    fn take(workspace: &dyn WorkspaceControl) -> Self {
        let ws = workspace.view();
        let views = ws
            .projects
            .iter()
            .map(|project| project.terminals.as_ref().map(|terminals| terminals.view()))
            .collect();
        let utility = ws
            .projects
            .iter()
            .map(|project| {
                project
                    .terminals
                    .as_ref()
                    .map_or_else(Arc::default, |terminals| terminals.utility_runs())
            })
            .collect();
        Self { ws, views, utility }
    }

    /// What the window shows while a remote host is selected: that host as the
    /// one open project, whose terminals are the host's. Everything else (the
    /// tree, the tabs, the panes) then works as it does for a project.
    fn for_host(
        &self,
        host: &connecto_core::HostRow,
        terminals: Arc<dyn TerminalsControl>,
    ) -> Self {
        let view = terminals.view();
        let note = match &host.link {
            _ if !view.terminals.is_empty() => None,
            connecto_core::HostLink::Disconnected => {
                Some("Not connected. Right-click the host and choose Connect.".to_owned())
            }
            connecto_core::HostLink::Connecting => Some("Connecting…".to_owned()),
            connecto_core::HostLink::Reconnecting { retry_in_secs } => {
                Some(format!("Reconnecting in {retry_in_secs}s…"))
            }
            connecto_core::HostLink::Refused(why) => Some(why.clone()),
            _ => Some("This host has no terminals to show.".to_owned()),
        };
        let id = format!("remote:{}", host.name);
        let project = connecto_core::ProjectView {
            id: id.clone(),
            name: host.name.clone(),
            path: format!("{} ({})", host.name, host.fingerprint),
            terminals: Some(terminals),
            error: note,
            expanded: true,
            active_terminal: None,
        };
        let ws = connecto_core::WorkspaceView {
            projects: vec![project],
            active: Some(id),
            recents: Vec::new(),
            sidebar_collapsed: self.ws.sidebar_collapsed,
            last_error: None,
            warning: None,
        };
        Self {
            ws: Arc::new(ws),
            views: vec![Some(view)],
            utility: vec![Arc::default()],
        }
    }

    /// A titling run is going for this terminal.
    pub(crate) fn titling(&self, project: &str, terminal: &str) -> bool {
        self.index_of(project)
            .and_then(|index| self.utility.get(index))
            .is_some_and(|runs| {
                runs.iter().any(|run| {
                    run.terminal_id == terminal && matches!(run.status, UtilityStatus::Running)
                })
            })
    }

    fn index_of(&self, id: &str) -> Option<usize> {
        self.ws.projects.iter().position(|project| project.id == id)
    }

    /// The project on show: the active one, or the first if the workspace has
    /// not named one.
    fn active(&self) -> Option<usize> {
        self.ws
            .active
            .as_deref()
            .and_then(|id| self.index_of(id))
            .or_else(|| (!self.ws.projects.is_empty()).then_some(0))
    }

    fn view(&self, index: usize) -> Option<&TerminalsView> {
        self.views.get(index).and_then(|view| view.as_deref())
    }

    fn project(&self, index: usize) -> Option<&ProjectView> {
        self.ws.projects.get(index)
    }
}

type Key = (String, String);

/// The utility session on show: its project and run.
struct UtilityView {
    project: String,
    run: u64,
    /// The name its pane carries, for the label above it.
    name: String,
}

impl UtilityView {
    fn key(&self) -> Key {
        (self.project.clone(), format!("utility-{}", self.run))
    }
}

/// A terminal the user just asked for, to show when it appears.
struct PendingNew {
    project: String,
    known: HashSet<String>,
    since: Instant,
}

/// Settings submitted and not yet confirmed by the backend's view.
struct PendingSettings {
    settings: connecto_core::ProjectSettings,
    /// The project's own install error as it stood at submission: a different
    /// one afterwards belongs to this change.
    install_error: Option<String>,
}

/// A project the user asked to open, to show its settings once it is there.
struct NewProject {
    known: HashSet<String>,
    since: Instant,
}

/// The backend is moving the window to an authoring terminal (Automations >
/// Open session, Create). It does so asynchronously, so the screen follows
/// the first change of any project's active terminal after this baseline.
struct Following {
    /// Each project's active terminal when the request was made.
    baseline: HashMap<String, Option<String>>,
    since: Instant,
}

/// The application state.
pub struct App {
    /// Panes of a static shell (`--pane`), without a sidebar.
    static_panes: Vec<Pane>,
    /// Index into `static_panes` of the one shown.
    visible: usize,
    workspace: Option<Arc<dyn WorkspaceControl>>,
    /// A pane per terminal of every open project, by (project, terminal).
    panes: HashMap<Key, Pane>,
    /// Turns each terminal had finished when the user last saw it.
    seen: HashMap<Key, u64>,
    /// The terminal shown in each project.
    selected: HashMap<String, String>,
    last_shown: Option<Key>,
    /// Parents whose children the user folded.
    folded: HashSet<Key>,
    rename: Option<Rename>,
    chooser: Option<Chooser>,
    /// The chooser a project without terminals shows in place of a terminal.
    inline_chooser: Option<Chooser>,
    history: Option<History>,
    utility: Option<UtilitySheet>,
    automations: Option<AutomationsSheet>,
    /// The automations engine, when it started.
    automations_control: Option<Arc<dyn AutomationsControl>>,
    /// Skills & Plugins (Terminal menu).
    skills: Option<SkillsSheet>,
    /// Settings (Connecto menu).
    settings: Option<SettingsSheet>,
    /// The Settings section on show. Kept while the app runs, so Settings opens
    /// where it was left.
    settings_tab: SettingsTab,
    /// Remote hosts and hosting (ADR 0017).
    remote: Arc<dyn RemoteControl>,
    /// The remote host shown, by name; `None` is this machine.
    host: Option<String>,
    /// Add host and Rename host.
    host_sheet: Option<HostSheet>,
    /// The pairing dialog, while a pairing is under way or just ended.
    pairing: Option<PairingSheet>,
    /// The confirmation before a machine is removed or a host forgotten.
    confirm: Option<Confirm>,
    /// Project Settings, for one project (the gear in the sidebar).
    project_settings: Option<ProjectSettingsSheet>,
    /// Connecto Home, packages and the per-project settings.
    packages: Arc<dyn PackagesControl>,
    /// Settings > Agents.
    agents: Arc<dyn connecto_core::AgentsControl>,
    /// Connecto updating itself, and the strip that offers it.
    updates: Arc<dyn connecto_core::UpdateControl>,
    update_strip: crate::update_strip::StripState,
    /// Why the last sign-in could not be opened, until Settings says it.
    agents_error: Option<String>,
    /// The launch message about it was handled (shown or not needed).
    /// Settings submitted per project that the backend's view does not show
    /// yet, with the failure markers at submission. Kept across closing the
    /// sheet, until the view matches or the change fails.
    pending_settings: HashMap<String, PendingSettings>,
    /// A project the user just asked to open: its settings show when it appears.
    new_project: Option<NewProject>,
    /// A utility run's session shown in the main area in place of a terminal.
    utility_view: Option<UtilityView>,
    /// Terminals the user asked a title for, with the newest run id then known
    /// for each. Busy until a newer run has finished, so the gap before the run
    /// shows in the list cannot take a second click.
    titling: HashMap<Key, u64>,
    pending_new: Option<PendingNew>,
    following: Option<Following>,
    /// The next pass gives the keyboard to the terminal on show.
    focus_visible: bool,
    /// The key that activated a control this pass, so the terminal that takes
    /// the keyboard does not also get it.
    activating_key: Option<egui::Key>,
    /// Why the last request was refused, until the next one.
    request_error: Option<String>,
    /// Terminals already started for being looked at, so each is tried once.
    woken: HashSet<Key>,
    menu: Option<NativeMenu>,
    /// The native menu owns the shortcut chords, so the window only drops them.
    menu_owns_shortcuts: bool,
    /// The menu bar drawn in the window where there is no native one (it
    /// draws nothing on macOS).
    bar: InWindowBar,
    /// A field or sheet had the keyboard when this pass began, so every event
    /// of the pass belongs to it, even if it closes mid-pass.
    modal_at_start: bool,
    /// Commands from the menu bar, run on the next pass.
    queued: Vec<Command>,
    picker: fn() -> Option<PathBuf>,
    clock: fn() -> (u64, i32),
    /// Where each project's New terminal row was drawn last pass.
    new_rows: HashMap<String, Rect>,
    /// Set while metrics are on.
    watch: Option<Arc<WindowWatch>>,
    #[cfg(feature = "ui-test-driver")]
    driver: Option<crate::driver::Driver>,
    wake: Arc<WakeSlot>,
    metrics: FrameMetrics,
    #[cfg(feature = "frame-capture")]
    shots: Option<ScreenshotSink>,
    cell: Option<CellMetrics>,
    #[cfg(test)]
    pub(crate) hits: crate::testkit::Hits,
}

fn system_clock() -> (u64, i32) {
    let now = chrono::Local::now();
    let ms = u64::try_from(now.timestamp_millis()).unwrap_or(0);
    (ms, chrono::Offset::fix(now.offset()).local_minus_utc())
}

/// Said where a refused command would have shown: a field keeps the keyboard.
const FIELD_BUSY: &str = "Finish the open dialog or field first.";

/// The commands that run while a text field is open. Each one leaves the
/// field or does not touch it.
fn allowed_over_field(command: &Command) -> bool {
    matches!(
        command,
        Command::History
            | Command::UtilitySessions
            | Command::Automations
            | Command::SkillsPlugins
            | Command::Settings
            | Command::SettingsAt(_)
            | Command::ProjectSettingsAt(_)
            | Command::ToggleSidebar
            | Command::TogglePause
            | Command::StopEverything
            | Command::Show { .. }
            | Command::Activate(_)
            | Command::SetExpanded(..)
            | Command::OpenProject
            | Command::OpenRecent(_)
            | Command::ClearRecents
            | Command::HistoryOf(_)
            | Command::SelectHost(_)
    )
}

/// The commands that open a sheet. Under a dialog they are refused: a sheet
/// opened above it would hide the dialog.
fn opens_sheet(command: &Command) -> bool {
    matches!(
        command,
        Command::History
            | Command::HistoryOf(_)
            | Command::UtilitySessions
            | Command::Automations
            | Command::SkillsPlugins
            | Command::Settings
            | Command::SettingsAt(_)
            | Command::ProjectSettingsAt(_)
            | Command::AddHost
    )
}

/// The commands that change what the terminal area shows or act on a terminal
/// or project in it. A sheet over that area is closed first, so the change is
/// seen. The sheets' own toggles and the chrome are not in it.
fn closes_sheets_first(command: &Command) -> bool {
    matches!(
        command,
        Command::OpenProject
            | Command::OpenRecent(_)
            | Command::RenameProject
            | Command::RenameProjectAt(_)
            | Command::CloseProject
            | Command::CloseProjectAt(_)
            | Command::NewTerminal
            | Command::NewTerminalIn(_)
            | Command::NewTool(_)
            | Command::RenameTerminal
            | Command::RenameTerminalAt { .. }
            | Command::CloseTerminal
            | Command::CloseTerminalAt { .. }
            | Command::CloseFinishedChildren
            | Command::CloseFinishedChildrenOf { .. }
            | Command::RestartTerminal
            | Command::RestartAt { .. }
            | Command::NextTerminal
            | Command::PreviousTerminal
            | Command::NextNeedingMe
            | Command::GotoProject(_)
            | Command::Show { .. }
            | Command::Activate(_)
            | Command::SelectHost(_)
    )
}

/// Whether `entry` is a child terminal whose work is finished.
fn is_finished_child(entry: &TerminalEntry) -> bool {
    entry.parent.is_some() && entry.state == TermActivity::Done
}

impl App {
    pub(crate) fn new(
        configs: Vec<PaneConfig>,
        wake: Arc<WakeSlot>,
        diagnostics: Diagnostics,
    ) -> Self {
        let metrics = FrameMetrics::new(diagnostics.metrics);
        let watch = metrics.sink().map(|sink| {
            let watch = Arc::new(WindowWatch::default());
            watchdog::spawn(Arc::clone(&wake), Arc::clone(&watch), sink.lines());
            watch
        });
        Self {
            wake,
            watch,
            #[cfg(feature = "ui-test-driver")]
            driver: std::env::var_os("CONNECTO_UI_SCRIPT")
                .and_then(|path| crate::driver::Driver::load(path.as_ref()))
                .map(|mut driver| {
                    if let Some(sink) = metrics.sink() {
                        driver.log_to(sink.lines());
                    }
                    driver
                }),
            static_panes: configs
                .into_iter()
                .map(|config| Pane::new("static", config))
                .collect(),
            visible: 0,
            workspace: diagnostics.workspace,
            panes: HashMap::new(),
            seen: HashMap::new(),
            selected: HashMap::new(),
            last_shown: None,
            folded: HashSet::new(),
            rename: None,
            chooser: None,
            inline_chooser: None,
            history: None,
            utility: None,
            automations: None,
            automations_control: diagnostics.automations,
            skills: None,
            settings: None,
            settings_tab: SettingsTab::default(),
            remote: diagnostics.remote,
            host: None,
            host_sheet: None,
            pairing: None,
            confirm: None,
            project_settings: None,
            packages: diagnostics.packages,
            agents: diagnostics.agents,
            updates: diagnostics.updates,
            update_strip: crate::update_strip::StripState::default(),
            agents_error: None,
            pending_settings: HashMap::new(),
            new_project: None,
            utility_view: None,
            titling: HashMap::new(),
            pending_new: None,
            following: None,
            focus_visible: true,
            activating_key: None,
            request_error: None,
            woken: HashSet::new(),
            menu: None,
            menu_owns_shortcuts: false,
            bar: InWindowBar::default(),
            modal_at_start: false,
            queued: Vec::new(),
            picker: picker::pick_folder,
            clock: system_clock,
            new_rows: HashMap::new(),
            metrics,
            #[cfg(feature = "frame-capture")]
            shots: diagnostics.shots,
            cell: None,
            #[cfg(test)]
            hits: crate::testkit::Hits::default(),
        }
    }

    /// Whether a text field keeps the keyboard: a rename or the host form.
    fn field_open(&self) -> bool {
        self.rename.is_some() || self.host_sheet.is_some() || self.overlay_open()
    }

    /// Whether a dialog drawn over everything is open: pairing or a confirm.
    fn overlay_open(&self) -> bool {
        self.pairing.is_some() || self.confirm.is_some()
    }

    /// A command the open field refused: say so where the field is.
    fn refuse_for_field(&mut self) {
        if let Some(sheet) = &mut self.pairing {
            sheet.notice = Some(FIELD_BUSY.to_owned());
        } else if let Some(confirm) = &mut self.confirm {
            confirm.error = Some(FIELD_BUSY.to_owned());
        } else if let Some(sheet) = &mut self.host_sheet {
            sheet.error = Some(FIELD_BUSY.to_owned());
        } else {
            self.request_error = Some(FIELD_BUSY.to_owned());
        }
    }

    fn modal_open(&self) -> bool {
        self.overlay_open()
            || self.rename.is_some()
            || self.chooser.is_some()
            || self.history.is_some()
            || self.utility.is_some()
            || self.automations.is_some()
            || self.skills.is_some()
            || self.settings.is_some()
            || self.host_sheet.is_some()
            || self.project_settings.is_some()
    }

    // ---------- state from the registry ----------

    /// Make `panes` and `seen` match the projects' lists, by (project, id). A
    /// terminal that stays keeps its screen, selection and size.
    fn reconcile(&mut self, snap: &Snapshot) {
        let mut alive: HashSet<Key> = HashSet::new();
        for (index, project) in snap.ws.projects.iter().enumerate() {
            let Some(view) = snap.view(index) else {
                continue;
            };
            for entry in &view.terminals {
                let key = (project.id.clone(), entry.id.clone());
                self.panes
                    .entry(key.clone())
                    .or_insert_with(|| Pane::from_entry(&project.id, entry))
                    .follow(entry);
                self.seen.entry(key.clone()).or_insert(entry.finished_turns);
                alive.insert(key);
            }
        }
        // The utility session on show keeps its pane while the backend has it.
        if let Some(view) = &self.utility_view {
            let entry = self
                .terminals_of(snap, &view.project)
                .and_then(|terminals| terminals.utility_pane(view.run));
            if let Some(entry) = entry {
                let key = view.key();
                let pane = self
                    .panes
                    .entry(key.clone())
                    .or_insert_with(|| Pane::from_entry(&view.project, &entry));
                pane.follow(&entry);
                alive.insert(key);
                let name = entry.name.unwrap_or_else(|| entry.id.clone());
                if let Some(view) = &mut self.utility_view {
                    view.name = name;
                }
            } else {
                self.utility_view = None;
                self.focus_visible = true;
            }
        }
        self.titling.retain(|(project, terminal), newest| {
            snap.index_of(project)
                .and_then(|index| snap.utility.get(index))
                .is_some_and(|runs| {
                    !runs.iter().any(|run| {
                        run.terminal_id == *terminal
                            && run.id > *newest
                            && !matches!(run.status, UtilityStatus::Running)
                    })
                })
        });
        self.panes.retain(|key, _| alive.contains(key));
        self.seen.retain(|key, _| alive.contains(key));
        self.selected
            .retain(|project, _| snap.index_of(project).is_some());
        self.folded.retain(|key| alive.contains(key));

        // The backend moved to an authoring terminal: what the user picked
        // before no longer outranks it.
        if let Some(following) = &self.following {
            let moved = snap.ws.projects.iter().find(|project| {
                project.active_terminal.is_some()
                    && following.baseline.get(&project.id) != Some(&project.active_terminal)
            });
            if let Some(project) = moved {
                let id = project.id.clone();
                self.selected.remove(&id);
                self.utility_view = None;
                self.focus_visible = true;
                self.following = None;
            } else if following.since.elapsed() > NEW_TERMINAL_WINDOW {
                self.following = None;
            }
        }

        // A project the user just added: its settings come first, so the
        // tools, config mode and default package can change before any work.
        if let Some(pending) = &self.new_project {
            let fresh = snap
                .ws
                .projects
                .iter()
                .find(|project| !pending.known.contains(&project.id));
            if let Some(project) = fresh {
                let id = project.id.clone();
                self.new_project = None;
                self.open_project_settings(&id);
            } else if pending.since.elapsed() > NEW_TERMINAL_WINDOW {
                self.new_project = None;
            }
        }

        // A terminal the user just asked for comes to the front when it shows
        // up. One an agent starts never does.
        if let Some(pending) = &self.pending_new {
            let fresh = snap
                .index_of(&pending.project)
                .and_then(|index| snap.view(index))
                .and_then(|view| {
                    view.terminals
                        .iter()
                        .find(|e| !pending.known.contains(&e.id))
                });
            if let Some(entry) = fresh {
                let project = pending.project.clone();
                self.selected.insert(project.clone(), entry.id.clone());
                if let Some(workspace) = &self.workspace {
                    workspace.set_active_terminal(&project, &entry.id);
                }
                self.pending_new = None;
            } else if pending.since.elapsed() > NEW_TERMINAL_WINDOW {
                self.pending_new = None;
            }
        }
    }

    /// The terminal shown in project `index`: the user's last pick, the one the
    /// workspace saved, or the first.
    fn shown_in(&self, snap: &Snapshot, index: usize) -> Option<String> {
        let project = snap.project(index)?;
        let view = snap.view(index)?;
        let has = |id: &str| view.terminals.iter().any(|entry| entry.id == id);
        self.selected
            .get(&project.id)
            .filter(|id| has(id))
            .or(project.active_terminal.as_ref().filter(|id| has(id)))
            .cloned()
            .or_else(|| tree::order(view).first().map(|node| node.entry.id.clone()))
    }

    /// The utility session on show in the active project.
    fn utility_shown(&self, snap: &Snapshot) -> Option<Key> {
        let view = self.utility_view.as_ref()?;
        let project = snap.project(snap.active()?)?;
        (project.id == view.project).then(|| view.key())
    }

    /// The terminal on show: its project and id.
    fn shown(&self, snap: &Snapshot) -> Option<Key> {
        let index = snap.active()?;
        let terminal = self.shown_in(snap, index)?;
        Some((snap.project(index)?.id.clone(), terminal))
    }

    // ---------- commands ----------

    /// Run commands against `snap`. Returns whether anything may have changed
    /// the workspace, so the caller takes a fresh snapshot.
    fn execute(&mut self, ctx: &egui::Context, snap: &Snapshot, commands: Vec<Command>) -> bool {
        let changed = !commands.is_empty();
        for command in commands {
            self.run(ctx, snap, command);
        }
        if changed {
            ctx.request_repaint();
        }
        changed
    }

    fn run(&mut self, ctx: &egui::Context, snap: &Snapshot, command: Command) {
        if self.workspace.is_none() {
            self.run_static(&command);
            return;
        }
        // A text field (a rename, the host form) keeps the keyboard: only the
        // commands that leave it run, and any other says why it did not.
        if self.overlay_open() && opens_sheet(&command)
            || self.field_open() && !allowed_over_field(&command)
        {
            self.refuse_for_field();
            return;
        }
        // A sheet over the terminal area goes when a command changes what that
        // area shows, so the change can be seen.
        if closes_sheets_first(&command) {
            self.close_sheets();
        }
        match command {
            Command::OpenProject => self.open_project(),
            Command::OpenRecent(index) => {
                if let Some(recent) = snap.ws.recents.get(index) {
                    self.open_path(Path::new(&recent.path));
                }
            }
            Command::ClearRecents => self.with_workspace(|workspace| workspace.clear_recents()),
            Command::RenameProject => {
                if let Some(project) = snap.active().and_then(|i| snap.project(i)) {
                    self.begin_project_rename(project);
                }
            }
            Command::RenameProjectAt(id) => {
                if let Some(project) = snap.index_of(&id).and_then(|i| snap.project(i)) {
                    self.begin_project_rename(project);
                }
            }
            Command::CloseProject => {
                if let Some(project) = snap.active().and_then(|i| snap.project(i)) {
                    self.close_project(&project.id);
                }
            }
            Command::CloseProjectAt(id) => self.close_project(&id),
            Command::NewTerminal => {
                if let Some(project) = snap.active().and_then(|i| snap.project(i)) {
                    self.open_chooser(&project.id);
                }
            }
            Command::NewTerminalIn(id) => {
                self.activate(snap, &id);
                self.open_chooser(&id);
            }
            Command::NewTool(tool) => {
                if let Some(index) = snap.active() {
                    self.add_terminal(snap, index, tool, None);
                }
            }
            Command::RenameTerminal => {
                if let Some((project, terminal)) = self.shown(snap) {
                    self.begin_terminal_rename(snap, &project, &terminal);
                }
            }
            Command::RenameTerminalAt { project, terminal } => {
                self.show(snap, &project, &terminal);
                self.begin_terminal_rename(snap, &project, &terminal);
            }
            Command::CloseTerminal => {
                if let Some((project, terminal)) = self.shown(snap) {
                    self.close_terminal(snap, &project, &terminal);
                }
            }
            Command::CloseTerminalAt { project, terminal } => {
                self.close_terminal(snap, &project, &terminal);
            }
            Command::CloseFinishedChildren => {
                if let Some((project, terminal)) = self.shown(snap) {
                    self.close_finished(snap, &project, Some(&terminal));
                }
            }
            Command::CloseFinishedChildrenOf { project, terminal } => {
                self.close_finished(snap, &project, Some(&terminal));
            }
            Command::UtilitySessions => {
                if self.utility.take().is_some() {
                    self.focus_visible = true;
                } else {
                    self.close_sheets();
                    self.utility = Some(UtilitySheet::default());
                }
            }
            Command::SkillsPlugins => {
                if self.skills.take().is_some() {
                    self.focus_visible = true;
                } else {
                    self.close_sheets();
                    self.skills = Some(SkillsSheet::default());
                }
            }
            Command::CheckForUpdates => {
                self.update_strip.closed_asked = false;
                self.updates.check_now();
            }
            Command::Settings => {
                if self.settings.take().is_some() {
                    self.focus_visible = true;
                } else {
                    self.close_sheets();
                    self.settings = Some(SettingsSheet::default());
                }
            }
            // A deep link: the section asked for, whether or not Settings is open.
            Command::SettingsAt(tab) => {
                self.close_sheets();
                self.settings_tab = tab;
                self.settings = Some(SettingsSheet::default());
            }
            Command::ProjectSettingsAt(id) => self.open_project_settings(&id),
            Command::AddHost => {
                self.close_sheets();
                self.open_pairing();
            }
            Command::SelectHost(name) => {
                self.host = name;
                if let Some(name) = &self.host {
                    self.remote.connect(name);
                }
                self.focus_visible = true;
            }
            Command::ConnectHost(name) => self.remote.connect(&name),
            Command::DisconnectHost(name) => self.remote.disconnect(&name),
            Command::RenameHostAt(name) => {
                self.close_sheets();
                self.host_sheet = Some(HostSheet::rename(&name));
            }
            Command::AskForgetHost(name) => self.confirm = Some(Confirm::host(&name)),
            Command::Automations => {
                if self.automations.take().is_some() {
                    self.focus_visible = true;
                } else {
                    self.open_automations();
                }
            }
            Command::GenerateTitleAt { project, terminal } => {
                let key = (project.clone(), terminal.clone());
                if self.titling.contains_key(&key) || snap.titling(&project, &terminal) {
                    return;
                }
                if let Some(terminals) = self.terminals_of(snap, &project) {
                    let newest = snap
                        .index_of(&project)
                        .and_then(|index| snap.utility.get(index))
                        .and_then(|runs| runs.iter().map(|run| run.id).max())
                        .unwrap_or(0);
                    self.titling.insert(key.clone(), newest);
                    self.request_error = terminals.start_titling(&terminal).err();
                    if self.request_error.is_some() {
                        self.titling.remove(&key);
                    }
                }
            }
            Command::History => self.toggle_history(snap, None),
            Command::HistoryOf(id) => self.toggle_history(snap, Some(id)),
            Command::RestartTerminal => {
                if let Some((project, terminal)) = self.shown(snap) {
                    self.restart(snap, &project, &terminal);
                }
            }
            Command::RestartAt { project, terminal } => self.restart(snap, &project, &terminal),
            Command::TogglePause => {
                if let Some(workspace) = &self.workspace {
                    let switch = workspace.pause_switch();
                    if switch.is_paused() {
                        switch.resume();
                    } else {
                        switch.pause();
                    }
                }
            }
            Command::StopEverything => self.with_workspace(|workspace| workspace.stop_everything()),
            Command::ToggleSidebar => {
                let collapsed = snap.ws.sidebar_collapsed;
                self.with_workspace(|workspace| workspace.set_sidebar_collapsed(!collapsed));
            }
            Command::NextTerminal => self.step(snap, 1),
            Command::PreviousTerminal => self.step(snap, -1),
            Command::NextNeedingMe => self.next_needing_me(snap),
            Command::GotoProject(index) => {
                if let Some(project) = snap.project(index) {
                    self.activate(snap, &project.id);
                }
            }
            Command::Show { project, terminal } => {
                // A sidebar click shows the terminal: no sheet stays over it.
                self.close_sheets();
                self.show(snap, &project, &terminal);
            }
            Command::Activate(id) => {
                self.close_sheets();
                self.activate(snap, &id);
            }
            Command::SetExpanded(id, expanded) => {
                self.with_workspace(|workspace| workspace.set_expanded(&id, expanded));
            }
        }
        let _ = ctx;
    }

    /// Static shells: only switching between them applies.
    fn run_static(&mut self, command: &Command) {
        let count = self.static_panes.len();
        if count == 0 {
            return;
        }
        match command {
            Command::NextTerminal => self.visible = (self.visible + 1) % count,
            Command::PreviousTerminal => self.visible = (self.visible + count - 1) % count,
            _ => return,
        }
        self.focus_visible = true;
    }

    fn with_workspace(&self, action: impl FnOnce(&dyn WorkspaceControl)) {
        if let Some(workspace) = &self.workspace {
            action(workspace.as_ref());
        }
    }

    fn open_project(&mut self) {
        if let Some(path) = (self.picker)() {
            self.open_path(&path);
        }
    }

    fn open_path(&mut self, path: &Path) {
        if let Some(workspace) = &self.workspace {
            let known = workspace
                .view()
                .projects
                .iter()
                .map(|project| project.id.clone())
                .collect();
            self.request_error = workspace.open_project(path).err();
            self.new_project = self.request_error.is_none().then(|| NewProject {
                known,
                since: Instant::now(),
            });
            self.focus_visible = true;
        }
    }

    fn close_project(&mut self, id: &str) {
        if let Some(workspace) = &self.workspace {
            self.request_error = workspace.close_project(id).err();
            self.selected.remove(id);
        }
    }

    fn activate(&mut self, snap: &Snapshot, id: &str) {
        self.utility_view = None;
        if snap.ws.active.as_deref() != Some(id) {
            self.with_workspace(|workspace| workspace.activate(id));
        }
        self.focus_visible = true;
    }

    /// Show a terminal. Nothing is sent to it, and nothing starts or stops.
    fn show(&mut self, snap: &Snapshot, project: &str, terminal: &str) {
        self.activate(snap, project);
        self.selected
            .insert(project.to_owned(), terminal.to_owned());
        self.with_workspace(|workspace| workspace.set_active_terminal(project, terminal));
    }

    fn terminals_of(&self, snap: &Snapshot, project: &str) -> Option<Arc<dyn TerminalsControl>> {
        let _ = self;
        snap.index_of(project)
            .and_then(|index| snap.project(index))
            .and_then(|project| project.terminals.clone())
    }

    fn begin_project_rename(&mut self, project: &ProjectView) {
        if self
            .workspace
            .as_ref()
            .is_some_and(|w| w.view().sidebar_collapsed)
        {
            self.with_workspace(|workspace| workspace.set_sidebar_collapsed(false));
        }
        self.rename = Some(Rename::new(
            RenameTarget::Project(project.id.clone()),
            &project.name,
        ));
    }

    fn begin_terminal_rename(&mut self, snap: &Snapshot, project: &str, terminal: &str) {
        let current = snap
            .index_of(project)
            .and_then(|index| snap.view(index))
            .and_then(|view| view.terminals.iter().find(|entry| entry.id == terminal))
            .map(|entry| entry.name.clone().unwrap_or_default());
        if let Some(current) = current {
            // The field lives in the sidebar row, so the row must show.
            if snap.ws.sidebar_collapsed {
                self.with_workspace(|workspace| workspace.set_sidebar_collapsed(false));
            }
            self.activate(snap, project);
            if snap
                .index_of(project)
                .and_then(|i| snap.project(i))
                .is_some_and(|p| !p.expanded)
            {
                self.with_workspace(|workspace| workspace.set_expanded(project, true));
            }
            let parent = snap
                .index_of(project)
                .and_then(|index| snap.view(index))
                .and_then(|view| view.terminals.iter().find(|entry| entry.id == terminal))
                .and_then(|entry| entry.parent.clone());
            if let Some(parent) = parent {
                self.folded.remove(&(project.to_owned(), parent));
            }
            self.rename = Some(Rename::new(
                RenameTarget::Terminal {
                    project: project.to_owned(),
                    terminal: terminal.to_owned(),
                },
                &current,
            ));
        }
    }

    /// Close every sheet and popover: they share the main area.
    fn close_sheets(&mut self) {
        self.history = None;
        self.chooser = None;
        self.utility = None;
        self.automations = None;
        self.skills = None;
        self.settings = None;
        self.host_sheet = None;
        self.project_settings = None;
    }

    fn open_automations(&mut self) {
        self.close_sheets();
        self.automations = Some(AutomationsSheet::default());
    }

    fn open_project_settings(&mut self, project: &str) {
        self.close_sheets();
        self.project_settings = Some(ProjectSettingsSheet::new(project.to_owned()));
    }

    fn open_chooser(&mut self, project: &str) {
        self.close_sheets();
        self.chooser = Some(Chooser::new(project.to_owned()));
    }

    fn add_terminal(
        &mut self,
        snap: &Snapshot,
        index: usize,
        tool: ToolKind,
        name: Option<String>,
    ) {
        let Some(project) = snap.project(index) else {
            return;
        };
        let Some(terminals) = project.terminals.clone() else {
            return;
        };
        let project_id = project.id.clone();
        if !self.tool_allowed(snap, &project_id, tool) {
            return;
        }
        let known = snap
            .view(index)
            .map(|view| {
                view.terminals
                    .iter()
                    .map(|entry| entry.id.clone())
                    .collect()
            })
            .unwrap_or_default();
        self.request_error = terminals.add(tool, name).err();
        if self.request_error.is_none() {
            self.pending_new = Some(PendingNew {
                project: project.id.clone(),
                known,
                since: Instant::now(),
            });
        }
        self.focus_visible = true;
    }

    fn close_terminal(&mut self, snap: &Snapshot, project: &str, terminal: &str) {
        let Some(terminals) = self.terminals_of(snap, project) else {
            return;
        };
        // The neighbour in the tree takes its place on screen.
        if let Some(view) = snap.index_of(project).and_then(|index| snap.view(index))
            && self.shown(snap) == Some((project.to_owned(), terminal.to_owned()))
        {
            let order = tree::order(view);
            if let Some(at) = order.iter().position(|node| node.entry.id == terminal) {
                let next = order
                    .get(at + 1)
                    .filter(|node| {
                        node.entry.parent.as_deref() == order[at].entry.parent.as_deref() || at == 0
                    })
                    .or_else(|| at.checked_sub(1).and_then(|before| order.get(before)))
                    .or_else(|| order.get(at + 1));
                if let Some(next) = next {
                    self.selected
                        .insert(project.to_owned(), next.entry.id.clone());
                }
            }
        }
        self.request_error = terminals.close(terminal).err();
        self.focus_visible = true;
    }

    /// Close the finished children of `parent`, or of every terminal in the
    /// project when the shown one is itself a child.
    fn close_finished(&mut self, snap: &Snapshot, project: &str, parent: Option<&str>) {
        let Some(terminals) = self.terminals_of(snap, project) else {
            return;
        };
        let Some(view) = snap.index_of(project).and_then(|index| snap.view(index)) else {
            return;
        };
        // On a child, its parent's finished children are meant.
        let parent = parent
            .and_then(|id| view.terminals.iter().find(|entry| entry.id == id))
            .map(|entry| entry.parent.as_deref().unwrap_or(&entry.id));
        let finished: Vec<String> = view
            .terminals
            .iter()
            .filter(|entry| is_finished_child(entry))
            .filter(|entry| parent.is_none_or(|parent| entry.parent.as_deref() == Some(parent)))
            .map(|entry| entry.id.clone())
            .collect();
        for id in finished {
            if let Err(error) = terminals.close(&id) {
                self.request_error = Some(error);
                break;
            }
        }
    }

    /// Whether the project's settings allow `tool`; sets the error if not.
    fn tool_allowed(&mut self, snap: &Snapshot, project: &str, tool: ToolKind) -> bool {
        if !tool.is_agent() {
            return true;
        }
        if self.agents.inactive().contains(&tool) {
            self.request_error = Some(format!(
                "{} is deactivated. Activate it in Settings > Agents to use it.",
                tool.label()
            ));
            return false;
        }
        if self.packages.allowed_tools(project).contains(&tool) {
            return true;
        }
        let name = snap
            .index_of(project)
            .and_then(|index| snap.project(index))
            .map_or(project, |p| p.name.as_str());
        self.request_error = Some(format!(
            "{} is turned off in the settings of {name}.",
            tool.label()
        ));
        false
    }

    fn restart(&mut self, snap: &Snapshot, project: &str, terminal: &str) {
        let tool = snap
            .index_of(project)
            .and_then(|index| snap.view(index))
            .and_then(|view| view.terminals.iter().find(|e| e.id == terminal))
            .map(|entry| entry.tool);
        if let Some(tool) = tool
            && !self.tool_allowed(snap, project, tool)
        {
            return;
        }
        if let Some(terminals) = self.terminals_of(snap, project) {
            self.request_error = terminals.restart(terminal).err();
        }
    }

    fn toggle_history(&mut self, snap: &Snapshot, project: Option<String>) {
        let scope_project = project.clone().or_else(|| {
            snap.active()
                .and_then(|i| snap.project(i))
                .map(|p| p.id.clone())
        });
        let scope = scope_project.map_or(Scope::All, Scope::This);
        if self
            .history
            .as_ref()
            .is_some_and(|open| open.scope == scope)
        {
            self.history = None;
            self.focus_visible = true;
        } else {
            self.close_sheets();
            self.history = Some(History::new(scope));
        }
    }

    /// Next or previous terminal of the project on show, in tree order.
    fn step(&mut self, snap: &Snapshot, delta: isize) {
        let Some(index) = snap.active() else { return };
        let (Some(view), Some(project)) = (snap.view(index), snap.project(index)) else {
            return;
        };
        let order = tree::order(view);
        if order.is_empty() {
            return;
        }
        let shown = self.shown_in(snap, index);
        let at = shown
            .and_then(|id| order.iter().position(|node| node.entry.id == id))
            .unwrap_or(0);
        let next = (at.cast_signed() + delta).rem_euclid(order.len().cast_signed());
        if let Some(node) = order.get(next.cast_unsigned()) {
            let (project, id) = (project.id.clone(), node.entry.id.clone());
            self.show(snap, &project, &id);
        }
    }

    /// The next terminal, across projects, that waits for the user.
    fn next_needing_me(&mut self, snap: &Snapshot) {
        let mut all: Vec<Key> = Vec::new();
        let mut needing: Vec<bool> = Vec::new();
        for (index, project) in snap.ws.projects.iter().enumerate() {
            let Some(view) = snap.view(index) else {
                continue;
            };
            for node in tree::order(view) {
                all.push((project.id.clone(), node.entry.id.clone()));
                needing.push(tree::terminal_dot(&self.seen, &project.id, node.entry).needs_me());
            }
        }
        if all.is_empty() {
            return;
        }
        let start = self
            .shown(snap)
            .and_then(|key| all.iter().position(|candidate| *candidate == key))
            .map_or(0, |at| at + 1);
        let found = (0..all.len())
            .map(|offset| (start + offset) % all.len())
            .find(|&at| needing[at] && self.shown(snap).as_ref() != Some(&all[at]));
        if let Some(at) = found {
            let (project, terminal) = all[at].clone();
            self.show(snap, &project, &terminal);
        }
    }

    // ---------- frame ----------

    /// Draw the window and route this frame's input to the terminal on show.
    pub(crate) fn draw(&mut self, ui: &mut Ui) {
        let ctx = ui.ctx().clone();
        let cell = *self.cell.get_or_insert_with(|| CellMetrics::measure(&ctx));
        let mut events = ui.input(|input| input.events.clone());
        // A shortcut is taken out of the events whoever acts on it, so it can
        // never reach a terminal. With a native menu installed the menu owns
        // the chord and acts, so a copy that still arrives here is dropped.
        let shortcuts = commands::take(&mut events);
        let mut pending = std::mem::take(&mut self.queued);
        if !self.menu_owns_shortcuts {
            pending.extend(shortcuts);
        }
        if self.workspace.is_none() {
            self.draw_static(ui, cell, events, pending);
            return;
        }
        let mut snap = self.snapshot();
        if self.execute(&ctx, &snap, pending) {
            snap = self.snapshot();
        }
        self.reconcile(&snap);
        self.draw_workspace(ui, cell, events, snap);
    }

    fn snapshot(&self) -> Snapshot {
        match &self.workspace {
            Some(workspace) => {
                let local = Snapshot::take(workspace.as_ref());
                let remote = self.host.as_deref().and_then(|name| {
                    let row = self
                        .remote
                        .view()
                        .hosts
                        .iter()
                        .find(|h| h.name == name)
                        .cloned()?;
                    Some((row, self.remote.terminals(name)?))
                });
                match remote {
                    Some((row, terminals)) => local.for_host(&row, terminals),
                    None => local,
                }
            }
            None => Snapshot {
                ws: Arc::default(),
                views: Vec::new(),
                utility: Vec::new(),
            },
        }
    }

    fn draw_static(
        &mut self,
        ui: &mut Ui,
        cell: CellMetrics,
        events: Vec<Event>,
        commands: Vec<Command>,
    ) {
        let ctx = ui.ctx().clone();
        for command in commands {
            self.run_static(&command);
        }
        let area = ui.max_rect();
        let area = self.banner(ui, area);
        let (head, content) = area.split_top_bottom_at_y(area.min.y + HEADER_HEIGHT);
        self.visible = self.visible.min(self.static_panes.len().saturating_sub(1));
        let tabs = (self.static_panes.len() > 1).then(|| {
            (
                self.static_panes
                    .iter()
                    .map(|pane| pane.label.clone())
                    .collect(),
                self.visible,
            )
        });
        let status = self.static_panes.get(self.visible).map(Pane::status_line);
        let input = HeadInput { status, tabs };
        let mut out = HeadOut::default();
        header::header(ui, head, &input, &mut out);
        if let Some(tab) = out.tab {
            self.visible = tab;
            self.focus_visible = true;
        }
        if out.activating.is_some() {
            self.activating_key = out.activating;
        }
        #[cfg(test)]
        {
            self.hits.head = out;
            self.hits.content = content;
        }
        let pane_id = self.static_panes.get(self.visible).map(|pane| pane.id);
        let input = self.take_input(ui, pane_id, events);
        let dropped = self.dropped_on(ui, content);
        if let Some(pane) = self.static_panes.get_mut(self.visible) {
            pane.drop_files(&dropped);
            pane.draw(ui, content, cell, &input, None);
        }
        let _ = ctx;
    }

    /// Paths of files dropped on the window this frame, when they landed on
    /// the terminal area `content` and no field or sheet is open.
    fn dropped_on(&self, ui: &Ui, content: Rect) -> Vec<PathBuf> {
        if self.modal_open() || self.modal_at_start {
            return Vec::new();
        }
        ui.input(|input| {
            let over = input.pointer.hover_pos().or(input.pointer.latest_pos());
            if over.is_some_and(|pos| !content.contains(pos)) {
                return Vec::new();
            }
            input
                .raw
                .dropped_files
                .iter()
                .map(|file| file.path().to_path_buf())
                .collect()
        })
    }

    /// Hand the keyboard to the terminal on show when asked, and the events it
    /// may take: none while a field or sheet is open.
    fn take_input(&mut self, ui: &Ui, pane: Option<Id>, events: Vec<Event>) -> Vec<Event> {
        // A pointer event makes egui drop focus from any widget it is not over,
        // so the terminal asks on the frame after the click.
        let pointer_event =
            ui.input(|input| input.pointer.any_pressed() || input.pointer.any_released());
        let modal = self.modal_open();
        let handed_over = self.focus_visible && !pointer_event && !modal;
        if handed_over {
            self.focus_visible = false;
            if let Some(id) = pane {
                ui.ctx().memory_mut(|memory| memory.request_focus(id));
            }
        }
        let activating = self.activating_key.take();
        // An open menu of the in-window bar takes the frame's keys and
        // clicks, as a native menu would.
        if modal || self.modal_at_start || self.bar.blocks_input() {
            return Vec::new();
        }
        // The key that chose a control, such as Enter or Space, was not meant
        // for the terminal. Everything else in the frame was.
        match activating {
            Some(key) if handed_over => drop_activation(events, key),
            _ => events,
        }
    }

    #[cfg(feature = "test-endpoints")]
    fn banner(&self, ui: &Ui, area: Rect) -> Rect {
        let (banner, rest) = area.split_top_bottom_at_y(area.min.y + BANNER_HEIGHT);
        let painter = ui.painter_at(banner);
        painter.rect_filled(banner, 0.0, theme::CORAL);
        painter.text(
            banner.center(),
            Align2::CENTER_CENTER,
            "FAKE MODEL",
            FontId::proportional(13.0),
            theme::ON_CYAN,
        );
        rest.intersect(area)
    }

    #[cfg(not(feature = "test-endpoints"))]
    #[allow(clippy::unused_self)]
    fn banner(&self, _ui: &Ui, area: Rect) -> Rect {
        area
    }

    fn draw_workspace(
        &mut self,
        ui: &mut Ui,
        cell: CellMetrics,
        events: Vec<Event>,
        snap: Snapshot,
    ) {
        let ctx = ui.ctx().clone();
        // Escape belongs to the dialog over everything, when one is open: the
        // sheets below must not also take it.
        let esc = self.overlay_open()
            && ctx.input_mut(|i| i.consume_key(egui::Modifiers::NONE, egui::Key::Escape));
        self.modal_at_start = self.modal_open();
        let time = ui.input(|input| input.time);
        let area = self.banner(ui, ui.max_rect());
        let paused = self.with_pause(|switch| switch.is_paused());
        // The in-window menu bar (none on macOS) above everything else. Its
        // clicks run on the next pass, as a native menu's do.
        let bar = self.bar.draw(ui, area, &snap.ws.recents, paused);
        let area = self.update_strip(ui, bar.rest);
        self.queued.extend(bar.commands);
        if bar.closed {
            self.focus_visible = true;
        }
        let collapsed = snap.ws.sidebar_collapsed;
        let side_width = if collapsed { RAIL_WIDTH } else { SIDEBAR_WIDTH };
        let (side, main) = area.split_left_right_at_x(area.min.x + side_width);
        let active = snap.active();
        let on_utility = self.utility_shown(&snap);
        let shown_id = active
            .filter(|_| on_utility.is_none())
            .and_then(|index| self.shown_in(&snap, index));
        let shown_key = on_utility.clone().or_else(|| self.shown(&snap));
        // Looking at a terminal clears its finished mark.
        if let (Some(key), Some(index)) = (&shown_key, active)
            && let Some(entry) = snap
                .view(index)
                .and_then(|view| view.terminals.iter().find(|entry| entry.id == key.1))
        {
            self.seen.insert(key.clone(), entry.finished_turns);
        }
        if self.last_shown != shown_key {
            self.last_shown.clone_from(&shown_key);
            self.focus_visible = true;
        }
        if let Some(menu) = &mut self.menu {
            menu.sync(&snap.ws.recents, paused);
        }

        // The pulse repaints only while something works.
        let working = snap.ws.projects.iter().enumerate().any(|(index, project)| {
            snap.view(index).is_some_and(|view| {
                view.terminals
                    .iter()
                    .any(|entry| tree::terminal_dot(&self.seen, &project.id, entry).is_working())
            })
        });
        let working = working
            || snap
                .utility
                .iter()
                .flat_map(|runs| runs.iter())
                .any(|run| matches!(run.status, UtilityStatus::Running));
        if working {
            ctx.request_repaint_after(dots::PULSE_FRAME);
        }

        // The sidebar.
        let remote_view = self.remote.view();
        let input = TreeInput {
            snap: &snap,
            active,
            shown_terminal: shown_id.as_deref(),
            seen: &self.seen,
            collapsed_kids: &self.folded,
            paused,
            history_open: self.history.is_some(),
            titling: &self.titling,
            time,
            hosts: &remote_view.hosts,
            host: self.host.as_deref(),
        };
        let mut tree_out = TreeOut::default();
        if collapsed {
            tree::rail(ui, side, &input, &mut tree_out);
        } else {
            tree::sidebar(ui, side, &input, self.rename.as_mut(), &mut tree_out);
        }

        let below = main;

        // What the draw asked for.
        let commands = std::mem::take(&mut tree_out.commands);
        for key in tree_out.toggled_kids.drain(..) {
            if !self.folded.remove(&key) {
                self.folded.insert(key);
            }
        }
        self.new_rows = std::mem::take(&mut tree_out.new_rows);
        if let Some(end) = tree_out.rename_end.take() {
            self.finish_rename(&ctx, &snap, end);
        } else if self.rename.as_ref().is_some_and(|rename| rename.age() >= 2)
            && !tree_out.rename_drawn
        {
            // Nothing drew the field (a rail, a folded project): do not leave
            // the keyboard with something invisible.
            self.rename = None;
            self.focus_visible = true;
        }
        // A control the keyboard activated, such as a row on Enter: the key
        // was not meant for the terminal.
        if !commands.is_empty() {
            let pane_id = shown_key
                .as_ref()
                .and_then(|key| self.panes.get(key))
                .map(|pane| pane.id);
            let on_sidebar = ctx
                .memory(|memory| memory.focused())
                .is_some_and(|id| Some(id) != pane_id);
            if on_sidebar {
                self.activating_key = ctx.input(|input| {
                    [egui::Key::Enter, egui::Key::Space]
                        .into_iter()
                        .find(|key| input.key_pressed(*key))
                });
            }
        }
        self.execute(&ctx, &snap, commands);
        let snap = self.snapshot();
        self.reconcile(&snap);

        // Notices, then the content.
        let notices = self.notices(&snap);
        let painter = ui.painter_at(below);
        let galleys: Vec<_> = notices
            .into_iter()
            .map(|(text, color)| {
                (
                    painter.layout(
                        text,
                        FontId::proportional(12.0),
                        color,
                        below.width() - 28.0,
                    ),
                    color,
                )
            })
            .collect();
        let notice_height: f32 = galleys.iter().map(|(g, _)| g.size().y + NOTICE_LINE).sum();
        let mut at = below.min.y + 2.0;
        if notice_height > 0.0 {
            painter.rect_filled(
                Rect::from_min_size(below.min, vec2(below.width(), notice_height + 4.0)),
                0.0,
                theme::WINDOW_BG,
            );
        }
        for (galley, color) in galleys {
            let height = galley.size().y + NOTICE_LINE;
            painter.galley(pos2(below.min.x + 14.0, at), galley, color);
            at += height;
        }
        let content = Rect::from_min_max(
            pos2(
                below.min.x,
                (below.min.y
                    + if notice_height > 0.0 {
                        notice_height + 4.0
                    } else {
                        0.0
                    })
                .min(below.max.y),
            ),
            below.max,
        );
        #[cfg(test)]
        {
            self.hits.tree = std::mem::take(&mut tree_out.hits);
            self.hits.host_rows = std::mem::take(&mut tree_out.host_rects);
            self.hits.content = content;
            self.hits.sidebar = side;
            self.hits.main = main;
            self.hits.notice_height = notice_height;
        }

        let content = self.utility_banner(ui, content, &snap);
        let key = self.draw_content(ui, content, &snap);

        // Overlays sit above everything else.
        self.draw_chooser(&ctx, &snap, main);
        self.draw_history(&ctx, &snap, main);
        self.draw_utility(&ctx, &snap, main);
        self.draw_automations(&ctx, main);
        self.draw_skills(&ctx, main);
        self.follow_agent_signins();
        self.draw_settings(&ctx, main);
        self.draw_host_sheet(&ctx, main);
        self.draw_project_settings(&ctx, &snap, main);
        self.draw_overlays(&ctx, esc);

        // The terminal last, so a sheet closed by this frame's Enter has
        // already given the keyboard back.
        let pane_id = key
            .as_ref()
            .and_then(|key| self.panes.get(key))
            .map(|pane| pane.id);
        let input = self.take_input(ui, pane_id, events);
        let dropped = self.dropped_on(ui, content);
        if let Some(pane) = key.as_ref().and_then(|key| self.panes.get_mut(key)) {
            pane.drop_files(&dropped);
        }
        let restart = key.as_ref().and_then(|(project, terminal)| {
            let view = snap.index_of(project).and_then(|index| snap.view(index))?;
            let entry = view.terminals.iter().find(|entry| entry.id == *terminal)?;
            match entry.state {
                TermActivity::Exited => Some("Exited"),
                TermActivity::Failed => Some("Failed"),
                TermActivity::NotStarted => Some("Not started"),
                _ => None,
            }
        });
        // A saved terminal that waited for someone to want it starts when it
        // is looked at. Once: a start that fails leaves the button to say so.
        if let Some((project, terminal)) = key.clone()
            && snap.active().is_some()
            && snap
                .index_of(&project)
                .and_then(|index| snap.view(index))
                .and_then(|view| view.terminals.iter().find(|e| e.id == terminal))
                .is_some_and(|e| e.state == TermActivity::NotStarted && !e.desired_running)
            && self.woken.insert((project.clone(), terminal.clone()))
        {
            self.restart(&snap, &project, &terminal);
        }
        let restart_key = key.clone();
        // A tool the project turned off is neither logged in nor restarted.
        let tool_off = key.as_ref().is_some_and(|(project, terminal)| {
            snap.index_of(project)
                .and_then(|index| snap.view(index))
                .and_then(|view| view.terminals.iter().find(|e| e.id == *terminal))
                .is_some_and(|entry| !self.packages.allowed_tools(project).contains(&entry.tool))
        });
        if let Some(pane) = key.as_ref().and_then(|key| self.panes.get_mut(key)) {
            pane.tool_off = tool_off;
        }
        let clicked = key
            .and_then(|key| self.panes.get_mut(&key))
            .filter(|_| snap.active().is_some())
            .is_some_and(|pane| pane.draw(ui, content, cell, &input, restart));
        if clicked && let Some((project, terminal)) = restart_key {
            self.restart(&snap, &project, &terminal);
        }
    }

    fn with_pause<R>(&self, read: impl FnOnce(&connecto_core::PauseSwitch) -> R) -> R {
        match &self.workspace {
            Some(workspace) => read(&workspace.pause_switch()),
            None => read(&connecto_core::PauseSwitch::default()),
        }
    }

    fn finish_rename(&mut self, ctx: &egui::Context, snap: &Snapshot, end: RenameEnd) {
        let Some(rename) = self.rename.take() else {
            return;
        };
        self.focus_visible = true;
        self.activating_key = ctx.input(|input| {
            [egui::Key::Enter, egui::Key::Escape]
                .into_iter()
                .find(|key| input.key_pressed(*key))
        });
        let RenameEnd::Save(name) = end else { return };
        self.request_error = match rename.target {
            RenameTarget::Terminal { project, terminal } => self
                .terminals_of(snap, &project)
                .and_then(|terminals| terminals.rename(&terminal, name).err()),
            RenameTarget::Project(id) => self
                .workspace
                .as_ref()
                .and_then(|workspace| workspace.rename_project(&id, name).err()),
        };
    }

    /// The main area: the terminal, or what stands in for it. Returns the
    /// terminal to draw last.
    fn draw_content(&mut self, ui: &mut Ui, content: Rect, snap: &Snapshot) -> Option<Key> {
        let Some(index) = snap.active() else {
            self.empty_workspace(ui, content, snap);
            return None;
        };
        let project = snap.project(index)?;
        if let Some(error) = &project.error {
            self.message(
                ui,
                content,
                &format!("{} could not be opened", project.name),
                error,
                true,
            );
            return None;
        }
        let Some(view) = snap.view(index) else {
            self.message(
                ui,
                content,
                &format!("Opening {}…", project.name),
                "",
                false,
            );
            return None;
        };
        if let Some(key) = self.utility_shown(snap) {
            return Some(key);
        }
        if view.terminals.is_empty() {
            self.empty_project(ui, content, snap, index);
            return None;
        }
        self.shown(snap)
    }

    /// How many agents of this machine are in a turn now.
    fn working_turns(&self) -> usize {
        let Some(workspace) = &self.workspace else {
            return 0;
        };
        Snapshot::take(workspace.as_ref())
            .views
            .iter()
            .flatten()
            .flat_map(|view| view.terminals.iter())
            .filter(|entry| entry.state == connecto_core::TermActivity::Working)
            .count()
    }

    /// The update strip at the top, when it has something to say. Returns
    /// the room left below it.
    fn update_strip(&mut self, ui: &mut Ui, area: Rect) -> Rect {
        let ctx = ui.ctx().clone();
        let view = self.updates.view();
        let waiting = self.update_strip.waiting;
        let working = if waiting { self.working_turns() } else { 0 };
        if waiting && working == 0 {
            self.restart_to_update(&ctx);
        }
        let Some(line) = crate::update_strip::line_for(&view, &self.update_strip, working) else {
            #[cfg(test)]
            {
                self.hits.update_strip = crate::forms::Marks::default();
            }
            return area;
        };
        if self.update_strip.waiting {
            // Look again for finished turns even when nothing else repaints.
            ctx.request_repaint_after(std::time::Duration::from_secs(1));
        }
        let mut out = crate::forms::Out::default();
        let rest = crate::update_strip::draw(ui, area, &line, &mut out);
        #[cfg(test)]
        {
            self.hits.update_strip = std::mem::take(&mut out.marks);
        }
        for action in out.actions {
            use crate::update_strip::StripAction;
            match action {
                StripAction::Install => {
                    if self.working_turns() == 0 {
                        self.restart_to_update(&ctx);
                    } else {
                        self.update_strip.waiting = true;
                    }
                }
                StripAction::Later => {
                    self.update_strip.later = match &view.status {
                        connecto_core::UpdateStatus::Ready { version }
                        | connecto_core::UpdateStatus::Manual { version, .. } => {
                            Some(version.clone())
                        }
                        _ => None,
                    };
                }
                StripAction::RestartNow => self.restart_to_update(&ctx),
                StripAction::CancelWait => self.update_strip.waiting = false,
                StripAction::Dismiss => {
                    if view.notice.is_some() {
                        self.updates.dismiss_notice();
                    } else if self.update_strip.error.is_some() {
                        self.update_strip.error = None;
                    } else {
                        self.update_strip.closed_asked = true;
                    }
                }
                StripAction::OpenSite => {
                    ctx.open_url(egui::OpenUrl::new_tab("https://connecto.studio/#download"));
                }
            }
        }
        rest
    }

    /// Hand the update over and quit: the installer starts the new version,
    /// which restores the terminals as at any start.
    fn restart_to_update(&mut self, ctx: &egui::Context) {
        self.update_strip.waiting = false;
        match self.updates.install_and_restart() {
            Ok(()) => ctx.send_viewport_cmd(egui::ViewportCommand::Close),
            Err(why) => self.update_strip.error = Some(why),
        }
    }

    /// A strip above a utility session saying what it is. Returns the room left
    /// for the terminal.
    fn utility_banner(&mut self, ui: &mut Ui, content: Rect, snap: &Snapshot) -> Rect {
        let Some(view) = self.utility_view.as_ref() else {
            return content;
        };
        if self.utility_shown(snap).is_none() {
            return content;
        }
        let bar = Rect::from_min_size(content.min, vec2(content.width(), 28.0));
        let painter = ui.painter_at(bar);
        painter.rect_filled(bar, 0.0, theme::PANEL_BG);
        painter.hline(
            bar.x_range(),
            bar.max.y - 0.5,
            Stroke::new(1.0, theme::BORDER),
        );
        painter.text(
            pos2(bar.min.x + 14.0, bar.center().y),
            Align2::LEFT_CENTER,
            "UTILITY SESSION",
            FontId::monospace(10.5),
            theme::AMBER,
        );
        let galley = painter.layout_no_wrap(
            view.name.clone(),
            FontId::proportional(12.0),
            theme::TEXT_ROW,
        );
        painter.galley(
            pos2(bar.min.x + 130.0, bar.center().y - galley.size().y / 2.0),
            galley,
            theme::TEXT_ROW,
        );
        painter.text(
            pos2(bar.max.x - 14.0, bar.center().y),
            Align2::RIGHT_CENTER,
            "select a terminal to go back",
            FontId::proportional(11.0),
            theme::FAINT,
        );
        #[cfg(test)]
        {
            self.hits.utility_banner = Some(bar);
        }
        Rect::from_min_max(
            pos2(content.min.x, (bar.max.y).min(content.max.y)),
            content.max,
        )
    }

    fn message(&mut self, ui: &mut Ui, area: Rect, title: &str, detail: &str, error: bool) {
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, theme::TERM_BG);
        let color = if error { theme::ERROR } else { Color32::WHITE };
        painter.text(
            area.center() - vec2(0.0, 12.0),
            Align2::CENTER_CENTER,
            title,
            FontId::proportional(16.0),
            color,
        );
        if !detail.is_empty() {
            let galley = painter.layout(
                detail.to_owned(),
                FontId::proportional(12.5),
                theme::TEXT_MUTED,
                (area.width() - 64.0).clamp(80.0, 520.0),
            );
            let size = galley.size();
            painter.galley(
                pos2(area.center().x - size.x / 2.0, area.center().y + 10.0),
                galley,
                theme::TEXT_MUTED,
            );
        }
    }

    /// No project is open: invite one, and list the recent ones (R22).
    fn empty_workspace(&mut self, ui: &mut Ui, area: Rect, snap: &Snapshot) {
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, theme::TERM_BG);
        let mut y = (area.center().y - 130.0).max(area.min.y + 40.0);
        let x = area.center().x;
        painter.text(
            pos2(x, y),
            Align2::CENTER_TOP,
            "Open a project",
            FontId::proportional(22.0),
            Color32::WHITE,
        );
        y += 36.0;
        let galley = painter.layout(
            crate::platform::OPEN_PROJECT_INVITE.to_owned(),
            FontId::proportional(13.0),
            theme::TEXT_MUTED,
            (area.width() - 64.0).clamp(120.0, 420.0),
        );
        let height = galley.size().y;
        painter.galley(
            pos2(x - galley.size().x / 2.0, y),
            galley,
            theme::TEXT_MUTED,
        );
        y += height + 18.0;
        // Wider off macOS, where the shortcut reads Ctrl+Shift+O, not ⌘O.
        let width = if cfg!(target_os = "macos") {
            190.0
        } else {
            240.0
        };
        let button = Rect::from_center_size(pos2(x, y + 16.0), vec2(width, 32.0));
        let response = ui.put(
            button,
            theme::primary(concat!("Open a project   ", crate::platform::kbd!("O"))),
        );
        #[cfg(test)]
        {
            self.hits.open_button = Some(response.rect);
        }
        if response.clicked() {
            self.queued.push(Command::OpenProject);
        }
        y += 56.0;
        #[cfg(test)]
        self.hits.recent_rows.clear();
        if snap.ws.recents.is_empty() {
            return;
        }
        painter.text(
            pos2(x - 210.0, y),
            Align2::LEFT_TOP,
            "RECENT",
            FontId::monospace(10.5),
            theme::FAINT,
        );
        y += 22.0;
        for (index, recent) in snap.ws.recents.iter().take(6).enumerate() {
            let row = Rect::from_min_size(pos2(x - 210.0, y), vec2(420.0, 40.0));
            let response = ui.interact(row, Id::unique(("recent", index)), Sense::click());
            if response.hovered() {
                painter.rect_filled(row, 8.0, theme::ROW_HOVER);
            }
            painter.text(
                row.min + vec2(12.0, 8.0),
                Align2::LEFT_TOP,
                &recent.name,
                FontId::proportional(13.0),
                theme::TEXT,
            );
            let path = tree::fit(
                &painter,
                &recent.path,
                FontId::monospace(11.0),
                theme::FAINT,
                row.width() - 24.0,
            );
            painter.galley(row.min + vec2(12.0, 24.0), path, theme::FAINT);
            #[cfg(test)]
            self.hits.recent_rows.push(row);
            let label = recent.name.clone();
            response.widget_info(|| {
                egui::WidgetInfo::labeled(
                    egui::accesskit::Role::Button,
                    true,
                    format!("Open {label}"),
                )
            });
            if response.clicked() {
                self.queued.push(Command::OpenRecent(index));
            }
            y += 42.0;
        }
    }

    /// A project without terminals offers the chooser in place of one (R22).
    fn empty_project(&mut self, ui: &mut Ui, area: Rect, snap: &Snapshot, index: usize) {
        let Some(project) = snap.project(index) else {
            return;
        };
        let painter = ui.painter_at(area);
        painter.rect_filled(area, 0.0, theme::TERM_BG);
        painter.text(
            pos2(
                area.center().x,
                (area.center().y - 130.0).max(area.min.y + 30.0),
            ),
            Align2::CENTER_TOP,
            format!("{} has no terminals", project.name),
            FontId::proportional(18.0),
            Color32::WHITE,
        );
        if self
            .inline_chooser
            .as_ref()
            .is_none_or(|chooser| chooser.project != project.id)
        {
            self.inline_chooser = Some(Chooser::new(project.id.clone()));
        }
        let keys = !self.modal_open();
        let info = self.chooser_info(snap, index);
        let top = (area.center().y - 90.0).max(area.min.y + 70.0);
        let card = Rect::from_min_size(
            pos2(area.center().x - chooser::POPOVER_WIDTH / 2.0, top),
            vec2(chooser::POPOVER_WIDTH, 190.0),
        );
        let mut hits = ChooserHits::default();
        let mut end = None;
        if let Some(inline) = &mut self.inline_chooser {
            ui.scope_builder(egui::UiBuilder::new().max_rect(card), |ui| {
                egui::Frame::new()
                    .fill(theme::POPOVER_BG)
                    .stroke(Stroke::new(1.0, theme::POPOVER_BORDER))
                    .corner_radius(10)
                    .inner_margin(6)
                    .show(ui, |ui| {
                        ui.set_width(chooser::POPOVER_WIDTH - 12.0);
                        end = chooser::body(ui, inline, &info.as_info(), keys, false, &mut hits);
                    });
            });
        }
        #[cfg(test)]
        {
            self.hits.inline = hits;
        }
        if let Some(ChooserEnd::Pick(tool, name)) = end {
            self.add_terminal(snap, index, tool, name);
            if let Some(inline) = &mut self.inline_chooser {
                inline.name.clear();
            }
        }
    }

    fn chooser_info(&self, snap: &Snapshot, index: usize) -> OwnedInfo {
        let project = snap.project(index);
        let mut tools = project.map_or_else(
            || vec![ToolKind::Claude, ToolKind::Codex],
            |p| self.packages.allowed_tools(&p.id),
        );
        if tools.is_empty() {
            tools = vec![ToolKind::Claude, ToolKind::Codex];
        }
        // An agent switched off in Settings is not offered for a new terminal.
        let off = self.agents.inactive();
        tools.retain(|tool| !off.contains(tool));
        // A shell is not one of the project's tools: it is always on offer.
        tools.push(ToolKind::Shell);
        let view = snap.view(index);
        let hint = |tool| {
            view.map_or("login checked when it starts", |view| {
                tree::tool_login_hint(view, tool)
            })
        };
        OwnedInfo {
            project_name: project.map(|p| p.name.clone()).unwrap_or_default(),
            logins: [
                hint(ToolKind::Claude),
                hint(ToolKind::Codex),
                "your login shell, ready at once",
            ],
            tools,
            blocked: view.and_then(|view| match view.save {
                SaveState::ReadOnly => Some("The terminal list could not be read."),
                _ if !view.can_add() => Some("A project holds up to 16 terminals."),
                _ => None,
            }),
        }
    }

    fn draw_chooser(&mut self, ctx: &egui::Context, snap: &Snapshot, main: Rect) {
        let Some(project) = self.chooser.as_ref().map(|chooser| chooser.project.clone()) else {
            return;
        };
        let Some(index) = snap.index_of(&project) else {
            self.chooser = None;
            return;
        };
        let info = self.chooser_info(snap, index);
        let anchor = self
            .new_rows
            .get(&project)
            .map_or(pos2(main.min.x + 24.0, main.min.y + 60.0), |row| {
                pos2(row.min.x + 12.0, row.max.y + 4.0)
            });
        let Some(chooser) = &mut self.chooser else {
            return;
        };
        let mut hits = ChooserHits::default();
        let end = chooser::popover(ctx, anchor, chooser, &info.as_info(), &mut hits);
        #[cfg(test)]
        {
            self.hits.chooser = hits;
        }
        let pressed = |key| ctx.input(|input| input.key_pressed(key)).then_some(key);
        match end {
            Some(ChooserEnd::Pick(tool, name)) => {
                self.chooser = None;
                self.activating_key = pressed(egui::Key::Enter);
                self.add_terminal(snap, index, tool, name);
            }
            Some(ChooserEnd::Cancel) => {
                self.chooser = None;
                self.focus_visible = true;
                self.activating_key = pressed(egui::Key::Escape);
            }
            None => {}
        }
    }

    fn draw_automations(&mut self, ctx: &egui::Context, area: Rect) {
        let control = self.automations_control.clone();
        let view = control.as_ref().map(|control| control.view());
        let (now_ms, utc_offset_s) = (self.clock)();
        let input = AutomationsInput {
            view: view.as_deref(),
            control: control.as_deref(),
            now_ms,
            utc_offset_s,
        };
        let mut out = AutomationsOut::default();
        if self.automations.is_none() {
            // Closed: nothing typed in the key field stays recoverable.
            automations::wipe_key_state(ctx);
        }
        if let Some(sheet) = &mut self.automations {
            automations::sheet(ctx, area, sheet, &input, &mut out);
            #[cfg(test)]
            {
                self.hits.automations = std::mem::take(&mut out.hits);
            }
        } else if let Some(view) = view.as_deref() {
            automations::banner(ctx, area, view, &mut out);
            #[cfg(test)]
            {
                self.hits.automations_banner = std::mem::take(&mut out.hits);
            }
        }
        if out.actions.is_empty() {
            return;
        }
        ctx.request_repaint();
        for action in out.actions {
            let close = self.apply_automation(ctx, control.as_deref(), action);
            if close {
                self.automations = None;
                self.focus_visible = true;
                return;
            }
        }
    }

    /// Run one action of the Automations sheet. Returns whether the sheet closes.
    /// The backend is about to show an authoring terminal: let the screen
    /// follow it, whatever was picked before.
    fn follow_backend(&mut self) {
        let snap = self.snapshot();
        self.utility_view = None;
        self.following = Some(Following {
            baseline: snap
                .ws
                .projects
                .iter()
                .map(|project| (project.id.clone(), project.active_terminal.clone()))
                .collect(),
            since: Instant::now(),
        });
    }

    fn apply_automation(
        &mut self,
        ctx: &egui::Context,
        control: Option<&dyn AutomationsControl>,
        action: AutomationsAction,
    ) -> bool {
        match action {
            AutomationsAction::Close => {
                self.activating_key =
                    ctx.input(|i| [egui::Key::Escape].into_iter().find(|k| i.key_pressed(*k)));
                return true;
            }
            AutomationsAction::OpenSheet => self.open_automations(),
            AutomationsAction::SaveKey(key) => {
                let result = control.map(|control| control.set_api_key(&key));
                if let (Some(Err(error)), Some(sheet)) = (result, &mut self.automations) {
                    sheet.key_error = Some(error);
                }
            }
            // The next pass opens it, once this sheet has stopped drawing.
            AutomationsAction::OpenSettings => {
                self.queued.push(Command::SettingsAt(SettingsTab::Home));
                return true;
            }
            AutomationsAction::OpenLogin(tool) => {
                self.follow_backend();
                match control.map(|control| control.open_login(tool)) {
                    Some(Err(error)) => {
                        if let Some(sheet) = &mut self.automations {
                            sheet.root_error = Some(error);
                        }
                    }
                    _ => return true,
                }
            }
            AutomationsAction::CreateDescribed {
                name,
                agent,
                screen,
                action,
            } => {
                self.follow_backend();
                match control
                    .map(|control| control.create_described(&name, agent, &screen, &action))
                {
                    Some(Err(error)) => {
                        if let Some(sheet) = &mut self.automations {
                            sheet.create_error = Some(error);
                        }
                    }
                    _ => {
                        if let Some(sheet) = &mut self.automations {
                            sheet.clear_form();
                        }
                        return true;
                    }
                }
            }
            AutomationsAction::Create(name, agent) => {
                self.follow_backend();
                match control.map(|control| control.create(&name, agent)) {
                    Some(Err(error)) => {
                        if let Some(sheet) = &mut self.automations {
                            sheet.create_error = Some(error);
                        }
                    }
                    // The authoring terminal is now the one to look at.
                    _ => {
                        if let Some(sheet) = &mut self.automations {
                            sheet.clear_form();
                        }
                        return true;
                    }
                }
            }
            AutomationsAction::OpenSession(slug) => {
                self.follow_backend();
                match control.map(|control| control.open_session(&slug)) {
                    Some(Err(error)) => {
                        if let Some(sheet) = &mut self.automations {
                            sheet.error = Some(error);
                        }
                    }
                    _ => return true,
                }
            }
            AutomationsAction::CreateGroup(title) => {
                let result = control.map(|control| control.create_group(&title));
                if let Some(sheet) = &mut self.automations {
                    match result {
                        Some(Err(error)) => sheet.group_error = Some(error),
                        _ => {
                            sheet.new_group = None;
                            sheet.group_error = None;
                        }
                    }
                }
            }
            AutomationsAction::RenameGroup { id, title } => {
                let result = control.map(|control| control.rename_group(&id, &title));
                if let Some(sheet) = &mut self.automations {
                    match result {
                        Some(Err(error)) => sheet.group_error = Some(error),
                        _ => {
                            sheet.renaming_group = None;
                            sheet.group_error = None;
                        }
                    }
                }
            }
            AutomationsAction::DeleteGroup(id) => {
                let result = control.map(|control| control.delete_group(&id));
                if let (Some(Err(error)), Some(sheet)) = (result, &mut self.automations) {
                    sheet.group_error = Some(error);
                }
            }
            AutomationsAction::SetGroup(slug, group) => {
                let result = control.map(|control| control.set_group(&slug, group.as_deref()));
                if let Some(sheet) = &mut self.automations {
                    match result {
                        Some(Err(error)) => sheet.group_error = Some(error),
                        _ => sheet.group_error = None,
                    }
                }
            }
            other => {
                if let Some(control) = control {
                    match other {
                        AutomationsAction::ClearKey => control.clear_api_key(),
                        AutomationsAction::SetEnabled(slug, on) => control.set_enabled(&slug, on),
                        AutomationsAction::SetManyEnabled(slugs, on) => {
                            for slug in slugs {
                                control.set_enabled(&slug, on);
                            }
                        }
                        AutomationsAction::Delete(slug) => control.delete(&slug),
                        AutomationsAction::SetThreshold(slug, value) => {
                            control.set_threshold(&slug, value);
                        }
                        AutomationsAction::Approve(id) => control.approve(id),
                        AutomationsAction::Dismiss(id) => control.dismiss(id),
                        AutomationsAction::ClearHistory => control.clear_history(),
                        AutomationsAction::ClearAudit(slug) => {
                            let result = control.clear_history_for(&slug);
                            if let Some(sheet) = &mut self.automations {
                                match result {
                                    Ok(()) => sheet.refetch_audit(&slug),
                                    Err(error) => sheet.audit_error = Some((slug, error)),
                                }
                            }
                        }
                        _ => {}
                    }
                }
            }
        }
        false
    }

    fn draw_skills(&mut self, ctx: &egui::Context, area: Rect) {
        let view = self.packages.view();
        let mut out = Out::default();
        let Some(sheet) = &mut self.skills else {
            return;
        };
        skills::sheet(ctx, area, sheet, &view, &mut out);
        #[cfg(test)]
        {
            self.hits.skills = std::mem::take(&mut out.marks);
        }
        if out.actions.is_empty() {
            return;
        }
        ctx.request_repaint();
        for action in out.actions {
            let packages = Arc::clone(&self.packages);
            let result = match action {
                SkillsAction::Close => {
                    self.skills = None;
                    self.focus_visible = true;
                    self.activating_key =
                        ctx.input(|i| [egui::Key::Escape].into_iter().find(|k| i.key_pressed(*k)));
                    return;
                }
                SkillsAction::OpenSettings => {
                    self.queued.push(Command::SettingsAt(SettingsTab::Home));
                    self.skills = None;
                    return;
                }
                SkillsAction::SetDefault(id) => packages.set_default_package(id.as_deref()),
                SkillsAction::Delete(id) => packages.delete_package(&id),
                SkillsAction::Edit(id) => {
                    self.follow_backend();
                    let result = packages.edit_package(&id);
                    if result.is_ok() {
                        self.skills = None;
                        return;
                    }
                    result
                }
                SkillsAction::Create(name, extends) => {
                    self.follow_backend();
                    let result = packages.create_package(&name, extends.as_deref());
                    if result.is_ok() {
                        // The authoring terminal is now the one to look at.
                        self.skills = None;
                        return;
                    }
                    result
                }
            };
            if let (Err(error), Some(sheet)) = (result, &mut self.skills) {
                sheet.error = Some(error);
            }
        }
    }

    /// Take the person to an agent's sign-in (a terminal in Connecto Home that
    /// starts its login). Said in Settings when that cannot be done.
    fn start_agent_signin(&mut self, tool: ToolKind) {
        let result = match &self.automations_control {
            Some(control) => control.open_login(tool),
            None => Err("Sign-in is not available here.".to_owned()),
        };
        self.agents.signin_started(tool);
        match result {
            Ok(()) => self.agents_error = None,
            // Kept until Settings is open, where it is said.
            Err(why) => self.agents_error = Some(why),
        }
    }

    /// An install that just finished asks for the sign-in once.
    fn follow_agent_signins(&mut self) {
        let wanted: Vec<ToolKind> = self
            .agents
            .view()
            .rows
            .iter()
            .filter(|row| row.signin_requested)
            .map(|row| row.tool)
            .collect();
        for tool in wanted {
            self.start_agent_signin(tool);
        }
    }

    fn draw_settings(&mut self, ctx: &egui::Context, area: Rect) {
        let view = self.packages.view();
        let remote = self.remote.view();
        let agents = self.agents.view();
        let mut out = Out::default();
        let Some(sheet) = &mut self.settings else {
            return;
        };
        if let Some(why) = self.agents_error.take() {
            sheet.agents_error = Some(why);
        }
        let modes = self
            .workspace
            .as_ref()
            .map_or_else(connecto_core::LaunchModes::default, |w| w.launch_modes());
        let update_view = self.updates.view();
        settings::sheet(
            ctx,
            area,
            sheet,
            settings::Inputs {
                view: &view,
                remote: &remote,
                agents: &agents,
                tab: self.settings_tab,
                secrets_in_system_store: self
                    .workspace
                    .as_ref()
                    .is_some_and(|w| w.secrets_in_system_store()),
                updates: &update_view,
            },
            modes,
            &mut out,
        );
        #[cfg(test)]
        {
            self.hits.settings = std::mem::take(&mut out.marks);
        }
        for action in out.actions {
            ctx.request_repaint();
            match action {
                SettingsAction::ActivateAgent(tool) => self.agents.activate(tool),
                SettingsAction::DeactivateAgent(tool) => self.agents.deactivate(tool),
                SettingsAction::RefreshAgents => self.agents.refresh(),
                SettingsAction::SignInAgent(tool) => self.start_agent_signin(tool),
                SettingsAction::Close => {
                    self.settings = None;
                    self.focus_visible = true;
                    self.activating_key =
                        ctx.input(|i| [egui::Key::Escape].into_iter().find(|k| i.key_pressed(*k)));
                    return;
                }
                SettingsAction::SelectTab(tab) => self.settings_tab = tab,
                SettingsAction::OpenUtilitySessions => {
                    // The utility sheet takes the place of Settings, as a menu
                    // command does: this one is queued, so Settings closes first.
                    self.settings = None;
                    self.queued.push(Command::UtilitySessions);
                    return;
                }
                SettingsAction::SetLaunchMode(tool, mode) => {
                    self.with_workspace(|workspace| {
                        workspace.set_launch_modes(workspace.launch_modes().with(tool, mode));
                    });
                }
                SettingsAction::SetHosting(on) => self.remote.set_hosting(on),
                SettingsAction::SetAutoUpdate(on) => self.updates.set_auto_check(on),
                SettingsAction::CheckForUpdates => {
                    self.update_strip.closed_asked = false;
                    self.updates.check_now();
                }
                SettingsAction::SetSecretsInSystemStore(on) => {
                    self.with_workspace(|workspace| workspace.set_secrets_in_system_store(on));
                }
                SettingsAction::StartPairing => self.open_pairing(),
                SettingsAction::AskRemoveMachine { id, name } => {
                    self.confirm = Some(Confirm::machine(&id, &name));
                }
                SettingsAction::SetRelay { url, token } => {
                    if let (Err(why), Some(sheet)) =
                        (self.remote.set_relay(url, token), &mut self.settings)
                    {
                        sheet.relay_error = Some(why);
                    }
                }
                SettingsAction::ChooseHome => {
                    let Some(path) = (self.picker)() else {
                        continue;
                    };
                    if let (Err(error), Some(sheet)) =
                        (self.packages.set_home(path), &mut self.settings)
                    {
                        sheet.error = Some(error);
                    }
                }
            }
        }
    }

    /// Open the pairing dialog and start a pairing. A refusal is said in it.
    fn open_pairing(&mut self) {
        self.pairing = Some(PairingSheet::fresh(self.remote.start_mutual_pairing()));
        self.focus_visible = true;
    }

    /// Draw the dialogs over everything, and act on what they ask for.
    fn draw_overlays(&mut self, ctx: &egui::Context, esc: bool) {
        if let Some(sheet) = &mut self.pairing {
            let view = self.remote.view();
            let mut out = Out::default();
            pairing::dialog(ctx, sheet, &view, esc, &mut out);
            #[cfg(test)]
            {
                self.hits.pairing = std::mem::take(&mut out.marks);
            }
            for action in out.actions {
                self.pairing_action(action);
            }
        }
        if let Some(confirm) = &mut self.confirm {
            let mut out = Out::default();
            confirm::dialog(ctx, confirm, esc, &mut out);
            #[cfg(test)]
            {
                self.hits.confirm = std::mem::take(&mut out.marks);
            }
            for action in out.actions {
                match action {
                    ConfirmAction::Keep => self.confirm = None,
                    ConfirmAction::Confirm => self.confirm_remove(),
                }
            }
        }
    }

    fn pairing_action(&mut self, action: PairingAction) {
        // The refusal of an earlier command is said until the person acts.
        if let Some(sheet) = &mut self.pairing {
            sheet.notice = None;
        }
        let result = match action {
            PairingAction::Close => {
                // The backend refuses a cancel while it finishes: the dialog
                // stays with the phase it is in.
                if matches!(
                    self.remote.view().pairing,
                    connecto_core::PairingPhase::Keeping
                ) {
                    return;
                }
                self.remote.cancel_pairing();
                self.pairing = None;
                self.focus_visible = true;
                return;
            }
            PairingAction::Start => {
                // A new attempt starts from nothing: no code, name or error of
                // the last one.
                self.pairing = Some(PairingSheet::fresh(self.remote.start_mutual_pairing()));
                return;
            }
            PairingAction::SubmitCode(digits) => self.remote.submit_peer_code(&digits),
            PairingAction::FinishName(name) => self.remote.finish_pairing(&name),
            PairingAction::ByAddress => {
                // The advanced way replaces the dialog: stop this pairing, unless
                // the backend is already finishing it.
                if matches!(
                    self.remote.view().pairing,
                    connecto_core::PairingPhase::Keeping
                ) {
                    return;
                }
                self.remote.cancel_pairing();
                self.pairing = None;
                self.close_sheets();
                self.host_sheet = Some(HostSheet::add());
                return;
            }
        };
        if let Some(sheet) = &mut self.pairing {
            sheet.error = result.err();
        }
    }

    /// Remove what the confirm dialog names; it stays open if that is refused.
    fn confirm_remove(&mut self) {
        let Some(confirm) = self.confirm.clone() else {
            return;
        };
        let result = match &confirm.target {
            Target::Machine(id) => self.remote.disconnect_machine(id),
            Target::Host(name) => self.remote.forget_host(name),
        };
        match result {
            Ok(()) => {
                if let Target::Host(name) = &confirm.target
                    && self.host.as_deref() == Some(name.as_str())
                {
                    self.host = None;
                }
                self.confirm = None;
            }
            Err(why) => {
                if let Some(open) = &mut self.confirm {
                    open.error = Some(why);
                }
            }
        }
    }

    fn draw_host_sheet(&mut self, ctx: &egui::Context, area: Rect) {
        let view = self.remote.view();
        let mut out = Out::default();
        let Some(sheet) = &mut self.host_sheet else {
            return;
        };
        hosts::sheet(ctx, area, sheet, &view, &mut out);
        #[cfg(test)]
        {
            self.hits.host_sheet = std::mem::take(&mut out.marks);
        }
        for action in out.actions {
            ctx.request_repaint();
            match action {
                HostAction::Close => {
                    self.host_sheet = None;
                    self.focus_visible = true;
                    return;
                }
                HostAction::Add { name, address } => {
                    if let (Err(why), Some(sheet)) =
                        (self.remote.add_host(&name, &address), &mut self.host_sheet)
                    {
                        sheet.error = Some(why);
                    }
                }
                HostAction::Confirm => {
                    // Shown once it is saved; the connection starts with it.
                    self.remote.confirm_host();
                    if let Some(sheet) = &self.host_sheet {
                        self.host = Some(sheet.name.trim().to_owned());
                    }
                }
                HostAction::CancelPending => self.remote.cancel_host(),
                HostAction::Rename { from, to } => match self.remote.rename_host(&from, &to) {
                    Ok(()) => {
                        if self.host.as_deref() == Some(from.as_str()) {
                            self.host = Some(to);
                        }
                        self.host_sheet = None;
                        self.focus_visible = true;
                        return;
                    }
                    Err(why) => {
                        if let Some(sheet) = &mut self.host_sheet {
                            sheet.error = Some(why);
                        }
                    }
                },
            }
        }
    }

    /// Forget submitted settings the view now shows, or that failed.
    fn settle_pending(&mut self, view: &connecto_core::PackagesView, snap: &Snapshot) {
        self.pending_settings.retain(|id, pending| {
            let open = snap.ws.projects.iter().any(|p| p.id == *id);
            let Some(shown) = view.project(id) else {
                return open;
            };
            let install_error = match &shown.status.state {
                connecto_core::InstallState::Error(why) => Some(why.clone()),
                _ => None,
            };
            let failed = install_error.is_some() && install_error != pending.install_error;
            open && !failed && shown.settings != pending.settings
        });
    }

    fn draw_project_settings(&mut self, ctx: &egui::Context, snap: &Snapshot, area: Rect) {
        let view = self.packages.view();
        self.settle_pending(&view, snap);
        if !self.pending_settings.is_empty() {
            ctx.request_repaint_after(Duration::from_millis(250));
        }
        let (now_ms, utc_offset_s) = (self.clock)();
        let Some(sheet) = &mut self.project_settings else {
            return;
        };
        let Some(project) = snap.ws.projects.iter().find(|p| p.id == sheet.project) else {
            // The project closed under the sheet.
            self.project_settings = None;
            self.focus_visible = true;
            return;
        };
        let input = ProjectSettingsInput {
            view: &view,
            project_name: &project.name,
            pending: self
                .pending_settings
                .get(&sheet.project)
                .map(|pending| &pending.settings),
            now_ms,
            utc_offset_s,
        };
        let mut out = Out::default();
        project_settings::sheet(ctx, area, sheet, &input, &mut out);
        let id = sheet.project.clone();
        #[cfg(test)]
        {
            self.hits.project_settings = std::mem::take(&mut out.marks);
        }
        for action in out.actions {
            ctx.request_repaint();
            match action {
                ProjectSettingsAction::Close => {
                    self.project_settings = None;
                    self.focus_visible = true;
                    self.activating_key =
                        ctx.input(|i| [egui::Key::Escape].into_iter().find(|k| i.key_pressed(*k)));
                    return;
                }
                ProjectSettingsAction::Save(settings) => {
                    let result = self.packages.set_project_settings(&id, settings.clone());
                    match result {
                        Ok(()) => {
                            self.pending_settings.insert(
                                id.clone(),
                                PendingSettings {
                                    settings,
                                    install_error: match &view
                                        .project(&id)
                                        .map(|p| p.status.state.clone())
                                    {
                                        Some(connecto_core::InstallState::Error(why)) => {
                                            Some(why.clone())
                                        }
                                        _ => None,
                                    },
                                },
                            );
                            ctx.request_repaint_after(Duration::from_millis(250));
                        }
                        Err(error) => {
                            // Earlier unconfirmed edits still stand.
                            if let Some(sheet) = &mut self.project_settings {
                                sheet.rejected(error);
                            }
                        }
                    }
                }
                ProjectSettingsAction::Refresh => self.packages.refresh_project(&id),
            }
        }
    }

    fn draw_utility(&mut self, ctx: &egui::Context, snap: &Snapshot, area: Rect) {
        let Some(sheet) = &mut self.utility else {
            return;
        };
        let (now_ms, utc_offset_s) = (self.clock)();
        let runs = snap
            .active()
            .and_then(|index| snap.utility.get(index))
            .cloned()
            .unwrap_or_default();
        let settings = self
            .workspace
            .as_ref()
            .map(|workspace| workspace.utility_settings())
            .unwrap_or_default();
        let openable: HashSet<u64> = snap
            .active()
            .and_then(|index| snap.project(index))
            .and_then(|project| project.terminals.as_ref())
            .map(|terminals| {
                runs.iter()
                    .filter(|run| terminals.utility_pane(run.id).is_some())
                    .map(|run| run.id)
                    .collect()
            })
            .unwrap_or_default();
        let input = UtilityInput {
            openable: &openable,
            runs: &runs,
            settings,
            now_ms,
            utc_offset_s,
        };
        let mut out = UtilityOut::default();
        utility::sheet(ctx, area, sheet, &input, &mut out);
        #[cfg(test)]
        {
            self.hits.utility = out.hits;
        }
        for action in out.actions {
            match action {
                UtilityAction::Close => {
                    self.utility = None;
                    self.focus_visible = true;
                    self.activating_key =
                        ctx.input(|i| [egui::Key::Escape].into_iter().find(|k| i.key_pressed(*k)));
                    return;
                }
                UtilityAction::Open(run) => {
                    if let Some(project) = snap
                        .active()
                        .and_then(|index| snap.project(index))
                        .map(|project| project.id.clone())
                    {
                        self.utility_view = Some(UtilityView {
                            project,
                            run,
                            name: String::new(),
                        });
                    }
                    self.utility = None;
                    self.focus_visible = true;
                    ctx.request_repaint();
                    return;
                }
                UtilityAction::Settings(settings) => {
                    self.with_workspace(|workspace| workspace.set_utility_settings(settings));
                }
            }
        }
    }

    fn draw_history(&mut self, ctx: &egui::Context, snap: &Snapshot, area: Rect) {
        let Some(history) = &mut self.history else {
            return;
        };
        let (now_ms, utc_offset_s) = (self.clock)();
        let name = |id: &str| {
            snap.index_of(id)
                .and_then(|index| snap.project(index))
                .map_or_else(String::new, |project| project.name.clone())
        };
        let input = HistoryInput {
            snap,
            now_ms,
            utc_offset_s,
            project_name: &name,
        };
        let mut out = HistoryOut::default();
        history::sheet(ctx, area, history, &input, &mut out);
        #[cfg(test)]
        {
            self.hits.history = out.hits;
        }
        for action in out.actions {
            match action {
                HistoryAction::Close => {
                    self.history = None;
                    self.focus_visible = true;
                    self.activating_key =
                        ctx.input(|i| [egui::Key::Escape].into_iter().find(|k| i.key_pressed(*k)));
                    return;
                }
                HistoryAction::Continue {
                    project,
                    history: id,
                } => {
                    let tool = snap
                        .index_of(&project)
                        .and_then(|index| snap.view(index))
                        .and_then(|view| view.history.iter().find(|e| e.id == id))
                        .map(|entry| entry.tool);
                    if let Some(tool) = tool
                        && !self.tool_allowed(snap, &project, tool)
                    {
                        self.history = None;
                        self.focus_visible = true;
                        return;
                    }
                    if let Some(terminals) = self.terminals_of(snap, &project) {
                        self.request_error = terminals.continue_from(&id).err();
                        if self.request_error.is_none() {
                            let known = snap
                                .index_of(&project)
                                .and_then(|index| snap.view(index))
                                .map(|view| view.terminals.iter().map(|e| e.id.clone()).collect())
                                .unwrap_or_default();
                            self.pending_new = Some(PendingNew {
                                project: project.clone(),
                                known,
                                since: Instant::now(),
                            });
                            self.activate(snap, &project);
                        }
                    }
                    self.history = None;
                    self.focus_visible = true;
                    self.activating_key = ctx.input(|i| {
                        [egui::Key::Enter, egui::Key::Space]
                            .into_iter()
                            .find(|k| i.key_pressed(*k))
                    });
                    return;
                }
                HistoryAction::ViewTranscript {
                    project,
                    history: id,
                } => {
                    if let Some(terminals) = self.terminals_of(snap, &project) {
                        self.request_error = terminals.request_transcript(&id).err();
                    }
                    if let Some(open) = &mut self.history {
                        open.open_viewer(project, id);
                    }
                }
            }
        }
    }

    /// What the registry wants the user to know, in the order of weight.
    fn notices(&self, snap: &Snapshot) -> Vec<(String, Color32)> {
        let mut lines = Vec::new();
        if let Some(warning) = &snap.ws.warning {
            lines.push((warning.clone(), theme::CORAL_SOFT));
        }
        if let Some(error) = &snap.ws.last_error {
            lines.push((error.clone(), theme::ERROR));
        }
        let mut errors: Vec<String> = snap.ws.last_error.iter().cloned().collect();
        if let Some(view) = snap.active().and_then(|index| snap.view(index)) {
            if let Some(warning) = &view.warning {
                lines.push((warning.clone(), theme::CORAL_SOFT));
            }
            match view.save {
                SaveState::Saved => {}
                SaveState::Unsaved => lines.push((UNSAVED_TEXT.to_owned(), theme::CORAL_SOFT)),
                SaveState::ReadOnly => lines.push((READ_ONLY_TEXT.to_owned(), theme::CORAL_SOFT)),
            }
            if let Some(error) = &view.last_error {
                lines.push((format!("Terminal list: {error}"), theme::ERROR));
                errors.push(error.clone());
            }
        }
        if let Some(warning) = self
            .shown(snap)
            .and_then(|key| self.panes.get(&key))
            .and_then(Pane::rejected_frames_warning)
        {
            lines.push((warning, theme::CORAL_SOFT));
        }
        if let Some(error) = &self.request_error
            && !errors.contains(error)
        {
            lines.push((error.clone(), theme::ERROR));
        }
        lines
    }
}

/// The chooser's text, owned so it outlives the borrow of `self`.
struct OwnedInfo {
    project_name: String,
    logins: [&'static str; 3],
    tools: Vec<ToolKind>,
    blocked: Option<&'static str>,
}

impl OwnedInfo {
    fn as_info(&self) -> ChooserInfo<'_> {
        ChooserInfo {
            project_name: &self.project_name,
            logins: self.logins,
            tools: &self.tools,
            blocked: self.blocked,
        }
    }
}

#[cfg(feature = "frame-capture")]
impl App {
    fn capture_frame(&mut self, ui: &Ui) {
        // Nothing is recorded from a login.
        let logging_in = self
            .panes
            .values()
            .chain(self.static_panes.iter())
            .any(|pane| pane.stage() == Some(connecto_core::PaneStage::LoggingIn));
        if let Some(shots) = &mut self.shots
            && !logging_in
        {
            shots.request_if_due(ui.ctx());
        }
    }
}

impl eframe::App for App {
    /// Runs before every `ui`, and without one while the window is hidden. No
    /// session or relay work may depend on `ui`, which stops then.
    fn logic(&mut self, ctx: &egui::Context, _frame: &mut Frame) {
        if let Some(watch) = &self.watch {
            let (visible, focused, occluded) = ctx.input(|input| {
                let viewport = input.viewport();
                (viewport.visible(), viewport.focused, viewport.occluded)
            });
            watch.window_state(visible, focused, occluded);
        }
        if let Some(menu) = &mut self.menu {
            let clicked = menu.poll();
            if !clicked.is_empty() {
                self.queued.extend(clicked);
                ctx.request_repaint();
            }
        }
    }

    fn ui(&mut self, ui: &mut Ui, frame: &mut Frame) {
        let start = Instant::now();
        if let Some(watch) = &self.watch {
            watch.frame_started();
        }
        let latency = self.wake.take_latency();
        self.draw(ui);
        // A request made before the frame starts is lost, so it is made here.
        #[cfg(feature = "ui-test-driver")]
        if self.driver.is_some() {
            ui.ctx().request_repaint_after(Duration::from_millis(20));
        }
        #[cfg(feature = "frame-capture")]
        self.capture_frame(ui);
        let pending = self.wake.is_pending();
        self.metrics
            .record(start, frame.info().cpu_usage, latency, pending);
        if self.metrics.enabled() {
            let mut counters: Vec<&mut PaneCounters> = self
                .panes
                .values_mut()
                .chain(self.static_panes.iter_mut())
                .map(|p| &mut p.counters)
                .collect();
            self.metrics.flush_if_due(&mut counters);
        }
    }

    #[cfg(feature = "ui-test-driver")]
    fn raw_input_hook(&mut self, ctx: &egui::Context, raw_input: &mut egui::RawInput) {
        if let Some(driver) = &mut self.driver {
            let more = driver.feed(ctx, raw_input);
            for name in driver.take_commands() {
                match name.as_str() {
                    "settings" => self.queued.push(Command::Settings),
                    "addhost" => self.queued.push(Command::AddHost),
                    "automations" => self.queued.push(Command::Automations),
                    "newterminal" => self.queued.push(Command::NewTerminal),
                    "newshell" => self.queued.push(Command::NewTool(ToolKind::Shell)),
                    _ => {}
                }
            }
            if let Some(watch) = &self.watch {
                watch.set_driver_steps(driver.remaining());
            }
            if !more {
                self.driver = None;
            }
        }
    }

    fn clear_color(&self, _visuals: &egui::Visuals) -> [f32; 4] {
        theme::WINDOW_BG.to_normalized_gamma_f32()
    }
}

/// Optional measurement outputs, all off by default.
pub struct Diagnostics {
    pub metrics: Option<MetricsSink>,
    #[cfg(feature = "frame-capture")]
    pub shots: Option<ScreenshotSink>,
    /// The workspace: open projects and their terminals. The window follows its
    /// view each frame. Absent for a static shell, which has no sidebar.
    pub workspace: Option<Arc<dyn WorkspaceControl>>,
    /// The automations engine behind Terminal > Automations. Absent when it
    /// did not start: the sheet then says so.
    pub automations: Option<Arc<dyn AutomationsControl>>,
    /// Connecto Home, packages and per-project settings behind Terminal >
    /// Skills & Plugins, Settings and the project gear. `NoPackages` when not
    /// wired in.
    pub packages: Arc<dyn PackagesControl>,
    /// Remote hosts and hosting behind the sidebar's host rows and Settings.
    /// `NoRemote` when not wired in.
    pub remote: Arc<dyn RemoteControl>,
    /// The agents Settings lists, installs and switches. `NoAgents` when not
    /// wired in.
    pub agents: Arc<dyn connecto_core::AgentsControl>,
    /// Connecto updating itself. `NoUpdates` when not wired in.
    pub updates: Arc<dyn connecto_core::UpdateControl>,
}

impl Default for Diagnostics {
    fn default() -> Self {
        Self {
            metrics: None,
            #[cfg(feature = "frame-capture")]
            shots: None,
            workspace: None,
            automations: None,
            packages: Arc::new(NoPackages),
            remote: Arc::new(connecto_core::NoRemote),
            agents: Arc::new(connecto_core::NoAgents),
            updates: Arc::new(connecto_core::NoUpdates),
        }
    }
}

/// The window. eframe runs no pass while a window is covered, so scripted runs
/// keep theirs on top, or a covering window would stop the script (ADR 0011).
fn viewport() -> egui::ViewportBuilder {
    // On macOS the content fills the whole window and the traffic lights sit
    // over the sidebar's header: a transparent title bar with no title text.
    let mut builder = egui::ViewportBuilder::default()
        .with_title("Connecto")
        .with_fullsize_content_view(true)
        .with_titlebar_shown(false)
        .with_title_shown(false)
        .with_inner_size([1280.0, 760.0])
        .with_min_inner_size([640.0, 300.0]);
    if let Ok(icon) = eframe::icon_data::from_png_bytes(ICON_PNG) {
        builder = builder.with_icon(icon);
    }
    #[cfg(feature = "ui-test-driver")]
    let builder = {
        let builder = builder.with_window_level(egui::WindowLevel::AlwaysOnTop);
        match std::env::var("CONNECTO_UI_POS")
            .ok()
            .as_deref()
            .and_then(start_position)
        {
            Some(position) => builder.with_position(position),
            None => builder,
        }
    };
    builder
}

/// Where a scripted run opens its window, from `<x>,<y>` in points. It picks
/// the display, so a run can cover a chosen scale.
#[cfg(feature = "ui-test-driver")]
fn start_position(text: &str) -> Option<egui::Pos2> {
    let (x, y) = text.split_once(',')?;
    let (x, y) = (x.trim().parse::<f32>().ok()?, y.trim().parse::<f32>().ok()?);
    (x.is_finite() && y.is_finite()).then(|| egui::pos2(x, y))
}

/// Open the window and run until it closes.
pub fn run(
    panes: Vec<PaneConfig>,
    wake: Arc<WakeSlot>,
    diagnostics: Diagnostics,
) -> eframe::Result {
    let options = eframe::NativeOptions {
        viewport: viewport(),
        ..eframe::NativeOptions::default()
    };
    eframe::run_native(
        "Connecto",
        options,
        Box::new(move |creation| {
            fonts::install(&creation.egui_ctx);
            theme::apply(&creation.egui_ctx);
            // Set exactly once; a second window would keep the first context.
            let _ = wake.ctx.set(creation.egui_ctx.clone());
            let mut app = App::new(panes, wake, diagnostics);
            // After winit has made its own menu, so ours replaces it.
            if app.workspace.is_some() {
                app.menu = NativeMenu::install(&creation.egui_ctx);
                app.menu_owns_shortcuts = app.menu.is_some();
            }
            Ok(Box::new(app))
        }),
    )
}

#[cfg(test)]
pub(crate) fn viewport_for_tests() -> egui::ViewportBuilder {
    viewport()
}

#[cfg(all(test, feature = "ui-test-driver"))]
pub(crate) fn start_position_for_tests(text: &str) -> Option<egui::Pos2> {
    start_position(text)
}

/// What tests need to look at or steer.
#[cfg(test)]
impl App {
    pub(crate) fn set_test_hooks(
        &mut self,
        picker: fn() -> Option<PathBuf>,
        clock: fn() -> (u64, i32),
    ) {
        self.picker = picker;
        self.clock = clock;
    }

    pub(crate) fn set_menu_owns_shortcuts(&mut self, owns: bool) {
        self.menu_owns_shortcuts = owns;
    }

    pub(crate) fn queue_for_tests(&mut self, command: Command) {
        self.queued.push(command);
    }

    /// What the in-window menu bar drew in the last pass.
    #[cfg(not(target_os = "macos"))]
    pub(crate) fn bar_hits(&self) -> &[(crate::menu::BarHit, Rect)] {
        &self.bar.hits
    }

    pub(crate) fn shown_for_tests(&self) -> Option<Key> {
        self.shown(&self.snapshot())
    }

    pub(crate) fn pane_count(&self) -> usize {
        self.panes.len()
    }

    pub(crate) fn pane_mut(&mut self, key: &Key) -> Option<&mut Pane> {
        self.panes.get_mut(key)
    }

    pub(crate) fn pane_hits(&self, key: &Key) -> &[Rect] {
        self.panes
            .get(key)
            .map_or(&[], |pane| pane.command_hits.as_slice())
    }

    pub(crate) fn seen_for_tests(&self) -> HashMap<Key, u64> {
        self.seen.clone()
    }

    pub(crate) fn request_error_for_tests(&self) -> Option<String> {
        self.request_error.clone()
    }

    pub(crate) fn is_renaming(&self) -> bool {
        self.rename.is_some()
    }

    pub(crate) fn chooser_open(&self) -> bool {
        self.chooser.is_some()
    }

    pub(crate) fn utility_view_for_tests(&self) -> Option<Key> {
        self.utility_shown(&self.snapshot())
    }

    pub(crate) fn utility_open(&self) -> bool {
        self.utility.is_some()
    }

    pub(crate) fn automations_key_input(&self) -> Option<String> {
        self.automations
            .as_ref()
            .map(|sheet| sheet.key_input.clone())
    }

    /// The New automation fields: name, screen, action.
    pub(crate) fn automations_form(&self) -> Option<(String, String, String)> {
        self.automations.as_ref().map(|sheet| {
            (
                sheet.name.clone(),
                sheet.screen.clone(),
                sheet.action.clone(),
            )
        })
    }

    pub(crate) fn automations_open(&self) -> bool {
        self.automations.is_some()
    }

    pub(crate) fn skills_open(&self) -> bool {
        self.skills.is_some()
    }

    pub(crate) fn modal_open_for_tests(&self) -> bool {
        self.host_sheet.is_some()
    }

    pub(crate) fn settings_open(&self) -> bool {
        self.settings.is_some()
    }

    pub(crate) fn pairing_open(&self) -> bool {
        self.pairing.is_some()
    }

    pub(crate) fn pairing_digits_for_tests(&self) -> Option<String> {
        self.pairing.as_ref().map(|sheet| sheet.digits.clone())
    }

    pub(crate) fn pairing_name_for_tests(&self) -> Option<String> {
        self.pairing.as_ref().and_then(|sheet| sheet.name.clone())
    }

    pub(crate) fn confirm_open(&self) -> bool {
        self.confirm.is_some()
    }

    pub(crate) fn settings_tab_for_tests(&self) -> SettingsTab {
        self.settings_tab
    }

    /// The project whose settings sheet is open.
    pub(crate) fn project_settings_of(&self) -> Option<String> {
        self.project_settings
            .as_ref()
            .map(|sheet| sheet.project.clone())
    }

    pub(crate) fn history_open(&self) -> bool {
        self.history.is_some()
    }

    pub(crate) fn cell_for_tests(&self) -> CellMetrics {
        self.cell.unwrap_or_else(measured_cell)
    }

    pub(crate) fn notices_for_tests(&self) -> Vec<(String, Color32)> {
        self.notices(&self.snapshot())
    }
}

#[cfg(test)]
fn measured_cell() -> CellMetrics {
    CellMetrics::measure(&egui::Context::default())
}
