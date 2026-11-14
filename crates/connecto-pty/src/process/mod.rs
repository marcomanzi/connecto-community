//! Ending a session's child, per platform.
//!
//! - **Unix**: the child leads its own process group (portable-pty calls
//!   `setsid`), so the leader's group and the terminal's foreground group are
//!   signalled with `killpg`.
//! - **Windows**: ConPTY has no process groups and no signals. The child is
//!   terminated through its process handle. Anything else still attached to
//!   the pseudoconsole is closed by Windows when the pseudoconsole itself is
//!   closed, which portable-pty does when the last master handle is dropped.

#[cfg(unix)]
mod unix;
#[cfg(unix)]
pub(crate) use unix::Killer;

#[cfg(windows)]
mod windows;
#[cfg(windows)]
pub(crate) use windows::Killer;

/// How hard to stop the child.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stop {
    /// Ask it to end: SIGTERM on Unix.
    Term,
    /// End it now: SIGKILL on Unix.
    Kill,
}
