//! Codex hook trust (section 5.10).
//!
//! Codex refuses to run a hook until it is trusted, and without trust it raises
//! a full-screen review modal that swallows typed input. The launch avoids the
//! modal by passing `-c hooks.state={"<key>"={trusted_hash="<hash>"}}`, and the
//! hash changes with the hook's definition. So it is read, per helper path and
//! Codex version, by asking `codex app-server` for `hooks/list`.
//!
//! The read runs in an empty `CODEX_HOME` in a run folder (Check 2: it writes
//! database files, and the hash is the same with an empty home), so it never
//! touches a profile. Launches that arrive during a read share its answer
//! (single-flight). A good answer is cached for the life of the app. **A failure
//! is cached only for [`FAILURE_WINDOW`]:** after that the next Codex launch reads
//! again, so one slow start does not turn hooks off for every later terminal.
//! The read has no project input, so there is no "not trusted yet" state here:
//! every failure is a read that could not be made (see `read`).

use std::collections::BTreeMap;
use std::fs;
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, PoisonError};
use std::time::{Duration, Instant};

use connecto_adapter::{hook_config, hook_trust_state};
use crossbeam_channel::{RecvTimeoutError, unbounded};
use serde_json::{Value, json};

/// The whole read, from spawn to answer.
pub const TRUST_READ_LIMIT: Duration = Duration::from_secs(15);

type Answer = Result<String, String>;

/// How long a failed read answers the launches that come after it, before the
/// next launch reads again. A burst of launches while Codex is slow does one read.
pub const FAILURE_WINDOW: Duration = Duration::from_secs(60);

/// The one trust answer per process: a good answer is kept, a failure for
/// [`FAILURE_WINDOW`], and a read in progress is shared by the launches that
/// arrive during it.
static READS: TrustReads = TrustReads::new(FAILURE_WINDOW);

type Key = (String, PathBuf);

/// One read in progress: the answer, once the reading launch has it.
struct Flight {
    answer: Mutex<Option<Answer>>,
    done: Condvar,
}

impl Flight {
    fn finish(&self, answer: Answer) {
        *self.answer.lock().unwrap_or_else(PoisonError::into_inner) = Some(answer);
        self.done.notify_all();
    }

    fn wait(&self) -> Answer {
        let mut answer = self.answer.lock().unwrap_or_else(PoisonError::into_inner);
        loop {
            if let Some(found) = answer.as_ref() {
                return found.clone();
            }
            answer = self
                .done
                .wait(answer)
                .unwrap_or_else(PoisonError::into_inner);
        }
    }
}

enum Slot {
    Good(String),
    Failed { at: Instant, why: String },
    Reading(Arc<Flight>),
}

/// The trust answers of one process, by Codex version and helper. Its lock is
/// never held across a read: a read runs once per key, and the launches that
/// arrive meanwhile wait for its answer.
pub struct TrustReads {
    slots: Mutex<BTreeMap<Key, Slot>>,
    /// How long a failure answers before the next read; set per instance so
    /// tests do not share it.
    window: Duration,
}

impl TrustReads {
    const fn new(window: Duration) -> Self {
        Self {
            slots: Mutex::new(BTreeMap::new()),
            window,
        }
    }

    /// An instance with its own failure window, for tests.
    #[cfg(test)]
    fn with_window(window: Duration) -> Self {
        Self {
            slots: Mutex::new(BTreeMap::new()),
            window,
        }
    }

