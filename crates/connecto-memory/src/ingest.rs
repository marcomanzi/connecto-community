//! From a [`Captured`] observation to queued events and evidence (spec §5.3,
//! §4.5, §4.6). Runs on a project's control thread, never on a terminal's.
//!
//! The rules, all applied before any model sees anything:
//!
//! - **Redaction first.** Every text is sanitized and passed through
//!   `connecto_redact::redact` with the host's known secrets before it is
//!   stored. When redaction cannot run, only the bare fact is stored: the
//!   event and its evidence carry no text.
//! - **Evidence kinds.** A typed prompt is a `dichiarazione_utente`; a bridge
//!   or relay message (`[connecto from …]`, `[connecto relay …]`) is not the
//!   user, so it is an `affermazione_agente`. A reply is an
//!   `affermazione_agente`; the lines of it that report a test, build or lint
//!   outcome also make an `esito_riportato` (only what the reply says: no
//!   outcome exists without a reply containing it). A difference between two
//!   git snapshots is an `osservazione_codice`. A session end is an event
//!   with no evidence.
//! - **Internal events.** A prompt that is a `connecto memory` command, and a
//!   reply that is only memory output, are kept as `interno` and never sent to
//!   a model. Utility sessions are not registry terminals, so they never
//!   arrive here at all.
//! - **Duplicates.** The key is a SHA-256 of (terminal, tool session,
//!   session start, turn, kind, redacted content): a key already seen queues
//!   nothing.
//! - **Dropped replies** are read back from the tool's session log through
//!   the host, in pieces. When the log cannot be read the event is queued as
//!   "content not available", with its log pointer, and no text is invented.
//! - **Git.** At every turn end the host takes a read-only snapshot; the
//!   difference from the previous one becomes a code observation. Memories
//!   whose anchored files now hash differently get `anchors_changed` (never a
//!   status change), and branch scopes whose branch is gone are marked gone
//!   (never promoted, never archived).

use std::collections::{BTreeMap, BTreeSet, HashMap};
use std::path::{Path, PathBuf};

use connecto_core::Redacted;
use connecto_core::memory::{
    Anchor, EvidenceKind, LogPointer, Provider, Scope, is_project_relative,
};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::capture::{Author, Captured, CapturedReply, Happened};
use crate::error::{Result, StoreError};
use crate::host::{ChangedFile, GitSnapshot, Host};
use crate::queue::{Enqueued, NewEvent};
use crate::store::NewEvidence;
use crate::writer::{WriterHandle, call};

/// The first line every `connecto memory` output starts with (spec §5.8). A
/// reply that is nothing but such output is internal. Must match the
/// renderer of `connecto_memory::commands`.
pub const MEMORY_OUTPUT_PREFIX: &str = "[connecto memory";

/// Lines of a reply kept as reported outcomes, at most.
const MAX_OUTCOME_LINES: usize = 40;
/// Anchored paths checked against the working tree at one turn end, at most.
const MAX_ANCHOR_PATHS: usize = 2000;

/// What an event is, as stored in its `kind` column and payload.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum EventWhat {
    Prompt,
    Reply,
    Outcome,
    Code,
    SessionEnd,
}

impl EventWhat {
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Prompt => "prompt",
            Self::Reply => "reply",
            Self::Outcome => "outcome",
            Self::Code => "code",
            Self::SessionEnd => "session_end",
        }
    }

    #[must_use]
    pub fn from_word(word: &str) -> Option<Self> {
        [
            Self::Prompt,
            Self::Reply,
            Self::Outcome,
            Self::Code,
            Self::SessionEnd,
        ]
        .into_iter()
        .find(|w| w.word() == word)
    }
}

/// The payload of a queued event: everything the development cycle shows a
/// model about it. Text is already redacted.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EventPayload {
    pub what: EventWhat,
    pub evidence_kind: Option<EvidenceKind>,
    pub provider: Option<Provider>,
    pub terminal: Option<String>,
    pub tool_session: Option<String>,
    pub turn: Option<String>,
    pub branch: Option<String>,
    pub head: Option<String>,
    /// `None`: no text (redaction could not run, the content is not
    /// available, or there never was any).
    pub text: Option<String>,
    /// A dropped reply whose log could not be read.
    pub unavailable: bool,
    /// Redaction could not run: the text was not stored.
    pub withheld: bool,
    /// Who wrote a prompt (prompts only). Only `User` is the user.
    pub author: Option<Author>,
    pub log: Option<LogPointer>,
}

