//! A per-project lease that keeps a terminal's start and the project's package
//! install from overlapping, so a terminal never sees a missing or half-written
//! `.claude/`, `.codex/` or `.agents/` while it is being started.
//!
//! A launch holds a shared [`Permit`] from before the tool is started until the
//! process is spawned; an install holds the exclusive [`Lease`]. They exclude
//! each other, and a launch also waits while the project's first install is
//! still pending. The app turns the gate on for its data folder before the
//! workspace starts; the workspace calls [`expect`] for each project as it sets
//! it up; the packages manager takes the lease for each install. With the gate
//! off (tests, a static shell) nothing waits.

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use std::sync::{Condvar, Mutex, MutexGuard, PoisonError};
use std::time::{Duration, Instant};

/// The longest a terminal's start is held.
pub const LIMIT: Duration = Duration::from_secs(30);

#[derive(Default)]
struct Slot {
    /// The project's install has not run yet, or is running.
    pending: bool,
    /// Launches between "permit" and "process spawned".
    launchers: usize,
    /// An install holds the lease.
    installing: bool,
    /// Installs waiting for the launches in progress. While any waits, no new
    /// launch is granted, so the installer cannot be starved.
    queued: usize,
}

#[derive(Default)]
struct Gate {
    projects: BTreeMap<String, Slot>,
}

static GATES: Mutex<BTreeMap<PathBuf, Gate>> = Mutex::new(BTreeMap::new());
static CHANGED: Condvar = Condvar::new();

fn gates() -> MutexGuard<'static, BTreeMap<PathBuf, Gate>> {
    GATES.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Turn the gate on for this data folder.
pub fn enable(data_root: &Path) {
    gates().entry(data_root.to_owned()).or_default();
}

/// Turn it off and release every waiting launch.
pub fn disable(data_root: &Path) {
    gates().remove(data_root);
    CHANGED.notify_all();
}

/// A project is set up and its packages are not installed yet: hold its
/// launches. A no-op while the gate is off.
pub fn expect(data_root: &Path, project_id: &str) {
    if let Some(gate) = gates().get_mut(data_root) {
        gate.projects
            .entry(project_id.to_owned())
            .or_default()
            .pending = true;
    }
}

/// The project's install is done (or there is none): release its launches.
pub fn finish(data_root: &Path, project_id: &str) {
    if let Some(slot) = gates()
        .get_mut(data_root)
        .and_then(|gate| gate.projects.get_mut(project_id))
    {
        slot.pending = false;
    }
    CHANGED.notify_all();
}

/// Whether the project's first install, or an install now, is still to come.
#[must_use]
pub fn is_pending(data_root: &Path, project_id: &str) -> bool {
    gates()
        .get(data_root)
        .and_then(|gate| gate.projects.get(project_id))
        .is_some_and(|slot| slot.pending)
}

/// Whether a package install, or a terminal's start, is under way in any
/// project of this data folder: an update restart waits for it.
#[must_use]
pub fn busy(data_root: &Path) -> bool {
    gates().get(data_root).is_some_and(|gate| {
        gate.projects
            .values()
            .any(|slot| slot.installing || slot.launchers > 0)
    })
}

/// A launch's shared hold on a project. Dropped once the process is spawned.
pub struct Permit {
    data_root: PathBuf,
    project: String,
    active: bool,
}

impl Drop for Permit {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(slot) = gates()
            .get_mut(&self.data_root)
            .and_then(|gate| gate.projects.get_mut(&self.project))
        {
            slot.launchers = slot.launchers.saturating_sub(1);
        }
        CHANGED.notify_all();
    }
}

/// An install's exclusive hold on a project. Dropping it releases the project:
/// its pending install is over.
pub struct Lease {
    data_root: PathBuf,
    project: String,
    active: bool,
}

impl Drop for Lease {
    fn drop(&mut self) {
        if !self.active {
            return;
        }
        if let Some(slot) = gates()
            .get_mut(&self.data_root)
            .and_then(|gate| gate.projects.get_mut(&self.project))
        {
            slot.installing = false;
            slot.pending = false;
        }
        CHANGED.notify_all();
    }
}

