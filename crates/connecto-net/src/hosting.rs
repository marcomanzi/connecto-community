//! The host side of remote sessions as the app wires it: a numbered ring per
//! launched terminal, and the list of terminals a peer may see.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex, PoisonError};

use connecto_core::{
    AgentsControl, OutputTap, TapFactory, TermActivity, ToolKind, WorkspaceControl,
};

use crate::host::{
    HostTerminal, HostTerminals, NO_PROJECT_PANELS, NO_SUCH_PROJECT, Right, RingTap, Target,
};
use crate::project_panels::{
    ProjectAsk, SettingsAnswer, TokensAnswer, done_json, history_json, settings_answer_json,
    tokens_answer_json, transcript_json,
};
use crate::seq::{RING_BYTES, Ring};
use crate::wire::{BackgroundInfo, DenyReason, ManageOp, ProjectInfo, ToolModels};

/// A peer's ask of `project`'s operations, answered through the same
/// control the window uses: its view (each operation with its files whole),
/// or its action, checked as the person's own is (an approval or a run names
/// the hash of the files the peer was sent, and is refused when the files on
/// this machine no longer have it).
///
/// # Errors
/// The action's refusal, as the person would read it.
pub fn answer_ops(
    ops: &dyn connecto_core::ops::OpsControl,
    project: &str,
    ask: &ProjectAsk,
) -> Result<Vec<u8>, String> {
    match ask {
        ProjectAsk::Ops { after } => Ok(crate::project_panels::ops_view_json(
            &ops.view(project),
            after.as_deref(),
        )),
        ProjectAsk::OpsAct(action) => ops.act(project, action.clone()).map(|_| done_json()),
        _ => Err(crate::project_panels::UNKNOWN_ASK.to_owned()),
    }
}

fn answer_siri(
    desk: &crate::siri::SiriDesk,
    project: &str,
    terminal: Option<&str>,
    ask: &ProjectAsk,
    caller: &dyn crate::siri::SiriCaller,
) -> Result<Vec<u8>, String> {
    desk.answer(project, terminal, ask, caller)
        .map(|v| v.to_string().into_bytes())
}

/// The caller of an ask made with no peer to answer as: a Siri ask is
/// refused for it.
struct NoCaller;

impl crate::siri::SiriCaller for NoCaller {
    fn owner(&self) -> &str {
        ""
    }

    fn type_into(&self, _: &connecto_core::TerminalEntry, _: Vec<u8>) -> Result<(), String> {
        Err("no peer to type for".to_owned())
    }
}

/// The app sessions of a workspace's open projects, for the Siri desk.
pub struct WorkspaceSessions(pub Arc<dyn WorkspaceControl>);

impl crate::siri::AppSessions for WorkspaceSessions {
    fn app_sessions(&self) -> Vec<crate::siri::AppSession> {
        let view = self.0.view();
        view.projects
            .iter()
            .filter_map(|p| {
                let terminals = p.terminals.as_ref()?.view();
                let entry = crate::siri::app_session_of(&terminals.terminals)?.clone();
                Some(crate::siri::AppSession {
                    project: p.id.clone(),
                    project_name: p.name.clone(),
                    entry,
                })
            })
            .collect()
    }
}

/// The ring of each terminal's current session, by `<project>/<terminal>`.
#[derive(Default)]
pub struct RingBook {
    /// Each terminal's ring, and whether it is real: tapped by a start since
    /// the app started, or the restored chat's (which waits for Start). A
    /// terminal that has not run is listed with an empty ring that is not
    /// real; its first start replaces it, as a restart replaces a ring, and
    /// closes it, so a peer that attached to it attaches again to the new
    /// one. Older peers are listed only terminals with a real ring.
    rings: Mutex<HashMap<String, (Arc<Ring>, bool)>>,
    /// Whether output is kept. Off (the start), a terminal's bytes are only
    /// numbered: no memory goes to output nobody can look at, and a terminal
    /// that was running when hosting is turned on is shared at once, starting
    /// from a repaint of its screen.
    record: Arc<std::sync::atomic::AtomicBool>,
}

impl RingBook {
    pub fn set_recording(&self, on: bool) {
        self.record.store(on, Ordering::Relaxed);
    }

    /// The ring `key` is listed with, and whether it is real: its own, or
    /// an empty one, the same each time, until it runs. A `chat`'s empty ring
    /// counts as real, as it always did. Looked up and made under one lock,
    /// so a start's ring is never followed by a new placeholder.
    fn listed(&self, key: &str, chat: bool) -> (Arc<Ring>, bool) {
        let mut rings = self.rings.lock().unwrap_or_else(PoisonError::into_inner);
        let slot = rings
            .entry(key.to_owned())
            .or_insert_with(|| (Arc::new(Ring::new(RING_BYTES)), false));
        slot.1 |= chat;
        (Arc::clone(&slot.0), slot.1)
    }

    /// The real ring of `key`: tapped by a start, or the restored chat's.
    #[must_use]
    pub fn ring(&self, key: &str) -> Option<Arc<Ring>> {
        self.rings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .get(key)
            .filter(|(_, real)| *real)
            .map(|(ring, _)| Arc::clone(ring))
    }
}

impl TapFactory for RingBook {
    fn tap_for(&self, key: &str) -> Arc<dyn OutputTap> {
        let ring = Arc::new(Ring::new(RING_BYTES));
        let old = self
            .rings
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .insert(key.to_owned(), (Arc::clone(&ring), true));
        // A restart, or a first start after a placeholder, is a new screen:
        // whoever follows the old ring is let go, and attaches again to the
        // new one.
        if let Some((old, _)) = old {
            old.close();
        }
        Arc::new(RingTap::new(ring, Arc::clone(&self.record)))
    }
}

/// A terminal's background work as a list carries it: how long each task has
/// run as of `now`.
fn background_infos(
    work: &[connecto_core::BackgroundWork],
    now: std::time::Instant,
) -> Vec<BackgroundInfo> {
    work.iter()
        .map(|task| BackgroundInfo {
            id: task.id.clone(),
            kind: task.kind.clone(),
            label: task.label.clone(),
            description: task.description.clone(),
            seconds: task.age(now).as_secs(),
        })
        .collect()
}

/// The wire's byte for a status dot.
#[must_use]
pub const fn activity_byte(activity: TermActivity) -> u8 {
    match activity {
        TermActivity::NotStarted => 0,
        TermActivity::Starting => 1,
        TermActivity::NeedsLogin => 2,
        TermActivity::Working => 3,
        TermActivity::NeedsUser => 4,
        TermActivity::Idle => 5,
        TermActivity::Interrupted => 6,
        TermActivity::Done => 7,
        TermActivity::Exited => 8,
        TermActivity::Failed => 9,
        TermActivity::Plain => 10,
    }
}

/// A listed terminal that has not started since its host launched, and that
/// is not a restored chat (which waits for Start): a peer lists it but opens
/// no stream for it (each costs threads on both machines) until a list says
/// it runs. Hosts send these only to a peer granted
/// [`crate::wire::FEATURE_TERMINAL_IDS`] (see [`crate::host::HostTerminal::ran`]).
#[must_use]
pub fn never_started(running: bool, activity: u8, name: &str) -> bool {
    !running
        && activity_from_byte(activity) == TermActivity::NotStarted
        && name != connecto_core::session::APP_SESSION
}

#[must_use]
pub const fn activity_from_byte(byte: u8) -> TermActivity {
    match byte {
        1 => TermActivity::Starting,
        2 => TermActivity::NeedsLogin,
        3 => TermActivity::Working,
        4 => TermActivity::NeedsUser,
        5 => TermActivity::Idle,
        6 => TermActivity::Interrupted,
        7 => TermActivity::Done,
        8 => TermActivity::Exited,
        9 => TermActivity::Failed,
        10 => TermActivity::Plain,
        _ => TermActivity::NotStarted,
    }
}

/// The wire's byte for a tool: 1 Claude, 2 Codex, 3 shell, 4 Pi, 5
/// Antigravity, 6 OpenCode. A peer from before Pi, Antigravity or OpenCode
/// reads an unknown byte as Claude.
#[must_use]
pub const fn tool_byte(tool: ToolKind) -> u8 {
    match tool {
        ToolKind::Claude => 1,
        ToolKind::Codex => 2,
        ToolKind::Shell => 3,
        ToolKind::Pi => 4,
        ToolKind::Antigravity => 5,
        ToolKind::OpenCode => 6,
    }
}

#[must_use]
pub const fn tool_from_byte(byte: u8) -> ToolKind {
    match byte {
        2 => ToolKind::Codex,
        3 => ToolKind::Shell,
        4 => ToolKind::Pi,
        5 => ToolKind::Antigravity,
        6 => ToolKind::OpenCode,
        _ => ToolKind::Claude,
    }
}

/// Why a peer gets no file: the same for a missing file and for one outside
/// the project folder, so a refusal tells nothing about the rest of the disk.
const NO_SUCH_HISTORY: &str = "that closed terminal is no longer in the project's history";

const NO_FILE: &str = "no such file in the terminal's project folder on that machine";

/// The regular file `path` names inside `folder` (relative to it, or
/// absolute or `~/…` and under it), opened for reading, with the path to
/// show for it. The file is opened through a handle on the folder that
/// cannot leave it, by `..` or by a link, so what is opened is inside it
/// whatever changes on disk meanwhile; it is opened without waiting, so a
/// pipe or a device cannot hold the host. A network share or device path is
/// never touched.
fn open_beneath(path: &str, folder: &Path) -> Result<(String, std::fs::File), &'static str> {
    use cap_std::ambient_authority;
    use cap_std::fs::{Dir, OpenOptions};
    if path.is_empty()
        || path.chars().any(char::is_control)
        || path.starts_with("\\\\")
        || path.starts_with("//")
        || reaches_network(folder)
    {
        return Err(NO_FILE);
    }
    let root = std::fs::canonicalize(folder).map_err(|_| NO_FILE)?;
    let relative = if let Some(rest) = path.strip_prefix("~/") {
        under(
            &std::env::home_dir().ok_or(NO_FILE)?.join(rest),
            folder,
            &root,
        )?
    } else if Path::new(path).is_absolute() {
        under(Path::new(path), folder, &root)?
    } else {
        PathBuf::from(path)
    };
    let dir = Dir::open_ambient_dir(&root, ambient_authority()).map_err(|_| NO_FILE)?;
    let mut options = OpenOptions::new();
    options.read(true);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(libc::O_NONBLOCK | libc::O_NOCTTY);
    }
    let file = dir
        .open_with(&relative, &options)
        .map_err(|_| NO_FILE)?
        .into_std();
    if !file.metadata().is_ok_and(|meta| meta.is_file()) {
        return Err(NO_FILE);
    }
    // Only for showing, from the path accepted: the file read is the one
    // opened above.
    let mut shown = root;
    for part in relative.components() {
        match part {
            std::path::Component::CurDir => {}
            std::path::Component::ParentDir => {
                shown.pop();
            }
            other => shown.push(other),
        }
    }
    Ok((plain(shown).display().to_string(), file))
}