impl EventPayload {
    /// Identifiers are kept only when they are plain ids holding no known
    /// secret ([`safe_id`]).
    fn new(what: EventWhat, c: &Captured, secrets: &[String]) -> Self {
        Self {
            what,
            evidence_kind: None,
            provider: Some(c.provider),
            terminal: safe_id(&c.terminal, secrets),
            tool_session: c.tool_session.as_deref().and_then(|s| safe_id(s, secrets)),
            turn: safe_id(&c.turn, secrets),
            branch: None,
            head: None,
            text: None,
            unavailable: false,
            withheld: false,
            author: None,
            log: None,
        }
    }

    #[must_use]
    pub fn to_json(&self) -> String {
        json!({
            "what": self.what.word(),
            "evidence_kind": self.evidence_kind.map(EvidenceKind::word),
            "provider": self.provider.map(Provider::word),
            "terminal": self.terminal,
            "tool_session": self.tool_session,
            "turn": self.turn,
            "branch": self.branch,
            "head": self.head,
            "text": self.text,
            "unavailable": self.unavailable,
            "withheld": self.withheld,
            "author": self.author.map(Author::word),
            "log": self.log.as_ref().map(|l| json!({"path": l.path, "offset": l.offset})),
        })
        .to_string()
    }

    pub fn from_json(text: &str) -> Result<Self> {
        let bad = || StoreError::Corrupt("event payload");
        let v: Value = serde_json::from_str(text).map_err(|_| bad())?;
        let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
        let b = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
        Ok(Self {
            what: s("what")
                .as_deref()
                .and_then(EventWhat::from_word)
                .ok_or_else(bad)?,
            evidence_kind: s("evidence_kind")
                .as_deref()
                .and_then(EvidenceKind::from_word),
            provider: s("provider").as_deref().and_then(Provider::from_word),
            terminal: s("terminal"),
            tool_session: s("tool_session"),
            turn: s("turn"),
            branch: s("branch"),
            head: s("head"),
            text: s("text"),
            unavailable: b("unavailable"),
            withheld: b("withheld"),
            author: s("author").as_deref().and_then(Author::from_word),
            log: v.get("log").and_then(|l| {
                Some(LogPointer {
                    path: l.get("path")?.as_str()?.to_owned(),
                    offset: l.get("offset")?.as_u64()?,
                })
            }),
        })
    }
}

/// What one capture did.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Ingested {
    /// Event seqs queued (internal ones included).
    pub queued: Vec<u64>,
    pub duplicates: usize,
    pub internal: usize,
    /// Memories newly marked `anchors_changed`.
    pub anchors_changed: usize,
    /// Branch scopes newly marked gone (or back).
    pub scopes_marked: usize,
}

/// A project's capture state: the last prompt per terminal (to find a dropped
/// reply in the log) and the last git snapshot (persisted in the store).
pub struct Ingest {
    project: PathBuf,
    last_prompt: HashMap<String, String>,
    /// `None` until loaded from the store.
    snapshot: Option<Option<GitSnapshot>>,
}

/// Sanitize and redact; `None` when redaction cannot run.
#[must_use]
pub fn redact(text: &str, secrets: &[String]) -> Option<String> {
    connecto_redact::redact(&connecto_redact::sanitize(text), secrets)
        .ok()
        .map(|r| r.get().clone())
}

/// Whether `text` holds one of the known secrets.
fn holds_secret(text: &str, secrets: &[String]) -> bool {
    secrets
        .iter()
        .any(|s| s.len() >= 4 && text.contains(s.as_str()))
}

/// An identifier (terminal, tool session, turn) as it may be stored and
/// shown: 1 to 128 of ASCII letters, digits and `- _ . : # @`, holding no
/// known secret. Anything else is withheld (`None`).
#[must_use]
pub fn safe_id(id: &str, secrets: &[String]) -> Option<String> {
    let plain = !id.is_empty()
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_' | '.' | ':' | '#' | '@'));
    (plain && !holds_secret(id, secrets)).then(|| id.to_owned())
}

/// A branch name as it may be stored: unchanged by redaction, no control
/// characters, at most 255 bytes; otherwise withheld.
#[must_use]
pub fn safe_branch(branch: &str, secrets: &[String]) -> Option<String> {
    let ok = !branch.is_empty()
        && branch.len() <= 255
        && !branch.chars().any(char::is_control)
        && redact(branch, secrets).as_deref() == Some(branch);
    ok.then(|| branch.to_owned())
}

