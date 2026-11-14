//! A project's closed terminals: `<data>/projects/<id>/history/` (R9).
//!
//! `index.json` lists the entries, newest first, at most [`MAX_ENTRIES`]; each
//! has a `<history-id>.txt` transcript beside it. Everything written here has
//! passed through `connecto-redact`. The registry thread is the only caller, so
//! nothing on the UI thread touches the disk.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use connecto_core::{HistoryEntry, ToolKind};
use serde_json::{Value, json};

/// Entries kept. Older ones are dropped with their transcripts.
pub(crate) const MAX_ENTRIES: usize = 500;
/// The largest transcript, in bytes. The tail is kept.
pub(crate) const MAX_TRANSCRIPT: usize = 2 * 1024 * 1024;
/// The longest `task` or `last_reply`, in characters.
pub(crate) const MAX_FIELD: usize = 300;
/// How much of a long field is looked at before redaction.
const FIELD_INPUT: usize = 4096;

const INDEX: &str = "index.json";
const VERSION: u64 = 1;

/// An entry and what Continue needs besides it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Record {
    pub entry: HistoryEntry,
    /// The tool's session id. `entry.resumable` is whether this is present.
    pub session: Option<String>,
}

/// Redacted plain text, or a marker when redaction could not run: content is
/// withheld, never written as it was.
fn redacted(text: &str) -> String {
    let clean = connecto_redact::sanitize(text);
    connecto_redact::redact(&clean, &[]).map_or_else(
        |_| "[withheld: redaction could not run]".to_owned(),
        |text| text.get().clone(),
    )
}

/// A short, redacted field for the list and for search.
pub(crate) fn field(text: &str) -> Option<String> {
    let head: String = text.trim().chars().take(FIELD_INPUT).collect();
    let text: String = redacted(&head)
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(MAX_FIELD)
        .collect();
    let text = text.trim_end().to_owned();
    (!text.is_empty()).then_some(text)
}

/// The redacted transcript, at most [`MAX_TRANSCRIPT`] bytes, tail kept.
pub(crate) fn transcript_text(text: &str) -> String {
    let tail = tail_bytes(text, MAX_TRANSCRIPT);
    let clean = redacted(tail);
    tail_bytes(&clean, MAX_TRANSCRIPT).to_owned()
}

fn tail_bytes(text: &str, max: usize) -> &str {
    if text.len() <= max {
        return text;
    }
    let mut start = text.len() - max;
    while !text.is_char_boundary(start) {
        start += 1;
    }
    &text[start..]
}

fn tool_from(name: &str) -> Option<ToolKind> {
    match name {
        "claude" => Some(ToolKind::Claude),
        "codex" => Some(ToolKind::Codex),
        _ => None,
    }
}

/// Safe as a file name: what Connecto itself makes.
fn valid_history_id(id: &str) -> bool {
    (1..=80).contains(&id.len())
        && id
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'-')
}

/// Whether `id` has the form [`History::mint`] gives: `h`, the closing time in
/// milliseconds, a dash, then the terminal id.
fn minted_id(id: &str) -> bool {
    valid_history_id(id)
        && id
            .strip_prefix('h')
            .and_then(|rest| rest.split_once('-'))
            .is_some_and(|(time, terminal)| {
                !time.is_empty()
                    && time.bytes().all(|byte| byte.is_ascii_digit())
                    && !terminal.is_empty()
            })
}

pub(crate) struct History {
    dir: PathBuf,
}

impl History {
    pub(crate) fn new(dir: PathBuf) -> Self {
        Self { dir }
    }

    pub(crate) fn dir_of(data_root: &Path, project_id: &str) -> PathBuf {
        data_root.join("projects").join(project_id).join("history")
    }

    fn index(&self) -> PathBuf {
        self.dir.join(INDEX)
    }

    fn write_atomic(path: &Path, text: &str) -> io::Result<()> {
        let staged = path.with_extension("tmp");
        fs::write(&staged, text)?;
        fs::rename(&staged, path)
    }

