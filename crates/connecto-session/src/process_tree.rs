//! The process tree: a process's parent, its descendants, whether one is still
//! the process that was listed, and killing it.
//!
//! macOS reads the BSD process info (libproc), as before. Other platforms,
//! Windows among them, use `sysinfo`'s safe process list. A process is known by
//! its pid and its start time, because a pid can be reused.

#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "macos")]
pub(crate) use macos::{Known, alive, descendants, kill, parent_of};

#[cfg(not(target_os = "macos"))]
mod listed;
#[cfg(not(target_os = "macos"))]
pub(crate) use listed::{Known, alive, descendants, kill, parent_of};
#[cfg(windows)]
pub(crate) use listed::{exists, started_by};

/// Whether `pid` is `root` or descends from it, following at most `depth`
/// parents.
pub(crate) fn descends_from(pid: i32, root: i32, depth: usize) -> bool {
    let mut current = pid;
    for _ in 0..depth {
        if current == root {
            return true;
        }
        if current <= 1 {
            return false;
        }
        match parent_of(current) {
            Some(parent) => current = parent,
            None => return false,
        }
    }
    false
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    use super::*;

    /// A child that waits until it is killed.
    fn sleeper() -> std::process::Child {
        #[cfg(unix)]
        let mut command = Command::new("/bin/sleep");
        #[cfg(unix)]
        command.arg("30");
        #[cfg(windows)]
        let mut command = Command::new("powershell.exe");
        #[cfg(windows)]
        command.args([
            "-NoLogo",
            "-NoProfile",
            "-Command",
            "Start-Sleep -Seconds 30",
        ]);
        command
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap()
    }

    #[test]
    fn a_child_is_a_descendant_and_is_killed() {
        let me = i32::try_from(std::process::id()).unwrap();
        let mut child = sleeper();
        let pid = i32::try_from(child.id()).unwrap();
        assert_eq!(parent_of(pid), Some(me));
        assert!(descends_from(pid, me, 64));
        assert!(!descends_from(me, pid, 64));
        let listed = descendants(me);
        let known = listed
            .iter()
            .copied()
            .find(|known| known.pid == pid)
            .unwrap();
        assert!(alive(known));
        kill(known);
        child.wait().unwrap();
        drop(child);
        let deadline = Instant::now() + Duration::from_secs(5);
        while alive(known) && Instant::now() < deadline {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert!(!alive(known));
    }
}