/// A commit id: hexadecimal, 7 to 64 characters; otherwise withheld.
#[must_use]
pub fn safe_head(head: &str) -> Option<String> {
    let ok = (7..=64).contains(&head.len()) && head.bytes().all(|b| b.is_ascii_hexdigit());
    ok.then(|| head.to_ascii_lowercase())
}

/// A file path as it may be stored: inside the project and unchanged by
/// redaction.
#[must_use]
pub fn safe_path(path: &str, secrets: &[String]) -> bool {
    is_project_relative(path) && redact(path, secrets).as_deref() == Some(path)
}

/// A log path as it may be stored: the tool's own file under its profile,
/// named by ids that look random to redaction (so redaction is not the
/// test): withheld when it holds a known secret or a control character.
pub fn safe_log(path: &Path, offset: u64, secrets: &[String]) -> Option<LogPointer> {
    let text = path.to_string_lossy();
    let ok =
        text.len() <= 4096 && !text.chars().any(char::is_control) && !holds_secret(&text, secrets);
    ok.then(|| LogPointer {
        path: text.into_owned(),
        offset,
    })
}

/// A snapshot with every field that could carry a secret checked: the
/// branch and HEAD withheld when not plain, paths that are not plain
/// dropped. Returns how many paths were dropped.
#[must_use]
pub fn sanitize_snapshot(s: GitSnapshot, secrets: &[String]) -> (GitSnapshot, usize) {
    let total = s.changed.len();
    let changed: Vec<ChangedFile> = s
        .changed
        .into_iter()
        .filter(|f| safe_path(&f.path, secrets))
        .map(|f| ChangedFile {
            blob: f.blob.filter(|b| Anchor::new("x", b).is_ok()),
            ..f
        })
        .collect();
    let dropped = total - changed.len();
    (
        GitSnapshot {
            branch: s.branch.as_deref().and_then(|b| safe_branch(b, secrets)),
            head: s.head.as_deref().and_then(safe_head),
            changed,
        },
        dropped,
    )
}

/// A reason or error text as it may be stored or shown: redacted, one line,
/// at most 300 characters; withheld when redaction cannot run.
#[must_use]
pub fn clean_reason(text: &str, secrets: &[String]) -> String {
    match redact(text, secrets) {
        Some(clean) => {
            let line = clean.split_whitespace().collect::<Vec<_>>().join(" ");
            if line.chars().count() > 300 {
                format!("{}…", line.chars().take(300).collect::<String>())
            } else {
                line
            }
        }
        None => "(reason withheld: redaction could not run)".to_owned(),
    }
}

fn dedup_key(parts: &[&str]) -> String {
    let mut hash = Sha256::new();
    for part in parts {
        hash.update(part.as_bytes());
        hash.update([0x1f]);
    }
    hash.finalize().iter().map(|b| format!("{b:02x}")).collect()
}

/// Whether a prompt came from another terminal through the bridge or the
/// manual relay rather than from the user.
#[must_use]
pub fn is_bridge_message(prompt: &str) -> bool {
    let first = prompt.trim_start();
    first.starts_with("[connecto from ") || first.starts_with("[connecto relay ")
}

/// Whether a prompt is Connecto's own notice (the resume note, a version
/// notice): nobody's statement, kept as internal.
#[must_use]
pub fn is_connecto_notice(prompt: &str) -> bool {
    prompt.trim_start().starts_with("[connecto notice]")
}

/// Whether a prompt is a `connecto memory` command.
#[must_use]
pub fn is_memory_command(prompt: &str) -> bool {
    let t = prompt.trim_start();
    let t = t.strip_prefix('!').unwrap_or(t).trim_start();
    t == "connecto memory" || t.starts_with("connecto memory ")
}

/// Whether a reply is nothing but `connecto memory` output.
#[must_use]
pub fn is_memory_output(reply: &str) -> bool {
    reply
        .trim_start()
        .lines()
        .next()
        .is_some_and(|l| l.starts_with(MEMORY_OUTPUT_PREFIX))
}

