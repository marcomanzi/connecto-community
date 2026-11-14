//! What differs between the platforms the app runs on: how it names the
//! computer, how it hears a request to stop, how it starts itself again after
//! the setup, how a fatal error reaches the user, free disk space, and whether
//! the agents (Claude Code, Codex) are supported.
//!
//! macOS is the reference: the Unix module is what the app did before the
//! Windows port. Each platform has its own small module; the rest of the crate
//! only uses the names re-exported here.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub use unix::*;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub use windows::*;
