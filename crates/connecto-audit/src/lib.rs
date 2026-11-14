//! The audit log: hash-chained JSON lines written by one thread.
//!
//! Each record holds the SHA-256 of the previous one and of itself, so an edit
//! anywhere breaks the chain. Deleting the tail leaves a shorter valid chain,
//! which this format cannot detect. Records hold redacted text only.

use std::fs::{self, File, OpenOptions};
use std::io::{self, BufRead, BufReader, Write};
use std::path::Path;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use connecto_core::Redacted;
use crossbeam_channel::{Receiver, Sender, bounded};
use serde_json::{Map, Value, json};
use sha2::{Digest, Sha256};

/// Records that may wait for the writer.
pub const QUEUE_CAPACITY: usize = 1024;

const GENESIS: &str = "0000000000000000000000000000000000000000000000000000000000000000";

/// Why a record is not durable. The caller must deny the action.
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum AuditError {
    #[error("the audit log did not answer in time")]
    Timeout,
    #[error("the audit log failed")]
    Failed,
}

/// Why a file does not verify.
#[derive(Debug, thiserror::Error)]
pub enum VerifyError {
    #[error("could not read the log: {0}")]
    Io(#[from] io::Error),
    #[error("record {0} is malformed")]
    Malformed(usize),
    #[error("record {0} does not follow the one before it")]
    BrokenChain(usize),
    #[error("record {0} was changed")]
    Changed(usize),
}

/// One audit record. Empty strings mean "not applicable".
#[derive(Debug, Clone, Default)]
pub struct Record {
    pub trace: String,
    pub message: String,
    pub actor: String,
    pub source: String,
    pub target: String,
    pub attempt: u64,
    pub action: String,
    pub decision: String,
    pub reason: String,
    pub policy_revision: u32,
    pub pause_epoch: u64,
    pub body_hash: String,
    pub redacted_body: Option<Redacted<String>>,
    pub outcome: String,
    pub evidence_class: String,
    /// The project the action belongs to (its data-folder id). Empty when the
    /// action is not about one.
    pub project: String,
}

impl Record {
    fn fields(&self, time_ms: u128) -> Map<String, Value> {
        let value = json!({
            "time_ms": u64::try_from(time_ms).unwrap_or(u64::MAX),
            "trace": self.trace,
            "message": self.message,
            "actor": self.actor,
            "source": self.source,
            "target": self.target,
            "attempt": self.attempt,
            "action": self.action,
            "decision": self.decision,
            "reason": self.reason,
            "policy_revision": self.policy_revision,
            "pause_epoch": self.pause_epoch,
            "body_hash": self.body_hash,
            "redacted_body": self.redacted_body.as_ref().map(Redacted::get),
            "outcome": self.outcome,
            "evidence_class": self.evidence_class,
            "project": self.project,
        });
        match value {
            Value::Object(map) => map,
            _ => Map::new(),
        }
    }
}

/// SHA-256 as lowercase hex.
#[must_use]
pub fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// Where lines go. A trait so a test can make the disk fail.
pub trait Sink: Send + 'static {
    /// Write one line and make it durable.
    fn write_line(&mut self, line: &str) -> io::Result<()>;
}

struct FileSink(File);

impl Sink for FileSink {
    fn write_line(&mut self, line: &str) -> io::Result<()> {
        self.0.write_all(line.as_bytes())?;
        self.0.write_all(b"\n")?;
        self.0.sync_data()
    }
}

/// `fields` plus `prev`, and the hash over that. Keys sort, so this is canonical.
fn seal(mut fields: Map<String, Value>, prev: &str) -> (String, String) {
    fields.insert("prev".to_owned(), Value::String(prev.to_owned()));
    let unsealed = Value::Object(fields.clone()).to_string();
    let hash = sha256_hex(unsealed.as_bytes());
    fields.insert("hash".to_owned(), Value::String(hash.clone()));
    (Value::Object(fields).to_string(), hash)
}

enum Job {
    Append {
        record: Box<Record>,
        done: Option<Sender<Result<(), AuditError>>>,
    },
    Flush(Sender<()>),
}