/// The lines of a reply that report a test, build or lint outcome.
#[must_use]
pub fn reported_outcomes(reply: &str) -> Vec<&str> {
    const SUBJECT: &[&str] = &[
        "test", "build", "lint", "clippy", "compil", "check", "cargo ", "npm ", "pytest", "ci ",
    ];
    const VERDICT: &[&str] = &[
        "pass", "fail", "succeed", "success", "green", "error", " ok", "broke",
    ];
    reply
        .lines()
        .map(str::trim)
        .filter(|line| {
            let l = line.to_lowercase();
            SUBJECT.iter().any(|w| l.contains(w)) && VERDICT.iter().any(|w| l.contains(w))
        })
        .take(MAX_OUTCOME_LINES)
        .collect()
}

fn snapshot_to_json(s: &GitSnapshot) -> String {
    json!({
        "branch": s.branch,
        "head": s.head,
        "changed": s.changed.iter().map(|f| json!([f.path, f.blob, f.deleted])).collect::<Vec<_>>(),
    })
    .to_string()
}

fn snapshot_from_json(text: &str) -> Option<GitSnapshot> {
    let v: Value = serde_json::from_str(text).ok()?;
    let s = |k: &str| v.get(k).and_then(Value::as_str).map(str::to_owned);
    Some(GitSnapshot {
        branch: s("branch"),
        head: s("head"),
        changed: v
            .get("changed")?
            .as_array()?
            .iter()
            .filter_map(|f| {
                Some(ChangedFile {
                    path: f.get(0)?.as_str()?.to_owned(),
                    blob: f.get(1).and_then(Value::as_str).map(str::to_owned),
                    deleted: f.get(2).and_then(Value::as_bool).unwrap_or(false),
                })
            })
            .collect(),
    })
}

/// A code observation: the difference between two snapshots.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Observation {
    pub text: String,
    pub files: Vec<Anchor>,
    /// Branch or HEAD moved.
    pub moved: bool,
}

fn short(hash: Option<&str>) -> String {
    hash.map_or_else(|| "none".to_owned(), |h| h.chars().take(12).collect())
}

/// What changed between `prev` and `now`, as the host observed it. `None`
/// when nothing changed. Only host facts: branch, HEAD, paths, blob hashes.
#[must_use]
pub fn observe(prev: &GitSnapshot, now: &GitSnapshot) -> Option<Observation> {
    // (blob, deleted); a file with no blob and not deleted is *unknown*.
    fn state(f: &ChangedFile) -> (Option<&str>, bool) {
        (f.blob.as_deref(), f.deleted)
    }
    let before: BTreeMap<&str, (Option<&str>, bool)> = prev
        .changed
        .iter()
        .map(|f| (f.path.as_str(), state(f)))
        .collect();
    let after: BTreeMap<&str, (Option<&str>, bool)> = now
        .changed
        .iter()
        .map(|f| (f.path.as_str(), state(f)))
        .collect();
    let mut modified = Vec::new();
    let mut deleted = Vec::new();
    let mut unknown = 0usize;
    for (path, now_state) in &after {
        match now_state {
            (Some(blob), _) => {
                if before.get(path).map(|b| b.0) != Some(Some(*blob)) {
                    modified.push((*path, *blob));
                }
            }
            (None, true) => {
                if before.get(path).map(|b| b.1) != Some(true) {
                    deleted.push(*path);
                }
            }
            // Not hashed (over the cap, or the hash failed): no claim at all.
            (None, false) => unknown += 1,
        }
    }
    let settled: Vec<&str> = before
        .keys()
        .filter(|p| !after.contains_key(*p))
        .copied()
        .collect();
    let branch_moved = prev.branch != now.branch;
    let head_moved = prev.head != now.head;
    if !branch_moved
        && !head_moved
        && modified.is_empty()
        && deleted.is_empty()
        && settled.is_empty()
    {
        return None;
    }
    let mut text = String::from("Git, observed by Connecto at the end of a turn.\n");
    let branch = |b: Option<&str>| b.unwrap_or("(detached, or withheld)").to_owned();
    text.push_str(&format!(
        "Branch: {}{}\n",
        branch(now.branch.as_deref()),
        if branch_moved {
            format!(" (was {})", branch(prev.branch.as_deref()))
        } else {
            String::new()
        }
    ));
    text.push_str(&format!(
        "HEAD: {}{}\n",
        short(now.head.as_deref()),
        if head_moved {
            format!(" (was {})", short(prev.head.as_deref()))
        } else {
            String::new()
        }
    ));
    if !modified.is_empty() {
        text.push_str("Changed in the working tree since the previous turn end:\n");
        for (path, blob) in &modified {
            text.push_str(&format!("- {path} (blob {})\n", short(Some(blob))));
        }
    }
    if !deleted.is_empty() {
        text.push_str("Deleted:\n");
        for path in &deleted {
            text.push_str(&format!("- {path}\n"));
        }
    }
    if !settled.is_empty() {
        text.push_str("No longer modified (committed or reverted):\n");
        for path in &settled {
            text.push_str(&format!("- {path}\n"));
        }
    }
    if unknown > 0 {
        text.push_str(&format!(
            "{unknown} more changed file(s) were not hashed (limit or error): not observed.\n"
        ));
    }
    Some(Observation {
        text,
        files: modified
            .iter()
            .filter_map(|(p, b)| Anchor::new(p, b).ok())
            .collect(),
        moved: branch_moved || head_moved,
    })
}

