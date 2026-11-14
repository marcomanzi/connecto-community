//! Killing what Stop's signals missed.
//!
//! A descendant in its own process group escapes the group signal, and once the
//! root dies it is reparented and can no longer be found. So its descendants are
//! listed before the first signal, and any still there at the grace deadline are
//! killed. A process that detached before Stop was pressed cannot be found at
//! all: that is disclosed, not chased.

use std::sync::atomic::{AtomicUsize, Ordering};
use std::thread;
use std::time::{Duration, Instant};

use crate::process_tree::{alive, descendants, kill};

const POLL: Duration = Duration::from_millis(50);

/// Sweeps under way. Process exit must wait for them.
static ACTIVE: AtomicUsize = AtomicUsize::new(0);

/// Marks a sweep as under way until dropped.
struct Active;

impl Active {
    fn new() -> Self {
        ACTIVE.fetch_add(1, Ordering::SeqCst);
        Self
    }
}

impl Drop for Active {
    fn drop(&mut self) {
        ACTIVE.fetch_sub(1, Ordering::SeqCst);
    }
}

/// List `root`'s descendants, run `signal_root`, then kill what is left of them
/// at `grace`. Returns at once when none are left. Blocking, so use a thread.
pub(crate) fn sweep(root: i32, grace: Duration, signal_root: impl FnOnce()) {
    let _active = Active::new();
    let listed = descendants(root);
    signal_root();
    let deadline = Instant::now() + grace;
    while Instant::now() < deadline {
        if !listed.iter().copied().any(alive) {
            return;
        }
        thread::sleep(POLL);
    }
    for known in listed.into_iter().filter(|known| alive(*known)) {
        kill(known);
    }
}

/// Wait up to `timeout` for sweeps under way. True if none are left.
#[must_use]
pub fn wait_for_sweeps(timeout: Duration) -> bool {
    let deadline = Instant::now() + timeout;
    while ACTIVE.load(Ordering::SeqCst) > 0 {
        if Instant::now() >= deadline {
            return false;
        }
        thread::sleep(POLL);
    }
    true
}
