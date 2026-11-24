//! A work type's optional `setup` and `prepare` commands: reviewed argv arrays
//! that Connecto runs, never the agent, in the project root. `setup` runs when
//! the type is published (and again on request) in a terminal the user can
//! watch and answer (see `State::run_setup`); `prepare` runs hidden before
//! each job with the job's prompt on stdin, and its JSON output replaces
//! `{{prepared}}` in the instructions.
use super::{db::Result, parse_json, store::Store};
use connecto_audit::{Audit, Record};
use serde_json::Value;
use std::{
    ffi::OsString,
    io,
    path::{Component, Path, PathBuf},
    process::Stdio,
    time::{Duration, Instant},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    time::{sleep, timeout},
};

pub(super) const PREPARED: &str = "{{prepared}}";
/// The most `prepare` may print.
pub(super) const STDOUT_LIMIT: usize = 256 * 1024;
/// EX_TEMPFAIL: `prepare` reports missing prerequisites, so `setup` runs again.
pub(super) const TEMPFAIL: i32 = 75;
const MAX_TIMEOUT_S: u64 = 1800;
const DEFAULT_TIMEOUT_S: u64 = 600;
const STDERR_KEEP: usize = 64 * 1024;
const STDERR_LINES: usize = 20;
const REASON_CHARS: usize = 4000;
/// The only variables a command inherits: no secret in Connecto's own
/// environment reaches it.
const ENV_ALLOW: [&str; 4] = ["PATH", "HOME", "LANG", "TMPDIR"];
/// Job folders, relative to the project root.
const JOBS: [&str; 2] = [".connecto", "works"];

#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct Command {
    pub argv: Vec<String>,
    pub timeout: Duration,
}

