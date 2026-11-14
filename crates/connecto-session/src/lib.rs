//! Running sessions: the only crate that touches a PTY.
//!
//! [`ManagedPane`] is what the application uses. It runs preflight on its own
//! thread, so a slow or failing check shows in the pane and never delays the
//! window. Hook events update state and nothing else.

mod auth;
mod bridge;
mod codex_trust;
mod history;
mod hook;
mod input_ready;
pub mod install_gate;
pub mod launch;
mod live;
mod local_socket;
mod pane;
mod platform;
mod process_tree;
pub mod project_folder;
pub mod project_settings;
mod relay;
pub mod remote_guard;
mod sweep;
mod terminal_list;
pub mod terminals;
pub mod turn;
pub mod utility;

pub use bridge::WriteOutcome;
pub use connecto_pty::{SIGKILL_GRACE, Wake};
pub use hook::{EVENT_QUEUE_CAPACITY, EventQueue, HookHandle};
pub use live::{INITIAL_SIZE, LiveSession, SHELL, shell_spec, spec};
pub use pane::{ManagedPane, PaneBridge, PaneSetup, StartRefusal, StartTicket};
pub use relay::{Relay, RelayConfig};
pub use remote_guard::RemoteGate;
pub use sweep::wait_for_sweeps;
pub use terminals::{Changed, Removal, TermError, TerminalsHandle};
