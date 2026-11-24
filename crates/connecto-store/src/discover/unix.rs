//! macOS (and other Unix) side of discovery: the login shell's `PATH` and the
//! folders the usual installers use.

use std::io::Read;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::OnceLock;
use std::time::{Duration, Instant};

use nix::sys::signal::{Signal, killpg};
use nix::unistd::Pid;

use super::{newest_first, push_absolute_unique};

/// How long the login shell may take to say its `PATH`.
const LOGIN_SHELL_WAIT: Duration = Duration::from_secs(3);
const BEGIN: &str = "__CONNECTO_PATH_BEGIN__";
const END: &str = "__CONNECTO_PATH_END__";

/// The folders a login shell would search. Asked until the shell answers,
/// then kept for the process: a shell that was too slow once (a busy
/// machine, a heavy startup file) is asked again at the next lookup, so one
/// miss does not leave the whole run without the user's search path.
#[must_use]
pub fn login_path() -> &'static [PathBuf] {
    static PATH: OnceLock<Vec<PathBuf>> = OnceLock::new();
    static ASKING: std::sync::Mutex<()> = std::sync::Mutex::new(());
    if let Some(path) = PATH.get() {
        return path;
    }
    // One question at a time; whoever waited here finds the answer.
    let _one = ASKING
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if let Some(path) = PATH.get() {
        return path;
    }
    let shell = std::env::var_os("SHELL")
        .map(PathBuf::from)
        .filter(|shell| shell.is_absolute() && shell.is_file())
        .unwrap_or_else(|| PathBuf::from("/bin/zsh"));
    match ask_shell(&shell, LOGIN_SHELL_WAIT) {
        Some(path) => PATH.get_or_init(|| path),
        None => &[],
    }
}

/// The most of the shell's output that is kept; a shell that says more than a
/// `PATH` could be is not believed.
const MAX_OUTPUT: usize = 64 * 1024;

/// `shell -i -l -c 'printf PATH'`, all of it within `wait`: the output is read
/// while the shell runs, and the shell's whole process group is killed at the
/// end, so a child that keeps the pipe open cannot hold anything up.
fn ask_shell(shell: &Path, wait: Duration) -> Option<Vec<PathBuf>> {
    let deadline = Instant::now() + wait;
    let mut child = Command::new(shell)
        .args(["-i", "-l", "-c"])
        .arg(format!("printf '%s' \"{BEGIN}${{PATH}}{END}\""))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        // Its own group, so that an interactive shell does not wait for a
        // terminal and everything it starts can be ended together.
        .process_group(0)
        .spawn()
        .ok()?;
    let group = Pid::from_raw(i32::try_from(child.id()).ok()?);
    let mut pipe = child.stdout.take()?;
    // The output is drained at once, on a thread, whatever the shell does.
    let (tx, rx) = std::sync::mpsc::channel::<Option<Vec<u8>>>();
    let reader = std::thread::Builder::new()
        .name("login-path-read".to_owned())
        .spawn(move || {
            let mut kept = Vec::new();
            let mut chunk = [0_u8; 4096];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) | Err(_) => break,
                    Ok(n) => {
                        kept.extend_from_slice(&chunk[..n]);
                        if kept.len() > MAX_OUTPUT {
                            let _ = tx.send(None);
                            return;
                        }
                    }
                }
            }
            let _ = tx.send(Some(kept));
        })
        .ok()?;
    let ended = |child: &mut std::process::Child| {
        let _ = killpg(group, Signal::SIGKILL);
        let _ = child.kill();
        let _ = child.wait();
    };
    // The shell itself ends, or the time is up.
    let exited_well = loop {
        match child.try_wait() {
            Ok(Some(status)) => break status.success(),
            Err(_) => break false,
            Ok(None) if Instant::now() >= deadline => break false,
            Ok(None) => std::thread::sleep(Duration::from_millis(10)),
        }
    };
    // Whatever it left running goes with it; that also closes the pipe.
    ended(&mut child);
    let output = if exited_well {
        rx.recv_timeout(
            deadline
                .saturating_duration_since(Instant::now())
                .max(Duration::from_millis(200)),
        )
        .ok()
        .flatten()
    } else {
        None
    };
    // The reader ends now that the pipe is closed; never wait for it for long.
    drop(rx);
    if reader.is_finished() {
        let _ = reader.join();
    }
    parse_marked_path(&String::from_utf8_lossy(&output?))
}

