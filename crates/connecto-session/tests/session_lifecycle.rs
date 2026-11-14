//! Session lifecycle against real child processes in real PTYs.

#![cfg(unix)]
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

mod common;

use std::path::{Path, PathBuf};
use std::thread;
use std::time::{Duration, Instant};

use common::{
    TIMEOUT, no_wake, process_exists, screen_text, shell, wait_for_exit, wait_for_screen,
    wait_for_text,
};
use connecto_core::{GridSize, InputRefused, ResizeOutcome, SessionStatus, TerminalSession};
use connecto_pty::{PtyError, SIGKILL_GRACE};
use connecto_session::{LiveSession, shell_spec};
use nix::sys::signal::{Signal, kill};
use nix::unistd::{Pid, getpgid};

#[test]
fn a_query_in_a_stalled_update_is_answered_with_no_ui_running() {
    let dir = std::env::temp_dir().join(format!("connecto-sync-{}", std::process::id()));
    std::fs::create_dir_all(&dir).unwrap();
    let child = dir.join("child.py");
    let result = dir.join("result");
    std::fs::write(&child, common::SYNC_QUERY_CHILD).unwrap();
    // Quoted: inside a Connecto terminal the temp folder is under
    // "Application Support", and the space split the path in two.
    let command = format!(
        "/usr/bin/python3 '{}' '{}'",
        child.display(),
        result.display()
    );
    let session = shell(&command);
    let (ms, reply) = common::sync_query_result(&result);
    drop(session);
    let _ = std::fs::remove_dir_all(&dir);
    assert!(reply.contains('R'), "no cursor report: {reply}");
    assert!(ms <= common::SYNC_REPLY_LIMIT_MS, "took {ms} ms");
}

#[test]
fn child_output_reaches_the_screen() {
    let session = shell("echo pty-hello");
    wait_for_text(&session, "pty-hello");
}

#[test]
fn keystrokes_reach_the_child() {
    let session = shell("read line; echo got:$line");
    session.send_input(b"typed\r".to_vec()).unwrap();
    wait_for_text(&session, "got:typed");
}

#[test]
fn a_normal_exit_code_is_reported() {
    let session = shell("exit 7");
    let status = wait_for_exit(&session);
    assert_eq!(
        status,
        SessionStatus::Exited {
            code: 7,
            signal: None
        }
    );
}

#[test]
fn death_by_signal_is_reported() {
    let session = shell("kill -KILL $$");
    match wait_for_exit(&session) {
        SessionStatus::Exited {
            signal: Some(signal),
            ..
        } => assert!(signal.to_lowercase().contains("kill"), "{signal}"),
        other => panic!("expected a signal death, got {other:?}"),
    }
}

#[test]
fn the_child_leads_its_own_process_group() {
    let session = shell("sleep 30");
    let pid = session.pid().unwrap();
    assert_eq!(getpgid(Some(Pid::from_raw(pid))).unwrap().as_raw(), pid);
    assert_ne!(pid, getpgid(None).unwrap().as_raw());
}

#[test]
fn the_child_environment_is_only_the_allowlist() {
    // Set by cargo for this test process, so it would leak if inherited.
    assert!(std::env::var_os("CARGO_PKG_NAME").is_some());
    let session = shell("echo \"[$CARGO_PKG_NAME]\"; echo term=$TERM; echo done");
    let snapshot = wait_for_text(&session, "done");
    let text = screen_text(&snapshot);
    assert!(text.contains("[]"), "{text}");
    assert!(text.contains("term=xterm-256color"), "{text}");
}

#[test]
fn a_child_sees_no_variables_beyond_the_allowlist() {
    let session = shell("env | cut -d= -f1 | sort | tr '\\n' ' '; echo; echo finished");
    let snapshot = wait_for_text(&session, "finished");
    let text = screen_text(&snapshot).replace('\n', "");
    let allowed = [
        "HOME",
        "USER",
        "LOGNAME",
        "SHELL",
        "PATH",
        "TMPDIR",
        "LANG",
        "LC_ALL",
        "LC_CTYPE",
        "TERM",
        "COLORTERM", // set by the shell itself:
        "PWD",
        "OLDPWD",
        "SHLVL",
        "_",
    ];
    let names = text.split("finished").next().unwrap();
    for name in names.split_whitespace() {
        assert!(allowed.contains(&name), "unexpected variable {name}");
    }
}