impl Ingest {
    #[must_use]
    pub fn new(project: PathBuf) -> Self {
        Self {
            project,
            last_prompt: HashMap::new(),
            snapshot: None,
        }
    }

    #[must_use]
    pub fn project(&self) -> &Path {
        &self.project
    }

    fn load_snapshot(&mut self, writer: &WriterHandle) -> Result<Option<GitSnapshot>> {
        if let Some(s) = &self.snapshot {
            return Ok(s.clone());
        }
        let stored = call(writer, |s| s.engine_state("git_snapshot"))?;
        let loaded = stored.as_deref().and_then(snapshot_from_json);
        self.snapshot = Some(loaded.clone());
        Ok(loaded)
    }

    fn save_snapshot(&mut self, writer: &WriterHandle, snapshot: GitSnapshot) -> Result<()> {
        let text = snapshot_to_json(&snapshot);
        call(writer, move |s| s.set_engine_state("git_snapshot", &text))?;
        self.snapshot = Some(Some(snapshot));
        Ok(())
    }

    /// At start: a first snapshot when there is none (no observation), and
    /// branch scopes checked.
    pub fn baseline(&mut self, writer: &WriterHandle, host: &dyn Host) -> Result<Ingested> {
        let mut out = Ingested::default();
        if self.load_snapshot(writer)?.is_none()
            && let Some(now) = host.git_snapshot(&self.project)
        {
            let (now, _) = sanitize_snapshot(now, &host.known_secrets());
            self.save_snapshot(writer, now)?;
        }
        out.scopes_marked = self.check_branches(writer, host)?;
        Ok(out)
    }