fn writer(mut sink: impl Sink, jobs: &Receiver<Job>) {
    let mut prev = GENESIS.to_owned();
    let mut broken = false;
    for job in jobs {
        match job {
            Job::Append { record, done } => {
                let result = if broken {
                    Err(AuditError::Failed)
                } else {
                    let time = SystemTime::now()
                        .duration_since(UNIX_EPOCH)
                        .map_or(0, |elapsed| elapsed.as_millis());
                    let (line, hash) = seal(record.fields(time), &prev);
                    match sink.write_line(&line) {
                        Ok(()) => {
                            prev = hash;
                            Ok(())
                        }
                        // A half-written line ends the chain: nothing after it
                        // could verify, so nothing more is claimed.
                        Err(_) => {
                            broken = true;
                            Err(AuditError::Failed)
                        }
                    }
                };
                if let Some(done) = done {
                    let _ = done.send(result);
                }
            }
            Job::Flush(done) => {
                let _ = done.send(());
            }
        }
    }
}

/// How many launch logs are kept. Every launch starts a new one.
pub const KEPT_LOGS: usize = 50;

const LOG_PREFIX: &str = "audit-";
const LOG_SUFFIX: &str = ".jsonl";

/// The file name of a launch's log, from its start time and process id.
#[must_use]
pub fn log_name(started_ms: u128, pid: u32) -> String {
    format!("{LOG_PREFIX}{started_ms}-{pid}{LOG_SUFFIX}")
}

/// The start time and process id in a name made by [`log_name`]. Any other
/// name has none.
fn log_key(name: &str) -> Option<(u128, u32)> {
    let digits = |text: &str| !text.is_empty() && text.bytes().all(|b| b.is_ascii_digit());
    let (stamp, pid) = name
        .strip_prefix(LOG_PREFIX)?
        .strip_suffix(LOG_SUFFIX)?
        .split_once('-')?;
    (digits(stamp) && digits(pid))
        .then(|| Some((stamp.parse().ok()?, pid.parse().ok()?)))
        .flatten()
}

/// Delete the oldest launch logs in `dir` so that at most `keep` stay,
/// `current` among them. Only regular files named as [`log_name`] names them
/// are looked at, and `current` is never deleted. Returns how many went.
pub fn prune_logs(dir: &Path, current: &Path, keep: usize) -> io::Result<usize> {
    let mut old = Vec::new();
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        let path = entry.path();
        if path == current || !entry.file_type()?.is_file() {
            continue;
        }
        if let Some(key) = entry.file_name().to_str().and_then(log_key) {
            old.push((key, path));
        }
    }
    // Newest first; `current` takes one of the places.
    old.sort_by_key(|(key, _)| std::cmp::Reverse(*key));
    let mut removed = 0;
    for (_, path) in old.into_iter().skip(keep.saturating_sub(1)) {
        fs::remove_file(&path)?;
        removed += 1;
    }
    Ok(removed)
}

/// Handle to the audit writer. Cheap to clone.
#[derive(Clone)]
pub struct Audit {
    jobs: Sender<Job>,
}

impl Audit {
    /// Start a new log at `path`.
    pub fn open(path: &Path) -> io::Result<Self> {
        if let Some(parent) = path.parent() {
            fs::create_dir_all(parent)?;
        }
        let file = OpenOptions::new()
            .create_new(true)
            .append(true)
            .open(path)?;
        Self::with_sink(FileSink(file))
    }

    /// Start a writer on any sink.
    pub fn with_sink(sink: impl Sink) -> io::Result<Self> {
        let (jobs, receiver) = bounded(QUEUE_CAPACITY);
        thread::Builder::new()
            .name("audit-writer".into())
            .spawn(move || writer(sink, &receiver))?;
        Ok(Self { jobs })
    }

    /// Append and wait until durable. On error the action must be denied.
    pub fn append_durable(&self, record: Record, timeout: Duration) -> Result<(), AuditError> {
        let (done, result) = bounded(1);
        let job = Job::Append {
            record: Box::new(record),
            done: Some(done),
        };
        self.jobs
            .send_timeout(job, timeout)
            .map_err(|_| AuditError::Timeout)?;
        result
            .recv_timeout(timeout)
            .map_err(|_| AuditError::Timeout)?
    }

