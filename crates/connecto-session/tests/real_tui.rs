//! Real full-screen programs driven through the PTY and the emulator.
//!
//! Evidence class REAL for terminal behaviour: real binaries, no window. These
//! prove rendering into the grid and key delivery, not what the window paints.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use common::{program, screen_text, wait_for_exit, wait_for_screen, wait_for_text};
use connecto_core::{
    GridSize, InputModes, Key, KeyPress, Mods, ResizeOutcome, SessionStatus, TerminalSession,
    encode_key, encode_paste,
};

const SIZE: GridSize = GridSize { cols: 80, rows: 24 };

fn press(session: &impl TerminalSession, key: Key, modes: InputModes) {
    let bytes = encode_key(
        KeyPress {
            key,
            mods: Mods::default(),
        },
        modes,
    )
    .unwrap();
    session.send_input(bytes).unwrap();
}

fn vim() -> connecto_session::LiveSession {
    program("/usr/bin/vim", &["-u", "NONE", "-N", "-n"], SIZE)
}

#[test]
fn real_vim_enters_the_alternate_screen_and_exits_cleanly() {
    let session = vim();
    let snapshot = wait_for_text(&session, "VIM - Vi IMproved");
    assert!(snapshot.modes.alt_screen);

    session.send_input(b"ihello from pty\x1b".to_vec()).unwrap();
    let snapshot = wait_for_text(&session, "hello from pty");
    assert_eq!(
        snapshot.row(1)[0].ch,
        '~',
        "vim draws tildes below the text"
    );

    session.send_input(b":q!\r".to_vec()).unwrap();
    assert_eq!(
        wait_for_exit(&session),
        SessionStatus::Exited {
            code: 0,
            signal: None
        }
    );
}

#[test]
fn real_vim_receives_arrow_keys_under_its_own_cursor_mode() {
    let session = vim();
    wait_for_text(&session, "VIM - Vi IMproved");
    session.send_input(b"iabc\rdef\x1b".to_vec()).unwrap();
    let snapshot = wait_for_text(&session, "def");
    let modes = snapshot.modes;

    press(&session, Key::Up, modes);
    session.send_input(b"rX".to_vec()).unwrap();
    let snapshot = wait_for_screen(&session, "the replaced letter", |snapshot| {
        screen_text(snapshot).contains('X')
    });
    let text = screen_text(&snapshot);
    assert_eq!(text.lines().nth(1), Some("def"), "{text}");
    assert!(text.lines().next().unwrap().contains('X'), "{text}");
}

#[test]
fn real_vim_redraws_after_a_resize() {
    let session = vim();
    wait_for_text(&session, "VIM - Vi IMproved");
    let size = GridSize { cols: 60, rows: 12 };
    assert_eq!(session.resize(size), ResizeOutcome::Applied);
    session.send_input(b":set columns?\r".to_vec()).unwrap();
    wait_for_text(&session, "columns=60");
}

#[test]
fn real_vim_accepts_a_bracketed_paste_as_literal_text() {
    let session = vim();
    wait_for_text(&session, "VIM - Vi IMproved");
    session.send_input(b"i".to_vec()).unwrap();
    let snapshot = wait_for_text(&session, "-- INSERT --");
    assert!(
        snapshot.modes.bracketed_paste,
        "vim enables bracketed paste"
    );
    session
        .send_input(encode_paste("pasted line", snapshot.modes))
        .unwrap();
    wait_for_text(&session, "pasted line");
}

#[test]
fn real_top_renders_and_quits_on_a_key() {
    let session = program("/usr/bin/top", &["-s", "1"], SIZE);
    let snapshot = wait_for_text(&session, "Processes:");
    assert!(screen_text(&snapshot).contains("PID"));
    session.send_input(b"q".to_vec()).unwrap();
    assert!(matches!(
        wait_for_exit(&session),
        SessionStatus::Exited { code: 0, .. }
    ));
}

#[test]
fn real_less_pages_with_keys() {
    let session = program("/usr/bin/less", &["/etc/services"], SIZE);
    wait_for_text(&session, "Network services");
    session.send_input(b"G".to_vec()).unwrap();
    wait_for_screen(&session, "the end of the file", |snapshot| {
        screen_text(snapshot).contains("(END)")
    });
    session.send_input(b"q".to_vec()).unwrap();
    wait_for_exit(&session);
}