/// An absolute path as one relative to the project folder, when it is
/// written under the folder as the workspace names it (`folder`) or as it is
/// canonically (`root`, also as Windows usually writes it). Decided from the
/// spelling alone: a path elsewhere is refused without touching the disk.
fn under(path: &Path, folder: &Path, root: &Path) -> Result<PathBuf, &'static str> {
    if reaches_network(path) {
        return Err(NO_FILE);
    }
    let plain_root = plain(root.to_path_buf());
    for base in [folder, root, plain_root.as_path()] {
        if let Ok(rest) = path.strip_prefix(base) {
            return Ok(rest.to_path_buf());
        }
    }
    Err(NO_FILE)
}

/// A canonical path as it is usually written: Windows' `\\?\C:\…` as
/// `C:\…`, so a peer can show it and send it back.
#[cfg(windows)]
fn plain(path: PathBuf) -> PathBuf {
    use std::path::{Component, Prefix};
    let verbatim_disk = matches!(
        path.components().next(),
        Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::VerbatimDisk(_))
    );
    if !verbatim_disk {
        return path;
    }
    let plain = path
        .to_str()
        .and_then(|text| text.strip_prefix("\\\\?\\"))
        .map(PathBuf::from);
    plain.unwrap_or(path)
}

#[cfg(not(windows))]
const fn plain(path: PathBuf) -> PathBuf {
    path
}

/// Whether looking at `path` could reach another machine.
#[cfg(windows)]
fn reaches_network(path: &Path) -> bool {
    use std::path::{Component, Prefix};
    match path.components().next() {
        Some(Component::Prefix(prefix)) => {
            !matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))
        }
        _ => false,
    }
}

#[cfg(not(windows))]
fn reaches_network(path: &Path) -> bool {
    path.as_os_str().as_encoded_bytes().starts_with(b"//")
}

/// Whether what a person has said so far in a voice call is a finished turn,
/// and whether it fits the call (the app answers with Jev, from the key in
/// its settings).
pub trait TurnJudge: Send + Sync {
    /// How likely `text` is a finished turn, and with a `context` how likely
    /// it fits the call, within `wait`.
    ///
    /// # Errors
    /// [`crate::wire::TurnVerdict::Unavailable`] without a key,
    /// [`crate::wire::TurnVerdict::Failed`] when this one failed.
    fn judge_turn(
        &self,
        text: &str,
        context: &str,
        wait: std::time::Duration,
    ) -> Result<crate::wire::TurnScores, crate::wire::TurnVerdict>;
}

/// Every open project's terminals that have a ring, numbered for the wire.
///
/// A wire id names one terminal entry (its project and entry id) for the life of
/// the app: the numbers are handed out once per key and never reused, so a new
/// entry always gets a new id.
pub struct WorkspaceTerminals {
    workspace: Arc<dyn WorkspaceControl>,
    book: Arc<RingBook>,
    attachment_root: Option<PathBuf>,
    /// Where Pi's models come from. Without it, Pi offers only its default.
    agents: Option<Arc<dyn AgentsControl>>,
    /// The host's Siri requests (see [`crate::siri`]), when peers may ask
    /// by voice.
    siri: Option<Arc<crate::siri::SiriDesk>>,
    /// The dialog instances issued to peers' voice calls (see
    /// [`crate::voice_answers::Dialogs`]).
    dialogs: Arc<crate::voice_answers::Dialogs>,
    /// The conversation model's settings file (see [`crate::assistant`]),
    /// read at each request, so a change applies at once.
    assistant: Option<PathBuf>,
    /// The projects' package settings, for a peer's Project Settings.
    packages: Option<Arc<dyn connecto_core::PackagesControl>>,
    /// Agent Token Optimization, for a peer's Tokens tab.
    tokens: Option<Arc<dyn connecto_core::TokenControl>>,
    /// The projects' operations, for a peer's Operations tab.
    ops: Option<Arc<dyn connecto_core::ops::OpsControl>>,
    /// The person's secrets, for a peer's project secrets and the requests
    /// of the projects' terminals.
    vault: Option<Arc<dyn connecto_core::VaultControl>>,
    /// Judges a voice call's turn for a peer (the app's Jev), when set.
    turn_judge: Option<Arc<dyn TurnJudge>>,
    /// The projects' functional memory, for a peer's voice call (ADR 0033).
    functional_memory: Option<Arc<dyn connecto_core::functional_memory::FunctionalMemoryControl>>,
    ids: Mutex<(u32, HashMap<String, u32>)>,
}

impl WorkspaceTerminals {
    #[must_use]
    pub fn new(workspace: Arc<dyn WorkspaceControl>, book: Arc<RingBook>) -> Self {
        Self {
            workspace,
            book,
            attachment_root: None,
            agents: None,
            assistant: None,
            packages: None,
            tokens: None,
            ops: None,
            vault: None,
            turn_judge: None,
            functional_memory: None,
            siri: None,
            dialogs: Arc::default(),
            ids: Mutex::new((0, HashMap::new())),
        }
    }

    /// Peers may read and change the projects' package settings.
    #[must_use]
    pub fn with_packages(mut self, packages: Arc<dyn connecto_core::PackagesControl>) -> Self {
        self.packages = Some(packages);
        self
    }

    /// Peers may read and use the projects' Agent Token Optimization.
    #[must_use]
    pub fn with_tokens(mut self, tokens: Arc<dyn connecto_core::TokenControl>) -> Self {
        self.tokens = Some(tokens);
        self
    }

    /// Peers may read each shared project's functional memory and ask for
    /// an update of it (ADR 0033).
    #[must_use]
    pub fn with_functional_memory(
        mut self,
        memory: Arc<dyn connecto_core::functional_memory::FunctionalMemoryControl>,
    ) -> Self {
        self.functional_memory = Some(memory);
        self
    }

    /// Peers may view, approve and run the projects' operations, as the
    /// person can here (ADR 0031): each one checked as a local one is.
    #[must_use]
    pub fn with_ops(mut self, ops: Arc<dyn connecto_core::ops::OpsControl>) -> Self {
        self.ops = Some(ops);
        self
    }

    /// Peers may list and change each shared project's own secrets, and
    /// answer its terminals' requests for one (ADR 0027): never the
    /// app-wide ones, and never a value back.
    #[must_use]
    pub fn with_vault(mut self, vault: Arc<dyn connecto_core::VaultControl>) -> Self {
        self.vault = Some(vault);
        self
    }

    #[must_use]
    pub fn with_agents(mut self, agents: Arc<dyn AgentsControl>) -> Self {
        self.agents = Some(agents);
        self
    }

    /// Peers may ask the conversation model set in `settings` (a JSON file;
    /// off while it names no model).
    #[must_use]
    pub fn with_assistant(mut self, settings: PathBuf) -> Self {
        self.assistant = Some(settings);
        self
    }

    /// Peers may ask `judge` whether what was said in a voice call is a
    /// finished turn.
    #[must_use]
    pub fn with_turn_judge(mut self, judge: Arc<dyn TurnJudge>) -> Self {
        self.turn_judge = Some(judge);
        self
    }

    /// Peers may ask the projects' app sessions by voice (Siri): `desk`
    /// takes the requests in charge, and should follow the same workspace
    /// (see [`WorkspaceSessions`]).
    #[must_use]
    pub fn with_siri(mut self, desk: Arc<crate::siri::SiriDesk>) -> Self {
        self.siri = Some(desk);
        self
    }

    #[must_use]
    pub fn with_attachment_root(mut self, root: PathBuf) -> Self {
        self.attachment_root = Some(root);
        self
    }

    /// A number for `key` that stays the same for the life of the app, and is
    /// never given to another key. When the numbers are used up, a new key gets
    /// none: its terminal is not offered to peers. Nothing is reused.
    fn id_of(&self, key: &str) -> Option<u32> {
        let mut ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
        if let Some(id) = ids.1.get(key) {
            return Some(*id);
        }
        let id = ids.0.checked_add(1)?;
        ids.0 = id;
        ids.1.insert(key.to_owned(), id);
        Some(id)
    }

    /// The `<project>/<terminal>` key a wire id was given to, if any.
    fn key_of(&self, id: u32) -> Option<String> {
        let ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
        ids.1
            .iter()
            .find(|(_, v)| **v == id)
            .map(|(k, _)| k.clone())
    }
}

impl WorkspaceTerminals {
    /// Why an agent of `tool` may not start in `project` on this host, as the
    /// host's window would refuse it: deactivated (or, for Pi, no provider
    /// that works for the project), or turned off in the project's settings.
    fn tool_refused(&self, project: &str, tool: ToolKind) -> Option<String> {
        if !tool.is_agent() {
            return None;
        }
        if let Some(agents) = &self.agents
            && agents.inactive_for(project).contains(&tool)
        {
            return Some(format!(
                "{} is deactivated or has no working provider on that machine",
                tool.label()
            ));
        }
        if let Some(packages) = &self.packages
            && !packages.allowed_tools(project).contains(&tool)
        {
            return Some(format!(
                "{} is turned off in that project's settings",
                tool.label()
            ));
        }
        None
    }

    /// The registry and entry id of the terminal `id`, when it is in `project`.
    fn locate(
        &self,
        id: u32,
        project: &str,
    ) -> Option<(Arc<dyn connecto_core::TerminalsControl>, String)> {
        let key = {
            let ids = self.ids.lock().unwrap_or_else(PoisonError::into_inner);
            ids.1
                .iter()
                .find(|(_, v)| **v == id)
                .map(|(k, _)| k.clone())?
        };
        let (key_project, entry) = key.split_once('/')?;
        if key_project != project {
            return None;
        }
        let view = self.workspace.view();
        let found = view.projects.iter().find(|p| p.id == project)?;
        Some((found.terminals.clone()?, entry.to_owned()))
    }
}

impl HostTerminals for WorkspaceTerminals {
    fn attachment_root(&self) -> Option<&Path> {
        self.attachment_root.as_deref()
    }

    /// Acts on the registry for the target the right names. The right was checked
    /// against the connection's list before this call, and every target here is
    /// an entry's own identity: a project's folder, or a terminal's entry, which
    /// the registry never gives to a second terminal.
    fn manage(&self, op: &ManageOp, right: &Right) -> Result<Option<u32>, String> {
        match (op, right.target()) {
            (
                ManageOp::Add {
                    tool, name, model, ..
                },
                Target::Project(project),
            ) => {
                let view = self.workspace.view();
                let registry = view
                    .projects
                    .iter()
                    .find(|p| &p.id == project)
                    .and_then(|p| p.terminals.clone())
                    .ok_or_else(|| NO_SUCH_PROJECT.to_owned())?;
                let name = name.clone().filter(|n| !n.trim().is_empty());
                let model = model.clone().filter(|m| !m.trim().is_empty());
                // Adding runs its agent: the checks the host's own window
                // makes (deactivated, or left out of the project's Tools).
                let tool = tool_from_byte(*tool);
                self.tool_refused(project, tool).map_or(Ok(()), Err)?;
                registry.add_on_model(tool, name, model)?;
                Ok(None)
            }
            (
                ManageOp::Close { .. }
                | ManageOp::Restart { .. }
                | ManageOp::Rename { .. }
                | ManageOp::StopBackground { .. },
                Target::Terminal { id, project },
            ) => {
                let (registry, entry) = self
                    .locate(*id, project)
                    .ok_or_else(|| DenyReason::NoSuchTerminal.to_string())?;
                match op {
                    // A peer's close goes through `close_within`, which checks the
                    // subtree where it runs; a plain close is never sent for a peer.
                    ManageOp::Close { .. } => Err(DenyReason::NoSuchTerminal.to_string()),
                    ManageOp::Restart { .. } => {
                        // Starting it runs its agent: the checks the host's
                        // own window makes before a restart.
                        let tool = registry
                            .view()
                            .terminals
                            .iter()
                            .find(|e| e.id == entry)
                            .map(|e| e.tool)
                            .ok_or_else(|| DenyReason::NoSuchTerminal.to_string())?;
                        self.tool_refused(project, tool).map_or(Ok(()), Err)?;
                        registry.restart(&entry).map(|()| None)
                    }
                    ManageOp::Rename { name, .. } => {
                        let name = name.clone().filter(|n| !n.trim().is_empty());
                        registry.rename(&entry, name).map(|()| None)
                    }
                    // Waits for the outcome, so a refusal found only once the
                    // shells are looked for reaches the peer in its reply.
                    ManageOp::StopBackground { task, .. } => registry
                        .stop_background_now(&entry, task.as_deref())
                        .map(|_| None),
                    ManageOp::Add { .. } => Err(DenyReason::NoSuchTerminal.to_string()),
                }
            }
            _ => Err(DenyReason::NoSuchTerminal.to_string()),
        }
    }

