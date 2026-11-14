//! Shared vocabulary for the connecto terminal: key encoding, terminal
//! snapshots and the session contract the UI talks to.
//!
//! This crate performs no I/O. The UI depends only on these types, so it can
//! never reach a PTY or the emulator directly.

pub mod agent_mode;
pub mod agents;
pub mod automations;
pub mod events;
pub mod input;
pub mod packages;
pub mod pane;
pub mod relay;
pub mod remote;
pub mod session;
pub mod setup;
pub mod snapshot;
pub mod terminals;
pub mod untrusted;
pub mod update;
pub mod utility;
pub mod workspace;

pub use agent_mode::{AgentMode, LaunchModes};
pub use agents::{AgentBusy, AgentRow, AgentsControl, AgentsView, NoAgents};
pub use automations::{
    AutomationGroup, AutomationItem, AutomationOutcome, AutomationsControl, AutomationsView,
    EngineState, EngineStatus, HistoryRecord, KeyStatus, LastCheck, PendingConfirmation,
    RECENT_HISTORY,
};
pub use events::{Reply, ToolEvent};
pub use input::{
    InputModes, Key, KeyPress, Mods, MouseButton, MouseEvent, MouseKind, encode_key, encode_mouse,
    encode_paste,
};
pub use packages::{
    BUILTIN_PACKAGE_ID, ConfigMode, HomeMigration, InstallState, InstalledPackage, NoPackages,
    PackageItem, PackagesControl, PackagesView, ProjectPackages, ProjectPackagesView,
    ProjectSettings, ProjectTools,
};
pub use pane::{AuthState, PaneControl, PaneStage};
pub use relay::{
    Capture, Confirmation, DeliveryState, LedgerRow, Origin, PauseSwitch, Readiness, RelayControl,
    RelayView, SendRequest, SessionRef, SessionView,
};
pub use remote::{
    AuthorizedKeyRow, GuardDenied, HostLink, HostRow, HostingStatus, LOCALHOST, MachineRow,
    NoRemote, PAIRING_CODE_DIGITS, PairingPhase, PendingHost, RemoteControl, RemoteGuard,
    RemotePeer, RemoteView,
};
pub use session::{
    InputRefused, OutputTap, ResizeOutcome, SessionStatus, SnapshotOutcome, TapFactory,
    TerminalSession,
};
pub use setup::{
    AgentChoice, ProblemAction, ReadyAgent, Screen, SetupControl, SetupProblem, SetupStep,
    SetupView, StepState,
};
pub use snapshot::{Cell, CellFlags, Cursor, CursorShape, GridSize, Rgb, TermSnapshot};
pub use terminals::{
    HistoryEntry, MAX_NAME_CHARS, MAX_TERMINALS, SaveState, TermActivity, TerminalEntry,
    TerminalsControl, TerminalsView, ToolKind, Transcript,
};
pub use untrusted::{EventText, MAX_EVENT_TEXT, MAX_REPLY_TEXT, Redacted, Untrusted};
pub use update::{NoUpdates, UpdateControl, UpdateStatus, UpdateView};
pub use utility::{
    MAX_UTILITY_RUNS, UtilityEffort, UtilityModel, UtilityRun, UtilitySettings, UtilityStatus,
};
pub use workspace::{ProjectView, RecentProject, WorkspaceControl, WorkspaceView};