    /// The saved entries, newest first. An unreadable index is kept aside as
    /// `index.json.bad` and the history starts empty; the second value says so.
    /// An entry that cannot be used is left out and the rest are kept, with a
    /// copy of the index as it was in `index.json.bad`. Transcripts that no
    /// entry names are removed, but only when the index itself was read.
    pub(crate) fn load(&self) -> (Vec<Record>, Option<String>) {
        let text = match fs::read_to_string(self.index()) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return (Vec::new(), None),
            Err(err) => {
                return (
                    Vec::new(),
                    Some(format!("The history could not be read ({err}).")),
                );
            }
        };
        match Self::parse(&text) {
            Some((records, 0)) => {
                self.sweep(&records);
                (records, None)
            }
            Some((records, skipped)) => {
                // The index as it was is kept before it is written without the
                // bad entries. If either step fails the next load tries again.
                let kept_aside = fs::write(self.index().with_extension("json.bad"), &text)
                    .and_then(|()| Self::write_atomic(&self.index(), &Self::render(&records)));
                if kept_aside.is_ok() {
                    self.sweep(&records);
                }
                let entries = if skipped == 1 { "entry" } else { "entries" };
                (
                    records,
                    Some(format!(
                        "{skipped} history {entries} could not be used and were left out. \
                         The history as it was is kept as index.json.bad."
                    )),
                )
            }
            None => {
                let _ = fs::rename(self.index(), self.index().with_extension("json.bad"));
                (
                    Vec::new(),
                    Some("The history was unusable and was kept as index.json.bad.".to_owned()),
                )
            }
        }
    }

    /// The entries of an index, and how many were left out as unusable. `None`
    /// when the file as a whole is not an index.
    fn parse(text: &str) -> Option<(Vec<Record>, usize)> {
        let value: Value = serde_json::from_str(text).ok()?;
        if value["version"].as_u64() != Some(VERSION) {
            return None;
        }
        let items = value["entries"].as_array()?;
        let mut records: Vec<Record> = Vec::new();
        for item in items {
            // An id used twice names one transcript: the first entry keeps it.
            if let Some(record) = Self::parse_entry(item)
                && records
                    .iter()
                    .all(|known| known.entry.id != record.entry.id)
            {
                records.push(record);
            }
        }
        let skipped = items.len() - records.len();
        records.truncate(MAX_ENTRIES);
        Some((records, skipped))
    }

    /// One entry, or `None` when a field it cannot do without is missing.
    fn parse_entry(item: &Value) -> Option<Record> {
        let text = |name: &str| item[name].as_str().map(str::to_owned);
        let id = text("id").filter(|id| valid_history_id(id))?;
        let session = text("session").filter(|s| crate::terminal_list::valid_session(s));
        Some(Record {
            entry: HistoryEntry {
                id,
                terminal_id: text("terminal_id")?,
                name: text("name"),
                tool: tool_from(item["tool"].as_str()?)?,
                parent: text("parent"),
                closed_at_ms: item["closed_at_ms"].as_u64()?,
                task: text("task"),
                last_reply: text("last_reply"),
                resumable: session.is_some(),
            },
            session,
        })
    }

    /// Remove the transcripts that no entry names: what a dropped entry, or a
    /// stop between the two writes of an add, left behind. Only regular files
    /// in the history folder itself, with a name [`History::mint`] makes, are
    /// touched, and a link is never followed.
    fn sweep(&self, records: &[Record]) {
        let is_folder = fs::symlink_metadata(&self.dir).is_ok_and(|meta| meta.is_dir());
        let Some(entries) = is_folder.then(|| fs::read_dir(&self.dir).ok()).flatten() else {
            return;
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(id) = name.to_str().and_then(|name| name.strip_suffix(".txt")) else {
                continue;
            };
            if !minted_id(id) || records.iter().any(|record| record.entry.id == id) {
                continue;
            }
            // `DirEntry::file_type` does not follow a link.
            if entry.file_type().is_ok_and(|kind| kind.is_file()) {
                let _ = fs::remove_file(entry.path());
            }
        }
    }

    fn render(records: &[Record]) -> String {
        let entries: Vec<Value> = records
            .iter()
            .map(|record| {
                let entry = &record.entry;
                let mut value = json!({
                    "id": entry.id,
                    "terminal_id": entry.terminal_id,
                    "tool": entry.tool.name(),
                    "closed_at_ms": entry.closed_at_ms,
                });
                for (name, text) in [
                    ("name", &entry.name),
                    ("parent", &entry.parent),
                    ("task", &entry.task),
                    ("last_reply", &entry.last_reply),
                    ("session", &record.session),
                ] {
                    if let Some(text) = text {
                        value[name] = json!(text);
                    }
                }
                value
            })
            .collect();
        let mut text = serde_json::to_string_pretty(&json!({
            "version": VERSION,
            "entries": entries,
        }))
        .unwrap_or_default();
        text.push('\n');
        text
    }

    /// A history id not used in `records`.
    pub(crate) fn mint(records: &[Record], terminal: &str, closed_ms: u64) -> String {
        let base = format!("h{closed_ms}-{terminal}");
        let mut id = base.clone();
        let mut n = 2_u32;
        while records.iter().any(|record| record.entry.id == id) {
            id = format!("{base}-{n}");
            n += 1;
        }
        id
    }

    /// Save a closed terminal: its transcript first, then the index with the
    /// new entry in front. Past [`MAX_ENTRIES`] the oldest go, with their
    /// transcripts. On `Err` nothing changed in `records`.
    pub(crate) fn add(
        &self,
        records: &mut Vec<Record>,
        record: Record,
        transcript: &str,
    ) -> io::Result<()> {
        fs::create_dir_all(&self.dir)?;
        let file = self.transcript_path(&record.entry.id);
        Self::write_atomic(&file, &transcript_text(transcript))?;
        let mut next = records.clone();
        next.insert(0, record);
        let dropped: Vec<Record> = if next.len() > MAX_ENTRIES {
            next.split_off(MAX_ENTRIES)
        } else {
            Vec::new()
        };
        if let Err(err) = Self::write_atomic(&self.index(), &Self::render(&next)) {
            let _ = fs::remove_file(&file);
            return Err(err);
        }
        for old in dropped {
            let _ = fs::remove_file(self.transcript_path(&old.entry.id));
        }
        *records = next;
        Ok(())
    }

    /// Take an entry back out, with its transcript: the undo of an add whose
    /// terminal then could not be closed. Best effort.
    pub(crate) fn remove(&self, records: &mut Vec<Record>, id: &str) {
        let mut next = records.clone();
        next.retain(|record| record.entry.id != id);
        if Self::write_atomic(&self.index(), &Self::render(&next)).is_ok() {
            *records = next;
            let _ = fs::remove_file(self.transcript_path(id));
        }
    }

    fn transcript_path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.txt"))
    }

    /// A saved transcript. The id is checked, because it makes a file name.
    pub(crate) fn transcript(&self, id: &str) -> io::Result<String> {
        if !valid_history_id(id) {
            return Err(io::Error::other("not a history id"));
        }
        fs::read_to_string(self.transcript_path(id))
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn record(id: &str, at: u64) -> Record {
        Record {
            entry: HistoryEntry {
                id: id.to_owned(),
                terminal_id: "claude".to_owned(),
                name: Some("fix".to_owned()),
                tool: ToolKind::Claude,
                parent: None,
                closed_at_ms: at,
                task: Some("a task".to_owned()),
                last_reply: None,
                resumable: true,
            },
            session: Some("abc-1".to_owned()),
        }
    }

    fn scratch(name: &str) -> History {
        let dir = std::env::temp_dir().join(format!("mg-hist-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        History::new(dir)
    }

    #[test]
    fn an_entry_round_trips_with_its_transcript() {
        let history = scratch("round");
        let mut records = Vec::new();
        history
            .add(&mut records, record("h1-claude", 1), "hello")
            .unwrap();
        history
            .add(&mut records, record("h2-claude", 2), "world")
            .unwrap();
        let (loaded, warning) = history.load();
        assert!(warning.is_none());
        assert_eq!(loaded, records);
        assert_eq!(loaded[0].entry.id, "h2-claude", "newest first");
        assert_eq!(history.transcript("h1-claude").unwrap(), "hello");
        let _ = fs::remove_dir_all(&history.dir);
    }

    #[test]
    fn text_is_redacted_and_cut() {
        let secret = format!("key sk-{}", "a1B2c3D4e5".repeat(3));
        assert!(!transcript_text(&secret).contains("sk-a1B2"));
        assert!(!field(&secret).unwrap().contains("sk-a1B2"));
        let long = "word ".repeat(200);
        assert_eq!(field(&long).unwrap().chars().count(), MAX_FIELD - 1);
        assert_eq!(field("   "), None);
        let huge = "é".repeat(MAX_TRANSCRIPT);
        let kept = transcript_text(&huge);
        assert!(kept.len() <= MAX_TRANSCRIPT);
        assert!(kept.chars().all(|ch| ch == 'é'), "cut on a character");
    }

    #[test]
    fn past_the_cap_the_oldest_go_with_their_transcripts() {
        let history = scratch("cap");
        let mut records = Vec::new();
        for n in 0..MAX_ENTRIES + 3 {
            let id = format!("h{n}-claude");
            history
                .add(&mut records, record(&id, n as u64), "t")
                .unwrap();
        }
        assert_eq!(records.len(), MAX_ENTRIES);
        assert_eq!(
            History::new(history.dir.clone()).load().0.len(),
            MAX_ENTRIES
        );
        assert!(
            history.transcript("h0-claude").is_err(),
            "dropped with its entry"
        );
        assert!(history.transcript("h2-claude").is_err());
        assert!(history.transcript("h3-claude").is_ok());
        let _ = fs::remove_dir_all(&history.dir);
    }

    #[test]
    fn a_bad_entry_is_left_out_and_the_rest_of_the_history_is_kept() {
        let history = scratch("skip");
        let mut records = Vec::new();
        for n in 1..=4 {
            history
                .add(&mut records, record(&format!("h{n}-claude"), n), "text")
                .unwrap();
        }
        // Three entries lose a field each, as a hand edit or an older build
        // might leave them. The fourth is whole.
        let mut value: Value =
            serde_json::from_str(&fs::read_to_string(history.index()).unwrap()).unwrap();
        let entries = value["entries"].as_array_mut().unwrap();
        entries[0].as_object_mut().unwrap().remove("terminal_id");
        entries[1]["tool"] = json!("emacs");
        entries[2]["closed_at_ms"] = json!("yesterday");
        let damaged = serde_json::to_string(&value).unwrap();
        fs::write(history.index(), &damaged).unwrap();

        let (loaded, warning) = history.load();
        let ids: Vec<&str> = loaded.iter().map(|r| r.entry.id.as_str()).collect();
        assert_eq!(ids, ["h1-claude"], "the whole entry survives");
        let warning = warning.unwrap();
        assert!(
            warning.starts_with("3 history entries") && warning.contains("index.json.bad"),
            "{warning}"
        );
        assert_eq!(
            fs::read_to_string(history.dir.join("index.json.bad")).unwrap(),
            damaged,
            "the index as it was is kept"
        );
        assert_eq!(history.transcript("h1-claude").unwrap(), "text");
        assert!(
            history.transcript("h4-claude").is_err(),
            "a dropped entry's transcript goes with it"
        );
        // The index was written without them, so the next load is quiet.
        assert_eq!(history.load(), (loaded, None));
        let _ = fs::remove_dir_all(&history.dir);
    }

    #[test]
    fn transcripts_no_entry_names_are_swept_and_nothing_else_is() {
        let history = scratch("sweep");
        let outside = history.dir.with_extension("outside");
        let _ = fs::remove_dir_all(&outside);
        fs::create_dir_all(&outside).unwrap();
        fs::write(outside.join("kept.txt"), "outside").unwrap();
        let mut records = Vec::new();
        history
            .add(&mut records, record("h1-claude", 1), "kept")
            .unwrap();
        let dir = &history.dir;
        fs::write(dir.join("h7-codex-2.txt"), "orphan").unwrap();
        // None of these is a transcript Connecto made.
        fs::write(dir.join("notes.txt"), "mine").unwrap();
        fs::write(dir.join("h7.txt"), "mine").unwrap();
        fs::write(dir.join("h7-codex.txt.bak"), "mine").unwrap();
        fs::write(dir.join("index.json.bad"), "older").unwrap();
        fs::create_dir_all(dir.join("h8-claude.txt")).unwrap();
        fs::create_dir_all(dir.join("sub")).unwrap();
        fs::write(dir.join("sub/h9-claude.txt"), "deeper").unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(outside.join("kept.txt"), dir.join("h10-claude.txt")).unwrap();

        let (loaded, warning) = history.load();
        assert_eq!((loaded, warning), (records, None));
        assert!(!dir.join("h7-codex-2.txt").exists(), "the orphan is gone");
        assert_eq!(history.transcript("h1-claude").unwrap(), "kept");
        for kept in [
            "notes.txt",
            "h7.txt",
            "h7-codex.txt.bak",
            "index.json.bad",
            "h8-claude.txt",
            "sub/h9-claude.txt",
        ] {
            assert!(dir.join(kept).exists(), "{kept} is not ours to remove");
        }
        #[cfg(unix)]
        assert!(
            fs::symlink_metadata(dir.join("h10-claude.txt")).is_ok(),
            "a link is left alone"
        );
        assert_eq!(
            fs::read_to_string(outside.join("kept.txt")).unwrap(),
            "outside"
        );

        // With no index that could be read, nothing is swept.
        fs::write(dir.join("h7-codex-2.txt"), "orphan").unwrap();
        fs::write(history.index(), "{ nope").unwrap();
        assert!(history.load().1.is_some());
        assert!(dir.join("h7-codex-2.txt").exists());
        assert!(dir.join("h1-claude.txt").exists());
        fs::remove_file(dir.join("index.json.bad")).unwrap();
        assert_eq!(history.load(), (Vec::new(), None), "no index at all");
        assert!(dir.join("h1-claude.txt").exists());
        let _ = fs::remove_dir_all(&history.dir);
        let _ = fs::remove_dir_all(&outside);
    }

    #[test]
    fn a_bad_index_is_kept_aside_and_ids_are_made_unique() {
        let history = scratch("bad");
        fs::create_dir_all(&history.dir).unwrap();
        fs::write(history.index(), "{ nope").unwrap();
        let (records, warning) = history.load();
        assert!(records.is_empty());
        assert!(warning.unwrap().contains("index.json.bad"));
        assert!(history.dir.join("index.json.bad").exists());
        let first = vec![record("h5-claude", 5)];
        assert_eq!(History::mint(&first, "claude", 5), "h5-claude-2");
        assert!(history.transcript("../x").is_err());
        let _ = fs::remove_dir_all(&history.dir);
    }
}