    fn set_recording(&self, on: bool) {
        self.book.set_recording(on);
    }

    fn list(&self) -> Vec<HostTerminal> {
        // Each listed terminal with the key of its parent, in project order and
        // then terminal order.
        let mut found = Vec::new();
        let now = std::time::Instant::now();
        for project in &self.workspace.view().projects {
            let Some(terminals) = &project.terminals else {
                continue;
            };
            for entry in &terminals.view().terminals {
                let key = format!("{}/{}", project.id, entry.id);
                // Every terminal is listed, as the host's sidebar lists it. One
                // that has not run since the app started gets an empty ring,
                // which its first start replaces; a restored chat waiting for
                // Start has always had one in the book. A ring takes no memory
                // until output comes. Older peers are listed, as before, only
                // what has a real ring (`ran`).
                let chat = entry.name.as_deref() == Some(connecto_core::session::APP_SESSION);
                let (ring, ran) = self.book.listed(&key, chat);
                // A terminal with no number left is not offered to peers.
                let Some(id) = self.id_of(&key) else {
                    continue;
                };
                let parent_key = entry
                    .parent
                    .as_ref()
                    .map(|parent| format!("{}/{parent}", project.id));
                // Only when it differs from the parent: a peer reads none as
                // the parent.
                let owner_key = entry
                    .asked_by
                    .as_ref()
                    .filter(|asker| entry.parent.as_ref() != Some(*asker))
                    .map(|asker| format!("{}/{asker}", project.id));
                let term = HostTerminal {
                    id,
                    name: entry.name.clone().unwrap_or_else(|| entry.id.clone()),
                    tool: tool_byte(entry.tool),
                    project_id: project.id.clone(),
                    project_name: project.name.clone(),
                    parent: None,
                    attempt: 0,
                    activity: activity_byte(entry.state),
                    session: Arc::clone(&entry.session),
                    ring,
                    background: background_infos(&entry.background, now),
                    entry: Some(entry.id.clone()),
                    ran,
                    owner: None,
                };
                found.push((key, parent_key, owner_key, term));
            }
        }
        // A parent or an owner is named only when it is listed too, and never
        // itself.
        let listed: HashMap<String, u32> = found
            .iter()
            .map(|(key, _, _, term)| (key.clone(), term.id))
            .collect();
        let named = |key: &str, other: Option<String>| {
            other
                .filter(|other| other != key)
                .and_then(|other| listed.get(&other).copied())
        };
        found
            .into_iter()
            .map(|(key, parent_key, owner_key, mut term)| {
                term.parent = named(&key, parent_key);
                term.owner = named(&key, owner_key);
                term
            })
            .collect()
    }

    fn projects(&self) -> Vec<ProjectInfo> {
        self.workspace
            .view()
            .projects
            .iter()
            .filter(|p| p.terminals.is_some())
            .map(|p| ProjectInfo {
                id: p.id.clone(),
                name: p.name.clone(),
            })
            .collect()
    }

    /// Closes the target, and everything under it, through the registry, which
    /// decides at the moment of the close: each entry under it must be one of the
    /// `listed` ones (wire ids in this project), or nothing is closed.
    fn close_within(&self, right: &Right, listed: &[u32]) -> Result<Option<u32>, String> {
        let Target::Terminal { id, project } = right.target() else {
            return Err(DenyReason::NoSuchTerminal.to_string());
        };
        let (registry, entry) = self
            .locate(*id, project)
            .ok_or_else(|| DenyReason::NoSuchTerminal.to_string())?;
        let allowed: Vec<String> = listed
            .iter()
            .filter_map(|wire| self.key_of(*wire))
            .filter_map(|key| {
                key.split_once('/')
                    .filter(|(key_project, _)| key_project == project)
                    .map(|(_, entry)| entry.to_owned())
            })
            .collect();
        registry.close_within(&entry, &allowed).map(|()| None)
    }

    fn active_project(&self) -> Option<String> {
        self.workspace.view().active.clone()
    }

    /// A path as a terminal printed it, on this machine: `~/…` from the
    /// home folder, a relative one from the terminal's project folder. Only
    /// a regular file inside that folder is sent: a peer may be let into a
    /// terminal that runs nothing it types (stopped, or an agent that asks
    /// first), so a terminal is no grant to the rest of the disk.
    fn open_file(&self, right: &Right, path: &str) -> Result<(String, std::fs::File), String> {
        let Target::Terminal { project, .. } = right.target() else {
            return Err(NO_FILE.to_owned());
        };
        let folder = self
            .workspace
            .view()
            .projects
            .iter()
            .find(|p| p.id == *project)
            .map(|p| PathBuf::from(&p.path))
            .ok_or(NO_FILE)?;
        open_beneath(path, &folder).map_err(str::to_owned)
    }

    /// The offers of `connecto send-file` in this machine's projects.
    fn groups(&self) -> connecto_core::RemoteGroups {
        let view = self.workspace.view();
        connecto_core::RemoteGroups {
            groups: view.groups.clone(),
            of: view
                .projects
                .iter()
                .filter_map(|p| p.group.clone().map(|g| (p.id.clone(), g)))
                .collect(),
        }
    }

    fn secret_asks(&self) -> Vec<connecto_core::SecretAsk> {
        self.vault
            .as_ref()
            .map(|vault| vault.view().asks.clone())
            .unwrap_or_default()
    }

    fn offers(&self) -> Vec<crate::wire::OfferInfo> {
        let now = std::time::Instant::now();
        connecto_core::offers::shared()
            .live()
            .into_iter()
            .map(|offer| crate::wire::OfferInfo {
                id: offer.id,
                age_secs: u32::try_from(now.saturating_duration_since(offer.offered).as_secs())
                    .unwrap_or(u32::MAX),
                project: offer.project_id,
                terminal: offer.terminal,
                name: offer.name,
                bytes: offer.bytes,
            })
            .collect()
    }

    /// The offered file, through the handle opened when it was offered: a
    /// file put at its path since (renamed, replaced, a link) is never what
    /// is sent. The host reads it by offset, so downloads at once do not
    /// share a position.
    fn open_offer(&self, id: u64) -> Result<(String, std::fs::File), String> {
        let offer = connecto_core::offers::shared()
            .get(id)
            .ok_or(crate::host::NO_SUCH_OFFER)?;
        let file = offer
            .file
            .try_clone()
            .map_err(|_| "that file cannot be read on that machine".to_owned())?;
        match file.metadata() {
            Ok(meta) if meta.is_file() && meta.len() <= connecto_core::offers::MAX_OFFER_BYTES => {
                Ok((offer.name, file))
            }
            Ok(meta) if meta.is_file() => Err("that file grew past what is sent".to_owned()),
            _ => Err("that file cannot be read on that machine".to_owned()),
        }
    }

    /// Through the same seams the window uses for this machine's projects.
    /// Without a caller, a Siri ask is not answered: it needs the peer.
    fn project_ask(&self, right: &Right, ask: &ProjectAsk) -> Result<Vec<u8>, String> {
        self.project_ask_by(right, ask, &NoCaller)
    }