/// The folders between the markers (the shell may print other things around them).
fn parse_marked_path(out: &str) -> Option<Vec<PathBuf>> {
    let from = out.rfind(BEGIN)? + BEGIN.len();
    let to = from + out[from..].find(END)?;
    Some(split_path(&out[from..to]))
}

/// Absolute folders of a `PATH` value, in order, without repeats.
#[must_use]
pub fn split_path(value: &str) -> Vec<PathBuf> {
    let mut out: Vec<PathBuf> = Vec::new();
    for part in value.split(':') {
        push_absolute_unique(&mut out, PathBuf::from(part));
    }
    out
}

/// Where the usual installers put global programs, under `home`.
pub fn known_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs: Vec<PathBuf> = [
        ".bun/bin",
        // Where Pi's installer puts `pi` when no folder of the search path
        // will take it.
        ".pi/agent/bin",
        // Where OpenCode's installer puts `opencode`.
        ".opencode/bin",
        ".cargo/bin",
        ".volta/bin",
        ".volta/tools/image/packages/@openai/codex/bin",
        ".npm-global/bin",
        ".yarn/bin",
        ".config/yarn/global/node_modules/.bin",
        "Library/pnpm",
        ".local/share/pnpm",
    ]
    .iter()
    .map(|rel| home.join(rel))
    .collect();
    // Node version managers: one folder per node version, the newest first.
    for (root, bin) in [
        (".nvm/versions/node", "bin"),
        (
            "Library/Application Support/fnm/node-versions",
            "installation/bin",
        ),
        (".local/share/fnm/node-versions", "installation/bin"),
        (".local/share/mise/installs/node", "bin"),
        (".asdf/installs/nodejs", "bin"),
    ] {
        dirs.extend(
            newest_first(&home.join(root))
                .into_iter()
                .map(|v| v.join(bin)),
        );
    }
    if machine_wide(home) {
        dirs.push(PathBuf::from("/opt/homebrew/opt/node/bin"));
    }
    dirs
}

/// Whether the folders outside `home` (Homebrew's, `/usr/local/bin`) are
/// searched: only for the user's real home. Any other home (a test, a trial
/// build) sees only what is under it, so what is installed on the machine
/// running it cannot change the answer. Same rule as `ambient_path`.
fn machine_wide(home: &Path) -> bool {
    crate::platform::home_dir().is_some_and(|real| real == home)
}

/// The fixed folders, searched last.
pub fn fixed_dirs(home: &Path) -> Vec<PathBuf> {
    let mut dirs = vec![home.join(".local/bin")];
    if machine_wide(home) {
        dirs.extend([
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
        ]);
    }
    dirs
}

/// The system's own folders, the end of every [`super::run_path`].
pub fn system_dirs() -> Vec<PathBuf> {
    ["/usr/bin", "/bin", "/usr/sbin", "/sbin"]
        .map(PathBuf::from)
        .to_vec()
}

/// [`system_dirs`] as one `PATH` value, for when they cannot be joined.
pub const SYSTEM_PATH: &str = "/usr/bin:/bin:/usr/sbin:/sbin";

