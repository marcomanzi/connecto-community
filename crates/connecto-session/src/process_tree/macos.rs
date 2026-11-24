//! macOS side of [`crate::process_tree`]: the BSD process info.

use libproc::bsd_info::BSDInfo;
use libproc::proc_pid::pidinfo;
use libproc::processes::{ProcFilter, pids_by_type};
use nix::sys::signal::{Signal, kill as send_signal};
use nix::unistd::Pid;

/// `SZOMB`: dead, and only waiting for its parent to reap it.
const ZOMBIE: u32 = 5;

/// One process, known by its start time too, as a PID can be reused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Known {
    pub(crate) pid: i32,
    started: (u64, u64),
}

impl Known {
    /// A process for a test that never signals it: `pid`, started at `at`.
    #[cfg(test)]
    pub(crate) fn for_tests(pid: i32, at: u64) -> Self {
        Self {
            pid,
            started: (at, 0),
        }
    }
}

fn info(pid: i32) -> Option<BSDInfo> {
    pidinfo::<BSDInfo>(pid, 0).ok()
}

/// The parent of `pid`, while `pid` exists.
pub(crate) fn parent_of(pid: i32) -> Option<i32> {
    info(pid).and_then(|info| i32::try_from(info.pbi_ppid).ok())
}

/// Every descendant of `root` now, not `root` itself.
pub(crate) fn descendants(root: i32) -> Vec<Known> {
    let Ok(pids) = pids_by_type(ProcFilter::All) else {
        return Vec::new();
    };
    let table: Vec<(i32, BSDInfo)> = pids
        .into_iter()
        .filter_map(|pid| {
            let pid = i32::try_from(pid).ok()?;
            Some((pid, info(pid)?))
        })
        .collect();
    let mut found: Vec<Known> = Vec::new();
    let mut parents = vec![root];
    while let Some(parent) = parents.pop() {
        for (pid, process) in &table {
            let is_child = i32::try_from(process.pbi_ppid).is_ok_and(|ppid| ppid == parent);
            if is_child && *pid > 1 && found.iter().all(|known| known.pid != *pid) {
                parents.push(*pid);
                found.push(Known {
                    pid: *pid,
                    started: (process.pbi_start_tvsec, process.pbi_start_tvusec),
                });
            }
        }
    }
    found
}

/// Whether the listed process still runs.
pub(crate) fn alive(known: Known) -> bool {
    info(known.pid).is_some_and(|process| {
        process.pbi_status != ZOMBIE
            && (process.pbi_start_tvsec, process.pbi_start_tvusec) == known.started
    })
}

/// SIGKILL. Failure means it just ended, which is the goal.
pub(crate) fn kill(known: Known) {
    let _ = send_signal(Pid::from_raw(known.pid), Signal::SIGKILL);
}