    /// Ingest one capture. Store errors come back; nothing is half-written
    /// (each event and its evidence are one transaction).
    pub fn ingest(
        &mut self,
        writer: &WriterHandle,
        host: &dyn Host,
        c: Captured,
    ) -> Result<Ingested> {
        let mut out = Ingested::default();
        let now = host.now().utc_ms;
        let secrets = host.known_secrets();
        let (branch, head) = self
            .load_snapshot(writer)?
            .map(|s| (s.branch, s.head))
            .unwrap_or_default();
        match &c.what {
            Happened::Prompt {
                text,
                author,
                truncated,
            } => {
                self.last_prompt.insert(c.terminal.clone(), text.clone());
                let mut p = EventPayload::new(EventWhat::Prompt, &c, &secrets);
                p.branch.clone_from(&branch);
                p.head.clone_from(&head);
                // Fail closed: only what the registry knows a person typed
                // in the window, under no Connecto header, is the user.
                let author = if is_connecto_notice(text) {
                    Author::Connecto
                } else if is_bridge_message(text) && *author == Author::User {
                    Author::Agent
                } else {
                    *author
                };
                p.author = Some(author);
                let internal = author == Author::Connecto || is_memory_command(text);
                if *truncated {
                    // The hook cut it and the log holds no prompt text to
                    // recover: kept as not available, never as a cut text.
                    p.unavailable = true;
                    p.log = c.log.as_deref().and_then(|l| safe_log(l, 0, &secrets));
                    self.queue(writer, &c, p, internal, now, &mut out)?;
                } else {
                    p.evidence_kind = (!internal).then_some(if author == Author::User {
                        EvidenceKind::UserStatement
                    } else {
                        EvidenceKind::AgentClaim
                    });
                    set_text(&mut p, text, &secrets);
                    self.queue(writer, &c, p, internal, now, &mut out)?;
                }
            }
            Happened::TurnEnd { reply } => {
                let mut p = EventPayload::new(EventWhat::Reply, &c, &secrets);
                p.branch.clone_from(&branch);
                p.head.clone_from(&head);
                let text = match reply {
                    CapturedReply::Text(text) => Some(text.clone()),
                    CapturedReply::None => None,
                    CapturedReply::Dropped => {
                        let sent = self.last_prompt.get(&c.terminal).map(String::as_str);
                        let logged = c
                            .log
                            .as_deref()
                            .and_then(|log| host.read_logged_reply(c.provider, log, sent));
                        p.log = c.log.as_deref().and_then(|log| {
                            safe_log(log, logged.as_ref().map_or(0, |l| l.offset), &secrets)
                        });
                        match logged {
                            Some(l) => Some(l.text),
                            None => {
                                p.unavailable = true;
                                None
                            }
                        }
                    }
                };
                if let Some(text) = &text {
                    let internal = is_memory_output(text);
                    p.evidence_kind = (!internal).then_some(EvidenceKind::AgentClaim);
                    set_text(&mut p, text, &secrets);
                    let outcomes = if internal || p.withheld {
                        Vec::new()
                    } else {
                        reported_outcomes(p.text.as_deref().unwrap_or_default())
                            .into_iter()
                            .map(str::to_owned)
                            .collect()
                    };
                    let log = p.log.clone();
                    self.queue(writer, &c, p, internal, now, &mut out)?;
                    if !outcomes.is_empty() {
                        let mut o = EventPayload::new(EventWhat::Outcome, &c, &secrets);
                        o.branch.clone_from(&branch);
                        o.head.clone_from(&head);
                        o.log = log;
                        o.evidence_kind = Some(EvidenceKind::ReportedOutcome);
                        // Already redacted: these are lines of the stored text.
                        o.text = Some(outcomes.join("\n"));
                        self.queue(writer, &c, o, false, now, &mut out)?;
                    }
                } else if p.unavailable {
                    // Kept with its pointer; no evidence of content, nothing
                    // made up.
                    self.queue(writer, &c, p, false, now, &mut out)?;
                }
                self.turn_end_git(writer, host, &c, now, &mut out)?;
            }
            Happened::SessionEnd { how } => {
                let mut p = EventPayload::new(EventWhat::SessionEnd, &c, &secrets);
                p.log = c.log.as_deref().and_then(|log| safe_log(log, 0, &secrets));
                set_text(&mut p, how, &secrets);
                self.last_prompt.remove(&c.terminal);
                self.queue(writer, &c, p, false, now, &mut out)?;
            }
        }
        Ok(out)
    }

    fn queue(
        &mut self,
        writer: &WriterHandle,
        c: &Captured,
        p: EventPayload,
        internal: bool,
        now: u64,
        out: &mut Ingested,
    ) -> Result<()> {
        let start = c.session_start_ms.to_string();
        let key = dedup_key(&[
            &c.terminal,
            c.tool_session.as_deref().unwrap_or(""),
            &start,
            &c.turn,
            p.what.word(),
            p.text
                .as_deref()
                .unwrap_or(if p.unavailable { "(unavailable)" } else { "" }),
        ]);
        self.queue_keyed(writer, key, p, internal, now, out)
    }

    fn queue_keyed(
        &mut self,
        writer: &WriterHandle,
        key: String,
        p: EventPayload,
        internal: bool,
        now: u64,
        out: &mut Ingested,
    ) -> Result<()> {
        let event = NewEvent {
            dedup_key: key,
            kind: p.what.word().to_owned(),
            created_ms: now,
            payload: p.to_json(),
            priority: false,
            internal,
        };
        let evidence = p.evidence_kind.filter(|_| !internal).map(|kind| {
            let excerpt = match (&p.text, p.withheld) {
                (Some(text), _) => text.clone(),
                (None, true) => "(text withheld: redaction could not run)".to_owned(),
                (None, false) => String::new(),
            };
            let mut e = NewEvidence::new(kind, now, Redacted::assume(excerpt));
            e.terminal.clone_from(&p.terminal);
            e.tool_session.clone_from(&p.tool_session);
            e.turn.clone_from(&p.turn);
            e.branch.clone_from(&p.branch);
            e.head.clone_from(&p.head);
            e.log.clone_from(&p.log);
            e
        });
        let (done, _) = call(writer, move |s| s.enqueue_with_evidence(event, evidence))?;
        match done {
            Enqueued::Inserted(seq) => {
                out.queued.push(seq);
                if internal {
                    out.internal += 1;
                }
            }
            Enqueued::Duplicate(_) => out.duplicates += 1,
        }
        Ok(())
    }