    /// Append without waiting. For outcomes, and for human Stop and Pause, which
    /// never wait on the log. False means the record was lost.
    pub fn append_nowait(&self, record: Record) -> bool {
        self.jobs
            .try_send(Job::Append {
                record: Box::new(record),
                done: None,
            })
            .is_ok()
    }

    /// Wait for everything queued so far.
    pub fn flush(&self, timeout: Duration) -> bool {
        let (done, result) = bounded(1);
        self.jobs
            .send_timeout(Job::Flush(done), timeout)
            .is_ok_and(|()| result.recv_timeout(timeout).is_ok())
    }
}

/// Check a log's chain. Returns how many records it holds.
pub fn verify(path: &Path) -> Result<usize, VerifyError> {
    let mut prev = GENESIS.to_owned();
    let mut count = 0;
    for (index, line) in BufReader::new(File::open(path)?).lines().enumerate() {
        let line = line?;
        let number = index + 1;
        let value: Value =
            serde_json::from_str(&line).map_err(|_| VerifyError::Malformed(number))?;
        let Value::Object(mut fields) = value else {
            return Err(VerifyError::Malformed(number));
        };
        let hash = fields
            .remove("hash")
            .and_then(|hash| hash.as_str().map(str::to_owned))
            .ok_or(VerifyError::Malformed(number))?;
        let linked = fields
            .remove("prev")
            .and_then(|prev| prev.as_str().map(str::to_owned))
            .ok_or(VerifyError::Malformed(number))?;
        if linked != prev {
            return Err(VerifyError::BrokenChain(number));
        }
        let (canonical, expected) = seal(fields, &prev);
        // Comparing the exact line also catches whitespace and key order.
        if expected != hash || canonical != line {
            return Err(VerifyError::Changed(number));
        }
        prev = hash;
        count += 1;
    }
    Ok(count)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::path::PathBuf;

    use super::*;

    const WAIT: Duration = Duration::from_secs(2);

    struct Scratch(PathBuf);

    impl Scratch {
        fn new(name: &str) -> Self {
            let dir = std::env::temp_dir().join(format!("mg-audit-{name}-{}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(&dir).unwrap();
            Self(dir.join("log.jsonl"))
        }
    }

    impl Drop for Scratch {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(self.0.parent().unwrap());
        }
    }

    fn record(message: &str) -> Record {
        Record {
            message: message.to_owned(),
            decision: "allow".to_owned(),
            redacted_body: Some(Redacted::assume("body".to_owned())),
            ..Record::default()
        }
    }

    fn written(scratch: &Scratch, count: usize) {
        let audit = Audit::open(&scratch.0).unwrap();
        for index in 0..count {
            audit
                .append_durable(record(&index.to_string()), WAIT)
                .unwrap();
        }
    }

    #[test]
    fn an_intact_chain_verifies() {
        let scratch = Scratch::new("ok");
        written(&scratch, 5);
        assert_eq!(verify(&scratch.0).unwrap(), 5);
    }

    #[test]
    fn every_single_byte_edit_is_detected() {
        let scratch = Scratch::new("edit");
        written(&scratch, 3);
        let original = fs::read(&scratch.0).unwrap();
        let mut missed = Vec::new();
        for index in 0..original.len() {
            if original[index] == b'\n' {
                continue;
            }
            let mut changed = original.clone();
            changed[index] ^= 1;
            fs::write(&scratch.0, &changed).unwrap();
            if verify(&scratch.0).is_ok() {
                missed.push(index);
            }
        }
        assert!(missed.is_empty(), "{missed:?}");
    }

    #[test]
    fn a_removed_or_swapped_record_is_detected() {
        let scratch = Scratch::new("order");
        written(&scratch, 3);
        let text = fs::read_to_string(&scratch.0).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        fs::write(&scratch.0, format!("{}\n{}\n", lines[0], lines[2])).unwrap();
        assert!(matches!(
            verify(&scratch.0),
            Err(VerifyError::BrokenChain(2))
        ));
        fs::write(&scratch.0, format!("{}\n{}\n", lines[1], lines[0])).unwrap();
        assert!(verify(&scratch.0).is_err());
    }

    struct FailingSink;

    impl Sink for FailingSink {
        fn write_line(&mut self, _line: &str) -> io::Result<()> {
            Err(io::Error::other("disk full"))
        }
    }

    #[test]
    fn a_failing_disk_is_an_error_and_stays_one() {
        let audit = Audit::with_sink(FailingSink).unwrap();
        for _ in 0..2 {
            assert_eq!(
                audit.append_durable(record("x"), WAIT),
                Err(AuditError::Failed)
            );
        }
    }

    struct StalledSink;

    impl Sink for StalledSink {
        fn write_line(&mut self, _line: &str) -> io::Result<()> {
            thread::sleep(Duration::from_secs(2));
            Ok(())
        }
    }

    #[test]
    fn a_stalled_disk_times_out_and_never_holds_a_nowait_record() {
        let audit = Audit::with_sink(StalledSink).unwrap();
        let result = audit.append_durable(record("x"), Duration::from_millis(50));
        assert_eq!(result, Err(AuditError::Timeout));
        let start = std::time::Instant::now();
        audit.append_nowait(record("stop"));
        assert!(start.elapsed() < Duration::from_millis(50));
    }

    #[test]
    fn a_log_is_never_appended_to_after_a_restart() {
        let scratch = Scratch::new("once");
        written(&scratch, 1);
        assert!(Audit::open(&scratch.0).is_err());
    }

    #[test]
    fn old_launch_logs_are_pruned_and_nothing_else_is_touched() {
        let scratch = Scratch::new("prune");
        let dir = scratch.0.parent().unwrap();
        for stamp in 1..=8_u128 {
            fs::write(dir.join(log_name(stamp * 1000, 77)), "old").unwrap();
        }
        // Same time, another process: the pid orders them.
        fs::write(dir.join(log_name(8000, 78)), "old").unwrap();
        // The clock went back: the log of this launch is not the newest name.
        let current = dir.join(log_name(500, 99));
        fs::write(&current, "current").unwrap();
        let others = [
            "audit-12-34.jsonl.bak",
            "audit-12.jsonl",
            "audit--34.jsonl",
            "audit-12-.jsonl",
            "audit-1x-34.jsonl",
            "xaudit-12-34.jsonl",
            "audit-12-34-56.jsonl",
            "notes.txt",
        ];
        for name in others {
            fs::write(dir.join(name), "keep").unwrap();
        }
        // A folder with a log's name is not a log.
        fs::create_dir(dir.join(log_name(1, 1))).unwrap();
        assert_eq!(prune_logs(dir, &current, 4).unwrap(), 6);
        let mut left: Vec<String> = fs::read_dir(dir)
            .unwrap()
            .flatten()
            .map(|entry| entry.file_name().to_string_lossy().into_owned())
            .filter(|name| log_key(name).is_some())
            .collect();
        left.sort();
        assert_eq!(
            left,
            [
                log_name(1, 1),
                log_name(500, 99),
                log_name(7000, 77),
                log_name(8000, 77),
                log_name(8000, 78),
            ]
        );
        assert_eq!(fs::read_to_string(&current).unwrap(), "current");
        for name in others {
            assert!(dir.join(name).is_file(), "{name}");
        }
        // Nothing more to do the second time, and fewer logs than the limit stay.
        assert_eq!(prune_logs(dir, &current, 4).unwrap(), 0);
        assert_eq!(prune_logs(dir, &current, KEPT_LOGS).unwrap(), 0);
        // Even a limit of none keeps this launch's log.
        assert_eq!(prune_logs(dir, &current, 0).unwrap(), 3);
        assert!(current.is_file());
    }

    #[test]
    fn a_pruned_folder_leaves_the_running_log_and_its_chain_whole() {
        let scratch = Scratch::new("prune-live");
        let dir = scratch.0.parent().unwrap();
        let current = dir.join(log_name(2000, 5));
        let audit = Audit::open(&current).unwrap();
        audit.append_durable(record("one"), WAIT).unwrap();
        fs::write(dir.join(log_name(1000, 5)), "old").unwrap();
        assert_eq!(prune_logs(dir, &current, 1).unwrap(), 1);
        audit.append_durable(record("two"), WAIT).unwrap();
        assert_eq!(verify(&current).unwrap(), 2);
    }
}