#[test]
fn broken_pipes_behave_normally_in_the_child() {
    // SIGPIPE must not be inherited as ignored, or `yes` would spin forever.
    let session = shell("yes | head -1; echo pipe-ok");
    wait_for_text(&session, "pipe-ok");
}

#[test]
fn resize_reaches_the_kernel_and_the_child() {
    let session = shell("trap 'stty size; exit' WINCH; echo armed; while :; do sleep 0.05; done");
    wait_for_text(&session, "armed");
    let size = GridSize { cols: 91, rows: 33 };
    assert_eq!(session.resize(size), ResizeOutcome::Applied);
    let snapshot = wait_for_text(&session, "33 91");
    assert_eq!((snapshot.cols, snapshot.rows), (91, 33));
}

#[test]
fn output_written_just_before_exit_is_not_lost() {
    let session = shell("seq 1 2000 | tail -3; exit 0");
    wait_for_exit(&session);
    wait_for_text(&session, "2000");
}

#[test]
fn a_dead_session_refuses_input_instead_of_blocking() {
    let session = shell("exit 0");
    wait_for_exit(&session);
    assert_eq!(session.send_input(b"x".to_vec()), Err(InputRefused::Closed));
}

#[test]
fn exit_status_is_reported_while_a_descendant_keeps_the_pty_open() {
    let session = shell("(sleep 3 &) ; exit 4");
    let started = Instant::now();
    let status = wait_for_exit(&session);
    assert!(started.elapsed() < Duration::from_secs(2));
    assert_eq!(
        status,
        SessionStatus::Exited {
            code: 4,
            signal: None
        }
    );
}

#[test]
fn killing_one_session_leaves_the_other_working() {
    let victim = shell("sleep 30");
    let survivor = shell("while read line; do echo echo:$line; done");
    kill(Pid::from_raw(victim.pid().unwrap()), Signal::SIGKILL).unwrap();
    assert!(matches!(
        wait_for_exit(&victim),
        SessionStatus::Exited { .. }
    ));
    assert_eq!(survivor.status(), SessionStatus::Running);
    survivor.send_input(b"alive\r".to_vec()).unwrap();
    wait_for_text(&survivor, "echo:alive");
}