    fn turn_end_git(
        &mut self,
        writer: &WriterHandle,
        host: &dyn Host,
        c: &Captured,
        now: u64,
        out: &mut Ingested,
    ) -> Result<()> {
        let Some(snapshot) = host.git_snapshot(&self.project) else {
            return Ok(());
        };
        // Branch, HEAD and paths are checked before anything is stored or
        // shown (a secret in a branch name is withheld).
        let (snapshot, _) = sanitize_snapshot(snapshot, &host.known_secrets());
        let prev = self.load_snapshot(writer)?;
        let observation = prev.as_ref().and_then(|prev| observe(prev, &snapshot));
        self.save_snapshot(writer, snapshot.clone())?;
        let Some(obs) = observation else {
            return Ok(());
        };
        let mut p = EventPayload::new(EventWhat::Code, c, &host.known_secrets());
        p.evidence_kind = Some(EvidenceKind::CodeObservation);
        p.branch.clone_from(&snapshot.branch);
        p.head.clone_from(&snapshot.head);
        p.text = redact(&obs.text, &host.known_secrets());
        p.withheld = p.text.is_none();
        let key = dedup_key(&[
            "code",
            &c.project_id,
            &snapshot_to_json(&snapshot),
            prev.as_ref().map(snapshot_to_json).as_deref().unwrap_or(""),
        ]);
        let files = obs.files.clone();
        let event = NewEvent {
            dedup_key: key,
            kind: EventWhat::Code.word().to_owned(),
            created_ms: now,
            payload: p.to_json(),
            priority: false,
            internal: false,
        };
        let mut e = NewEvidence::new(
            EvidenceKind::CodeObservation,
            now,
            Redacted::assume(p.text.clone().unwrap_or_default()),
        );
        e.terminal.clone_from(&p.terminal);
        e.branch = snapshot.branch.clone();
        e.head = snapshot.head.clone();
        e.files = files;
        let (done, _) = call(writer, move |s| s.enqueue_with_evidence(event, Some(e)))?;
        match done {
            Enqueued::Inserted(seq) => out.queued.push(seq),
            Enqueued::Duplicate(_) => out.duplicates += 1,
        }
        out.anchors_changed += self.check_anchors(writer, host)?;
        if obs.moved {
            out.scopes_marked += self.check_branches(writer, host)?;
        }
        Ok(())
    }

    /// Mark `anchors_changed` on live memories whose anchored files now hash
    /// differently. Never touches status, never clears the mark (only a
    /// confirmation in a later cycle does).
    fn check_anchors(&self, writer: &WriterHandle, host: &dyn Host) -> Result<usize> {
        let memories = call(writer, |s| s.read().memories())?;
        let paths: BTreeSet<String> = memories
            .iter()
            .filter(|m| !m.freshness.anchors_changed)
            .flat_map(|m| m.anchors.iter().map(|a| a.path.clone()))
            .take(MAX_ANCHOR_PATHS)
            .collect();
        if paths.is_empty() {
            return Ok(0);
        }
        let paths: Vec<String> = paths.into_iter().collect();
        let Some(blobs) = host.file_blobs(&self.project, &paths) else {
            return Ok(0);
        };
        let current: BTreeMap<String, (Option<String>, bool)> = blobs
            .into_iter()
            .map(|f| (f.path, (f.blob, f.deleted)))
            .collect();
        let mut marked = 0;
        for m in memories.iter().filter(|m| !m.freshness.anchors_changed) {
            // Changed: a known different hash, or a confirmed deletion. An
            // unknown hash (cap, failure) says nothing.
            let changed = m.anchors.iter().any(|a| match current.get(&a.path) {
                Some((Some(blob), _)) => blob != &a.blob,
                Some((None, true)) => true,
                _ => false,
            });
            if changed {
                let id = m.id.clone();
                let confirmed = m.freshness.last_confirmed_ms;
                call(writer, move |s| s.set_freshness(&id, confirmed, true))?;
                marked += 1;
            }
        }
        Ok(marked)
    }

