//! While a remote peer has a terminal's size, the local view cannot change it,
//! and the local view's size comes back when the peer lets go (ADR 0017).

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use connecto_core::{GridSize, ResizeOutcome, SnapshotOutcome, TermSnapshot, TerminalSession};
use connecto_session::{LiveSession, spec};

fn session(size: GridSize) -> LiveSession {
    let mut spec = spec("/bin/cat", vec![], Path::new("/tmp"));
    spec.size = size;
    LiveSession::start(spec, Arc::new(|| {})).unwrap()
}

fn grid(s: &LiveSession) -> GridSize {
    let mut snap = TermSnapshot::default();
    for _ in 0..200 {
        if s.poll_snapshot(&mut snap) == SnapshotOutcome::Updated {
            break;
        }
        std::thread::sleep(Duration::from_millis(2));
    }
    GridSize {
        cols: snap.cols,
        rows: snap.rows,
    }
}

fn wait_for(s: &LiveSession, want: GridSize) {
    let deadline = Instant::now() + Duration::from_secs(5);
    while Instant::now() < deadline {
        let g = grid(s);
        if g == want {
            return;
        }
        std::thread::sleep(Duration::from_millis(5));
    }
    let got = grid(s);
    panic!("wanted {want:?}, the grid is {got:?}");
}

const LOCAL: GridSize = GridSize { cols: 80, rows: 24 };
const REMOTE: GridSize = GridSize {
    cols: 120,
    rows: 40,
};

#[test]
fn the_local_view_cannot_resize_while_a_peer_has_the_size() {
    let s = session(LOCAL);
    s.set_remote_size_control(true);
    assert_eq!(s.resize_remote(REMOTE), ResizeOutcome::Applied);
    wait_for(&s, REMOTE);
    // The window's layout asks for another size: it is told yes, and nothing moves.
    assert_eq!(
        s.resize(GridSize { cols: 60, rows: 20 }),
        ResizeOutcome::Applied
    );
    std::thread::sleep(Duration::from_millis(50));
    assert_eq!(grid(&s), REMOTE);
    s.terminate();
}

#[test]
fn letting_go_gives_the_local_view_its_last_size_back() {
    let s = session(LOCAL);
    s.set_remote_size_control(true);
    s.resize_remote(REMOTE);
    wait_for(&s, REMOTE);
    s.resize(GridSize { cols: 60, rows: 20 });
    s.set_remote_size_control(false);
    wait_for(&s, GridSize { cols: 60, rows: 20 });
    s.terminate();
}

#[test]
fn with_no_local_change_the_size_it_started_with_comes_back() {
    let s = session(LOCAL);
    s.set_remote_size_control(true);
    s.resize_remote(REMOTE);
    wait_for(&s, REMOTE);
    s.set_remote_size_control(false);
    wait_for(&s, LOCAL);
    s.terminate();
}

#[test]
fn without_a_peer_the_local_view_resizes_as_always() {
    let s = session(LOCAL);
    assert_eq!(
        s.resize(GridSize {
            cols: 100,
            rows: 30
        }),
        ResizeOutcome::Applied
    );
    wait_for(
        &s,
        GridSize {
            cols: 100,
            rows: 30,
        },
    );
    s.terminate();
}

#[test]
fn a_click_in_the_local_view_takes_the_size_back_until_the_peer_resizes() {
    let s = session(LOCAL);
    s.set_remote_size_control(true);
    s.resize_remote(REMOTE);
    wait_for(&s, REMOTE);
    let wish = GridSize { cols: 60, rows: 20 };
    s.resize(wish);
    assert_eq!(s.local_size_claims(), 0);

    s.claim_local_size();
    assert_eq!(s.local_size_claims(), 1);
    wait_for(&s, wish);
    // The local view's layout drives it again.
    s.resize(LOCAL);
    wait_for(&s, LOCAL);

    // The peer takes it back at its next resize.
    s.set_remote_size_control(true);
    s.resize_remote(REMOTE);
    wait_for(&s, REMOTE);
    s.terminate();
}

#[test]
fn a_click_when_no_peer_has_the_size_changes_nothing() {
    let s = session(LOCAL);
    s.claim_local_size();
    assert_eq!(s.local_size_claims(), 0);
    wait_for(&s, LOCAL);
    s.terminate();
}