    fn get(&self, key: Key, read: impl FnOnce() -> Answer) -> Answer {
        let flight = {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            match slots.get(&key) {
                Some(Slot::Good(value)) => return Ok(value.clone()),
                Some(Slot::Failed { at, why }) if at.elapsed() < self.window => {
                    return Err(why.clone());
                }
                Some(Slot::Reading(flight)) => {
                    let flight = Arc::clone(flight);
                    drop(slots);
                    return flight.wait();
                }
                _ => {}
            }
            let flight = Arc::new(Flight {
                answer: Mutex::new(None),
                done: Condvar::new(),
            });
            slots.insert(key.clone(), Slot::Reading(Arc::clone(&flight)));
            flight
        };
        // The read runs with no lock held. If it panics, the waiters get a
        // failure rather than waiting forever.
        let answer = std::panic::catch_unwind(std::panic::AssertUnwindSafe(read))
            .unwrap_or_else(|_| Err("the trust read panicked".to_owned()));
        {
            let mut slots = self.slots.lock().unwrap_or_else(PoisonError::into_inner);
            let slot = match &answer {
                Ok(value) => Slot::Good(value.clone()),
                Err(why) => Slot::Failed {
                    at: Instant::now(),
                    why: why.clone(),
                },
            };
            slots.insert(key, slot);
        }
        flight.finish(answer.clone());
        answer
    }
}

static SCRATCH_ID: AtomicU64 = AtomicU64::new(0);

/// Read the hooks `codex` lists for `helper`, check every expected one is
/// there, and build the trust value from key and `currentHash`.
fn read(codex: &Path, helper: &Path, scratch: &Path) -> Answer {
    fs::create_dir_all(scratch).map_err(|err| format!("no scratch folder: {err}"))?;
    let mut command = Command::new(codex);
    // The helper path in the form the launch gives it (no `\\?\` on
    // Windows): the hash covers the command text.
    let helper = crate::platform::child_cwd(helper);
    for value in hook_config(&helper).map_err(|err| err.to_string())? {
        command.arg("-c").arg(value);
    }
    command
        .args(["app-server", "--listen", "stdio://"])
        .env_clear()
        .envs(crate::platform::codex_trust_env(scratch))
        .current_dir(scratch)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null());
    let mut child = crate::platform::no_console(&mut command)
        .spawn()
        .map_err(|err| format!("could not run codex app-server: {err}"))?;
    let result = converse(&mut child, scratch);
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn converse(child: &mut std::process::Child, cwd: &Path) -> Answer {
    let mut stdin = child.stdin.take().ok_or("no stdin")?;
    let stdout = child.stdout.take().ok_or("no stdout")?;
    let (lines, incoming) = unbounded::<String>();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if lines.send(line).is_err() {
                return;
            }
        }
    });
    let deadline = Instant::now() + TRUST_READ_LIMIT;
    let mut send = |value: Value| -> Result<(), String> {
        writeln!(stdin, "{value}").map_err(|err| format!("codex app-server closed: {err}"))?;
        stdin.flush().map_err(|err| err.to_string())
    };
    let wait_for = |id: u64| -> Result<Value, String> {
        loop {
            let left = deadline.saturating_duration_since(Instant::now());
            match incoming.recv_timeout(left) {
                Ok(line) => {
                    if let Ok(value) = serde_json::from_str::<Value>(&line)
                        && value.get("id").and_then(Value::as_u64) == Some(id)
                    {
                        return Ok(value);
                    }
                }
                Err(RecvTimeoutError::Timeout) => {
                    return Err(format!(
                        "codex app-server did not answer in {} s",
                        TRUST_READ_LIMIT.as_secs()
                    ));
                }
                Err(RecvTimeoutError::Disconnected) => {
                    return Err("codex app-server ended without an answer".to_owned());
                }
            }
        }
    };
    send(json!({
        "id": 1, "method": "initialize",
        "params": { "clientInfo": { "name": "connecto", "version": "0" } }
    }))?;
    wait_for(1)?;
    send(json!({ "method": "initialized" }))?;
    send(json!({
        "id": 2, "method": "hooks/list",
        "params": { "cwds": [cwd] }
    }))?;
    let reply = wait_for(2)?;
    let listed = reply
        .pointer("/result/data/0/hooks")
        .and_then(Value::as_array)
        .ok_or("hooks/list gave no hooks")?;
    let hooks: Vec<(&str, Option<&str>)> = listed
        .iter()
        .filter_map(|hook| {
            let key = hook.get("key").and_then(Value::as_str)?;
            Some((key, hook.get("currentHash").and_then(Value::as_str)))
        })
        .collect();
    hook_trust_state(&hooks)
}