/// Parse and check a reviewed command: `{"argv": [...], "timeout_s": n}`.
pub(super) fn parse(raw: &str) -> Result<Command> {
    let Value::Object(map) = parse_json(raw)? else {
        return Err("A command must be a JSON object with argv and timeout_s.".into());
    };
    if let Some(key) = map
        .keys()
        .find(|k| !matches!(k.as_str(), "argv" | "timeout_s"))
    {
        return Err(format!(
            "Unknown command field {key}: only argv and timeout_s are allowed."
        ));
    }
    let argv = map
        .get("argv")
        .and_then(Value::as_array)
        .ok_or("A command needs an argv array.")?
        .iter()
        .map(|a| {
            // No control characters: setup's argv is typed into a terminal,
            // where one could end the line or send a signal.
            a.as_str()
                .filter(|s| !s.chars().any(char::is_control))
                .map(str::to_owned)
                .ok_or_else(|| {
                    "Each argv element must be a string without control characters.".to_owned()
                })
        })
        .collect::<Result<Vec<_>>>()?;
    let program = argv.first().ok_or("argv must not be empty.")?;
    let path = Path::new(program);
    if program.trim() != program
        || path.file_name().is_none()
        || path.is_absolute()
        || path
            .components()
            .any(|c| !matches!(c, Component::Normal(_) | Component::CurDir))
    {
        return Err(
            "argv[0] must be a program name or a relative path inside the project, without ..."
                .into(),
        );
    }
    let seconds = match map.get("timeout_s") {
        None => DEFAULT_TIMEOUT_S,
        Some(v) => v
            .as_u64()
            .filter(|s| (1..=MAX_TIMEOUT_S).contains(s))
            .ok_or("timeout_s must be a whole number from 1 to 1800.")?,
    };
    Ok(Command {
        argv,
        timeout: Duration::from_secs(seconds),
    })
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Ended {
    Exit(Option<i32>),
    Timeout,
    Cancelled,
}
pub(super) struct Ran {
    pub ended: Ended,
    pub stdout: Vec<u8>,
    pub overflow: bool,
    pub stderr: Vec<u8>,
    pub elapsed: Duration,
}
impl Ran {
    /// Why the run did not succeed, for the user: a sanitized stderr tail.
    fn failure(&self, stage: &str, limit: Duration) -> Option<String> {
        let head = match self.ended {
            Ended::Exit(Some(0)) if self.overflow => {
                format!("{stage} printed more than {} KiB.", STDOUT_LIMIT / 1024)
            }
            Ended::Exit(Some(0)) => return None,
            Ended::Exit(Some(code)) => format!("{stage} exited with code {code}."),
            Ended::Exit(None) => format!("{stage} was ended by a signal."),
            Ended::Timeout => format!("{stage} timed out after {} s.", limit.as_secs()),
            Ended::Cancelled => format!("{stage} cancelled by Stop, Pause, or project close."),
        };
        let tail = stderr_tail(&self.stderr);
        Some(if tail.is_empty() {
            head
        } else {
            format!("{head}\n{tail}")
        })
    }
    fn outcome(&self) -> String {
        match self.ended {
            Ended::Exit(Some(code)) => format!("exit {code}"),
            Ended::Exit(None) => "signal".into(),
            Ended::Timeout => "timeout".into(),
            Ended::Cancelled => "cancelled".into(),
        }
    }
}

/// The last lines of stderr, with control characters and known secrets removed.
fn stderr_tail(bytes: &[u8]) -> String {
    let text = connecto_redact::sanitize(&String::from_utf8_lossy(bytes));
    let lines: Vec<&str> = text.trim_end().lines().collect();
    let tail = lines[lines.len().saturating_sub(STDERR_LINES)..].join("\n");
    let tail = connecto_redact::hide_known(&tail);
    let skip = tail.chars().count().saturating_sub(REASON_CHARS);
    tail.chars().skip(skip).collect()
}

/// Kills the command's whole process group when dropped: on Stop, on timeout,
/// when the waiting future is dropped, and after the command exits (so nothing
/// it started outlives it).
struct Group(#[cfg_attr(not(unix), allow(dead_code))] Option<u32>);
impl Drop for Group {
    fn drop(&mut self) {
        #[cfg(unix)]
        if let Some(pid) = self.0.and_then(|p| i32::try_from(p).ok()) {
            use nix::{sys::signal, unistd::Pid};
            let _ = signal::killpg(Pid::from_raw(pid), signal::Signal::SIGKILL);
        }
    }
}

enum Keep {
    Head(usize),
    Tail(usize),
}
/// Read a pipe to its end, keeping at most `keep` bytes. Draining it all
/// means a chatty command never blocks on a full pipe.
async fn drain(reader: Option<impl AsyncRead + Unpin>, keep: Keep) -> (Vec<u8>, bool) {
    let mut out = Vec::new();
    let mut overflow = false;
    let Some(mut reader) = reader else {
        return (out, overflow);
    };
    let mut buf = [0u8; 8192];
    loop {
        let n = match reader.read(&mut buf).await {
            Ok(0) | Err(_) => break,
            Ok(n) => n,
        };
        match keep {
            Keep::Head(max) => {
                let room = max.saturating_sub(out.len());
                out.extend_from_slice(&buf[..n.min(room)]);
                overflow |= n > room;
            }
            Keep::Tail(max) => {
                out.extend_from_slice(&buf[..n]);
                if out.len() > 2 * max {
                    out.drain(..out.len() - max);
                }
            }
        }
    }
    (out, overflow)
}

/// Run `cmd` in `root` with only the allowlisted environment plus `extra`.
/// `stdin` is written as bytes and closed. Ends at `limit` or when `cancel`
/// turns true, killing the process group. Err only when it cannot start.
pub(super) async fn run(
    cmd: &Command,
    root: &Path,
    stdin: Vec<u8>,
    extra: &[(&str, OsString)],
    limit: Duration,
    cancel: &(dyn Fn() -> bool + Send + Sync),
) -> Result<Ran> {
    let start = Instant::now();
    let name = &cmd.argv[0];
    let program = if Path::new(name).components().count() > 1 {
        root.join(name)
    } else {
        PathBuf::from(name)
    };
    let mut command = tokio::process::Command::new(program);
    command
        .args(&cmd.argv[1..])
        .current_dir(root)
        .env_clear()
        .envs(
            ENV_ALLOW
                .iter()
                .filter_map(|k| Some((*k, std::env::var_os(k)?))),
        )
        .envs(extra.iter().map(|(k, v)| (*k, v)))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(unix)]
    command.process_group(0);
    let mut child = command
        .spawn()
        .map_err(|e| format!("Cannot start {name}: {e}"))?;
    let group = Group(child.id());
    let input = child.stdin.take();
    let writer = tokio::spawn(async move {
        if let Some(mut input) = input {
            let _ = input.write_all(&stdin).await;
        }
    });
    let out = tokio::spawn(drain(child.stdout.take(), Keep::Head(STDOUT_LIMIT)));
    let err = tokio::spawn(drain(child.stderr.take(), Keep::Tail(STDERR_KEEP)));
    let ended = loop {
        tokio::select! {
            status = child.wait() => break Ended::Exit(status.ok().and_then(|s| s.code())),
            () = sleep(Duration::from_millis(100)) => {
                if cancel() {
                    break Ended::Cancelled;
                }
                if start.elapsed() >= limit {
                    break Ended::Timeout;
                }
            }
        }
    };
    let _ = child.start_kill();
    drop(group);
    let _ = child.wait().await;
    writer.abort();
    let grace = Duration::from_secs(2);
    let (stdout, overflow) = timeout(grace, out)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    let (stderr, _) = timeout(grace, err)
        .await
        .ok()
        .and_then(|r| r.ok())
        .unwrap_or_default();
    Ok(Ran {
        ended,
        stdout,
        overflow,
        stderr,
        elapsed: start.elapsed(),
    })
}

/// Runs one work type's commands for one project, journalling each run.
pub(super) struct Runner<'a> {
    pub audit: &'a Audit,
    pub store: &'a Store,
    pub project: &'a str,
    pub kind: &'a str,
    pub root: &'a Path,
}
impl Runner<'_> {
    fn journal(&self, stage: &str, job: Option<i64>, ran: &Result<Ran>) {
        let (outcome, elapsed) = match ran {
            Ok(ran) => (ran.outcome(), ran.elapsed),
            Err(_) => ("not started".into(), Duration::ZERO),
        };
        self.journal_outcome(stage, job, outcome, elapsed);
    }
    /// Journal one run: its exit result and duration, never its output.
    pub fn journal_outcome(
        &self,
        stage: &str,
        job: Option<i64>,
        outcome: String,
        elapsed: Duration,
    ) {
        self.audit.append_nowait(Record {
            actor: "connecto".into(),
            source: "agentic_works".into(),
            target: self.kind.into(),
            action: format!("agentic_works.{stage}"),
            attempt: job.and_then(|id| u64::try_from(id).ok()).unwrap_or(0),
            outcome,
            reason: format!("{} ms", elapsed.as_millis()),
            project: self.project.into(),
            ..Record::default()
        });
    }

    /// Run `setup` hidden; Ok holds the time it succeeded. Used where no
    /// terminal can be typed into (Windows), and by tests.
    #[cfg_attr(all(unix, not(test)), allow(dead_code))]
    pub async fn run_setup(
        &self,
        cmd: &Command,
        limit: Duration,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<String> {
        let limit = cmd.timeout.min(limit);
        let extra = [("CONNECTO_WORK_TYPE", OsString::from(self.kind))];
        let ran = run(cmd, self.root, Vec::new(), &extra, limit, cancel).await;
        self.journal("setup", None, &ran);
        match ran?.failure("Setup", limit) {
            None => Ok(now()),
            Some(failure) => Err(failure),
        }
    }

    /// Run a published type's `setup` hidden and record when it last
    /// succeeded.
    #[cfg_attr(not(test), allow(dead_code))]
    pub async fn setup(
        &self,
        cmd: &Command,
        limit: Duration,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<()> {
        let ran = self.run_setup(cmd, limit, cancel).await;
        self.store.setup_ok(
            self.project,
            self.kind,
            ran.as_ref().ok().map(String::as_str),
        )?;
        ran.map(drop)
    }

    /// Run `prepare` for a job and return its JSON output, compact. Exit 75
    /// runs `setup` (when the type has one) once and `prepare` once more.
    pub async fn prepare<S, F>(
        &self,
        cmd: &Command,
        setup: Option<S>,
        job: &super::db::Job,
        dir: &Path,
        deadline: Instant,
        cancel: &(dyn Fn() -> bool + Send + Sync),
    ) -> Result<String>
    where
        S: FnOnce() -> F,
        F: std::future::Future<Output = Result<()>>,
    {
        let mut setup = setup;
        let extra = [
            ("CONNECTO_JOB_ID", OsString::from(job.id.to_string())),
            ("CONNECTO_JOB_DIR", dir.as_os_str().to_owned()),
            ("CONNECTO_WORK_TYPE", OsString::from(self.kind)),
        ];
        let left = || deadline.saturating_duration_since(Instant::now());
        loop {
            let limit = cmd.timeout.min(left());
            let ran = run(
                cmd,
                self.root,
                job.prompt.as_bytes().to_vec(),
                &extra,
                limit,
                cancel,
            )
            .await;
            self.journal("prepare", Some(job.id), &ran);
            let ran = ran?;
            if ran.ended == Ended::Exit(Some(TEMPFAIL))
                && let Some(setup) = setup.take()
            {
                setup()
                    .await
                    .map_err(|e| format!("Prepare reported missing prerequisites. {e}"))?;
                continue;
            }
            if let Some(failure) = ran.failure("Prepare", limit) {
                return Err(failure);
            }
            let text = std::str::from_utf8(&ran.stdout)
                .map_err(|_| "Prepare output is not UTF-8.".to_owned())?;
            return parse_json(text)
                .map(|v| v.to_string())
                .map_err(|e| format!("Prepare output: {e}"));
        }
    }
}

/// Quote one word for a POSIX shell: inside single quotes, with each `'`
/// written as `'\''`.
fn quote(word: &str) -> String {
    format!("'{}'", word.replace('\'', r"'\''"))
}

/// The `/bin/sh` script the setup terminal runs. `exec` makes the program the
/// terminal's process, so the terminal's exit status is setup's; `exit 127`
/// is reached only when the folder or the program cannot be used.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) fn shell_line(root: &Path, kind: &str, cmd: &Command) -> Result<String> {
    let root = root.to_str().ok_or("The project path is not valid text.")?;
    if root.chars().chain(kind.chars()).any(char::is_control) {
        return Err("The project path or work type contains control characters.".into());
    }
    let name = &cmd.argv[0];
    let program = if Path::new(name).components().count() > 1 {
        Path::new(root).join(name).to_string_lossy().into_owned()
    } else {
        name.clone()
    };
    let words: Vec<String> = std::iter::once(program)
        .chain(cmd.argv[1..].iter().cloned())
        .map(|w| quote(&w))
        .collect();
    Ok(format!(
        "cd {} && exec env CONNECTO_WORK_TYPE={} {}; exit 127",
        quote(root),
        quote(kind),
        words.join(" ")
    ))
}

