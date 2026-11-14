//! Process-group signals for a child in a Unix PTY.

use std::sync::{Mutex, PoisonError};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;
use portable_pty::{Child, MasterPty};

use super::Stop;

/// Signals a child's process groups. Holds nothing: the groups are found from
/// the leader's pid and the terminal at the time of each signal.
pub(crate) struct Killer;

impl Killer {
    pub(crate) fn new(_child: &(dyn Child + Send + Sync)) -> Self {
        Self
    }

    /// Signal the child's process group and the terminal's foreground group.
    ///
    /// An interactive shell moves each job into its own group, so the
    /// foreground job is not reached through the leader's group alone.
    /// The caller must not call this after the child is reaped, because its
    /// PID may be reused.
    pub(crate) fn stop(
        &self,
        leader: Option<i32>,
        master: &Mutex<Box<dyn MasterPty + Send>>,
        stop: Stop,
    ) {
        let signal = match stop {
            Stop::Term => Signal::SIGTERM,
            Stop::Kill => Signal::SIGKILL,
        };
        let Some(leader) = leader else { return };
        let foreground = master
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .process_group_leader();
        // Failures mean the group is already gone, which is the goal.
        let _ = killpg(Pid::from_raw(leader), signal);
        if let Some(group) = foreground.filter(|group| *group != leader && *group > 0) {
            let _ = killpg(Pid::from_raw(group), signal);
        }
    }
}