/// The trust value for `codex` at `version` with this `helper`, or why there is
/// none. Shares one `hooks/list` read per pair: a good answer is kept for the app
/// run, a failure for [`FAILURE_WINDOW`].
pub fn trust_state(codex: &Path, version: &str, helper: &Path, run_root: &Path) -> Answer {
    trust_state_in(&READS, codex, version, helper, run_root)
}

fn trust_state_in(
    reads: &TrustReads,
    codex: &Path,
    version: &str,
    helper: &Path,
    run_root: &Path,
) -> Answer {
    let key = (version.to_owned(), helper.to_owned());
    reads.get(key, || {
        // Each read has its own folder, so reads of different keys can run at once.
        let id = SCRATCH_ID.fetch_add(1, Ordering::Relaxed);
        let scratch = run_root.join(format!("trust-{}-{id}", std::process::id()));
        let answer = read(codex, helper, &scratch);
        let _ = fs::remove_dir_all(&scratch);
        answer
    })
}

/// The fake `codex` is a `/bin/sh` script, so the tests that run it are Unix
/// only; the shared-read tests run everywhere.
#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    #[cfg(unix)]
    use std::os::unix::fs::PermissionsExt;

    use super::*;

    /// A stand-in for `codex app-server` that answers the two requests and
    /// counts how often it ran.
    #[cfg(unix)]
    fn fake_codex(root: &Path, hash: &str, broken: bool) -> PathBuf {
        fs::create_dir_all(root).unwrap();
        let counter = root.join("runs");
        let script = root.join("codex");
        let reply = if broken {
            "exit 1".to_owned()
        } else {
            format!(
                r#"
read init
echo '{{"id":1,"result":{{}}}}'
read initialized
read list
echo '{{"id":2,"result":{{"data":[{{"hooks":[{{"key":"/<session-flags>/config.toml:session_start:0:0","currentHash":"{hash}1"}},{{"key":"/<session-flags>/config.toml:user_prompt_submit:0:0","currentHash":"{hash}2"}}]}}]}}}}'
sleep 1
"#
            )
        };
        fs::write(
            &script,
            format!("#!/bin/sh\necho x >> '{}'\n{reply}\n", counter.display()),
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        script
    }

    #[cfg(unix)]
    fn runs(root: &Path) -> usize {
        fs::read_to_string(root.join("runs")).map_or(0, |text| text.lines().count())
    }

    #[cfg(unix)]
    fn scratch(name: &str) -> PathBuf {
        let root = std::env::temp_dir().join(format!("mg-trust-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&root);
        root
    }

    /// Each test has its own `TrustReads`, so no test sees another's answers.
    fn reads(window: Duration) -> TrustReads {
        TrustReads::with_window(window)
    }

    /// `hooks/list` runs once per launch burst and a good answer is kept.
    #[cfg(unix)]
    #[test]
    fn the_hashes_are_read_once_and_become_a_trust_value() {
        let root = scratch("ok");
        let codex = fake_codex(&root, "sha256:abc", false);
        let helper = Path::new("/data/cli/bin/connecto-hook-ok");
        let reads = reads(FAILURE_WINDOW);
        let first = trust_state_in(&reads, &codex, "0.166.0", helper, &root.join("run")).unwrap();
        assert_eq!(
            first,
            r#"hooks.state={"/<session-flags>/config.toml:session_start:0:0"={trusted_hash="sha256:abc1"},"/<session-flags>/config.toml:user_prompt_submit:0:0"={trusted_hash="sha256:abc2"}}"#
        );
        let second = trust_state_in(&reads, &codex, "0.166.0", helper, &root.join("run")).unwrap();
        assert_eq!(second, first);
        assert_eq!(runs(&root), 1, "the second launch hit the cache");
        let _ = fs::remove_dir_all(&root);
    }

    /// A failure answers the launches inside its window, and a launch after the
    /// window reads again.
    #[cfg(unix)]
    #[test]
    fn a_failed_read_answers_briefly_then_is_read_again() {
        let root = scratch("broken");
        let codex = fake_codex(&root, "", true);
        let helper = Path::new("/data/cli/bin/connecto-hook-broken");
        let reads = reads(Duration::from_millis(300));
        assert!(trust_state_in(&reads, &codex, "0.166.0", helper, &root.join("run")).is_err());
        assert!(trust_state_in(&reads, &codex, "0.166.0", helper, &root.join("run")).is_err());
        assert_eq!(runs(&root), 1, "inside the window the failure answers");
        std::thread::sleep(Duration::from_millis(400));
        assert!(trust_state_in(&reads, &codex, "0.166.0", helper, &root.join("run")).is_err());
        assert_eq!(
            runs(&root),
            2,
            "after the window the next launch reads again"
        );
        let _ = fs::remove_dir_all(&root);
    }

    /// Launches that arrive while one read is in progress share its answer: one
    /// read, and the wait is about one read, not one read per launch.
    #[test]
    fn concurrent_launches_share_one_read() {
        let reads = reads(FAILURE_WINDOW);
        let key = ("0.166.0".to_owned(), PathBuf::from("/h/slow"));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        let started = Instant::now();
        let answers: Vec<Answer> = std::thread::scope(|scope| {
            let handles: Vec<_> = (0..8)
                .map(|_| {
                    let key = key.clone();
                    let reads = &reads;
                    let calls = &calls;
                    scope.spawn(move || {
                        reads.get(key, || {
                            calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                            std::thread::sleep(Duration::from_millis(400));
                            Err("slow read".to_owned())
                        })
                    })
                })
                .collect();
            handles.into_iter().map(|h| h.join().unwrap()).collect()
        });
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 1);
        assert!(
            answers
                .iter()
                .all(|a| matches!(a, Err(why) if why == "slow read"))
        );
        assert!(
            started.elapsed() < Duration::from_millis(1500),
            "{:?}",
            started.elapsed()
        );
    }

    /// A read with no failure window is read again by every launch.
    #[test]
    fn with_no_window_every_launch_reads() {
        let reads = reads(Duration::ZERO);
        let key = ("0.166.0".to_owned(), PathBuf::from("/h/none"));
        let calls = std::sync::atomic::AtomicUsize::new(0);
        for _ in 0..3 {
            let _ = reads.get(key.clone(), || {
                calls.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                Err("no".to_owned())
            });
        }
        assert_eq!(calls.load(std::sync::atomic::Ordering::SeqCst), 3);
    }

    #[cfg(unix)]
    #[test]
    fn a_missing_hook_in_the_answer_is_a_failure() {
        let root = scratch("missing");
        fs::create_dir_all(&root).unwrap();
        let script = root.join("codex");
        fs::write(
            &script,
            "#!/bin/sh\nread a\necho '{\"id\":1,\"result\":{}}'\nread b\nread c\necho '{\"id\":2,\"result\":{\"data\":[{\"hooks\":[]}]}}'\nsleep 1\n",
        )
        .unwrap();
        fs::set_permissions(&script, fs::Permissions::from_mode(0o755)).unwrap();
        let answer = trust_state_in(
            &reads(FAILURE_WINDOW),
            &script,
            "0.166.0",
            Path::new("/h/missing"),
            &root.join("run"),
        );
        assert!(answer.unwrap_err().contains("did not list"));
        let _ = fs::remove_dir_all(&root);
    }
}