/// Wait for the right to launch in this project: no install running or still
/// pending. `None` if `limit` passes first. With the gate off, always granted.
#[must_use]
pub fn permit(data_root: &Path, project_id: &str, limit: Duration) -> Option<Permit> {
    let deadline = Instant::now() + limit;
    let mut table = gates();
    loop {
        let Some(gate) = table.get_mut(data_root) else {
            return Some(Permit {
                data_root: data_root.to_owned(),
                project: project_id.to_owned(),
                active: false,
            });
        };
        let slot = gate.projects.entry(project_id.to_owned()).or_default();
        if !slot.pending && !slot.installing && slot.queued == 0 {
            slot.launchers += 1;
            return Some(Permit {
                data_root: data_root.to_owned(),
                project: project_id.to_owned(),
                active: true,
            });
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            return None;
        }
        table = CHANGED
            .wait_timeout(table, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

/// How many installs are waiting for this project's launches to finish.
#[must_use]
pub fn queued(data_root: &Path, project_id: &str) -> usize {
    gates()
        .get(data_root)
        .and_then(|gate| gate.projects.get(project_id))
        .map_or(0, |slot| slot.queued)
}

/// Wait for the right to install in this project: no launch between permit and
/// spawn, no other install. `None` if `limit` passes first (try again later).
///
/// From the moment it starts waiting the install is queued, and no new launch
/// is granted until it has had its turn: the launches already holding a permit
/// finish, then the install runs, then launches resume. A launch waiting for
/// the first install (`pending`) holds no permit, so the two never wait on each
/// other.
#[must_use]
pub fn lease(data_root: &Path, project_id: &str, limit: Duration) -> Option<Lease> {
    let deadline = Instant::now() + limit;
    let mut table = gates();
    let mut waiting = false;
    loop {
        let Some(gate) = table.get_mut(data_root) else {
            return Some(Lease {
                data_root: data_root.to_owned(),
                project: project_id.to_owned(),
                active: false,
            });
        };
        let slot = gate.projects.entry(project_id.to_owned()).or_default();
        if slot.launchers == 0 && !slot.installing {
            slot.installing = true;
            slot.pending = true;
            if waiting {
                slot.queued = slot.queued.saturating_sub(1);
            }
            return Some(Lease {
                data_root: data_root.to_owned(),
                project: project_id.to_owned(),
                active: true,
            });
        }
        if !waiting {
            slot.queued += 1;
            waiting = true;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() {
            slot.queued = slot.queued.saturating_sub(1);
            drop(table);
            // Launches held back by this install may go on.
            CHANGED.notify_all();
            return None;
        }
        table = CHANGED
            .wait_timeout(table, left)
            .unwrap_or_else(PoisonError::into_inner)
            .0;
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Arc;
    use std::sync::mpsc;
    use std::thread;

    use super::*;

    const SHORT: Duration = Duration::from_millis(40);
    const LONG: Duration = Duration::from_secs(10);

    fn root(name: &str) -> PathBuf {
        PathBuf::from(format!("/gate-test/{name}"))
    }

    #[test]
    fn nothing_waits_while_the_gate_is_off() {
        let data = root("off");
        expect(&data, "p");
        assert!(!is_pending(&data, "p"));
        assert!(permit(&data, "p", SHORT).is_some());
        assert!(lease(&data, "p", SHORT).is_some());
    }

    #[test]
    fn a_launch_waits_for_the_first_install_and_others_do_not() {
        let data = root("first");
        enable(&data);
        expect(&data, "p");
        assert!(permit(&data, "p", SHORT).is_none(), "held until installed");
        assert!(
            permit(&data, "other", SHORT).is_some(),
            "other projects are free"
        );
        finish(&data, "p");
        assert!(permit(&data, "p", SHORT).is_some());
        disable(&data);
    }

    #[test]
    fn a_launch_and_an_install_never_overlap() {
        let data = root("exclusive");
        enable(&data);
        // A launch is between permit and spawn: the install cannot start.
        let launch = permit(&data, "p", SHORT).unwrap();
        assert!(
            lease(&data, "p", SHORT).is_none(),
            "install waits for the spawn"
        );
        drop(launch);
        // The install runs: a launch cannot start.
        let install = lease(&data, "p", SHORT).unwrap();
        assert!(is_pending(&data, "p"));
        assert!(
            permit(&data, "p", SHORT).is_none(),
            "launch waits for the install"
        );
        assert!(lease(&data, "p", SHORT).is_none(), "one install at a time");
        drop(install);
        assert!(!is_pending(&data, "p"));
        let launch = permit(&data, "p", SHORT).unwrap();
        let second = permit(&data, "p", SHORT).unwrap();
        drop((launch, second));
        disable(&data);
    }

    #[test]
    fn waiters_are_woken_in_order_by_the_other_side_letting_go() {
        let data = Arc::new(root("wake"));
        enable(&data);
        let install = lease(&data, "p", SHORT).unwrap();
        let (tx, rx) = mpsc::channel();
        let launcher = {
            let (data, tx) = (Arc::clone(&data), tx.clone());
            thread::spawn(move || {
                let permit = permit(&data, "p", LONG).unwrap();
                tx.send("launch").unwrap();
                thread::sleep(Duration::from_millis(50));
                tx.send("spawned").unwrap();
                drop(permit);
            })
        };
        assert!(
            rx.recv_timeout(Duration::from_millis(100)).is_err(),
            "launch is held"
        );
        drop(install);
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "launch");
        // A new install now waits until the launch has spawned.
        let installer = {
            let (data, tx) = (Arc::clone(&data), tx);
            thread::spawn(move || {
                let lease = lease(&data, "p", LONG).unwrap();
                tx.send("install").unwrap();
                drop(lease);
            })
        };
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "spawned");
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "install");
        launcher.join().unwrap();
        installer.join().unwrap();
        disable(&data);
    }

    #[test]
    fn a_queued_install_holds_back_newer_launches_and_still_gets_its_turn() {
        let data = Arc::new(root("writer-first"));
        enable(&data);
        let first = permit(&data, "p", SHORT).unwrap();
        let (tx, rx) = mpsc::channel();
        let installer = {
            let (data, tx) = (Arc::clone(&data), tx);
            thread::spawn(move || {
                let lease = lease(&data, "p", LONG).unwrap();
                tx.send("leased").unwrap();
                // Held until the test lets go of it.
                tx.send("holding").unwrap();
                rx_wait_release(&data);
                drop(lease);
            })
        };
        // The installer is queued behind the launch already in progress.
        while queued(&data, "p") == 0 {
            thread::yield_now();
        }
        assert!(
            permit(&data, "p", SHORT).is_none(),
            "a newer launch must not slip past the waiting install"
        );
        assert!(
            rx.try_recv().is_err(),
            "the install waits for the first launch"
        );
        drop(first);
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "leased");
        assert_eq!(rx.recv_timeout(LONG).unwrap(), "holding");
        assert!(permit(&data, "p", SHORT).is_none(), "and not while it runs");
        RELEASE.store(true, std::sync::atomic::Ordering::SeqCst);
        installer.join().unwrap();
        assert_eq!(queued(&data, "p"), 0);
        assert!(
            permit(&data, "p", SHORT).is_some(),
            "launches resume afterwards"
        );
        disable(&data);
    }

    static RELEASE: std::sync::atomic::AtomicBool = std::sync::atomic::AtomicBool::new(false);

    fn rx_wait_release(_: &Path) {
        while !RELEASE.load(std::sync::atomic::Ordering::SeqCst) {
            thread::yield_now();
        }
    }

    #[test]
    fn an_install_succeeds_while_launches_keep_coming() {
        use std::sync::atomic::{AtomicBool, Ordering};
        let data = Arc::new(root("starve"));
        enable(&data);
        let stop = Arc::new(AtomicBool::new(false));
        let launchers: Vec<_> = (0..4)
            .map(|_| {
                let (data, stop) = (Arc::clone(&data), Arc::clone(&stop));
                thread::spawn(move || {
                    while !stop.load(Ordering::SeqCst) {
                        // Short, overlapping launches.
                        if let Some(held) = permit(&data, "p", Duration::from_millis(5)) {
                            thread::sleep(Duration::from_millis(2));
                            drop(held);
                        }
                    }
                })
            })
            .collect();
        thread::sleep(Duration::from_millis(20));
        let got = lease(&data, "p", LONG);
        assert!(got.is_some(), "the installer must not starve");
        drop(got);
        stop.store(true, Ordering::SeqCst);
        for launcher in launchers {
            launcher.join().unwrap();
        }
        assert_eq!(queued(&data, "p"), 0);
        disable(&data);
    }

    #[test]
    fn an_install_that_gives_up_unblocks_the_launches_it_held_back() {
        let data = Arc::new(root("giveup"));
        enable(&data);
        let held = permit(&data, "p", SHORT).unwrap();
        let waiter = {
            let data = Arc::clone(&data);
            thread::spawn(move || permit(&data, "p", LONG).is_some())
        };
        let installer = {
            let data = Arc::clone(&data);
            thread::spawn(move || lease(&data, "p", Duration::from_millis(60)).is_none())
        };
        assert!(installer.join().unwrap(), "the install timed out");
        assert_eq!(queued(&data, "p"), 0, "its mark is cleared");
        assert!(
            waiter.join().unwrap(),
            "and the launch it held back goes on"
        );
        drop(held);
        disable(&data);
    }

    #[test]
    fn turning_the_gate_off_releases_everyone() {
        let data = Arc::new(root("disable"));
        enable(&data);
        expect(&data, "p");
        let waiter = {
            let data = Arc::clone(&data);
            thread::spawn(move || permit(&data, "p", LONG).is_some())
        };
        thread::sleep(Duration::from_millis(50));
        disable(&data);
        assert!(waiter.join().unwrap_or(false));
    }
}
