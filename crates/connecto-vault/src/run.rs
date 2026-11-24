//! Running one allowed program with a secret: `connecto secret run NAME --
//! PROGRAM [ARGS]`.
//!
//! The app starts the program itself, so the value is in the program's
//! environment and in no process of the terminal. The program must be one the
//! secret's command binding names (compared after resolving links), it runs
//! in the terminal's project folder (or the folder the terminal asked for,
//! when that is inside it), with a short environment, no input, and a time
//! limit. Its output comes back with every vault value hidden, in every form
//! the redactor knows. A program that prints the value in another form can
//! still reveal it: the person is told so when allowing one.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

use connecto_core::{Binding, RunOutput, SecretCaller};

use crate::Vault;
use connecto_core::VaultBridge as _;

/// Longest a program runs.
pub const TIME_LIMIT: Duration = Duration::from_secs(10 * 60);
/// Most output kept of each stream; more is read and dropped.
const MAX_OUTPUT: usize = 1024 * 1024;
const POLL: Duration = Duration::from_millis(50);

fn command_binding(binding: &Binding) -> Option<(Vec<String>, String)> {
    match binding {
        Binding::Command { programs, env } => Some((programs.clone(), env.clone())),
        _ => None,
    }
}

/// The program to run, if `asked` is one of `allowed` once links are resolved.
pub(crate) fn allowed_program(asked: &str, allowed: &[String]) -> Result<PathBuf, String> {
    let asked_path = Path::new(asked);
    if !asked_path.is_absolute() {
        return Err(format!("{asked} is not a full path."));
    }
    let real = std::fs::canonicalize(asked_path)
        .map_err(|_| format!("{asked} is not a program on this machine."))?;
    let ok = allowed.iter().any(|allow| {
        Path::new(allow) == asked_path
            || std::fs::canonicalize(allow).is_ok_and(|allowed| allowed == real)
    });
    if ok {
        Ok(real)
    } else {
        Err(format!(
            "{asked} is not a program this secret may be given to. Allowed: {}.",
            allowed.join(", ")
        ))
    }
}

/// The folder to run in: `asked` when it is inside the project, else the
/// project.
fn folder(caller: &SecretCaller, asked: Option<&Path>) -> Result<PathBuf, String> {
    let project = caller
        .project_folder
        .as_ref()
        .and_then(|p| std::fs::canonicalize(p).ok())
        .ok_or("This terminal has no project folder to run in.")?;
    Ok(asked
        .and_then(|asked| std::fs::canonicalize(asked).ok())
        .filter(|asked| asked.starts_with(&project))
        .unwrap_or(project))
}

/// Read a stream to its end, keeping at most [`MAX_OUTPUT`] bytes.
fn drain(mut from: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0_u8; 8192];
        loop {
            match from.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = MAX_OUTPUT.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        kept
    })
}

/// Hide every vault value, and this one in every form, in program output.
fn clean(bytes: Vec<u8>, value: &str) -> Vec<u8> {
    let bytes = crate::proxy::hide_value(bytes, value);
    match String::from_utf8(bytes) {
        Ok(text) => connecto_redact::hide_known(&text).into_bytes(),
        Err(err) => err.into_bytes(),
    }
}

pub(crate) fn run(
    vault: &Vault,
    caller: &SecretCaller,
    name: &str,
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
) -> Result<RunOutput, String> {
    run_with_limit(vault, caller, name, program, args, cwd, TIME_LIMIT)
}

pub(crate) fn run_with_limit(
    vault: &Vault,
    caller: &SecretCaller,
    name: &str,
    program: &str,
    args: &[String],
    cwd: Option<&Path>,
    limit: Duration,
) -> Result<RunOutput, String> {
    // Checked before the value is taken out.
    let entry = vault
        .visible(caller.project.as_deref())
        .into_iter()
        .find(|e| e.name == name)
        .ok_or_else(|| format!("There is no secret named {name} for this terminal."))?;
    let (programs, _) = entry
        .bindings
        .iter()
        .find_map(command_binding)
        .ok_or_else(|| format!("{name} is not for running a program."))?;
    let real = allowed_program(program, &programs)?;
    let dir = folder(caller, cwd)?;
    let (entry, (_, env), value) =
        vault.use_secret(caller, name, "running a program", command_binding)?;

    let mut command = Command::new(&real);
    command
        .args(args)
        .current_dir(&dir)
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    for key in ["PATH", "HOME", "USER", "LOGNAME", "LANG", "TMPDIR"] {
        if let Some(val) = std::env::var_os(key) {
            command.env(key, val);
        }
    }
    command.env("TERM", "dumb").env(&env, &value);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own group, so a time limit stops what it started too.
        command.process_group(0);
    }
    let mut child = command
        .spawn()
        .map_err(|err| format!("{} could not start: {err}", real.display()))?;
    vault.log_use(caller, &entry, "command", &real.display().to_string());
    let stdout = child.stdout.take().map(drain);
    let stderr = child.stderr.take().map(drain);
    let started = Instant::now();
    let (code, timed_out) = loop {
        match child.try_wait() {
            Ok(Some(status)) => break (status.code(), false),
            Ok(None) if started.elapsed() >= limit => {
                #[cfg(unix)]
                if let Ok(pid) = i32::try_from(child.id()) {
                    let _ = nix::sys::signal::killpg(
                        nix::unistd::Pid::from_raw(pid),
                        nix::sys::signal::Signal::SIGKILL,
                    );
                }
                let _ = child.kill();
                let _ = child.wait();
                break (None, true);
            }
            Ok(None) => thread::sleep(POLL),
            Err(err) => return Err(format!("{} could not be waited for: {err}", real.display())),
        }
    };
    let out = stdout
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    let err = stderr
        .map(|h| h.join().unwrap_or_default())
        .unwrap_or_default();
    Ok(RunOutput {
        code,
        stdout: clean(out, &value),
        stderr: clean(err, &value),
        timed_out,
    })
}

#[cfg(all(test, unix))]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_program_is_allowed_only_by_its_resolved_path() {
        let dir = tempfile::tempdir().unwrap();
        let real = dir.path().join("tool");
        std::fs::write(&real, "").unwrap();
        let link = dir.path().join("link");
        std::os::unix::fs::symlink(&real, &link).unwrap();
        let real_s = real.to_string_lossy().into_owned();
        let link_s = link.to_string_lossy().into_owned();
        assert!(allowed_program(&link_s, std::slice::from_ref(&real_s)).is_ok());
        assert!(allowed_program(&real_s, std::slice::from_ref(&link_s)).is_ok());
        assert!(allowed_program("tool", std::slice::from_ref(&real_s)).is_err());
        assert!(allowed_program("/bin/sh", std::slice::from_ref(&real_s)).is_err());
    }
}