/// Write the setup script into `dir` (Connecto's own, private folder) and
/// give the line to type into the user's login shell. Only the script's path
/// is typed, so the login shell's quoting rules (zsh, bash, fish…) never
/// apply to the reviewed argv.
#[cfg_attr(not(unix), allow(dead_code))]
pub(super) fn setup_script(
    dir: &Path,
    root: &Path,
    kind: &str,
    cmd: &Command,
) -> Result<(PathBuf, String)> {
    let script = dir.join(format!("setup-{}.sh", uuid::Uuid::new_v4()));
    let shown = script
        .to_str()
        .ok_or("The data folder path is not valid text.")?;
    if shown.contains(['\'', '\\']) || shown.chars().any(char::is_control) {
        return Err("The data folder path cannot be typed into a shell.".into());
    }
    let mut file = std::fs::OpenOptions::new();
    file.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        file.mode(0o600);
    }
    let body = format!("{}\n", shell_line(root, kind, cmd)?);
    std::io::Write::write_all(
        &mut file.open(&script).map_err(|e| e.to_string())?,
        body.as_bytes(),
    )
    .map_err(|e| e.to_string())?;
    let typed = format!("exec /bin/sh {}", quote(shown));
    Ok((script, typed))
}

pub(super) fn now() -> String {
    chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, false)
}

