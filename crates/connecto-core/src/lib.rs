//! Shared vocabulary for the connecto terminal: key encoding, terminal
//! snapshots and the session contract the UI talks to.
//!
//! This crate performs no I/O. The UI depends only on these types, so it can
//! never reach a PTY or the emulator directly.

pub mod agent_mode;
pub mod agentic_works;
pub mod agents;
pub mod automations;
pub mod cli_improvements;
pub mod copy_text;
pub mod events;
pub mod functional_memory;
pub mod input;
pub mod memory;
pub mod offers;
pub mod ops;
pub mod packages;
pub mod pane;
pub mod pi;
pub mod project_files;
pub mod providers;
pub mod question;
pub mod relay;
pub mod remote;
pub mod roster;
pub mod session;
pub mod setup;
pub mod siri_reply;
pub mod snapshot;
pub mod terminals;
pub mod tokens;
pub mod untrusted;
pub mod update;
pub mod utility;
pub mod vault;
pub mod workspace;

pub use agent_mode::{AgentMode, LaunchModes};
pub use agents::{AgentBusy, AgentRow, AgentsControl, AgentsView, NoAgents};
pub use automations::{
    AutomationGroup, AutomationItem, AutomationOutcome, AutomationsControl, AutomationsView,
    EngineState, EngineStatus, HistoryRecord, KeyStatus, LastCheck, PendingConfirmation,
    RECENT_HISTORY,
};
pub use events::{BackgroundReport, Reply, ToolEvent};
pub use input::{
    InputModes, Key, KeyPress, Mods, MouseButton, MouseEvent, MouseKind, encode_key, encode_mouse,
    encode_paste,
};
pub use memory::{
    MemoryControl, MemorySettings, MemoryView, ModelChoice, NoMemory, ProjectMemoryView,
};
pub use packages::{
    BUILTIN_PACKAGE_ID, ConfigMode, GAME_DEV_PACKAGE_ID, HomeMigration, InstallState,
    InstalledPackage, LEGACY_BUILTIN_PACKAGE_ID, NoPackages, PackageItem, PackagesControl,
    PackagesView, ProjectPackages, ProjectPackagesView, ProjectSettings, ProjectTools,
    RequirementState, RequirementView, current_package_id, package_refusal, requirements_refusal,
};
pub use pane::{AuthState, PaneControl, PaneStage};
pub use pi::{PiLocalState, PiProvider, PiProvidersView};
pub use providers::{ClaudeProvidersView, ProviderScope, TogetherLinkState};
pub use relay::{
    Capture, Confirmation, DeliveryState, LedgerRow, Origin, PauseSwitch, Readiness, RelayControl,
    RelayView, SendRequest, SessionRef, SessionView,
};
pub use remote::{
    AuthorizedKeyRow, GuardDenied, HostLink, HostRow, HostingStatus, LOCALHOST,
    MAX_REMOTE_NAME_CHARS, MachineRow, NoRemote, PAIRING_CODE_DIGITS, PairingPhase, PendingHost,
    RemoteControl, RemoteGroups, RemoteGuard, RemoteOffer, RemotePeer, RemoteProject,
    RemoteSecretAsk, RemoteView,
};
pub use session::{
    InputCondition, InputGeneration, InputRefused, OutputTap, PastedFiles, ResizeOutcome, SelectOp,
    SelectionText, SessionStatus, SnapshotOutcome, TapFactory, TerminalSession,
};
pub use setup::{
    AgentChoice, ProblemAction, ReadyAgent, Screen, SetupControl, SetupProblem, SetupStep,
    SetupView, StepState,
};
pub use snapshot::{
    Cell, CellFlags, Cursor, CursorShape, GridSize, Rgb, SelectedRange, TermSnapshot,
};
pub use terminals::{
    BackgroundWork, CLOSE_NOT_ALL_LISTED, HistoryEntry, LONG_RUNNING, MAX_NAME_CHARS,
    MAX_TERMINALS, RemoteModels, SaveState, TermActivity, TerminalEntry, TerminalsControl,
    TerminalsView, ToolKind, Transcript, short_age,
};
pub use tokens::{NoTokens, TokenControl, TokenView};
pub use untrusted::{EventText, MAX_EVENT_TEXT, MAX_REPLY_TEXT, Redacted, Untrusted};
pub use update::{NoUpdates, UpdateControl, UpdateStatus, UpdateView};
pub use utility::{
    MAX_UTILITY_RUNS, UtilityEffort, UtilityModel, UtilityRun, UtilitySettings, UtilityStatus,
};
pub use vault::{
    AnswerState, AskAnswer, AskKind, AskReply, AskRequest, Binding, ChangeError, GeneratedKey,
    NoVault, RunOutput, SecretAsk, SecretCaller, SecretEntry, SecretsPage, SshAccess, VaultBridge,
    VaultChange, VaultControl, VaultView,
};
pub use workspace::{ProjectGroup, ProjectView, RecentProject, WorkspaceControl, WorkspaceView};
