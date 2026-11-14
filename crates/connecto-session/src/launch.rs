//! Launching a store binary in a PTY under a per-launch profile folder, and the
//! tool's login.
//!
//! There is no sandbox (ADR 0013): the tool runs directly, with its own bypass
//! flags. What remains is the project-folder rule, the private run folders, and
//! the per-attempt hook socket.

use std::collections::BTreeMap;
use std::ffi::OsString;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError};

use connecto_adapter::{
    BridgeLaunch, Claude, Codex, HookCtx, LaunchCtx, LoginCtx, Plain, ToolAdapter,
};
use connecto_core::{AuthState, GridSize, SessionStatus};
use connecto_pty::{OpenPty, PtyError, PtySession, PtySpec, Wake};
use connecto_store::{Installed, Store, StoreError, Tool};

use crate::LiveSession;
use crate::auth::auth_status;
use crate::hook::{BoundSocket, ServerConfig, serve};
use crate::platform::{self, INHERITED, private_dir, random_bytes};
use crate::project_folder::{ForbiddenRoots, PathError, validate_project};

/// Makes every attempt's folders and socket its own.
static ATTEMPT: AtomicU64 = AtomicU64::new(1);

/// Why a session did not start.
#[derive(Debug, thiserror::Error)]
pub enum LaunchError {
    #[error(transparent)]
    Path(#[from] PathError),
    #[error(transparent)]
    Store(#[from] StoreError),
    #[error(transparent)]
    Pty(#[from] PtyError),
    #[error("could not prepare the session: {0}")]
    Io(#[from] io::Error),
    #[error("the session started without a process ID")]
    NoProcess,
    #[error("stop this tool's session before logging in")]
    SessionLive,
    #[error("Another terminal is logging in")]
    LoginRunning,
    #[error(
        "this project's packages are still being installed: start the terminal again in a moment"
    )]
    InstallPending,
}

/// One session's claim on a profile. It is unbound while the session launches,
/// then bound to the session's PTY. A claim whose PTY is no longer running stops
/// counting, so a stopped terminal kept for reading blocks nobody.
struct SessionClaim {
    id: u64,
    pty: Option<Arc<PtySession>>,
}

impl SessionClaim {
    fn counts(&self) -> bool {
        self.pty
            .as_ref()
            .is_none_or(|pty| pty.status() == SessionStatus::Running)
    }
}

/// What uses one tool's profile in this process. A login stops counting as
/// soon as its process ends, whoever still holds its screen.
#[derive(Default)]
struct ProfileUse {
    sessions: Vec<SessionClaim>,
    login: Option<SessionClaim>,
}

impl ProfileUse {
    fn login_running(&self) -> bool {
        self.login.as_ref().is_some_and(SessionClaim::counts)
    }
}

/// The reservation table, keyed by tool root: one entry per project and tool.
/// One mutex covers every decision, and the generation is read or rotated while
/// it is held, so a session always launches on the generation it reserved and
/// a login rotates only with nothing launching (ADR 0014).
static PROFILES: Mutex<BTreeMap<PathBuf, ProfileUse>> = Mutex::new(BTreeMap::new());
static CLAIM_ID: AtomicU64 = AtomicU64::new(1);

fn profiles() -> MutexGuard<'static, BTreeMap<PathBuf, ProfileUse>> {
    PROFILES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// What a claim is for.
enum ClaimKind {
    Session(u64),
    Login(u64),
}

/// Holds a reservation until dropped. A session's claim is held by its
/// [`LiveSession`], a login's by the login's, which the pane drops as soon as
/// the login exits.
pub(crate) struct ProfileClaim {
    root: PathBuf,
    kind: ClaimKind,
}

impl ProfileClaim {
    /// Tie a session claim to its PTY. A login claim has nothing to bind.
    pub(crate) fn bind(&self, pty: &Arc<PtySession>) {
        let mut table = profiles();
        let Some(entry) = table.get_mut(&self.root) else {
            return;
        };
        let claim = match self.kind {
            ClaimKind::Session(id) => entry.sessions.iter_mut().find(|claim| claim.id == id),
            ClaimKind::Login(id) => entry.login.as_mut().filter(|claim| claim.id == id),
        };
        if let Some(claim) = claim {
            claim.pty = Some(Arc::clone(pty));
        }
    }
}

impl Drop for ProfileClaim {
    fn drop(&mut self) {
        let mut table = profiles();
        if let Some(entry) = table.get_mut(&self.root) {
            match self.kind {
                ClaimKind::Session(id) => entry.sessions.retain(|claim| claim.id != id),
                ClaimKind::Login(id) => {
                    if entry.login.as_ref().is_some_and(|claim| claim.id == id) {
                        entry.login = None;
                    }
                }
            }
            if entry.sessions.is_empty() && entry.login.is_none() {
                table.remove(&self.root);
            }
        }
    }
}

/// Reserve a session on the current generation, or say why not.
fn reserve_session(tool_root: &Path) -> Result<(ProfileClaim, PathBuf), LaunchError> {
    let mut table = profiles();
    let entry = table.entry(tool_root.to_owned()).or_default();
    if entry.login_running() {
        return Err(LaunchError::LoginRunning);
    }
    let generation = current_generation(tool_root)?;
    let id = CLAIM_ID.fetch_add(1, Ordering::Relaxed);
    entry.sessions.push(SessionClaim { id, pty: None });
    Ok((
        ProfileClaim {
            root: tool_root.to_owned(),
            kind: ClaimKind::Session(id),
        },
        generation_dir(tool_root, generation),
    ))
}

/// Whether a profile generation holds conversations: transcripts that a
/// resume would look for (Claude's `projects/`, Codex's `sessions/`).
pub(crate) fn generation_has_history(generation: &Path, tool: Tool) -> bool {
    let folder = match tool {
        Tool::Claude => "projects",
        Tool::Codex => "sessions",
        Tool::Shell => return false,
    };
    fn any_jsonl(dir: &Path) -> bool {
        let Ok(entries) = fs::read_dir(dir) else {
            return false;
        };
        entries.flatten().any(|entry| {
            let path = entry.path();
            match entry.file_type() {
                Ok(kind) if kind.is_dir() => any_jsonl(&path),
                Ok(kind) if kind.is_file() => {
                    path.extension().is_some_and(|ext| ext == "jsonl")
                        && entry.metadata().is_ok_and(|m| m.len() > 0)
                }
                _ => false,
            }
        })
    }
    any_jsonl(&generation.join(folder))
}

/// Reserve a login, or say why not. A login rotates to a fresh, empty
/// generation (so a different account starts clean) unless `in_place`: the
/// tool is signed out and the current generation holds conversations, so there
/// is no login to lose and the conversations must stay where a resume looks.
fn reserve_login(tool_root: &Path, in_place: bool) -> Result<(ProfileClaim, PathBuf), LaunchError> {
    let mut table = profiles();
    let entry = table.entry(tool_root.to_owned()).or_default();
    if entry.login_running() {
        return Err(LaunchError::LoginRunning);
    }
    if entry.sessions.iter().any(SessionClaim::counts) {
        return Err(LaunchError::SessionLive);
    }
    // The generation rotates under the lock, with nothing launching.
    let profile = if in_place {
        let generation = generation_dir(tool_root, current_generation(tool_root)?);
        private_dir(&generation)?;
        real(&generation)?
    } else {
        new_generation(tool_root)?
    };
    let id = CLAIM_ID.fetch_add(1, Ordering::Relaxed);
    entry.login = Some(SessionClaim { id, pty: None });
    Ok((
        ProfileClaim {
            root: tool_root.to_owned(),
            kind: ClaimKind::Login(id),
        },
        profile,
    ))
}

/// A run folder. It goes away with its owner, on success and on every failure.
pub(crate) struct RunFolder(PathBuf);

impl Drop for RunFolder {
    fn drop(&mut self) {
        // Best effort: what is left is an ended session's scratch files.
        let _ = fs::remove_dir_all(&self.0);
    }
}

/// Where a project's session state lives.
#[derive(Debug, Clone)]
pub struct Layout {
    pub data_root: PathBuf,
    pub home: PathBuf,
}

/// What a terminal gets so its agent can use the bridge (section 5.15). The
/// pane decides it in preflight: a bridge-off terminal has no text, no hooks and
/// no trust, but still gets the socket and `PATH`, so the command can say why.
#[derive(Debug, Clone)]
pub struct BridgeInfo {
    pub socket: PathBuf,
    pub terminal: String,
    pub text: Option<String>,
    pub hooks: bool,
    pub trust: Option<String>,
}

impl BridgeInfo {
    /// No bridge: for launches that are not part of a relay.
    #[must_use]
    pub fn absent() -> Self {
        Self {
            socket: PathBuf::from("/nonexistent/bridge.sock"),
            terminal: String::new(),
            text: None,
            hooks: false,
            trust: None,
        }
    }
}

/// Launch settings beyond the common ones: what a utility session asks for,
/// and (test builds only) a fake model.
#[derive(Debug, Clone, Default)]
pub struct LaunchExtras {
    /// Makes the session a hidden utility session (Claude only).
    pub utility: Option<connecto_adapter::UtilityLaunch>,
    /// The first message of a new conversation (not a resume).
    pub initial_prompt: Option<String>,
    /// A short first message for a resumed conversation.
    pub resume_note: Option<String>,
    /// The name of the terminal, for tools that can show it (Claude).
    pub session_name: Option<String>,
    /// The model the terminal was added with. `None` is the tool's default.
    pub model: Option<String>,
    /// Base URL of a local fake model.
    #[cfg(feature = "test-endpoints")]
    pub fake_model: Option<String>,
}

static LAUNCH_MODES: std::sync::RwLock<connecto_core::LaunchModes> =
    std::sync::RwLock::new(connecto_core::LaunchModes {
        claude: connecto_core::AgentMode::Bypass,
        codex: connecto_core::AgentMode::Bypass,
    });

/// How each tool is launched from now on (setup). Every launch path that
/// starts an agent (a new terminal, a restart, the relaunch at app start,
/// automations, a remote peer's add and restart) goes through
/// [`start_resuming`], which reads this, so one setting covers them all. A
/// terminal already running keeps what it was launched with.
pub fn set_launch_modes(modes: connecto_core::LaunchModes) {
    *LAUNCH_MODES
        .write()
        .unwrap_or_else(std::sync::PoisonError::into_inner) = modes;
}

/// The modes in force now.
#[must_use]
pub fn launch_modes() -> connecto_core::LaunchModes {
    *LAUNCH_MODES
        .read()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

static TAP_FACTORY: std::sync::OnceLock<Arc<dyn connecto_core::TapFactory>> =
    std::sync::OnceLock::new();

/// Number every terminal's output from now on, for remote hosting (ADR 0017).
/// Set once at startup; false if it was set already. A terminal started
/// before this has no tap, and a utility session never has one.
pub fn set_tap_factory(factory: Arc<dyn connecto_core::TapFactory>) -> bool {
    TAP_FACTORY.set(factory).is_ok()
}

/// The adapter for a tool.
#[must_use]
pub fn adapter(tool: Tool) -> &'static dyn ToolAdapter {
    match tool {
        Tool::Claude => &Claude,
        Tool::Codex => &Codex,
        Tool::Shell => &Plain,
    }
}

/// Short and stable name for a project folder, from its real path.
pub fn project_id(project: &Path) -> String {
    // FNV-1a: this names a folder and is not a security boundary.
    let hash = project
        .as_os_str()
        .as_encoded_bytes()
        .iter()
        .fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
            (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
        });
    format!("{hash:016x}")
}

fn real(path: &Path) -> io::Result<PathBuf> {
    fs::canonicalize(path)
}

fn hex(bytes: &[u8]) -> String {
    bytes.iter().map(|byte| format!("{byte:02x}")).collect()
}

/// A random version 4 UUID, which Claude requires for `--session-id`.
fn new_uuid() -> io::Result<String> {
    let mut bytes = random_bytes::<16>()?;
    bytes[6] = (bytes[6] & 0x0f) | 0x40;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    let text = hex(&bytes);
    Ok(format!(
        "{}-{}-{}-{}-{}",
        &text[..8],
        &text[8..12],
        &text[12..16],
        &text[16..20],
        &text[20..]
    ))
}

/// The owner-only socket folder, checked before use (on Windows, the
/// named-pipe namespace).
pub(crate) fn socket_dir() -> io::Result<PathBuf> {
    platform::socket_dir()
}

/// A decimal number as `format!` writes one: digits only, no sign, no padding.
fn plain_number(text: &str) -> Option<u64> {
    let plain = text.bytes().all(|byte| byte.is_ascii_digit())
        && (text == "0" || !text.starts_with('0'))
        && !text.is_empty();
    plain.then(|| text.parse().ok()).flatten()
}

fn plain_pid(text: &str) -> Option<i32> {
    plain_number(text)
        .and_then(|pid| i32::try_from(pid).ok())
        .filter(|pid| *pid > 1)
}

fn is_tool_name(text: &str) -> bool {
    [Tool::Claude, Tool::Codex, Tool::Shell]
        .iter()
        .any(|tool| tool.name() == text)
}

/// The pid in a run folder's name, when the name is one Connecto makes:
/// `<project id>-<tool>-<pid>-<attempt>` or the trust check's `trust-<pid>`.
pub(crate) fn run_folder_pid(name: &str) -> Option<i32> {
    if let Some(pid) = name.strip_prefix("trust-") {
        return plain_pid(pid);
    }
    let mut parts = name.split('-');
    let (id, tool, pid, attempt) = (parts.next()?, parts.next()?, parts.next()?, parts.next()?);
    let project = id.len() == 16
        && id
            .bytes()
            .all(|byte| byte.is_ascii_digit() || (b'a'..=b'f').contains(&byte));
    if parts.next().is_some() || !project || !is_tool_name(tool) {
        return None;
    }
    plain_number(attempt)?;
    plain_pid(pid)
}

/// The pid in a socket's name, when the name is one Connecto makes:
/// `bridge-<pid>.sock`, `bridge-<pid>-<n>.sock` or `<tool>-<pid>-<attempt>.sock`.
fn socket_pid(name: &str) -> Option<i32> {
    let parts: Vec<&str> = name.strip_suffix(".sock")?.split('-').collect();
    match parts.as_slice() {
        ["bridge", pid] => plain_pid(pid),
        [kind, pid, number] if *kind == "bridge" || is_tool_name(kind) => {
            plain_number(number)?;
            plain_pid(pid)
        }
        _ => None,
    }
}

/// Whether what `pid` names is certainly gone. This process is never gone, and
/// neither is a process that only cannot be signalled.
pub(crate) fn process_gone(pid: i32) -> bool {
    if u32::try_from(pid).is_ok_and(|pid| pid == std::process::id()) {
        return false;
    }
    platform::no_such_process(pid)
}

/// The entries of `dir` by name, or nothing when `dir` is not a real folder.
fn own_entries(dir: &Path) -> Vec<(String, PathBuf, fs::FileType)> {
    let is_folder = fs::symlink_metadata(dir).is_ok_and(|meta| meta.is_dir());
    let Some(entries) = is_folder.then(|| fs::read_dir(dir).ok()).flatten() else {
        return Vec::new();
    };
    entries
        .flatten()
        .filter_map(|entry| {
            // `DirEntry::file_type` does not follow a link.
            let kind = entry.file_type().ok()?;
            Some((entry.file_name().into_string().ok()?, entry.path(), kind))
        })
        .collect()
}

/// Remove the run folders in `run_root` whose process is gone.
fn sweep_run_folders(run_root: &Path) {
    for (name, path, kind) in own_entries(run_root) {
        if kind.is_dir() && run_folder_pid(&name).is_some_and(process_gone) {
            let _ = fs::remove_dir_all(path);
        }
    }
}

/// Remove the sockets in `dir` whose process is gone.
fn sweep_sockets(dir: &Path) {
    for (name, path, kind) in own_entries(dir) {
        if platform::is_socket(&kind) && socket_pid(&name).is_some_and(process_gone) {
            let _ = fs::remove_file(path);
        }
    }
}

/// Remove what earlier runs left when they ended without cleaning up (a crash,
/// a kill): their run folders under `<data>/run` and their sockets. Each is
/// normally removed when its owner is dropped.
///
/// Only names of the exact forms Connecto makes are looked at, only when the
/// process whose pid they carry no longer exists, and never this process's
/// own. A pid taken by another process since keeps its leftovers until that
/// one ends. Links are not followed, and a failure skips that entry.
pub fn sweep_stale(layout: &Layout) {
    sweep_run_folders(&layout.data_root.join("run"));
    if let Ok(dir) = socket_dir() {
        sweep_sockets(&dir);
    }
}

/// The folders of one attempt.
struct SessionDirs {
    run: RunFolder,
    tmp: PathBuf,
    /// Holds the tool's generations. No session can write here.
    tool_root: PathBuf,
    profile: PathBuf,
    attempt: u64,
    /// The reservation, for a session. A status check holds none.
    claim: Option<ProfileClaim>,
}

/// Names the current generation. It sits beside the generations, where no
/// session's rules reach, so a session cannot choose what a later one uses.
const CURRENT_GENERATION: &str = "current";

pub(crate) fn generation_dir(tool_root: &Path, generation: u64) -> PathBuf {
    tool_root.join(format!("g{generation}"))
}

/// The tool's current profile folder for a project, without reserving it.
/// For reading what a session left there.
pub(crate) fn current_profile(layout: &Layout, project: &Path, tool: Tool) -> Option<PathBuf> {
    let tool_root = layout
        .data_root
        .join("projects")
        .join(project_id(project))
        .join("profiles")
        .join(tool.name());
    let generation = current_generation(&tool_root).ok()?;
    Some(generation_dir(&tool_root, generation))
}

pub(crate) fn current_generation(tool_root: &Path) -> io::Result<u64> {
    match fs::read_to_string(tool_root.join(CURRENT_GENERATION)) {
        Ok(text) => text
            .trim()
            .parse()
            .map_err(|_| io::Error::other("the profile generation file is damaged")),
        Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(1),
        Err(err) => Err(err),
    }
}

/// The generations on disk, by name only.
fn generations(tool_root: &Path) -> io::Result<Vec<u64>> {
    Ok(fs::read_dir(tool_root)?
        .flatten()
        .filter_map(|entry| {
            let name = entry.file_name();
            name.to_str()?.strip_prefix('g')?.parse().ok()
        })
        .collect())
}

/// Make a new, empty generation current. Older generations stay on disk
/// (ADR 0013): with no sandbox, a kept generation is normal.
fn new_generation(tool_root: &Path) -> io::Result<PathBuf> {
    let existing = generations(tool_root)?;
    let highest = existing.iter().copied().max().unwrap_or(0);
    let next = highest.max(current_generation(tool_root)?) + 1;
    let profile = generation_dir(tool_root, next);
    // Not recursive: this must be the call that creates it.
    platform::new_private_dir(&profile)?;
    // Its sign-in is kept under this path, wherever the folder goes later.
    // The path as Claude is given its config folder (no `\\?\` on Windows).
    if tool_root.file_name().is_some_and(|name| name == "claude") {
        let identity = for_child(&real(&profile)?);
        fs::write(
            profile.join(connecto_adapter::SECURE_STORAGE_FILE),
            identity.to_string_lossy().as_bytes(),
        )?;
    }
    let staged = tool_root.join(format!("{CURRENT_GENERATION}.new"));
    fs::write(&staged, next.to_string())?;
    fs::rename(&staged, tool_root.join(CURRENT_GENERATION))?;
    real(&profile)
}

fn session_dirs(
    layout: &Layout,
    project: &Path,
    tool: Tool,
    reserve: bool,
) -> Result<SessionDirs, LaunchError> {
    let id = project_id(project);
    let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let root = layout.data_root.join("projects").join(&id);
    let tool_root = root.join("profiles").join(tool.name());
    private_dir(&tool_root)?;
    let (claim, profile) = if reserve {
        let (claim, profile) = reserve_session(&tool_root)?;
        (Some(claim), profile)
    } else {
        (
            None,
            generation_dir(&tool_root, current_generation(&tool_root)?),
        )
    };
    let run = layout.data_root.join("run").join(format!(
        "{id}-{}-{}-{attempt}",
        tool.name(),
        std::process::id()
    ));
    let tmp = run.join("tmp");
    private_dir(&profile)?;
    // Owned from here, so a failure below cannot leak it.
    private_dir(&run)?;
    let run = RunFolder(real(&run)?);
    private_dir(&tmp)?;
    Ok(SessionDirs {
        run,
        tmp: real(&tmp)?,
        tool_root: real(&tool_root)?,
        profile: real(&profile)?,
        attempt,
        claim,
    })
}

/// Whether this project's terminals of `tool` use the tool's own configuration
/// folder (`~/.claude`, `~/.codex`) instead of a Connecto profile. Read from the
/// project's settings at each launch.
fn uses_system_config(layout: &Layout, project: &Path, tool: Tool) -> bool {
    use connecto_core::ConfigMode;
    let settings = crate::project_settings::load(&layout.data_root, &project_id(project));
    let mode = match tool {
        Tool::Claude => settings.claude_config,
        Tool::Codex => settings.codex_config,
        // A shell keeps no configuration of its own.
        Tool::Shell => return false,
    };
    mode == ConfigMode::System
}

/// The tool's own configuration folder in the user's home.
fn system_profile(layout: &Layout, tool: Tool) -> PathBuf {
    layout.home.join(match tool {
        Tool::Claude => ".claude",
        Tool::Codex => ".codex",
        Tool::Shell => ".",
    })
}

fn inherited_env() -> Vec<(OsString, OsString)> {
    INHERITED
        .iter()
        .filter_map(|name| Some(((*name).into(), std::env::var_os(name)?)))
        .collect()
}

fn login_ctx(
    layout: &Layout,
    entry: PathBuf,
    dirs: &SessionDirs,
    system: Option<PathBuf>,
) -> LoginCtx {
    LoginCtx {
        entry: for_child(&entry),
        system_config: system.is_some(),
        profile: for_child(&system.unwrap_or_else(|| dirs.profile.clone())),
        run: for_child(&dirs.run.0),
        session_tmp: for_child(&dirs.tmp),
        home: for_child(&layout.home),
        inherited: inherited_env(),
    }
}

/// A path as a tool is given it, on its command line, in its environment or
/// in a file it reads. Canonical Windows paths are verbatim (`\\?\C:\...`),
/// which would leak into `TEMP`, `CLAUDE_CONFIG_DIR` and settings paths, and
/// which Claude would encode into its transcript folder names; a drive path
/// loses the prefix ([`platform::child_cwd`]). Elsewhere it is unchanged.
fn for_child(path: &Path) -> PathBuf {
    platform::child_cwd(path)
}

/// The project folder as its real path, or why it may not be used.
pub fn validated(layout: &Layout, project: &Path) -> Result<PathBuf, PathError> {
    validate_project(
        project,
        &ForbiddenRoots {
            home: layout.home.clone(),
            data_root: layout.data_root.clone(),
            allowed: crate::project_folder::allowed_root(&layout.data_root),
        },
    )
}

/// The shared hold on a project's package install, or `InstallPending`.
fn hold_for_install(
    layout: &Layout,
    project: &Path,
    limit: std::time::Duration,
) -> Result<crate::install_gate::Permit, LaunchError> {
    crate::install_gate::permit(&layout.data_root, &project_id(project), limit)
        .ok_or(LaunchError::InstallPending)
}

/// Start `tool`'s store copy inside `project`. Blocking: hashes the copy, so
/// call this off the UI thread.
pub fn start_verified_copy(
    layout: &Layout,
    project: &Path,
    tool: Tool,
    version: &str,
    size: GridSize,
    wake: Wake,
) -> Result<LiveSession, LaunchError> {
    platform::agents_supported()?;
    let installed = Store::new(&layout.data_root).verify(tool, version)?;
    start_verified(
        layout,
        project,
        tool,
        &installed,
        size,
        wake,
        &LaunchExtras::default(),
        &BridgeInfo::absent(),
    )
}

/// As [`start_verified_copy`], for a copy that was just verified.
#[allow(clippy::too_many_arguments)]
pub fn start_verified(
    layout: &Layout,
    project: &Path,
    tool: Tool,
    installed: &Installed,
    size: GridSize,
    wake: Wake,
    extras: &LaunchExtras,
    bridge: &BridgeInfo,
) -> Result<LiveSession, LaunchError> {
    start_resuming(
        layout, project, tool, installed, size, wake, extras, bridge, None,
    )
}

/// As [`start_verified`], picking up the tool session `resume` names instead of
/// starting a new one. The id is checked here, because it goes on a command
/// line: a Claude or Codex session id is letters, digits, `-` and `_`.
#[allow(clippy::too_many_arguments)]
pub fn start_resuming(
    layout: &Layout,
    project: &Path,
    tool: Tool,
    installed: &Installed,
    size: GridSize,
    wake: Wake,
    extras: &LaunchExtras,
    bridge: &BridgeInfo,
    resume: Option<&str>,
) -> Result<LiveSession, LaunchError> {
    if let Some(id) = resume
        && !crate::terminal_list::valid_session(id)
    {
        return Err(LaunchError::Io(io::Error::other(
            "the saved session id is not an id",
        )));
    }
    platform::agents_supported()?;
    let project = validated(layout, project)?;
    let terminal_key = format!("{}/{}", project_id(&project), bridge.terminal);
    // Held until the process is spawned, and kept apart from the project's
    // package install, so the tool starts on a complete `.claude/`, `.codex/`
    // and `.agents/`. After the limit the start fails visibly; a restart works
    // once the install is done.
    let _install_permit = hold_for_install(layout, &project, crate::install_gate::LIMIT)?;
    let hook_binary = Store::new(&layout.data_root).verify_hook()?;
    let mut dirs = session_dirs(layout, &project, tool, true)?;
    let pty = OpenPty::open(size)?;
    let hook_socket = socket_path(tool, dirs.attempt)?;

    let hook_token = hex(&random_bytes::<32>()?);
    let system = uses_system_config(layout, &project, tool).then(|| system_profile(layout, tool));
    let ctx = LaunchCtx {
        entry: for_child(&installed.entry),
        hook_binary: for_child(&hook_binary),
        // The PTY's working folder too (`launch.cwd`), and Codex's `-C`.
        project: for_child(&project),
        system_config: system.is_some(),
        profile: for_child(&system.clone().unwrap_or_else(|| dirs.profile.clone())),
        session_tmp: for_child(&dirs.tmp),
        run: for_child(&dirs.run.0),
        home: for_child(&layout.home),
        inherited: inherited_env(),
        session_id: new_uuid()?,
        resume: resume.map(str::to_owned),
        hook_socket: hook_socket.clone(),
        hook_token: hook_token.clone(),
        bridge: BridgeLaunch {
            socket: bridge.socket.clone(),
            terminal: bridge.terminal.clone(),
            bin_dir: for_child(&Store::new(&layout.data_root).bin_dir()),
            text: bridge.text.clone(),
            hooks: bridge.hooks,
            trust: bridge.trust.clone(),
        },
        initial_prompt: extras.initial_prompt.clone(),
        session_name: (tool == Tool::Claude)
            .then(|| extras.session_name.clone())
            .flatten(),
        model: extras.model.clone(),
        resume_note: extras.resume_note.clone(),
        utility: extras.utility.clone(),
        agent_mode: launch_modes().for_tool(crate::terminals::kind(tool)),
        #[cfg(feature = "test-endpoints")]
        fake_model: extras.fake_model.clone(),
    };
    let adapter = adapter(tool);
    let launch = adapter.launch(&ctx);
    for (path, body) in &launch.files {
        fs::write(path, body)?;
    }

    let socket = BoundSocket::bind(&hook_socket)?;
    let claim = dirs.claim.take();
    let spec = PtySpec {
        program: launch.program,
        args: launch.args,
        cwd: launch.cwd,
        env: launch.env,
        size,
    };
    let tap = if extras.utility.is_none() && !bridge.terminal.is_empty() {
        TAP_FACTORY
            .get()
            .map(|factory| factory.tap_for(&terminal_key))
    } else {
        None
    };
    let mut session = LiveSession::start_tapped(pty, spec, wake.clone(), tap)?;
    let hook_ctx = HookCtx {
        profile: for_child(&system.unwrap_or_else(|| dirs.profile.clone())),
        hooks: bridge.hooks,
    };
    let config = |root_pid| ServerConfig {
        token: hook_token,
        root_pid,
        adapter,
        ctx: hook_ctx.clone(),
        wake,
    };
    let served = session
        .pid()
        .ok_or(LaunchError::NoProcess)
        .and_then(|pid| Ok(serve(socket, config(pid))?));
    match served {
        Ok(hook) => {
            if let Some(claim) = &claim {
                claim.bind(&session.pty());
            }
            session.attach(Some(hook), Some(hook_ctx), dirs.run, claim);
            Ok(session)
        }
        Err(err) => {
            // A session without its hook channel must not keep running.
            session.terminate();
            Err(err)
        }
    }
}

static SHELL_PROGRAM: std::sync::RwLock<Option<PathBuf>> = std::sync::RwLock::new(None);

/// Use `program` for every plain shell started from now on, instead of `$SHELL`
/// (`None` goes back to `$SHELL`). For setup code and tests; a path that is not
/// an executable file is ignored at launch.
pub fn set_shell_program(program: Option<PathBuf>) {
    *SHELL_PROGRAM
        .write()
        .unwrap_or_else(PoisonError::into_inner) = program;
}

/// The shell a plain terminal runs: the one set with [`set_shell_program`], else
/// `$SHELL` when it names an executable file, else the first of zsh, bash and sh
/// that exists. On Windows `SHELL` is not consulted, and the fallback is
/// `pwsh.exe` on `PATH` or where its installers put it, else Windows
/// PowerShell (`powershell.exe`).
#[must_use]
pub fn shell_program() -> PathBuf {
    let chosen = SHELL_PROGRAM
        .read()
        .unwrap_or_else(PoisonError::into_inner)
        .clone();
    shell_program_from(
        chosen
            .map(PathBuf::into_os_string)
            .or_else(platform::shell_from_env)
            .as_deref(),
    )
}

fn shell_program_from(shell: Option<&std::ffi::OsStr>) -> PathBuf {
    let usable = |path: &Path| {
        path.is_absolute() && fs::metadata(path).is_ok_and(|m| platform::is_executable(path, &m))
    };
    shell
        .map(PathBuf::from)
        .filter(|path| usable(path))
        .or_else(|| {
            platform::shell_candidates()
                .into_iter()
                .find(|path| usable(path))
        })
        .unwrap_or_else(platform::last_resort_shell)
}

/// Start the user's login shell in `project`. There is no store copy, no login,
/// no profile and no launch mode: the shell runs as an ordinary interactive login
/// shell, with the same small environment the agents get, plus `HOME`, `SHELL`
/// and `TERM`, so its own startup files make it the shell the user knows. The
/// bridge is reachable from it (the socket, the terminal's id, and `connecto` on
/// `PATH`), but no hooks and no instructions are given. Blocking.
///
/// On Windows the shell is PowerShell with `-NoLogo` (its profile runs), it
/// gets the Windows system variables and no `SHELL`, and its sockets are named
/// pipes.
pub fn start_shell(
    layout: &Layout,
    project: &Path,
    size: GridSize,
    wake: Wake,
    bridge: &BridgeInfo,
) -> Result<LiveSession, LaunchError> {
    let project = validated(layout, project)?;
    let terminal_key = format!("{}/{}", project_id(&project), bridge.terminal);
    let _install_permit = hold_for_install(layout, &project, crate::install_gate::LIMIT)?;
    let attempt = ATTEMPT.fetch_add(1, Ordering::Relaxed);
    let run = layout.data_root.join("run").join(format!(
        "{}-shell-{}-{attempt}",
        project_id(&project),
        std::process::id()
    ));
    private_dir(&run)?;
    let run = RunFolder(real(&run)?);
    let hook_socket = socket_path(Tool::Shell, attempt)?;
    let program = shell_program();
    let mut env = inherited_env();
    env.push(("HOME".into(), layout.home.clone().into_os_string()));
    env.extend(platform::shell_env(&program));
    env.push(("TERM".into(), "xterm-256color".into()));
    env.push(("COLORTERM".into(), "truecolor".into()));
    if !bridge.terminal.is_empty() {
        env.push((
            "CONNECTO_BRIDGE_SOCK".into(),
            bridge.socket.clone().into_os_string(),
        ));
        env.push(("CONNECTO_TERMINAL".into(), bridge.terminal.clone().into()));
        let bin = Store::new(&layout.data_root).bin_dir();
        let existing = env
            .iter()
            .position(|(name, _)| name == "PATH")
            .map(|index| env.remove(index).1);
        let mut path = bin.into_os_string();
        if let Some(existing) = existing {
            path.push(platform::PATH_SEPARATOR);
            path.push(existing);
        }
        env.push(("PATH".into(), path));
    }
    let socket = BoundSocket::bind(&hook_socket)?;
    let spec = PtySpec {
        args: platform::login_shell_args(&program),
        program,
        cwd: platform::child_cwd(&project),
        env,
        size,
    };
    let tap = if bridge.terminal.is_empty() {
        None
    } else {
        TAP_FACTORY
            .get()
            .map(|factory| factory.tap_for(&terminal_key))
    };
    let pty = OpenPty::open(size)?;
    let mut session = LiveSession::start_tapped(pty, spec, wake.clone(), tap)?;
    // The hook channel carries nothing for a shell. It is served all the same,
    // because "has a hook channel" is how the relay tells a terminal from a login.
    let hook_ctx = HookCtx {
        profile: run.0.clone(),
        hooks: false,
    };
    let config = |root_pid| ServerConfig {
        token: hex(&random_bytes::<32>().unwrap_or([0; 32])),
        root_pid,
        adapter: &Plain,
        ctx: hook_ctx.clone(),
        wake,
    };
    let served = session
        .pid()
        .ok_or(LaunchError::NoProcess)
        .and_then(|pid| Ok(serve(socket, config(pid))?));
    match served {
        Ok(hook) => {
            session.attach(Some(hook), Some(hook_ctx), run, None);
            Ok(session)
        }
        Err(err) => {
            session.terminate();
            Err(err)
        }
    }
}

fn socket_path(tool: Tool, attempt: u64) -> io::Result<PathBuf> {
    platform::endpoint(
        "hook",
        format!("{}-{}-{attempt}.sock", tool.name(), std::process::id()),
    )
}

/// Run the tool's login in a PTY. Call this only for a user's click. It gets a
/// new, empty profile generation, its working directory is the run folder, and
/// there is no bypass flag, no hook channel, and no model work.
pub fn start_login(
    layout: &Layout,
    project: &Path,
    tool: Tool,
    installed: &Installed,
    size: GridSize,
    wake: Wake,
) -> Result<LiveSession, LaunchError> {
    platform::agents_supported()?;
    let project = validated(layout, project)?;
    let system = uses_system_config(layout, &project, tool).then(|| system_profile(layout, tool));
    let mut dirs = session_dirs(layout, &project, tool, false)?;
    // Signed out with conversations in this profile (the usual way is a data
    // folder that moved: the tool's sign-in is kept per config folder path, so
    // the path changing signs it out): the login runs where the conversations
    // are, and nothing is abandoned. Signed in, a login is a deliberate switch.
    let in_place = system.is_none()
        && !matches!(
            auth_status(
                adapter(tool),
                &login_ctx(layout, installed.entry.clone(), &dirs, None),
            ),
            AuthState::LoggedIn { .. }
        )
        && generation_has_history(&dirs.profile, tool);
    let (claim, profile) = reserve_login(&dirs.tool_root, in_place)?;
    dirs.profile = profile;
    let launch = adapter(tool).login(&login_ctx(layout, installed.entry.clone(), &dirs, system));
    let spec = PtySpec {
        program: launch.program,
        args: launch.args,
        cwd: launch.cwd,
        env: launch.env,
        size,
    };
    let mut session = LiveSession::start_on(OpenPty::open(size)?, spec, wake)?;
    claim.bind(&session.pty());
    session.attach(None, None, dirs.run, Some(claim));
    Ok(session)
}

/// Ask the tool's status command, in this project's own profile. Blocking.
pub fn check_auth(
    layout: &Layout,
    project: &Path,
    tool: Tool,
    installed: &Installed,
) -> Result<AuthState, LaunchError> {
    platform::agents_supported()?;
    let project = validated(layout, project)?;
    let system = uses_system_config(layout, &project, tool).then(|| system_profile(layout, tool));
    let dirs = session_dirs(layout, &project, tool, false)?;
    let ctx = login_ctx(layout, installed.entry.clone(), &dirs, system);
    Ok(auth_status(adapter(tool), &ctx))
}

/// Repair a profile that a login left empty while its conversations sit in an
/// earlier generation: the saved sessions then "cannot be found", because a resume
/// looks only in the current generation. Each earlier generation's conversations
/// are copied (never moved, never over an existing file) into the current one when
/// it holds none. Runs at startup; a profile that is fine is not touched. Returns
/// how many conversation files were brought across.
pub fn repair_empty_generations(data_root: &Path) -> usize {
    let mut brought = 0;
    let Ok(projects) = fs::read_dir(data_root.join("projects")) else {
        return 0;
    };
    for project in projects.flatten() {
        for tool in [Tool::Claude, Tool::Codex] {
            let tool_root = project.path().join("profiles").join(tool.name());
            let Ok(current) = current_generation(&tool_root) else {
                continue;
            };
            let current_dir = generation_dir(&tool_root, current);
            if !current_dir.is_dir() || generation_has_history(&current_dir, tool) {
                continue;
            }
            // The newest earlier generation that has conversations.
            let Some(source) = generations(&tool_root)
                .unwrap_or_default()
                .into_iter()
                .filter(|g| *g < current)
                .map(|g| generation_dir(&tool_root, g))
                .filter(|dir| generation_has_history(dir, tool))
                .max()
            else {
                continue;
            };
            let folders: &[&str] = match tool {
                Tool::Claude => &["projects", "file-history"],
                _ => &["sessions"],
            };
            for folder in folders {
                brought += copy_missing(&source.join(folder), &current_dir.join(folder));
            }
        }
    }
    brought
}

/// Copy the files of `from` that `to` lacks, keeping the layout. Best effort.
fn copy_missing(from: &Path, to: &Path) -> usize {
    let Ok(entries) = fs::read_dir(from) else {
        return 0;
    };
    let mut copied = 0;
    if fs::create_dir_all(to).is_err() {
        return 0;
    }
    for entry in entries.flatten() {
        let target = to.join(entry.file_name());
        match entry.file_type() {
            Ok(kind) if kind.is_dir() => copied += copy_missing(&entry.path(), &target),
            Ok(kind)
                if kind.is_file()
                    && !target.exists()
                    && fs::copy(entry.path(), &target).is_ok() =>
            {
                copied += 1;
            }
            _ => {}
        }
    }
    copied
}

/// Whether the user is signed in to `tool` on this Mac (the system
/// configuration), asked of the tool's own status command at `entry`. Only the
/// answer is kept: no credential is read here, and nothing is written outside
/// a scratch folder under the data root. `Unknown` when it cannot be told.
pub fn system_signin(layout: &Layout, tool: Tool, entry: &Path) -> AuthState {
    if platform::agents_supported().is_err() {
        return AuthState::Unknown;
    }
    let Ok(run) = signin_folder(layout, tool) else {
        return AuthState::Unknown;
    };
    let tmp = run.0.join("tmp");
    if private_dir(&tmp).is_err() {
        return AuthState::Unknown;
    }
    let mut inherited = inherited_env();
    if connecto_store::is_launcher(entry) {
        // A launcher script runs `node`, which a Finder-started app's PATH lacks.
        inherited.retain(|(name, _)| name != "PATH");
        inherited.push(("PATH".into(), connecto_store::run_path(entry, &layout.home)));
    }
    let ctx = LoginCtx {
        entry: for_child(entry),
        system_config: true,
        profile: for_child(&system_profile(layout, tool)),
        run: for_child(&run.0),
        session_tmp: for_child(&tmp),
        home: for_child(&layout.home),
        inherited,
    };
    // `run` is removed when it drops, on this return and on every path above.
    auth_status(adapter(tool), &ctx)
}

/// Counts the sign-in checks of this process, so each has a folder of its own.
static SIGNIN_CHECKS: AtomicU64 = AtomicU64::new(0);

/// A new, empty run folder for one sign-in check. Two checks of the same tool
/// in one process (Settings and setup, say) never share one, so neither can
/// delete the other's. The folder goes away with the returned guard.
fn signin_folder(layout: &Layout, tool: Tool) -> io::Result<RunFolder> {
    let n = SIGNIN_CHECKS.fetch_add(1, Ordering::Relaxed);
    let run = layout.data_root.join("run").join(format!(
        "signin-{}-{n}-{}",
        std::process::id(),
        tool.name()
    ));
    private_dir(&run)?;
    Ok(RunFolder(run))
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// Concurrent sign-in checks of one tool each get their own folder, all
    /// exist while held, and every one is removed when its guard drops.
    #[test]
    fn concurrent_sign_in_checks_get_their_own_folders() {
        let root = temp("signin");
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let folders: Vec<PathBuf> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let layout = &layout;
                    scope.spawn(move || {
                        let run = signin_folder(layout, Tool::Claude).unwrap();
                        std::thread::sleep(std::time::Duration::from_millis(100));
                        let path = run.0.clone();
                        assert!(path.is_dir(), "still there while held");
                        path
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        let distinct: std::collections::BTreeSet<&PathBuf> = folders.iter().collect();
        assert_eq!(distinct.len(), folders.len(), "{folders:?}");
        for path in &folders {
            assert!(!path.exists(), "removed after its guard dropped: {path:?}");
        }
        let _ = fs::remove_dir_all(&root);
    }

    fn temp(name: &str) -> PathBuf {
        let nanos = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |d| d.as_nanos());
        let dir = std::env::temp_dir().join(format!(
            "connecto-launch-{name}-{}-{nanos}",
            std::process::id()
        ));
        fs::create_dir_all(&dir).unwrap();
        fs::canonicalize(dir).unwrap()
    }

    /// The folder Claude keeps a working folder's transcripts in.
    #[cfg(unix)]
    fn claude_folder(cwd: &str) -> String {
        cwd.replace(['/', '.'], "-")
    }

    /// The folder Claude keeps a working folder's transcripts in. A Windows
    /// path has `\`, `:` and a `\\?\` prefix too; none may stay in one name.
    #[cfg(windows)]
    fn claude_folder(cwd: &str) -> String {
        cwd.replace(|c: char| !c.is_ascii_alphanumeric() && c != '-', "-")
    }

    /// A transcript where Claude's resume looks for it:
    /// `<config dir>/projects/<working folder with / and . as ->/<session>.jsonl`.
    fn transcript(generation: &Path, cwd: &str, session: &str) -> PathBuf {
        let folder = claude_folder(cwd);
        let path = generation
            .join("projects")
            .join(folder)
            .join(format!("{session}.jsonl"));
        fs::create_dir_all(path.parent().unwrap()).unwrap();
        fs::write(&path, "{\"type\":\"user\"}\n").unwrap();
        path
    }

    fn can_resume(layout: &Layout, project: &Path, session: &str) -> bool {
        let profile = current_profile(layout, project, Tool::Claude).unwrap();
        let folder = claude_folder(&project.to_string_lossy());
        profile
            .join("projects")
            .join(folder)
            .join(format!("{session}.jsonl"))
            .is_file()
    }

    /// What a login on the moved data left: a fresh current generation, empty, and
    /// the conversations in the one before.
    fn login_rotated(root: &Path) -> (Layout, PathBuf) {
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let project = root.join("work/p");
        fs::create_dir_all(&project).unwrap();
        let project = fs::canonicalize(project).unwrap();
        let tool_root = layout
            .data_root
            .join("projects")
            .join(project_id(&project))
            .join("profiles")
            .join("claude");
        let g2 = generation_dir(&tool_root, 2);
        transcript(
            &g2,
            &project.to_string_lossy(),
            "6bfbf2dd-4945-48e5-9029-31fe586d0723",
        );
        fs::create_dir_all(generation_dir(&tool_root, 3)).unwrap();
        fs::write(tool_root.join(CURRENT_GENERATION), "3").unwrap();
        (layout, project)
    }

    #[test]
    fn a_resume_finds_nothing_after_a_login_rotated_the_profile_and_the_repair_brings_it_back() {
        let root = temp("repair");
        let (layout, project) = login_rotated(&root);
        let session = "6bfbf2dd-4945-48e5-9029-31fe586d0723";
        // The bug, as the user met it: the saved session is not where the CLI looks.
        assert!(!can_resume(&layout, &project, session));
        let brought = repair_empty_generations(&layout.data_root);
        assert_eq!(brought, 1);
        assert!(
            can_resume(&layout, &project, session),
            "the lookup now finds it"
        );
        // The earlier generation is untouched, and a second run changes nothing.
        let tool_root = layout
            .data_root
            .join("projects")
            .join(project_id(&project))
            .join("profiles/claude");
        assert!(generation_has_history(
            &generation_dir(&tool_root, 2),
            Tool::Claude
        ));
        assert_eq!(repair_empty_generations(&layout.data_root), 0);
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn the_repair_never_overwrites_and_leaves_a_profile_that_has_history_alone() {
        let root = temp("repair-keep");
        let (layout, project) = login_rotated(&root);
        let tool_root = layout
            .data_root
            .join("projects")
            .join(project_id(&project))
            .join("profiles/claude");
        // The current generation already has a conversation of its own: nothing is copied.
        let own = transcript(
            &generation_dir(&tool_root, 3),
            &project.to_string_lossy(),
            "mine",
        );
        assert_eq!(repair_empty_generations(&layout.data_root), 0);
        assert!(own.is_file());
        assert!(!can_resume(
            &layout,
            &project,
            "6bfbf2dd-4945-48e5-9029-31fe586d0723"
        ));
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_login_on_a_signed_out_profile_with_conversations_stays_in_that_generation() {
        let root = temp("in-place");
        let tool_root = root.join("claude");
        let g2 = generation_dir(&tool_root, 2);
        transcript(&g2, "/work/p", "abc");
        fs::write(tool_root.join(CURRENT_GENERATION), "2").unwrap();
        let (_claim, profile) = reserve_login(&tool_root, true).unwrap();
        assert_eq!(profile, fs::canonicalize(&g2).unwrap());
        assert_eq!(
            fs::read_to_string(tool_root.join(CURRENT_GENERATION))
                .unwrap()
                .trim(),
            "2"
        );
        // Not in place (a deliberate switch): a fresh generation, which says where its
        // sign-in will live.
        let other = temp("rotate");
        let tool_root = other.join("claude");
        fs::create_dir_all(generation_dir(&tool_root, 1)).unwrap();
        let (_claim, profile) = reserve_login(&tool_root, false).unwrap();
        assert_eq!(profile.file_name().unwrap(), "g2");
        let identity =
            fs::read_to_string(profile.join(connecto_adapter::SECURE_STORAGE_FILE)).unwrap();
        assert_eq!(PathBuf::from(identity), for_child(&profile));
        let _ = fs::remove_dir_all(&root);
        let _ = fs::remove_dir_all(&other);
    }

    #[cfg(windows)]
    #[test]
    fn the_shell_is_the_chosen_one_when_it_is_usable_else_powershell() {
        let system = PathBuf::from(std::env::var_os("SystemRoot").unwrap()).join("System32");
        let cmd = system.join("cmd.exe");
        assert_eq!(shell_program_from(Some(cmd.as_os_str())), cmd);
        // Not absolute, missing, a folder or not a program: PowerShell instead.
        let not_a_program = system.join("drivers").join("etc").join("hosts");
        for bad in [
            Path::new("cmd.exe"),
            Path::new(r"C:\no\such\shell.exe"),
            system.as_path(),
            not_a_program.as_path(),
        ] {
            let chosen = shell_program_from(Some(bad.as_os_str()));
            assert!(
                chosen.is_absolute() && chosen.is_file(),
                "{bad:?}: {chosen:?}"
            );
        }
        let default = shell_program_from(None);
        let name = default
            .file_name()
            .unwrap()
            .to_string_lossy()
            .to_lowercase();
        assert!(
            name == "pwsh.exe" || name == "powershell.exe",
            "{default:?}"
        );
        assert!(default.is_file());
        // Neither `SHELL` (Git Bash sets a Unix path) nor its absence matters.
        assert_eq!(platform::shell_from_env(), None);
    }

    #[cfg(windows)]
    #[test]
    fn the_sweep_removes_the_run_folders_of_ended_runs() {
        let root = temp("sweep");
        let run = root.join("run");
        let mut child = std::process::Command::new("cmd.exe")
            .args(["/C", "exit"])
            .spawn()
            .unwrap();
        let dead = i32::try_from(child.id()).unwrap();
        child.wait().unwrap();
        // The handle keeps the ended process object; let it go first.
        drop(child);
        let own = std::process::id();
        let id = "87a3cfc65dafc230";
        let folder = |name: String| {
            fs::create_dir_all(run.join(&name).join("tmp")).unwrap();
            run.join(name)
        };
        let stale = folder(format!("{id}-shell-{dead}-3"));
        let kept = [
            folder(format!("{id}-shell-{own}-3")),
            folder(format!("notes-{dead}")),
        ];
        sweep_run_folders(&run);
        assert!(!stale.exists(), "{stale:?} was left by a dead run");
        for path in &kept {
            assert!(path.exists(), "{path:?} must stay");
        }
        // Sockets are named pipes, which leave nothing behind.
        sweep_sockets(&root);
        let _ = fs::remove_dir_all(&root);
    }

    #[cfg(unix)]
    #[test]
    fn the_shell_is_the_users_when_it_is_usable_else_a_system_one() {
        let sh = shell_program_from(Some(std::ffi::OsStr::new("/bin/sh")));
        assert_eq!(sh, PathBuf::from("/bin/sh"));
        // Not absolute, missing, or a folder: a system shell instead.
        for bad in ["zsh", "/no/such/shell", "/tmp"] {
            let chosen = shell_program_from(Some(std::ffi::OsStr::new(bad)));
            assert!(
                chosen.is_absolute() && chosen.is_file(),
                "{bad}: {chosen:?}"
            );
        }
        assert!(shell_program_from(None).is_file());
    }

    #[test]
    fn only_the_names_connecto_makes_carry_a_pid() {
        assert_eq!(
            run_folder_pid("87a3cfc65dafc230-claude-84896-42"),
            Some(84896)
        );
        assert_eq!(run_folder_pid("0000000000000000-codex-77-1"), Some(77));
        assert_eq!(run_folder_pid("trust-84896"), Some(84896));
        for other in [
            "87a3cfc65dafc230-claude-84896",
            "87a3cfc65dafc230-claude-84896-42-1",
            "87a3cfc65dafc230-emacs-84896-42",
            "87A3CFC65DAFC230-claude-84896-42",
            "87a3cfc65dafc23-claude-84896-42",
            "87a3cfc65dafc230-claude-084896-42",
            "87a3cfc65dafc230-claude-+84896-42",
            "87a3cfc65dafc230-claude-84896-x",
            "87a3cfc65dafc230-claude-1-42",
            "87a3cfc65dafc230-claude-99999999999-42",
            "trust-",
            "trust-12-3",
            "tmp",
            "",
        ] {
            assert_eq!(run_folder_pid(other), None, "{other}");
        }
        assert_eq!(socket_pid("bridge-84896.sock"), Some(84896));
        assert_eq!(socket_pid("bridge-84896-2.sock"), Some(84896));
        assert_eq!(socket_pid("claude-84896-42.sock"), Some(84896));
        assert_eq!(socket_pid("codex-84896-1.sock"), Some(84896));
        for other in [
            "bridge-84896",
            "bridge.sock",
            "bridge-x.sock",
            "bridge-84896-2-3.sock",
            "claude-84896.sock",
            "emacs-84896-1.sock",
            "claude-0-1.sock",
            "a.sock",
        ] {
            assert_eq!(socket_pid(other), None, "{other}");
        }
    }

    /// A pid whose process has ended: a child that was waited for.
    #[cfg(unix)]
    fn dead_pid() -> i32 {
        let mut child = std::process::Command::new("/usr/bin/true").spawn().unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        child.wait().unwrap();
        pid
    }

    #[cfg(unix)]
    #[test]
    fn the_sweep_removes_what_dead_runs_left_and_nothing_else() {
        use std::os::unix::net::UnixListener;

        // A short path: a socket's is limited to about a hundred bytes.
        let root = PathBuf::from(format!("/tmp/mg-sw-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let (run, socks, outside) = (root.join("run"), root.join("s"), root.join("outside"));
        for dir in [&run, &socks, &outside] {
            fs::create_dir_all(dir).unwrap();
        }
        fs::write(outside.join("kept"), "outside").unwrap();
        let mut sleeper = std::process::Command::new("/bin/sleep")
            .arg("30")
            .spawn()
            .unwrap();
        let (dead, live, own) = (dead_pid(), sleeper.id(), std::process::id());
        let id = "87a3cfc65dafc230";

        let folder = |name: String| {
            fs::create_dir_all(run.join(&name).join("tmp")).unwrap();
            fs::write(run.join(&name).join("tmp/scratch"), "x").unwrap();
            run.join(name)
        };
        let stale = [
            folder(format!("{id}-claude-{dead}-3")),
            folder(format!("{id}-codex-{dead}-4")),
            folder(format!("trust-{dead}")),
        ];
        // A dead run's folder may hold a link out: the target stays.
        std::os::unix::fs::symlink(&outside, stale[0].join("tmp/out")).unwrap();
        let kept = [
            folder(format!("{id}-claude-{live}-3")),
            folder(format!("{id}-claude-{own}-3")),
            folder(format!("trust-{own}")),
            folder(format!("{id}-emacs-{dead}-3")),
            folder(format!("{id}-claude-{dead}")),
            folder(format!("notes-{dead}")),
        ];
        // Right names, wrong kinds: a file and a link are not run folders.
        fs::write(run.join(format!("{id}-claude-{dead}-8")), "a file").unwrap();
        std::os::unix::fs::symlink(&outside, run.join(format!("{id}-claude-{dead}-9"))).unwrap();

        let socket = |name: String| {
            drop(UnixListener::bind(socks.join(&name)).unwrap());
            socks.join(name)
        };
        let stale_sockets = [
            socket(format!("bridge-{dead}.sock")),
            socket(format!("bridge-{dead}-2.sock")),
            socket(format!("claude-{dead}-7.sock")),
        ];
        let kept_sockets = [
            socket(format!("bridge-{live}.sock")),
            socket(format!("bridge-{own}.sock")),
            socket(format!("codex-{own}-7.sock")),
            socket(format!("other-{dead}-7.sock")),
            socket(format!("bridge-{dead}.sock.old")),
        ];
        fs::write(socks.join(format!("codex-{dead}-1.sock")), "a file").unwrap();

        sweep_run_folders(&run);
        sweep_sockets(&socks);
        // A folder that is itself a link is not gone through.
        std::os::unix::fs::symlink(&run, root.join("link")).unwrap();
        sweep_run_folders(&root.join("link"));
        sweep_sockets(&root.join("missing"));

        for path in stale.iter().chain(&stale_sockets) {
            assert!(
                fs::symlink_metadata(path).is_err(),
                "{path:?} was left by a dead run"
            );
        }
        for path in kept.iter().chain(&kept_sockets) {
            assert!(fs::symlink_metadata(path).is_ok(), "{path:?} must stay");
        }
        assert!(run.join(format!("{id}-claude-{dead}-8")).is_file());
        assert!(fs::symlink_metadata(run.join(format!("{id}-claude-{dead}-9"))).is_ok());
        assert!(socks.join(format!("codex-{dead}-1.sock")).is_file());
        assert_eq!(
            fs::read_to_string(outside.join("kept")).unwrap(),
            "outside",
            "nothing is followed out of a run folder"
        );
        let _ = sleeper.kill();
        let _ = sleeper.wait();
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn project_ids_are_stable_and_distinct() {
        let a = project_id(Path::new("/work/a"));
        assert_eq!(a, project_id(Path::new("/work/a")));
        assert_ne!(a, project_id(Path::new("/work/b")));
        assert_eq!(a.len(), 16);
    }

    #[test]
    fn profiles_are_separate_per_tool_and_project() {
        let dirs = |project: &str, tool: Tool| {
            format!(
                "{}/profiles/{}",
                project_id(Path::new(project)),
                tool.name()
            )
        };
        assert_ne!(dirs("/a", Tool::Claude), dirs("/a", Tool::Codex));
        assert_ne!(dirs("/a", Tool::Claude), dirs("/b", Tool::Claude));
    }

    #[test]
    fn the_projects_setting_picks_the_profile_or_the_tools_own_folder() {
        use connecto_core::{ConfigMode, ProjectSettings};
        let root = std::env::temp_dir().join(format!("mg-launch-mode-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        let layout = Layout {
            data_root: root.join("data"),
            home: root.join("home"),
        };
        let project = Path::new("/work/p");
        // A new project uses the system configuration.
        assert!(uses_system_config(&layout, project, Tool::Claude));
        let settings = ProjectSettings {
            claude_config: ConfigMode::System,
            codex_config: ConfigMode::Isolated,
            ..ProjectSettings::default()
        };
        crate::project_settings::save(&layout.data_root, &project_id(project), &settings)
            .ok()
            .unwrap_or_default();
        assert!(uses_system_config(&layout, project, Tool::Claude));
        assert!(!uses_system_config(&layout, project, Tool::Codex));
        // Another new project: the default again.
        assert!(uses_system_config(
            &layout,
            Path::new("/work/q"),
            Tool::Claude
        ));
        assert_eq!(
            system_profile(&layout, Tool::Codex),
            root.join("home/.codex")
        );
        let _ = fs::remove_dir_all(&root);
    }

    #[test]
    fn a_launch_held_past_the_limit_fails_visibly_and_works_after_the_install() {
        let layout = Layout {
            data_root: PathBuf::from("/launch-gate-test/data"),
            home: PathBuf::from("/launch-gate-test/home"),
        };
        let project = Path::new("/launch-gate-test/work");
        let short = std::time::Duration::from_millis(30);
        crate::install_gate::enable(&layout.data_root);
        crate::install_gate::expect(&layout.data_root, &project_id(project));
        let held = hold_for_install(&layout, project, short);
        assert!(matches!(held, Err(LaunchError::InstallPending)));
        assert!(
            LaunchError::InstallPending
                .to_string()
                .contains("start the terminal again")
        );
        crate::install_gate::finish(&layout.data_root, &project_id(project));
        assert!(
            hold_for_install(&layout, project, short).is_ok(),
            "a retry works"
        );
        crate::install_gate::disable(&layout.data_root);
    }

    #[test]
    fn session_ids_are_version_4_uuids() {
        let id = new_uuid().ok().unwrap_or_default();
        let parts: Vec<&str> = id.split('-').collect();
        assert_eq!(
            parts.iter().map(|p| p.len()).collect::<Vec<_>>(),
            [8, 4, 4, 4, 12]
        );
        assert!(parts[2].starts_with('4'));
        assert!(matches!(parts[3].as_bytes()[0], b'8' | b'9' | b'a' | b'b'));
    }

    #[test]
    fn the_environment_inherits_nothing_outside_the_allowlist() {
        assert!(!INHERITED.contains(&"HOME"));
        assert!(!INHERITED.iter().any(|name| name.contains("KEY")));
    }
}