    /// Mark branch scopes whose branch is no longer present (or back). The
    /// memory itself is left as it is (spec §4.6).
    fn check_branches(&self, writer: &WriterHandle, host: &dyn Host) -> Result<usize> {
        let Some(branches) = host.branches(&self.project) else {
            return Ok(0);
        };
        let memories = call(writer, |s| s.read().memories())?;
        let mut marked = 0;
        for m in &memories {
            for entry in &m.scopes {
                if let Scope::Branch(name) = &entry.scope {
                    let gone = !branches.iter().any(|b| b == name);
                    if gone != entry.gone {
                        let (id, scope) = (m.id.clone(), entry.scope.clone());
                        if call(writer, move |s| s.set_scope_gone(&id, &scope, gone))? {
                            marked += 1;
                        }
                    }
                }
            }
        }
        Ok(marked)
    }
}

fn set_text(p: &mut EventPayload, text: &str, secrets: &[String]) {
    match redact(text, secrets) {
        Some(clean) => p.text = Some(clean),
        None => {
            p.text = None;
            p.withheld = true;
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn file(path: &str, blob: Option<&str>) -> ChangedFile {
        ChangedFile {
            path: path.to_owned(),
            blob: blob.map(str::to_owned),
            deleted: blob.is_none(),
        }
    }

    const A: &str = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa";
    const B: &str = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb";

    #[test]
    fn observations_name_only_what_git_shows() {
        let prev = GitSnapshot {
            branch: Some("main".into()),
            head: Some("1111".into()),
            changed: vec![file("a.rs", Some(A)), file("old.rs", Some(A))],
        };
        assert_eq!(observe(&prev, &prev), None);
        let now = GitSnapshot {
            branch: Some("main".into()),
            head: Some("2222".into()),
            changed: vec![file("a.rs", Some(B)), file("gone.rs", None)],
        };
        let obs = observe(&prev, &now).unwrap();
        assert!(obs.moved);
        assert_eq!(obs.files, [Anchor::new("a.rs", B).unwrap()]);
        assert!(obs.text.contains("Deleted:\n- gone.rs"));
        assert!(
            obs.text
                .contains("No longer modified (committed or reverted):\n- old.rs")
        );
        assert!(obs.text.contains("HEAD: 2222 (was 1111)"));
        assert!(!obs.text.to_lowercase().contains("test"));
    }

    #[test]
    fn rules_for_prompts_and_replies() {
        assert!(is_bridge_message("[connecto from a1, request r3.]\nhi"));
        assert!(is_bridge_message(
            "[connecto relay 4 from t2. Relayed content]"
        ));
        assert!(!is_bridge_message("please fix [connecto from x"));
        assert!(is_connecto_notice(
            "[connecto notice] this conversation was resumed"
        ));
        assert!(!is_connecto_notice("tell me about [connecto notice]"));
        assert!(is_memory_command("connecto memory brief"));
        assert!(is_memory_command("!connecto memory search build"));
        assert!(!is_memory_command("what does connecto memory do?"));
        assert!(is_memory_output(
            "[connecto memory v3] notes from past sessions"
        ));
        assert!(!is_memory_output("I read [connecto memory] output"));
        let lines = reported_outcomes(
            "Done.\ncargo test: 12 passed, 0 failed\nThe lint is clean and green\nI like red apples",
        );
        assert_eq!(
            lines,
            [
                "cargo test: 12 passed, 0 failed",
                "The lint is clean and green"
            ]
        );
        assert!(reported_outcomes("I planned the work; nothing ran.").is_empty());
    }

    #[test]
    fn payloads_round_trip() {
        let p = EventPayload {
            what: EventWhat::Reply,
            evidence_kind: Some(EvidenceKind::AgentClaim),
            provider: Some(Provider::Codex),
            terminal: Some("t".into()),
            tool_session: None,
            turn: Some("#2".into()),
            branch: Some("main".into()),
            head: None,
            text: Some("x".into()),
            unavailable: false,
            withheld: false,
            author: Some(Author::Agent),
            log: Some(LogPointer {
                path: "/l".into(),
                offset: 9,
            }),
        };
        assert_eq!(EventPayload::from_json(&p.to_json()).unwrap(), p);
        let snap = GitSnapshot {
            branch: None,
            head: Some("h".into()),
            changed: vec![file("a", None)],
        };
        assert_eq!(snapshot_from_json(&snapshot_to_json(&snap)), Some(snap));
    }
}