/// The file names under which a program called `name` is looked for.
pub fn executable_names(name: &str) -> Vec<String> {
    vec![name.to_owned()]
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    fn tree(name: &str) -> PathBuf {
        let root =
            std::env::temp_dir().join(format!("connecto-discover-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        std::fs::canonicalize(root).unwrap()
    }

    /// Homebrew's and `/usr/local/bin` are the real home's only: a test home
    /// never finds the agents installed on the machine running it.
    #[test]
    fn the_machines_folders_are_searched_for_the_real_home_only() {
        let fake = tree("machine-wide");
        let machine = [
            PathBuf::from("/opt/homebrew/bin"),
            PathBuf::from("/usr/local/bin"),
            PathBuf::from("/opt/homebrew/opt/node/bin"),
        ];
        let fake_dirs: Vec<PathBuf> = fixed_dirs(&fake)
            .into_iter()
            .chain(known_dirs(&fake))
            .collect();
        assert!(fake_dirs.contains(&fake.join(".local/bin")));
        for dir in &machine {
            assert!(!fake_dirs.contains(dir), "{dir:?} for a test home");
        }
        if let Some(home) = crate::platform::home_dir() {
            let real: Vec<PathBuf> = fixed_dirs(&home)
                .into_iter()
                .chain(known_dirs(&home))
                .collect();
            for dir in &machine {
                assert!(real.contains(dir), "{dir:?} for the real home");
            }
        }
    }

    #[test]
    fn the_shells_path_is_read_between_markers_and_cleaned() {
        let out = format!("noise\n{BEGIN}/a/bin:relative:/b/bin:/a/bin::{END}trailing");
        assert_eq!(
            parse_marked_path(&out).unwrap(),
            [PathBuf::from("/a/bin"), PathBuf::from("/b/bin")]
        );
        assert!(parse_marked_path("no markers").is_none());
    }

    #[test]
    fn a_shell_that_prints_its_path_is_believed_and_a_slow_one_is_not_waited_for() {
        let root = tree("shell");
        let good = root.join("good-shell");
        std::fs::write(
            &good,
            format!("#!/bin/sh\nprintf '%s' '{BEGIN}/x/bin:/y/bin{END}'\n"),
        )
        .unwrap();
        let slow = root.join("slow-shell");
        std::fs::write(&slow, "#!/bin/sh\nsleep 60\n").unwrap();
        for shell in [&good, &slow] {
            std::fs::set_permissions(shell, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        assert_eq!(
            ask_shell(&good, Duration::from_secs(5)).unwrap(),
            [PathBuf::from("/x/bin"), PathBuf::from("/y/bin")]
        );
        let began = Instant::now();
        assert!(ask_shell(&slow, Duration::from_millis(300)).is_none());
        assert!(began.elapsed() < Duration::from_secs(5));
        assert!(ask_shell(&root.join("missing"), Duration::from_secs(1)).is_none());
    }

    fn script(path: &Path, body: &str) {
        std::fs::write(path, format!("#!/bin/sh\n{body}\n")).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn a_child_that_keeps_the_output_open_cannot_hold_discovery_up() {
        let root = tree("inherit");
        let shell = root.join("shell");
        // Prints the path, exits at once, and leaves a child holding stdout.
        script(
            &shell,
            &format!("sleep 60 &\nprintf '%s' '{BEGIN}/x/bin{END}'\nexit 0"),
        );
        let began = Instant::now();
        let got = ask_shell(&shell, Duration::from_secs(10));
        assert!(
            began.elapsed() < Duration::from_secs(8),
            "{:?}",
            began.elapsed()
        );
        assert_eq!(got.unwrap(), [PathBuf::from("/x/bin")]);
    }

    #[test]
    fn a_child_that_never_lets_go_and_a_shell_that_never_ends_are_both_cut_off() {
        let root = tree("forever");
        let hang = root.join("hang");
        script(&hang, "sleep 60 &\nsleep 60");
        let began = Instant::now();
        assert!(ask_shell(&hang, Duration::from_millis(500)).is_none());
        assert!(
            began.elapsed() < Duration::from_secs(8),
            "{:?}",
            began.elapsed()
        );
        // Exits with a failure while a child holds the pipe.
        let fail = root.join("fail");
        script(&fail, "sleep 60 &\nexit 1");
        let began = Instant::now();
        assert!(ask_shell(&fail, Duration::from_secs(10)).is_none());
        assert!(
            began.elapsed() < Duration::from_secs(8),
            "{:?}",
            began.elapsed()
        );
    }

    #[test]
    fn output_far_bigger_than_a_path_is_refused_without_waiting() {
        let root = tree("huge");
        let shell = root.join("shell");
        script(
            &shell,
            "yes aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa | head -c 10000000",
        );
        let began = Instant::now();
        assert!(ask_shell(&shell, Duration::from_secs(3)).is_none());
        assert!(began.elapsed() < Duration::from_secs(3));
    }
}