    fn project_ask_by(
        &self,
        right: &Right,
        ask: &ProjectAsk,
        caller: &dyn crate::siri::SiriCaller,
    ) -> Result<Vec<u8>, String> {
        let Target::Project(project) = right.target() else {
            return Err(NO_SUCH_PROJECT.to_owned());
        };
        let registry = || {
            self.workspace
                .view()
                .projects
                .iter()
                .find(|p| &p.id == project)
                .and_then(|p| p.terminals.clone())
                .ok_or_else(|| NO_SUCH_PROJECT.to_owned())
        };
        let packages = || self.packages.clone().ok_or(NO_PROJECT_PANELS);
        let tokens = || self.tokens.clone().ok_or(NO_PROJECT_PANELS);
        match ask {
            ProjectAsk::History => Ok(history_json(&registry()?.view().history)),
            ProjectAsk::Transcript { history } => {
                let registry = registry()?;
                if !registry.view().history.iter().any(|e| e.id == *history) {
                    return Err(NO_SUCH_HISTORY.to_owned());
                }
                let text = registry.read_transcript(history)?;
                Ok(transcript_json(history, &text))
            }
            ProjectAsk::Continue { history } => {
                let registry = registry()?;
                let tool = registry
                    .view()
                    .history
                    .iter()
                    .find(|e| e.id == *history)
                    .map(|e| e.tool)
                    .ok_or(NO_SUCH_HISTORY)?;
                // The project's own settings decide which agents it runs.
                if let Some(packages) = &self.packages
                    && !packages.allowed_tools(project).contains(&tool)
                {
                    return Err(format!(
                        "{} is turned off in that project's settings",
                        tool.label()
                    ));
                }
                registry.continue_from(history)?;
                Ok(done_json())
            }
            ProjectAsk::Settings => {
                let view = packages()?.view();
                Ok(settings_answer_json(&SettingsAnswer {
                    packages: view.packages.clone(),
                    default_package: view.default_package.clone(),
                    project: view.project(project).cloned(),
                    last_error: view.last_error.clone(),
                    // Said by its settings (they name `tool_list`).
                    tools_one_or_all: false,
                }))
            }
            ProjectAsk::SetSettings(settings)
            | ProjectAsk::SetSettingsBeforeOpenCode(settings)
            | ProjectAsk::SetSettingsBeforeToolSets(settings) => {
                // A package the host does not have is refused, never kept.
                let view = packages()?.view();
                if let Some(missing) = settings
                    .packages
                    .iter()
                    .find(|id| !view.packages.iter().any(|p| p.id == **id))
                {
                    return Err(format!("that machine has no package {missing}"));
                }
                // What an older peer does not say (OpenCode's config), or
                // says only as one tool or all, is resolved against the
                // project as it is when the write is applied, after every
                // write queued before it.
                let settings = settings.clone();
                match ask {
                    ProjectAsk::SetSettingsBeforeOpenCode(_) => {
                        packages()?.set_project_settings_keeping_opencode(project, settings)?;
                    }
                    ProjectAsk::SetSettingsBeforeToolSets(_) => {
                        packages()?.set_project_settings_keeping_tools(project, settings)?;
                    }
                    _ => packages()?.set_project_settings(project, settings)?,
                }
                Ok(done_json())
            }
            ProjectAsk::Refresh => {
                packages()?.refresh_project(project);
                Ok(done_json())
            }
            ProjectAsk::CheckRequirements { package } => {
                packages()?.check_requirements(package)?;
                Ok(done_json())
            }
            // The command is the host's own package's, for the host's system,
            // and runs in a terminal the host's user sees (ADR 0037).
            ProjectAsk::InstallRequirement {
                package,
                requirement,
                command,
            } => {
                packages()?.install_requirement(project, package, requirement, command)?;
                Ok(done_json())
            }
            ProjectAsk::Tokens => {
                let tokens = tokens()?;
                tokens.open(project);
                let view = tokens.view();
                Ok(tokens_answer_json(&TokensAnswer {
                    available: view.available,
                    project: view.project(project).cloned(),
                }))
            }
            ProjectAsk::TokensAnalyze => tokens()?.analyze(project).map(|()| done_json()),
            ProjectAsk::TokensApply { plugins, skills } => {
                // Only what the project's latest analysis offers to cut, as
                // the window offers it here: the whole request or none of it.
                let tokens = tokens()?;
                let view = tokens.view();
                let report = view
                    .project(project)
                    .and_then(|p| p.report.as_ref())
                    .ok_or("Analyze past sessions first.")?;
                let offered = |kind: connecto_core::tokens::ProposalKind| -> Vec<&String> {
                    report
                        .proposals
                        .iter()
                        .filter(|p| p.kind == kind)
                        .flat_map(|p| p.ids.iter())
                        .collect()
                };
                let plugin_ids = offered(connecto_core::tokens::ProposalKind::Plugin);
                let skill_ids = offered(connecto_core::tokens::ProposalKind::Skill);
                if let Some(other) = plugins
                    .iter()
                    .find(|id| !plugin_ids.contains(id))
                    .or_else(|| skills.iter().find(|id| !skill_ids.contains(id)))
                {
                    return Err(format!(
                        "{other} is not among the cuts this project's analysis proposes"
                    ));
                }
                tokens
                    .apply(project, plugins.clone(), skills.clone())
                    .map(|()| done_json())
            }
            ProjectAsk::TokensUndo => tokens()?.undo(project).map(|()| done_json()),
            ProjectAsk::TokensAutocompact(n) => {
                tokens()?.set_autocompact(project, *n).map(|()| done_json())
            }
            ProjectAsk::FunctionalMemory
            | ProjectAsk::FunctionalMemoryUpdate
            | ProjectAsk::FunctionalMemorySet { .. } => {
                let memory = self.functional_memory.as_deref().ok_or(NO_PROJECT_PANELS)?;
                match ask {
                    ProjectAsk::FunctionalMemoryUpdate => memory.update_now(project)?,
                    // As the window here changes them; a model only from
                    // what this machine's CLIs offer (checked by the host).
                    ProjectAsk::FunctionalMemorySet { auto, model } => {
                        if let Some(on) = auto {
                            memory.set_auto(project, *on)?;
                        }
                        if let Some(model) = model {
                            memory.set_model(project, model.clone())?;
                        }
                    }
                    _ => {}
                }
                let files = memory.files(project)?;
                Ok(crate::project_panels::functional_memory_json(
                    &files,
                    &memory.status(project),
                ))
            }
            ProjectAsk::Ops { .. } | ProjectAsk::OpsAct(_) => {
                let ops = self.ops.as_deref().ok_or(NO_PROJECT_PANELS)?;
                answer_ops(ops, project, ask)
            }
            ProjectAsk::SiriAsk { terminal, .. } | ProjectAsk::SiriStatus { terminal } => {
                let desk = self
                    .siri
                    .as_deref()
                    .ok_or(crate::project_panels::UNKNOWN_ASK)?;
                // The host's own id of the terminal the peer named, when it is
                // one of this project's.
                let named = self.key_of(*terminal).and_then(|key| {
                    key.split_once('/')
                        .filter(|(p, _)| p == project)
                        .map(|(_, id)| id.to_owned())
                });
                answer_siri(desk, project, named.as_deref(), ask, caller)
            }
            // Any agent terminal of the project: a coder's or a reviewer's
            // permission too. Typed as the peer's input, through the guard.
            ProjectAsk::Questions
            | ProjectAsk::QuestionAnswer { .. }
            | ProjectAsk::SendPrompt { .. } => {
                // Only the terminals the peer's latest list grants it, each
                // by its wire id, read again at each step as they are now;
                // the grant is checked again with each write.
                let now = || {
                    registry().map_or_else(
                        |_| Vec::new(),
                        |r| {
                            crate::voice_answers::granted_terminals(
                                r.view().terminals.clone(),
                                |id| self.id_of(&format!("{project}/{id}")),
                                |wire| caller.granted(wire),
                            )
                        },
                    )
                };
                registry()?;
                let type_now = |entry: &connecto_core::TerminalEntry,
                                wire: u32,
                                condition: &connecto_core::InputCondition,
                                bytes: Vec<u8>| {
                    caller.type_now(entry, wire, condition, bytes)
                };
                let answer = match ask {
                    ProjectAsk::QuestionAnswer {
                        terminal,
                        question,
                        occurrence,
                        choice,
                    } => crate::voice_answers::answer(
                        &self.dialogs,
                        &now,
                        *terminal,
                        question,
                        occurrence,
                        choice,
                        &type_now,
                    )?,
                    ProjectAsk::SendPrompt { terminal, text } => {
                        crate::voice_answers::send_prompt(&now, *terminal, text, &type_now)?
                    }
                    _ => crate::voice_answers::questions(&self.dialogs, &now()),
                };
                Ok(answer.to_string().into_bytes())
            }
            ProjectAsk::SiriResult { .. }
            | ProjectAsk::SiriDelivered { .. }
            | ProjectAsk::PushRegister { .. }
            | ProjectAsk::ActivityToken { .. } => {
                let desk = self
                    .siri
                    .as_deref()
                    .ok_or(crate::project_panels::UNKNOWN_ASK)?;
                answer_siri(desk, project, None, ask, caller)
            }
            ProjectAsk::Secrets { .. } | ProjectAsk::SecretsAct(_) => {
                let vault = self.vault.as_deref().ok_or(NO_PROJECT_PANELS)?;
                let name = self
                    .workspace
                    .view()
                    .projects
                    .iter()
                    .find(|p| &p.id == project)
                    .map(|p| p.name.clone())
                    .ok_or_else(|| NO_SUCH_PROJECT.to_owned())?;
                crate::remote_secrets::answer_secrets(vault, project, &name, ask)
            }
        }
    }

    fn judge_turn(
        &self,
        text: &str,
        context: &str,
        wait: std::time::Duration,
    ) -> Result<crate::wire::TurnScores, crate::wire::TurnVerdict> {
        match &self.turn_judge {
            Some(judge) => judge.judge_turn(text, context, wait),
            None => Err(crate::wire::TurnVerdict::Unavailable(
                crate::wire::NO_TURN_JUDGE.to_owned(),
            )),
        }
    }

    fn complete(&self, request: &crate::wire::Completion) -> Result<String, String> {
        let config = self
            .assistant
            .as_deref()
            .and_then(crate::assistant::AssistantConfig::load)
            .ok_or(crate::assistant::NOT_SET_UP)?;
        let wait = std::time::Duration::from_millis(u64::from(request.wait_ms));
        crate::assistant::complete(&config, request, wait)
    }

