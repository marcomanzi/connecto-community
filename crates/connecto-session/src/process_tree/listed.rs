//! [`crate::process_tree`] from `sysinfo`'s process list (Windows, and any
//! platform other than macOS).
//!
//! Windows keeps a process's parent pid after the parent ended, and that pid
//! can be reused, so a parent counts only if it started no later than the child.

use sysinfo::{Pid, Process, ProcessRefreshKind, ProcessStatus, ProcessesToUpdate, System};

/// One process, known by its start time too, as a PID can be reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Known {
    pub(crate) pid: i32,
    started: u64,
}

impl Known {
    /// A process for a test that never signals it: `pid`, started at `at`.
    #[cfg(test)]
    pub(crate) fn for_tests(pid: i32, at: u64) -> Self {
        Self { pid, started: at }
    }
}

fn sys_pid(pid: i32) -> Option<Pid> {
    u32::try_from(pid).ok().map(Pid::from_u32)
}

fn plain_pid(pid: Pid) -> Option<i32> {
    i32::try_from(pid.as_u32()).ok()
}

/// The processes `pids` names, as they are now.
fn system_with(pids: &[Pid]) -> Option<System> {
    let mut system = System::new().ok()?;
    system.refresh_processes_specifics(
        ProcessesToUpdate::Some(pids),
        true,
        ProcessRefreshKind::nothing(),
    );
    Some(system)
}

fn running(process: &Process) -> bool {
    process.status() != ProcessStatus::Zombie
}

/// Whether a process has this pid, or `None` when the list cannot be read.
#[cfg(windows)]
pub(crate) fn exists(pid: i32) -> Option<bool> {
    let Some(pid) = sys_pid(pid) else {
        return Some(false);
    };
    Some(system_with(&[pid])?.process(pid).is_some())
}

/// Whether a process with this pid runs now and started no later than `at`
/// (seconds since the Unix epoch, rounded down as the start time is). A pid
/// taken over by a later process fails, and so does one whose start time
/// cannot be read (it reads as 0). Rounding to the second leaves a pid reused
/// within the same second undetected.
#[cfg(windows)]
pub(crate) fn started_by(pid: i32, at: u64) -> bool {
    let Some(pid) = sys_pid(pid) else {
        return false;
    };
    system_with(&[pid])
        .as_ref()
        .and_then(|system| system.process(pid))
        .is_some_and(|process| {
            let started = process.start_time();
            running(process) && started != 0 && started <= at
        })
}

/// The parent of `pid`, while `pid` exists and its parent is still the
/// process that started it.
pub(crate) fn parent_of(pid: i32) -> Option<i32> {
    let pid = sys_pid(pid)?;
    let system = system_with(&[pid])?;
    let child = system.process(pid)?;
    let parent = child.parent()?;
    let started = child.start_time();
    let system = system_with(&[parent])?;
    let parent_started = system.process(parent)?.start_time();
    if parent_started > started {
        // The parent ended and its pid went to a later process.
        return None;
    }
    plain_pid(parent)
}

/// Every descendant of `root` now, not `root` itself.
pub(crate) fn descendants(root: i32) -> Vec<Known> {
    let Ok(mut system) = System::new() else {
        return Vec::new();
    };
    system.refresh_processes_specifics(ProcessesToUpdate::All, true, ProcessRefreshKind::nothing());
    let table: Vec<(i32, i32, u64)> = system
        .processes()
        .iter()
        .filter_map(|(pid, process)| {
            Some((
                plain_pid(*pid)?,
                plain_pid(process.parent()?)?,
                process.start_time(),
            ))
        })
        .collect();
    let Some(root_started) = sys_pid(root)
        .and_then(|pid| system.process(pid))
        .map(Process::start_time)
    else {
        return Vec::new();
    };
    let mut found: Vec<Known> = Vec::new();
    let mut parents = vec![(root, root_started)];
    while let Some((parent, parent_started)) = parents.pop() {
        for (pid, ppid, started) in &table {
            let is_child = *ppid == parent && *started >= parent_started;
            if is_child && *pid > 1 && *pid != root && found.iter().all(|known| known.pid != *pid) {
                parents.push((*pid, *started));
                found.push(Known {
                    pid: *pid,
                    started: *started,
                });
            }
        }
    }
    found
}

/// Whether the listed process still runs.
pub(crate) fn alive(known: Known) -> bool {
    let Some(pid) = sys_pid(known.pid) else {
        return false;
    };
    system_with(&[pid])
        .as_ref()
        .and_then(|system| system.process(pid))
        .is_some_and(|process| running(process) && process.start_time() == known.started)
}

/// Kill it (SIGKILL; `TerminateProcess` on Windows), if it is still the listed
/// process. Failure means it just ended, which is the goal.
pub(crate) fn kill(known: Known) {
    let Some(pid) = sys_pid(known.pid) else {
        return;
    };
    if let Some(system) = system_with(&[pid])
        && let Some(process) = system.process(pid)
        && process.start_time() == known.started
    {
        let _ = process.kill();
    }
}