/// `<root>/.connecto/works`, refusing a link anywhere on the way (an agent
/// could plant one to aim creation or cleanup elsewhere). None when it does not
/// exist and `create` is false.
fn jobs_base(root: &Path, create: bool) -> Result<Option<PathBuf>> {
    let mut path = root.to_path_buf();
    for part in JOBS {
        path.push(part);
        match std::fs::symlink_metadata(&path) {
            Ok(m) if m.is_dir() => {}
            Ok(_) => return Err(format!("{} must be a plain folder.", path.display())),
            Err(e) if e.kind() == io::ErrorKind::NotFound && create => {
                private_dir(&path).map_err(|e| format!("Cannot create {}: {e}", path.display()))?;
                if part == "works" {
                    let _ = std::fs::write(path.join(".gitignore"), "*\n");
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => return Ok(None),
            Err(e) => return Err(format!("{}: {e}", path.display())),
        }
    }
    Ok(Some(path))
}
fn private_dir(path: &Path) -> io::Result<()> {
    let mut builder = std::fs::DirBuilder::new();
    #[cfg(unix)]
    {
        use std::os::unix::fs::DirBuilderExt;
        builder.mode(0o700);
    }
    builder.create(path)
}
/// The job folder's name, from the queue id and Connecto's token only.
fn job_name(id: i64, token: &str) -> String {
    format!("{id}-{}", token.get(..12).unwrap_or(token))
}
fn is_job_name(name: &str) -> bool {
    name.split_once('-').is_some_and(|(id, token)| {
        !id.is_empty()
            && id.bytes().all(|b| b.is_ascii_digit())
            && token.len() == 12
            && token.bytes().all(|b| b.is_ascii_hexdigit() || b == b'-')
    })
}

/// Create a fresh, private folder for a job.
pub(super) fn job_dir(root: &Path, id: i64, token: &str) -> Result<PathBuf> {
    let base = jobs_base(root, true)?.ok_or("Cannot create the job folder.")?;
    let dir = base.join(job_name(id, token));
    if std::fs::symlink_metadata(&dir).is_ok() {
        remove(&dir);
    }
    private_dir(&dir).map_err(|e| format!("Cannot create {}: {e}", dir.display()))?;
    Ok(dir)
}

/// Delete one job's folder.
pub(super) fn remove_job(root: &Path, id: i64, token: &str) {
    if let Ok(Some(base)) = jobs_base(root, false) {
        remove(&base.join(job_name(id, token)));
    }
}

/// Delete every job folder left in the project.
pub(super) fn sweep(root: &Path) {
    let Ok(Some(base)) = jobs_base(root, false) else {
        return;
    };
    let Ok(entries) = std::fs::read_dir(&base) else {
        return;
    };
    for entry in entries.flatten() {
        if entry.file_name().to_str().is_some_and(is_job_name) {
            remove(&entry.path());
        }
    }
}

/// Remove a folder without following links (a link is removed, not its target).
fn remove(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(m) if m.is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}