#[test]
fn a_flooding_session_does_not_delay_another_session() {
    let flood = shell("yes flood");
    let quiet = shell("while read line; do echo echo:$line; done");
    wait_for_text(&flood, "flood");
    let started = Instant::now();
    quiet.send_input(b"ping\r".to_vec()).unwrap();
    wait_for_text(&quiet, "echo:ping");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_flooding_child_can_be_killed_and_its_output_drained() {
    let session = shell("yes flood");
    wait_for_text(&session, "flood");
    session.terminate();
    assert!(matches!(
        wait_for_exit(&session),
        SessionStatus::Exited { .. }
    ));
}

#[test]
fn input_to_a_child_that_never_reads_is_refused_not_blocking() {
    let session = shell("sleep 30");
    let chunk = vec![b'x'; 64 * 1024];
    let started = Instant::now();
    let mut refused = false;
    for _ in 0..64 {
        if session.send_input(chunk.clone()).is_err() {
            refused = true;
            break;
        }
    }
    assert!(refused, "a bounded queue must fill up");
    assert!(started.elapsed() < Duration::from_secs(1));
}

#[test]
fn sigterm_ends_a_normal_child() {
    let session = shell("sleep 30");
    let pid = session.pid().unwrap();
    session.terminate();
    wait_for_exit(&session);
    assert!(!process_exists(pid));
}

#[test]
fn a_child_ignoring_sigterm_is_killed_after_the_grace_period() {
    let session = shell("trap '' TERM HUP; echo armed; while :; do sleep 0.1; done");
    wait_for_text(&session, "armed");
    let started = Instant::now();
    session.terminate();
    assert!(session.wait_exit(SIGKILL_GRACE + TIMEOUT));
    assert!(started.elapsed() >= SIGKILL_GRACE - Duration::from_millis(100));
}

#[test]
fn terminate_returns_at_once_even_for_a_stubborn_child() {
    let session = shell("trap '' TERM HUP; echo armed; while :; do sleep 0.1; done");
    wait_for_text(&session, "armed");
    let started = Instant::now();
    session.terminate();
    assert!(started.elapsed() < Duration::from_millis(100));
}

#[test]
fn dropping_a_session_terminates_its_child() {
    let session = shell("sleep 30");
    let pid = session.pid().unwrap();
    drop(session);
    let deadline = Instant::now() + TIMEOUT;
    while process_exists(pid) && Instant::now() < deadline {
        thread::sleep(Duration::from_millis(20));
    }
    assert!(!process_exists(pid));
}

#[test]
fn a_relative_program_is_rejected() {
    let mut spec = shell_spec(None, Path::new("/tmp"));
    spec.program = PathBuf::from("zsh");
    assert!(matches!(
        LiveSession::start(spec, no_wake()),
        Err(PtyError::RelativeProgram(_))
    ));
}

#[test]
fn a_missing_working_directory_is_rejected_not_replaced_by_home() {
    let spec = shell_spec(None, Path::new("/nonexistent/connecto"));
    assert!(matches!(
        LiveSession::start(spec, no_wake()),
        Err(PtyError::MissingCwd(_))
    ));
}

#[test]
fn a_missing_program_fails_to_start_without_panicking() {
    let mut spec = shell_spec(None, Path::new("/tmp"));
    spec.program = PathBuf::from("/nonexistent/program");
    assert!(LiveSession::start(spec, no_wake()).is_err());
}

#[test]
fn twenty_kills_never_wedge_the_session() {
    for _ in 0..20 {
        let session = shell("yes flood");
        wait_for_screen(&session, "output", |snapshot| {
            screen_text(snapshot).contains("flood")
        });
        kill(Pid::from_raw(session.pid().unwrap()), Signal::SIGKILL).unwrap();
        wait_for_exit(&session);
    }
}

/// In its own process group, so the group signal misses it, and it ignores
/// SIGTERM. Only the list taken before the first signal can still find it.
#[test]
fn stop_kills_a_descendant_that_left_the_group_and_ignored_sigterm() {
    let pid_file = std::env::temp_dir().join(format!("mg-sweep-{}", std::process::id()));
    let _ = std::fs::remove_file(&pid_file);
    let script = format!(
        r#"/usr/bin/python3 -c "
import os,signal,time
if os.fork()==0:
    os.setsid()
    signal.signal(signal.SIGTERM,signal.SIG_IGN)
    signal.signal(signal.SIGHUP,signal.SIG_IGN)
    open('{}.new','w').write(str(os.getpid()))
    os.rename('{}.new','{}')
    time.sleep(60)
else:
    time.sleep(60)
""#,
        pid_file.display(),
        pid_file.display(),
        pid_file.display()
    );
    let session = shell(&script);
    let deadline = Instant::now() + TIMEOUT;
    while !pid_file.exists() {
        assert!(Instant::now() < deadline, "the child never started");
        thread::sleep(Duration::from_millis(20));
    }
    let pid: i32 = std::fs::read_to_string(&pid_file).unwrap().parse().unwrap();
    let _ = std::fs::remove_file(&pid_file);

    session.terminate();
    assert!(
        session.wait_exit(Duration::from_secs(2)),
        "the root ends first"
    );
    assert!(process_exists(pid), "SIGTERM must not have reached it");
    assert!(connecto_session::wait_for_sweeps(
        SIGKILL_GRACE + Duration::from_secs(2)
    ));
    let deadline = Instant::now() + Duration::from_secs(2);
    while process_exists(pid) {
        assert!(Instant::now() < deadline, "the descendant survived Stop");
        thread::sleep(Duration::from_millis(20));
    }
}
