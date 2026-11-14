//! Ending a child in a Windows pseudoconsole (ConPTY).

use std::sync::{Mutex, PoisonError};

use portable_pty::{Child, ChildKiller, MasterPty};

use super::Stop;

/// Terminates the child through its own process handle.
///
/// The handle is held open until the session is dropped, so the process ID
/// cannot be reused while it is held and a late kill can never reach another
/// process.
pub(crate) struct Killer(Mutex<Box<dyn ChildKiller + Send + Sync>>);

impl Killer {
    pub(crate) fn new(child: &(dyn Child + Send + Sync)) -> Self {
        Self(Mutex::new(child.clone_killer()))
    }

    /// Terminate the child.
    ///
    /// Windows has no SIGTERM a console program can be relied on to handle,
    /// so both stages terminate at once. There are no process groups either:
    /// other processes attached to the pseudoconsole end when it is closed,
    /// which happens when the session's master is dropped.
    pub(crate) fn stop(
        &self,
        _leader: Option<i32>,
        _master: &Mutex<Box<dyn MasterPty + Send>>,
        _stop: Stop,
    ) {
        // A failure means the child already ended, which is the goal.
        let _ = self.0.lock().unwrap_or_else(PoisonError::into_inner).kill();
    }
}