    fn models(&self, tool: u8, project: &str) -> ToolModels {
        let tool = tool_from_byte(tool);
        if tool != ToolKind::Pi {
            // The tool's own list (Codex's model cache) when it keeps one.
            let own = self
                .workspace
                .view()
                .projects
                .iter()
                .find(|p| p.id == project)
                .and_then(|p| p.terminals.clone())
                .and_then(|registry| registry.offered_models(tool));
            return ToolModels {
                default: None,
                models: own.unwrap_or_else(|| {
                    tool.suggested_models()
                        .iter()
                        .map(|m| (*m).to_owned())
                        .collect()
                }),
            };
        }
        // As the window's chooser offers them: the project's Pi providers, and
        // the default only while it is one a new terminal really gets.
        let Some(agents) = &self.agents else {
            return ToolModels::default();
        };
        let pi = agents.pi_providers(&agents.provider_scope(project, ToolKind::Pi));
        ToolModels {
            default: pi
                .default_model
                .filter(|model| pi.models.is_empty() || pi.models.contains(model)),
            models: pi.models,
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;

    #[test]
    fn a_peers_path_is_opened_inside_the_project_folder_and_only_as_a_file() {
        use std::io::Read;
        let dir = tempfile::tempdir().unwrap();
        let folder = dir.path().join("project");
        std::fs::create_dir_all(folder.join("docs")).unwrap();
        std::fs::write(folder.join("docs/plan.md"), "# Plan").unwrap();
        std::fs::write(dir.path().join("secret.txt"), "no").unwrap();
        let file = plain(std::fs::canonicalize(folder.join("docs/plan.md")).unwrap());
        let open = |path: &str| {
            open_beneath(path, &folder).map(|(shown, mut f)| {
                let mut text = String::new();
                f.read_to_string(&mut text).unwrap();
                (shown, text)
            })
        };
        let plan = Ok((file.display().to_string(), "# Plan".to_owned()));
        assert_eq!(open("docs/plan.md"), plan);
        assert_eq!(open("docs/../docs/plan.md"), plan);
        // The path it shows is one the peer can send back, and the folder as
        // the workspace names it works too.
        assert_eq!(open(&file.display().to_string()), plan);
        assert_eq!(
            open(&folder.join("docs/plan.md").display().to_string()),
            plan
        );
        for refused in [
            "docs",
            "docs/missing.md",
            "",
            "//server/share/a.md",
            "\\\\server\\share\\a.md",
            "docs/plan.md\n",
            // Outside the folder, however it is named, and told apart from
            // nothing missing.
            "../secret.txt",
            "../missing.txt",
        ] {
            assert_eq!(open(refused), Err(NO_FILE), "{refused}");
        }
        let outside = dir.path().join("secret.txt").display().to_string();
        assert_eq!(open(&outside), Err(NO_FILE));
        #[cfg(unix)]
        {
            std::os::unix::fs::symlink(dir.path().join("secret.txt"), folder.join("link.txt"))
                .unwrap();
            assert_eq!(open("link.txt"), Err(NO_FILE), "a link out");
            std::os::unix::fs::symlink(dir.path(), folder.join("up")).unwrap();
            assert_eq!(open("up/secret.txt"), Err(NO_FILE), "a folder link out");
        }
    }

    #[test]
    fn a_restart_closes_the_old_ring_and_numbers_afresh() {
        let book = RingBook::default();
        let first = book.tap_for("p/claude-1");
        first.feed(b"abc", &mut |_| {});
        let old = book.ring("p/claude-1").unwrap();
        assert_eq!(old.end(), 3);
        let second = book.tap_for("p/claude-1");
        second.feed(b"x", &mut |_| {});
        assert!(old.is_closed());
        let new = book.ring("p/claude-1").unwrap();
        assert_eq!(new.end(), 1);
        assert!(!new.is_closed());
        assert!(book.ring("p/other").is_none());
    }

    #[test]
    fn output_is_kept_only_while_recording_and_always_numbered() {
        let book = RingBook::default();
        let tap = book.tap_for("p/t");
        tap.feed(b"before", &mut |_| {});
        let ring = book.ring("p/t").unwrap();
        assert_eq!(ring.end(), 6, "numbered while off");
        assert!(matches!(
            ring.read(0, 10),
            crate::seq::RingRead::Evicted { .. }
        ));
        book.set_recording(true);
        tap.feed(b"after", &mut |_| {});
        assert_eq!(ring.end(), 11);
        assert!(matches!(
            ring.read(6, 10),
            crate::seq::RingRead::Data { .. }
        ));
        book.set_recording(false);
        tap.feed(b"x", &mut |_| {});
        assert!(matches!(
            ring.read(6, 10),
            crate::seq::RingRead::Evicted { .. }
        ));
    }

    #[test]
    fn the_status_dot_and_tool_survive_the_wire() {
        for byte in 0..=9 {
            assert_eq!(activity_byte(activity_from_byte(byte)), byte);
        }
        assert_eq!(tool_from_byte(tool_byte(ToolKind::Codex)), ToolKind::Codex);
        assert_eq!(tool_from_byte(tool_byte(ToolKind::Shell)), ToolKind::Shell);
        for tool in ToolKind::ALL {
            assert_eq!(tool_from_byte(tool_byte(tool)), tool);
        }
        assert_eq!(
            activity_from_byte(activity_byte(TermActivity::Plain)),
            TermActivity::Plain
        );
        assert_eq!(
            tool_from_byte(tool_byte(ToolKind::Claude)),
            ToolKind::Claude
        );
    }

    use std::path::Path;

    use connecto_core::{
        GridSize, InputRefused, PauseSwitch, ProjectView, ResizeOutcome, SaveState, SessionStatus,
        SnapshotOutcome, TermSnapshot, TerminalEntry, TerminalSession, TerminalsControl,
        TerminalsView, WorkspaceControl, WorkspaceView,
    };

    use crate::host::HostTerminals;

    /// A session with no output and no process.
    struct Quiet;

    impl TerminalSession for Quiet {
        fn poll_snapshot(&self, _out: &mut TermSnapshot) -> SnapshotOutcome {
            SnapshotOutcome::Unchanged
        }
        fn send_input(&self, _bytes: Vec<u8>) -> Result<(), InputRefused> {
            Ok(())
        }
        fn resize(&self, _size: GridSize) -> ResizeOutcome {
            ResizeOutcome::Applied
        }
        fn scroll(&self, _lines: i32) -> bool {
            false
        }
        fn status(&self) -> SessionStatus {
            SessionStatus::Running
        }
    }

    /// A project's registry: a list, and a record of what was added.
    #[derive(Default)]
    struct Registry {
        entries: Mutex<Vec<TerminalEntry>>,
        adds: Mutex<Vec<(ToolKind, Option<String>)>>,
        /// The model of each add, in the same order.
        models: Mutex<Vec<Option<String>>>,
        /// What Codex lists itself here, as its model cache would say.
        codex_models: Option<Vec<String>>,
        /// The entries the registry was asked to close, by entry id.
        closed: Mutex<Vec<String>>,
        /// The entries the registry was asked to restart, by entry id.
        restarted: Mutex<Vec<String>>,
        /// Why a stop of background shells is refused once the shells are
        /// looked for: the answer a window does not wait for.
        stop_refusal: Mutex<Option<String>>,
    }

    impl TerminalsControl for Registry {
        fn view(&self) -> Arc<TerminalsView> {
            Arc::new(TerminalsView {
                terminals: self.entries.lock().unwrap().clone(),
                history: Arc::default(),
                transcript: None,
                save: SaveState::Saved,
                last_error: None,
                warning: None,
            })
        }
        fn add(&self, tool: ToolKind, name: Option<String>) -> Result<(), String> {
            self.add_on_model(tool, name, None)
        }
        fn add_on_model(
            &self,
            tool: ToolKind,
            name: Option<String>,
            model: Option<String>,
        ) -> Result<(), String> {
            self.adds.lock().unwrap().push((tool, name));
            self.models.lock().unwrap().push(model);
            Ok(())
        }
        fn offered_models(&self, tool: ToolKind) -> Option<Vec<String>> {
            (tool == ToolKind::Codex)
                .then(|| self.codex_models.clone())
                .flatten()
        }
        fn close(&self, _id: &str) -> Result<(), String> {
            Ok(())
        }
        fn close_within(&self, id: &str, _allowed: &[String]) -> Result<(), String> {
            self.closed.lock().unwrap().push(id.to_owned());
            Ok(())
        }
        fn rename(&self, _id: &str, _name: Option<String>) -> Result<(), String> {
            Ok(())
        }
        fn restart(&self, id: &str) -> Result<(), String> {
            self.restarted.lock().unwrap().push(id.to_owned());
            Ok(())
        }
        fn stop_background(&self, _id: &str, _task: Option<&str>) -> Result<String, String> {
            Ok("Stopping 1 background shell.".to_owned())
        }
        fn stop_background_now(&self, id: &str, task: Option<&str>) -> Result<String, String> {
            match self.stop_refusal.lock().unwrap().clone() {
                Some(why) => Err(why),
                None => self.stop_background(id, task),
            }
        }
        fn continue_from(&self, _history_id: &str) -> Result<(), String> {
            Ok(())
        }
        fn request_transcript(&self, _history_id: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn entry(id: &str, parent: Option<&str>) -> TerminalEntry {
        TerminalEntry {
            id: id.to_owned(),
            name: Some(format!("name {id}")),
            parent: parent.map(str::to_owned),
            asked_by: None,
            tool: ToolKind::Claude,
            state: TermActivity::Idle,
            in_turn: false,
            background: Vec::new(),
            finished_turns: 0,
            session: Arc::new(Quiet),
            control: None,
            desired_running: true,
            shown_id: None,
            activity: 0,
        }
    }

    fn project(id: &str, name: &str, registry: Option<Arc<Registry>>) -> ProjectView {
        ProjectView {
            id: id.to_owned(),
            name: name.to_owned(),
            path: String::new(),
            terminals: registry.map(|r| r as Arc<dyn TerminalsControl>),
            error: None,
            expanded: true,
            active_terminal: None,
            group: None,
        }
    }

    /// A workspace whose view is fixed.
    struct Projects(Arc<WorkspaceView>);

    impl WorkspaceControl for Projects {
        fn view(&self) -> Arc<WorkspaceView> {
            Arc::clone(&self.0)
        }
        fn open_project(&self, _path: &Path) -> Result<(), String> {
            Ok(())
        }
        fn activate(&self, _project: &str) {}
        fn close_project(&self, _project: &str) -> Result<(), String> {
            Ok(())
        }
        fn rename_project(&self, _project: &str, _name: Option<String>) -> Result<(), String> {
            Ok(())
        }
        fn move_project(&self, _project: &str, _index: usize) {}
        fn set_expanded(&self, _project: &str, _expanded: bool) {}
        fn set_sidebar_collapsed(&self, _collapsed: bool) {}
        fn set_active_terminal(&self, _project: &str, _terminal: &str) {}
        fn clear_recents(&self) {}
        fn pause_switch(&self) -> Arc<PauseSwitch> {
            Arc::new(PauseSwitch::default())
        }
        fn stop_everything(&self) {}
    }

    /// Two open projects, "a" active with a child under its first terminal,
    /// and "b" with one terminal. "c" is open but not set up. "d" is open with
    /// a terminal that has no ring, so nothing of it is listed.
    fn workspace() -> (
        Arc<Registry>,
        Arc<Registry>,
        WorkspaceTerminals,
        Arc<RingBook>,
    ) {
        let a = Arc::new(Registry {
            entries: Mutex::new(vec![
                entry("t1", None),
                entry("t2", Some("t1")),
                entry("t3", None),
            ]),
            adds: Mutex::default(),
            models: Mutex::default(),
            codex_models: None,
            closed: Mutex::default(),
            restarted: Mutex::default(),
            stop_refusal: Mutex::default(),
        });
        let b = Arc::new(Registry {
            entries: Mutex::new(vec![entry("u1", None)]),
            adds: Mutex::default(),
            models: Mutex::default(),
            codex_models: None,
            closed: Mutex::default(),
            restarted: Mutex::default(),
            stop_refusal: Mutex::default(),
        });
        let view = WorkspaceView {
            projects: vec![
                project("a", "Alpha", Some(Arc::clone(&a))),
                project("b", "Beta", Some(Arc::clone(&b))),
                project("c", "Gamma", None),
                project(
                    "d",
                    "Delta",
                    Some(Arc::new(Registry {
                        entries: Mutex::new(vec![entry("x1", None)]),
                        adds: Mutex::default(),
                        models: Mutex::default(),
                        codex_models: None,
                        closed: Mutex::default(),
                        restarted: Mutex::default(),
                        stop_refusal: Mutex::default(),
                    })),
                ),
                project("e", "Epsilon", Some(Arc::new(Registry::default()))),
            ],
            active: Some("a".to_owned()),
            groups: Vec::new(),
            recents: Vec::new(),
            sidebar_collapsed: false,
            last_error: None,
            warning: None,
        };
        let book = Arc::new(RingBook::default());
        for key in ["a/t1", "a/t2", "a/t3", "b/u1"] {
            let _ = book.tap_for(key);
        }
        let terminals =
            WorkspaceTerminals::new(Arc::new(Projects(Arc::new(view))), Arc::clone(&book));
        (a, b, terminals, book)
    }

    /// The host's own groups, from its workspace: in its order, empty ones
    /// too, and the group of each project in one.
    #[test]
    fn the_hosts_groups_are_its_workspaces_in_its_order() {
        use connecto_core::ProjectGroup;
        let mut alpha = project("a", "Alpha", Some(Arc::new(Registry::default())));
        alpha.group = Some("group-2".to_owned());
        let beta = project("b", "Beta", Some(Arc::new(Registry::default())));
        let mut gamma = project("c", "Gamma", Some(Arc::new(Registry::default())));
        gamma.group = Some("group-1".to_owned());
        let view = WorkspaceView {
            projects: vec![alpha, beta, gamma],
            groups: vec![
                ProjectGroup {
                    id: "group-2".into(),
                    name: "HARBOR".into(),
                },
                ProjectGroup {
                    id: "group-1".into(),
                    name: "Clients".into(),
                },
                ProjectGroup {
                    id: "group-3".into(),
                    name: "Empty".into(),
                },
            ],
            ..WorkspaceView::default()
        };
        let terminals = WorkspaceTerminals::new(
            Arc::new(Projects(Arc::new(view))),
            Arc::new(RingBook::default()),
        );
        let groups = crate::host::HostTerminals::groups(&terminals);
        assert_eq!(
            groups
                .groups
                .iter()
                .map(|g| g.name.as_str())
                .collect::<Vec<_>>(),
            ["HARBOR", "Clients", "Empty"]
        );
        assert_eq!(
            groups.of,
            vec![
                ("a".to_owned(), "group-2".to_owned()),
                ("c".to_owned(), "group-1".to_owned()),
            ],
            "b is in none"
        );
    }

    /// The right the host would have given for an open project.
    fn project_right(_terminals: &WorkspaceTerminals, id: &str) -> Right {
        Right::for_test(Target::Project(id.to_owned()))
    }

    fn terminal_right(term: &HostTerminal) -> Right {
        Right::for_test(Target::Terminal {
            id: term.id,
            project: term.project_id.clone(),
        })
    }

    #[test]
    fn the_list_carries_projects_in_order_and_names_only_listed_parents() {
        let (_, _, terminals, _) = workspace();
        let list = terminals.list();
        let shape: Vec<(&str, &str, &str, Option<u32>)> = list
            .iter()
            .map(|t| {
                (
                    t.project_id.as_str(),
                    t.project_name.as_str(),
                    t.name.as_str(),
                    t.parent,
                )
            })
            .collect();
        let first = list[0].id;
        assert_eq!(
            shape,
            vec![
                ("a", "Alpha", "name t1", None),
                ("a", "Alpha", "name t2", Some(first)),
                ("a", "Alpha", "name t3", None),
                ("b", "Beta", "name u1", None),
                ("d", "Delta", "name x1", None),
            ]
        );
        // Each carries its id in the host's own list.
        let ids: Vec<Option<&str>> = list.iter().map(|t| t.entry.as_deref()).collect();
        assert_eq!(
            ids,
            vec![Some("t1"), Some("t2"), Some("t3"), Some("u1"), Some("x1")]
        );
        // The ids are the ones the terminal stream and management use, and a
        // close goes to the entry the right names.
        assert_eq!(
            terminals.close_within(&terminal_right(&list[3]), &[list[3].id]),
            Ok(None)
        );
    }

    /// A helper an agent asked for, filed under the top-level terminal, is
    /// listed with that agent as its owner; one whose asker is its parent,
    /// or is not listed, names none (a peer reads none as the parent).
    #[test]
    fn the_list_names_the_agent_that_asked_for_a_terminal_as_its_owner() {
        let mut helper = entry("claude-80", Some("claude-68"));
        helper.asked_by = Some("claude-78".to_owned());
        let mut direct = entry("claude-81", Some("claude-68"));
        direct.asked_by = Some("claude-68".to_owned());
        let mut orphan = entry("claude-82", Some("claude-68"));
        orphan.asked_by = Some("claude-99".to_owned());
        let registry = Arc::new(Registry {
            entries: Mutex::new(vec![
                entry("claude-68", None),
                entry("claude-78", Some("claude-68")),
                helper,
                direct,
                orphan,
            ]),
            ..Registry::default()
        });
        let view = WorkspaceView {
            projects: vec![project("p", "connecto", Some(registry))],
            active: Some("p".to_owned()),
            groups: Vec::new(),
            recents: Vec::new(),
            sidebar_collapsed: false,
            last_error: None,
            warning: None,
        };
        let terminals = WorkspaceTerminals::new(
            Arc::new(Projects(Arc::new(view))),
            Arc::new(RingBook::default()),
        );
        let list = terminals.list();
        let id = |entry: &str| {
            list.iter()
                .find(|t| t.entry.as_deref() == Some(entry))
                .map(|t| t.id)
        };
        let owners: Vec<(&str, Option<u32>)> = list
            .iter()
            .map(|t| (t.entry.as_deref().unwrap(), t.owner))
            .collect();
        assert_eq!(
            owners,
            vec![
                ("claude-68", None),
                ("claude-78", None),
                ("claude-80", id("claude-78")),
                ("claude-81", None),
                ("claude-82", None),
            ]
        );
        assert!(id("claude-78").is_some());
    }

    /// A peer sees the host's stopped terminals too, in the host's order and
    /// nesting: one that has not run since the host started has no output
    /// yet, and was left out, so a remote Mac showed only the terminals
    /// running there.
    #[test]
    fn a_stopped_terminal_is_listed_like_a_running_one() {
        let mut stopped = entry("claude-7", None);
        stopped.state = TermActivity::NotStarted;
        stopped.desired_running = false;
        let mut child = entry("claude-8", Some("claude-7"));
        child.state = TermActivity::NotStarted;
        let registry = Arc::new(Registry {
            entries: Mutex::new(vec![
                entry("claude", None),
                stopped,
                child,
                entry("claude-12", None),
                entry("claude-14", Some("claude-12")),
            ]),
            ..Registry::default()
        });
        let view = WorkspaceView {
            projects: vec![project("p", "harbor_app", Some(registry))],
            active: Some("p".to_owned()),
            groups: Vec::new(),
            recents: Vec::new(),
            sidebar_collapsed: false,
            last_error: None,
            warning: None,
        };
        let book = Arc::new(RingBook::default());
        // Only the two running ones have started since the app did.
        for key in ["p/claude-12", "p/claude-14"] {
            let _ = book.tap_for(key);
        }
        let terminals = WorkspaceTerminals::new(Arc::new(Projects(Arc::new(view))), book);
        let list = terminals.list();
        let by_id = |id: &str| {
            list.iter()
                .find(|t| t.entry.as_deref() == Some(id))
                .map(|t| t.id)
        };
        let shape: Vec<(&str, Option<u32>, u8)> = list
            .iter()
            .map(|t| (t.entry.as_deref().unwrap(), t.parent, t.activity))
            .collect();
        assert_eq!(
            shape,
            vec![
                ("claude", None, activity_byte(TermActivity::Idle)),
                ("claude-7", None, activity_byte(TermActivity::NotStarted)),
                (
                    "claude-8",
                    by_id("claude-7"),
                    activity_byte(TermActivity::NotStarted)
                ),
                ("claude-12", None, activity_byte(TermActivity::Idle)),
                (
                    "claude-14",
                    by_id("claude-12"),
                    activity_byte(TermActivity::Idle)
                ),
            ]
        );
        // A stopped one is granted to the peer that was sent it, so the peer
        // can start it as it can a running one.
        let host = service(Arc::new(terminals));
        let (_, granted) = host.listing(true);
        assert!(
            host.authorize(
                &granted,
                crate::host::Target::Terminal {
                    id: by_id("claude-7").unwrap(),
                    project: "p".into(),
                }
            )
            .is_some()
        );
    }

    /// An older peer is listed only what has a real ring, as before: a
    /// terminal whose launch failed before its output was tapped has none,
    /// and stays out of its list even after a newer peer was listed it (its
    /// placeholder is not a ring). A newer peer is listed both.
    #[test]
    fn an_older_peer_is_not_listed_a_terminal_that_never_had_output() {
        let mut untapped = entry("claude-3", None);
        untapped.state = TermActivity::Failed;
        let mut tapped = entry("claude-4", None);
        tapped.state = TermActivity::Failed;
        let registry = Arc::new(Registry {
            entries: Mutex::new(vec![untapped, tapped]),
            ..Registry::default()
        });
        let view = WorkspaceView {
            projects: vec![project("p", "Project", Some(registry))],
            active: Some("p".to_owned()),
            groups: Vec::new(),
            recents: Vec::new(),
            sidebar_collapsed: false,
            last_error: None,
            warning: None,
        };
        let book = Arc::new(RingBook::default());
        let _ = book.tap_for("p/claude-4");
        let terminals = Arc::new(WorkspaceTerminals::new(
            Arc::new(Projects(Arc::new(view))),
            book,
        ));
        let host = service(Arc::clone(&terminals));
        let ids = |all: bool| -> Vec<String> {
            host.listing(all)
                .0
                .terminals
                .into_iter()
                .filter_map(|t| t.entry)
                .collect()
        };
        assert_eq!(ids(false), ["claude-4"]);
        assert_eq!(ids(true), ["claude-3", "claude-4"]);
        assert_eq!(ids(false), ["claude-4"], "a placeholder became a ring");
        // Nor may an older peer name it.
        let untapped_id = terminals
            .list()
            .into_iter()
            .find(|t| t.entry.as_deref() == Some("claude-3"))
            .unwrap()
            .id;
        let (_, granted) = host.listing(false);
        assert!(
            host.authorize(
                &granted,
                crate::host::Target::Terminal {
                    id: untapped_id,
                    project: "p".into(),
                }
            )
            .is_none()
        );
    }

    /// A terminal listed before it ever ran is listed with an empty ring; a
    /// peer may attach to it (a failed launch is not "not started"). Its
    /// first real start closes that ring, as a restart closes the old one, so
    /// the peer lets it go and attaches again to the ring with the output.
    #[test]
    fn a_first_start_closes_the_empty_ring_a_peer_was_listed() {
        let mut failed = entry("claude-3", None);
        failed.state = TermActivity::Failed;
        let registry = Arc::new(Registry {
            entries: Mutex::new(vec![failed]),
            ..Registry::default()
        });
        let view = WorkspaceView {
            projects: vec![project("p", "Project", Some(registry))],
            active: Some("p".to_owned()),
            groups: Vec::new(),
            recents: Vec::new(),
            sidebar_collapsed: false,
            last_error: None,
            warning: None,
        };
        let book = Arc::new(RingBook::default());
        let terminals =
            WorkspaceTerminals::new(Arc::new(Projects(Arc::new(view))), Arc::clone(&book));
        let listed = terminals.list();
        let placeholder = Arc::clone(&listed[0].ring);
        assert!(!listed[0].ran);
        assert!(
            Arc::ptr_eq(&placeholder, &terminals.list()[0].ring),
            "each list names the same stream"
        );

        let tap = book.tap_for("p/claude-3");
        tap.feed(b"hello", &mut |_| {});
        assert!(placeholder.is_closed(), "the peer on it is let go");
        let now = terminals.list();
        assert!(now[0].ran, "an older peer is listed it now");
        assert!(!Arc::ptr_eq(&now[0].ring, &placeholder));
        assert_eq!(now[0].ring.end(), 5, "the new list names the real ring");
    }

    /// Agents switched off on the host.
    struct Inactive(Vec<ToolKind>);

    impl connecto_core::AgentsControl for Inactive {
        fn view(&self) -> Arc<connecto_core::AgentsView> {
            Arc::default()
        }
        fn inactive(&self) -> Vec<ToolKind> {
            self.0.clone()
        }
    }

    /// A project's settings that allow only these tools.
    struct Allowed(Vec<ToolKind>);

    impl connecto_core::PackagesControl for Allowed {
        fn view(&self) -> Arc<connecto_core::PackagesView> {
            Arc::default()
        }
        fn allowed_tools(&self, _project_id: &str) -> Vec<ToolKind> {
            self.0.clone()
        }
    }

    /// A peer may start a stopped terminal only as the host's window would:
    /// an agent deactivated there, or turned off in the project's settings,
    /// is refused before the registry hears of it.
    #[test]
    fn a_peer_restart_is_refused_where_the_host_would_refuse_it() {
        let restart = |terminals: WorkspaceTerminals| {
            let terminals = Arc::new(terminals);
            let host = service(Arc::clone(&terminals));
            let (_, granted) = host.listing(true);
            let t1 = terminals
                .list()
                .into_iter()
                .find(|t| t.name == "name t1")
                .unwrap();
            let right = host
                .authorize(
                    &granted,
                    crate::host::Target::Terminal {
                        id: t1.id,
                        project: "a".into(),
                    },
                )
                .unwrap();
            terminals.manage(&ManageOp::Restart { terminal: t1.id }, &right)
        };

        let (a, _, terminals, _) = workspace();
        let off = restart(terminals.with_agents(Arc::new(Inactive(vec![ToolKind::Claude]))));
        assert!(off.unwrap_err().contains("deactivated"));
        assert!(a.restarted.lock().unwrap().is_empty());

        let (a, _, terminals, _) = workspace();
        let not_here = restart(terminals.with_packages(Arc::new(Allowed(vec![ToolKind::Codex]))));
        assert!(not_here.unwrap_err().contains("turned off"));
        assert!(a.restarted.lock().unwrap().is_empty());

        let (a, _, terminals, _) = workspace();
        let allowed = terminals
            .with_agents(Arc::new(Inactive(vec![ToolKind::Codex])))
            .with_packages(Arc::new(Allowed(vec![ToolKind::Claude])));
        assert_eq!(restart(allowed), Ok(None));
        assert_eq!(*a.restarted.lock().unwrap(), vec!["t1".to_owned()]);
    }

    /// A peer's new terminal is checked as the host's window would check
    /// it: a CLI the project's Tools setting leaves out is refused before the
    /// registry hears of it; an allowed one, and a shell, are added.
    #[test]
    fn a_peer_add_of_a_cli_the_project_leaves_out_is_refused() {
        let (a, _, terminals, _) = workspace();
        let terminals =
            terminals.with_packages(Arc::new(Allowed(vec![ToolKind::Codex, ToolKind::Pi])));
        let right = project_right(&terminals, "a");
        let add = |tool| {
            terminals.manage(
                &ManageOp::Add {
                    tool: tool_byte(tool),
                    name: None,
                    project: Some("a".into()),
                    model: None,
                },
                &right,
            )
        };
        let refused = add(ToolKind::Claude);
        assert!(refused.unwrap_err().contains("turned off"));
        assert!(
            a.adds.lock().unwrap().is_empty(),
            "the registry heard nothing"
        );
        assert_eq!(add(ToolKind::Codex), Ok(None));
        assert_eq!(add(ToolKind::Shell), Ok(None));
        assert_eq!(
            a.adds.lock().unwrap().as_slice(),
            [(ToolKind::Codex, None), (ToolKind::Shell, None)]
        );
    }

    /// A peer's stop of one background shell is answered with its outcome:
    /// a refusal found only once the shells are looked for (two running the
    /// same command) is the management reply's error, which the peer's
    /// window shows, not a success while only the host's own list says why.
    #[test]
    fn a_peer_stop_of_background_work_answers_with_its_refusal() {
        let (a, _, terminals, _) = workspace();
        let terminals = Arc::new(terminals);
        let host = service(Arc::clone(&terminals));
        let (_, granted) = host.listing(true);
        let t1 = terminals
            .list()
            .into_iter()
            .find(|t| t.name == "name t1")
            .unwrap();
        let right = host
            .authorize(
                &granted,
                crate::host::Target::Terminal {
                    id: t1.id,
                    project: "a".into(),
                },
            )
            .unwrap();
        let stop = ManageOp::StopBackground {
            terminal: t1.id,
            task: Some("b1".into()),
        };
        *a.stop_refusal.lock().unwrap() =
            Some("This background shell cannot be told apart.".into());
        let refused = terminals.manage(&stop, &right);
        assert_eq!(
            refused,
            Err("This background shell cannot be told apart.".to_owned())
        );
        *a.stop_refusal.lock().unwrap() = None;
        assert_eq!(terminals.manage(&stop, &right), Ok(None));
    }

    /// Accepts every peer: this test is about the grant rule, not the guard.
    struct OpenGuard;

    impl connecto_core::RemoteGuard for OpenGuard {
        fn enabled(&self) -> bool {
            true
        }
        fn set_enabled(&self, _on: bool) {}
        fn connected(&self, _peer: &connecto_core::RemotePeer, _outcome: Result<(), &str>) {}
        fn attach(
            &self,
            _peer: &connecto_core::RemotePeer,
            _terminal: &str,
            _attempt: u64,
        ) -> Result<(), connecto_core::GuardDenied> {
            Ok(())
        }
        fn input(
            &self,
            _peer: &connecto_core::RemotePeer,
            _terminal: &str,
            _attempt: u64,
            _bytes: &[u8],
        ) -> Result<(), connecto_core::GuardDenied> {
            Ok(())
        }
        fn resize(
            &self,
            _peer: &connecto_core::RemotePeer,
            _terminal: &str,
            _attempt: u64,
        ) -> Result<(), connecto_core::GuardDenied> {
            Ok(())
        }
        fn manage(
            &self,
            _peer: &connecto_core::RemotePeer,
            _action: &'static str,
            _terminal: &str,
            _attempt: u64,
        ) -> Result<(), connecto_core::GuardDenied> {
            Ok(())
        }
        fn detach(&self, _peer: &connecto_core::RemotePeer, _terminal: &str, _attempt: u64) {}
    }

    /// A host service over `terminals`, with a guard that allows everything.
    fn service(terminals: Arc<WorkspaceTerminals>) -> crate::host::HostService {
        crate::host::HostService::new(
            terminals,
            Arc::new(|| Arc::new(crate::auth::AuthorizedKeySet::parse("").unwrap())),
            Arc::new(OpenGuard),
        )
    }

    #[test]
    fn a_closed_entry_and_a_new_one_are_different_identities() {
        let (a, _, terminals, _) = workspace();
        let terminals = Arc::new(terminals);
        let host = service(Arc::clone(&terminals));
        let before = terminals.list();
        let t1 = before.iter().find(|t| t.name == "name t1").unwrap().clone();
        let (_, granted) = host.listing(true);
        assert!(
            host.authorize(
                &granted,
                crate::host::Target::Terminal {
                    id: t1.id,
                    project: "a".into(),
                }
            )
            .is_some(),
            "a listed terminal is granted"
        );
        // Its entry is closed (it leaves the list), and a new entry is created
        // in the same project after the list was sent.
        {
            let mut entries = a.entries.lock().unwrap();
            entries.retain(|e| e.id != "t1");
            entries.push(entry("t9", None));
        }
        let after = terminals.list();
        assert!(after.iter().all(|t| t.name != "name t1"));
        let t9 = after.iter().find(|t| t.name == "name t9").unwrap();
        assert_ne!(t9.id, t1.id, "a new entry got a closed entry's id");
        assert!(
            host.authorize(
                &granted,
                crate::host::Target::Terminal {
                    id: t1.id,
                    project: "a".into(),
                }
            )
            .is_none(),
            "a closed entry's old grant still matches"
        );
        assert!(
            host.authorize(
                &granted,
                crate::host::Target::Terminal {
                    id: t9.id,
                    project: "a".into(),
                }
            )
            .is_none(),
            "a new entry was granted without a list"
        );
    }

    #[test]
    fn a_close_goes_to_the_entry_it_names() {
        let (_, b, terminals, _) = workspace();
        let u1 = terminals
            .list()
            .into_iter()
            .find(|t| t.name == "name u1")
            .unwrap();
        terminals
            .close_within(&terminal_right(&u1), &[u1.id])
            .unwrap();
        assert_eq!(b.closed.lock().unwrap().as_slice(), ["u1".to_owned()]);
    }

    #[test]
    fn add_goes_to_the_project_its_right_names() {
        let (a, b, terminals, _) = workspace();
        terminals
            .manage(
                &ManageOp::Add {
                    tool: 2,
                    name: None,
                    project: Some("a".into()),
                    model: None,
                },
                &project_right(&terminals, "a"),
            )
            .unwrap();
        terminals
            .manage(
                &ManageOp::Add {
                    tool: 1,
                    name: Some("review".into()),
                    project: Some("b".into()),
                    model: None,
                },
                &project_right(&terminals, "b"),
            )
            .unwrap();
        assert_eq!(a.adds.lock().unwrap().as_slice(), [(ToolKind::Codex, None)]);
        assert_eq!(
            b.adds.lock().unwrap().as_slice(),
            [(ToolKind::Claude, Some("review".to_owned()))]
        );
    }

    #[test]
    fn an_add_launches_on_the_model_it_names() {
        let (a, _, terminals, _) = workspace();
        for model in [Some("opus"), None, Some("  ")] {
            terminals
                .manage(
                    &ManageOp::Add {
                        tool: 1,
                        name: Some("app-session".into()),
                        project: Some("a".into()),
                        model: model.map(str::to_owned),
                    },
                    &project_right(&terminals, "a"),
                )
                .unwrap();
        }
        assert_eq!(
            a.models.lock().unwrap().as_slice(),
            [Some("opus".to_owned()), None, None],
            "a blank model is the default"
        );
    }

    /// Pi's providers as Settings has them for one project.
    struct PiAgents(connecto_core::PiProvidersView);

    impl connecto_core::AgentsControl for PiAgents {
        fn view(&self) -> Arc<connecto_core::AgentsView> {
            Arc::default()
        }
        fn provider_scope(&self, project: &str, _tool: ToolKind) -> connecto_core::ProviderScope {
            connecto_core::ProviderScope::Project(project.to_owned())
        }
        fn pi_providers(
            &self,
            scope: &connecto_core::ProviderScope,
        ) -> connecto_core::PiProvidersView {
            assert_eq!(
                scope,
                &connecto_core::ProviderScope::Project("a".to_owned())
            );
            self.0.clone()
        }
    }

    #[test]
    fn models_come_from_pi_providers_or_the_suggested_list() {
        let (_, _, terminals, _) = workspace();
        let claude = terminals.models(tool_byte(ToolKind::Claude), "a");
        assert_eq!(claude.default, None);
        assert!(claude.models.contains(&"opus".to_owned()));
        assert_eq!(
            terminals
                .models(tool_byte(ToolKind::Antigravity), "a")
                .models
                .len(),
            ToolKind::Antigravity.suggested_models().len()
        );
        assert_eq!(
            terminals.models(tool_byte(ToolKind::Pi), "a"),
            ToolModels::default(),
            "no providers wired in: only the default"
        );

        // Codex: its own list where it keeps one, else the fallback.
        assert_eq!(
            terminals.models(tool_byte(ToolKind::Codex), "a").models,
            ToolKind::Codex.suggested_models()
        );
        let live = vec!["gpt-6.2-sol".to_owned(), "gpt-5.6-luna".to_owned()];
        let registry = Arc::new(Registry {
            codex_models: Some(live.clone()),
            ..Registry::default()
        });
        let with_cache = WorkspaceTerminals::new(
            Arc::new(Projects(Arc::new(WorkspaceView {
                projects: vec![project("k", "Kappa", Some(registry))],
                active: None,
                groups: Vec::new(),
                recents: Vec::new(),
                sidebar_collapsed: false,
                last_error: None,
                warning: None,
            }))),
            Arc::new(RingBook::default()),
        );
        assert_eq!(
            with_cache.models(tool_byte(ToolKind::Codex), "k").models,
            live
        );
        assert_eq!(
            with_cache.models(tool_byte(ToolKind::Claude), "k").models,
            ToolKind::Claude.suggested_models()
        );

        let pi = |default: &str| {
            let (_, _, terminals, _) = workspace();
            terminals
                .with_agents(Arc::new(PiAgents(connecto_core::PiProvidersView {
                    models: vec!["together/x".into(), "openrouter/y".into()],
                    default_model: Some(default.into()),
                    ..Default::default()
                })))
                .models(tool_byte(ToolKind::Pi), "a")
        };
        assert_eq!(
            pi("together/x"),
            ToolModels {
                default: Some("together/x".into()),
                models: vec!["together/x".into(), "openrouter/y".into()],
            }
        );
        assert_eq!(pi("gone/z").default, None, "a default Pi no longer offers");
    }

    #[test]
    fn add_refuses_an_unknown_project_and_one_that_is_not_set_up() {
        let (a, b, terminals, _) = workspace();
        for id in ["zz", "c"] {
            let refused = terminals.manage(
                &ManageOp::Add {
                    tool: 1,
                    name: None,
                    project: Some(id.into()),
                    model: None,
                },
                &Right::for_test(Target::Project(id.into())),
            );
            assert_eq!(refused, Err(NO_SUCH_PROJECT.to_owned()), "{id}");
        }
        assert!(a.adds.lock().unwrap().is_empty());
        assert!(b.adds.lock().unwrap().is_empty());
    }

    /// An offered file is the file chosen: one put at its path since, by the
    /// same name or through a link, is not what is read.
    #[test]
    fn an_offer_sends_only_the_file_that_was_offered() {
        let (_, _, terminals, _) = workspace();
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("report.html");
        std::fs::write(&path, b"first").unwrap();
        let offer = connecto_core::offers::shared()
            .offer("a", "claude-1", &path)
            .unwrap();
        let read = |id: u64| {
            // By offset, as the host sends it: each read starts at 0.
            let (name, file) = terminals.open_offer(id).unwrap();
            let mut buf = vec![0; 64];
            let n = crate::host::read_at(&file, &mut buf, 0).unwrap();
            (name, String::from_utf8(buf[..n].to_vec()).unwrap())
        };
        assert_eq!(read(offer.id), ("report.html".into(), "first".into()));

        std::fs::remove_file(&path).unwrap();
        std::fs::write(&path, b"other").unwrap();
        assert_eq!(read(offer.id).1, "first", "a new file there is not read");

        let again = connecto_core::offers::shared()
            .offer("a", "claude-1", &path)
            .unwrap();
        std::fs::remove_file(&path).unwrap();
        let elsewhere = dir.path().join("secret.txt");
        std::fs::write(&elsewhere, b"secret").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&elsewhere, &path).unwrap();
        assert_eq!(read(again.id).1, "other", "a link there is not followed");
    }

    /// Token optimization with one analysis: a plugin and a skill to cut,
    /// and advice that cannot be applied.
    #[derive(Default)]
    struct Tokens {
        applied: Mutex<Vec<(Vec<String>, Vec<String>)>>,
    }

    impl connecto_core::TokenControl for Tokens {
        fn view(&self) -> Arc<connecto_core::TokenView> {
            use connecto_core::tokens::{ProjectTokenView, Proposal, ProposalKind, TokenReport};
            let proposal = |kind, id: &str| Proposal {
                kind,
                name: id.into(),
                ids: vec![id.into()],
                tokens: 1,
                evidence: String::new(),
                preselected: true,
                hint: None,
            };
            Arc::new(connecto_core::TokenView {
                available: true,
                projects: vec![ProjectTokenView {
                    project_id: "a".into(),
                    report: Some(TokenReport {
                        analyzed_ms: 1,
                        totals: connecto_core::tokens::Totals::default(),
                        loaded: None,
                        proposals: vec![
                            proposal(ProposalKind::Plugin, "p@synced"),
                            proposal(ProposalKind::Skill, "s"),
                            proposal(ProposalKind::McpServer, "srv"),
                        ],
                        compact: connecto_core::tokens::CompactEstimate::default(),
                        after: None,
                        biggest: None,
                    }),
                    ..ProjectTokenView::default()
                }],
            })
        }
        fn apply(&self, _: &str, plugins: Vec<String>, skills: Vec<String>) -> Result<(), String> {
            self.applied.lock().unwrap().push((plugins, skills));
            Ok(())
        }
    }

    /// Which settings writes reached the project: (keeping OpenCode's, the
    /// settings).
    use connecto_core::ProjectSettings;

    #[derive(Default)]
    struct Writes(Mutex<Vec<&'static str>>);

    impl connecto_core::PackagesControl for Writes {
        fn view(&self) -> Arc<connecto_core::PackagesView> {
            Arc::default()
        }
        fn set_project_settings(&self, _: &str, _: ProjectSettings) -> Result<(), String> {
            self.0.lock().unwrap().push("as sent");
            Ok(())
        }
        fn set_project_settings_keeping_opencode(
            &self,
            _: &str,
            _: ProjectSettings,
        ) -> Result<(), String> {
            self.0.lock().unwrap().push("keeping opencode");
            Ok(())
        }
        fn set_project_settings_keeping_tools(
            &self,
            _: &str,
            _: ProjectSettings,
        ) -> Result<(), String> {
            self.0.lock().unwrap().push("keeping tools");
            Ok(())
        }
    }

    /// An older peer's settings write reaches the project as one that keeps
    /// what that peer does not say (OpenCode's config), or says only as one
    /// tool or all (the set of tools): the packages worker resolves it in
    /// turn. A new peer's goes as it was sent.
    #[test]
    fn an_older_peers_settings_write_keeps_what_it_cannot_say() {
        use crate::project_panels::ProjectAsk;
        let (_, _, terminals, _) = workspace();
        let writes = Arc::new(Writes::default());
        let terminals =
            terminals.with_packages(Arc::clone(&writes) as Arc<dyn connecto_core::PackagesControl>);
        let right = project_right(&terminals, "a");
        let before_opencode = br#"{"ask":"set-settings","settings":{"tools":"all","claude_config":"system","codex_config":"system","pi_config":"system","use_default_package":true,"packages":[]}}"#;
        let before_sets = br#"{"ask":"set-settings","settings":{"tools":"all","claude_config":"system","codex_config":"system","pi_config":"system","opencode_config":"system","use_default_package":true,"packages":[]}}"#;
        let new = ProjectAsk::SetSettings(ProjectSettings::default()).encode();
        for bytes in [&before_opencode[..], &before_sets[..], &new] {
            terminals
                .project_ask(&right, &ProjectAsk::decode(bytes).unwrap())
                .unwrap();
        }
        assert_eq!(
            *writes.0.lock().unwrap(),
            ["keeping opencode", "keeping tools", "as sent"]
        );
    }

    /// A peer can cut only what the project's analysis proposes, as the
    /// window offers it: anything else refuses the whole request.
    #[test]
    fn a_peers_token_cuts_are_only_the_ones_the_analysis_proposes() {
        use crate::project_panels::ProjectAsk;
        let (_, _, terminals, _) = workspace();
        let tokens = Arc::new(Tokens::default());
        let terminals =
            terminals.with_tokens(Arc::clone(&tokens) as Arc<dyn connecto_core::TokenControl>);
        let right = project_right(&terminals, "a");
        let ask = |plugins: &[&str], skills: &[&str]| ProjectAsk::TokensApply {
            plugins: plugins.iter().map(|s| (*s).to_owned()).collect(),
            skills: skills.iter().map(|s| (*s).to_owned()).collect(),
        };
        for refused in [
            ask(&["p@synced", "evil@market"], &[]),
            ask(&[], &["p@synced"]),
            ask(&["srv"], &[]),
        ] {
            assert!(
                terminals.project_ask(&right, &refused).is_err(),
                "{refused:?}"
            );
        }
        assert!(tokens.applied.lock().unwrap().is_empty());
        terminals
            .project_ask(&right, &ask(&["p@synced"], &["s"]))
            .unwrap();
        assert_eq!(
            *tokens.applied.lock().unwrap(),
            [(vec!["p@synced".to_owned()], vec!["s".to_owned()])]
        );
    }

    /// Functional memory that records what a peer asked of it.
    #[derive(Default)]
    struct Memory {
        updated: Mutex<Vec<String>>,
        set: Mutex<Vec<String>>,
    }

    impl connecto_core::functional_memory::FunctionalMemoryControl for Memory {
        fn files(
            &self,
            project_id: &str,
        ) -> Result<connecto_core::functional_memory::FmFiles, String> {
            Ok(connecto_core::functional_memory::FmFiles {
                files: vec![connecto_core::functional_memory::FmFile {
                    path: "README.md".into(),
                    text: format!("# {project_id}\n"),
                }],
                truncated: false,
            })
        }
        fn update_now(&self, project_id: &str) -> Result<(), String> {
            self.updated.lock().unwrap().push(project_id.to_owned());
            Ok(())
        }
        fn set_auto(&self, project_id: &str, on: bool) -> Result<(), String> {
            self.set
                .lock()
                .unwrap()
                .push(format!("{project_id} auto {on}"));
            Ok(())
        }
        fn set_model(
            &self,
            project_id: &str,
            model: Option<connecto_core::functional_memory::FmModel>,
        ) -> Result<(), String> {
            let model = model.map_or_else(|| "default".to_owned(), |m| m.label());
            self.set
                .lock()
                .unwrap()
                .push(format!("{project_id} model {model}"));
            Ok(())
        }
    }

    /// A peer reads the files of the project its right names, and asks for
    /// an update only with the update ask; a host without it says so.
    #[test]
    fn a_peer_reads_a_projects_functional_memory_and_can_ask_for_an_update() {
        use crate::project_panels::{ProjectAsk, functional_memory_from};
        let (_, _, terminals, _) = workspace();
        let right = project_right(&terminals, "a");
        assert!(
            terminals
                .project_ask(&right, &ProjectAsk::FunctionalMemory)
                .is_err()
        );
        let memory = Arc::new(Memory::default());
        let terminals = terminals.with_functional_memory(Arc::clone(&memory) as _);
        let bytes = terminals
            .project_ask(&right, &ProjectAsk::FunctionalMemory)
            .unwrap();
        let (files, _) = functional_memory_from(&bytes).unwrap();
        assert_eq!(files.files[0].text, "# a\n");
        assert!(memory.updated.lock().unwrap().is_empty());
        terminals
            .project_ask(&right, &ProjectAsk::FunctionalMemoryUpdate)
            .unwrap();
        assert_eq!(*memory.updated.lock().unwrap(), ["a"]);
        terminals
            .project_ask(
                &right,
                &ProjectAsk::FunctionalMemorySet {
                    auto: Some(false),
                    model: Some(None),
                },
            )
            .unwrap();
        assert_eq!(
            *memory.set.lock().unwrap(),
            ["a auto false", "a model default"]
        );
    }

    #[test]
    fn projects_are_listed_whether_or_not_they_have_a_terminal() {
        let (_, _, terminals, _) = workspace();
        let ids: Vec<String> = terminals.projects().into_iter().map(|p| p.id).collect();
        assert_eq!(ids, ["a", "b", "d", "e"], "c is not set up");
    }

    #[test]
    fn an_empty_project_takes_its_first_terminal() {
        let (_, _, terminals, _) = workspace();
        let e = terminals.manage(
            &ManageOp::Add {
                tool: 1,
                name: Some("first".into()),
                project: Some("e".into()),
                model: None,
            },
            &project_right(&terminals, "e"),
        );
        assert_eq!(e, Ok(None));
    }

    #[test]
    fn numbers_run_out_without_wrapping_or_reuse() {
        let (_, _, terminals, _) = workspace();
        // Two numbers are left: the first new key gets the last one, the rest get none.
        terminals.ids.lock().unwrap().0 = u32::MAX - 1;
        let list = terminals.list();
        assert_eq!(
            list.len(),
            1,
            "only the terminal that got a number is offered"
        );
        assert_eq!(list[0].id, u32::MAX);
        assert_eq!(
            terminals.id_of("b/u1"),
            None,
            "a number was handed out past the end"
        );
        assert_eq!(
            terminals.id_of("a/t1"),
            Some(u32::MAX),
            "a known key lost its number"
        );
    }
}
