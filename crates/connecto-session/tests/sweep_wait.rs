//! Alone in its binary: `wait_for_sweeps` counts every sweep in the process, so
//! a sibling test's child that ignores SIGTERM would hold it open past the limit.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::time::{Duration, Instant};

use common::shell;

#[test]
fn a_stop_with_nothing_left_over_does_not_wait_out_the_grace_period() {
    let session = shell("/bin/sleep 60");
    session.terminate();
    assert!(session.wait_exit(Duration::from_secs(2)));
    let start = Instant::now();
    assert!(connecto_session::wait_for_sweeps(Duration::from_secs(4)));
    assert!(start.elapsed() < Duration::from_secs(2));
}
