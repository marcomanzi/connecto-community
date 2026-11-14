//! The history: one JSON object per line in `<data>/automations/history.jsonl`,
//! bounded by lines and by bytes. It holds no API key; screen text is
//! truncated, and the request and response bodies are capped.

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use connecto_core::{AutomationOutcome, HistoryRecord};
use serde_json::{Value, json};

/// Lines kept after a trim.
pub const KEEP: usize = 1000;
/// Lines at which a trim happens.
const TRIM_AT: usize = 1500;
/// The file's size at which a trim happens, and the size it is cut down to.
const MAX_BYTES: u64 = 32 << 20;
const KEEP_BYTES: usize = 16 << 20;
/// Characters of screen text kept per record.
pub const SCREEN_CHARS: usize = 600;
/// Bytes kept of a request or a response.
pub const BODY_CAP: usize = 64 << 10;

pub struct History {
    path: PathBuf,
    lines: usize,
    bytes: u64,
    last_id: u64,
}

fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::SystemTime::UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// Give `each` every line of the file in turn, without its line end, holding
/// one line in memory at a time. Returns how many lines and bytes there were.
/// A file that is missing or stops being readable just ends.
fn each_line(path: &Path, mut each: impl FnMut(&[u8])) -> (usize, u64) {
    let Ok(file) = File::open(path) else {
        return (0, 0);
    };
    let mut reader = BufReader::new(file);
    let mut line = Vec::new();
    let (mut lines, mut bytes) = (0, 0);
    loop {
        line.clear();
        match reader.read_until(b'\n', &mut line) {
            Ok(0) | Err(_) => return (lines, bytes),
            Ok(read) => {
                lines += 1;
                bytes += read as u64;
            }
        }
        let mut text = line.as_slice();
        for end in *b"\n\r" {
            text = text.strip_suffix(&[end]).unwrap_or(text);
        }
        each(text);
    }
}

fn contains(line: &[u8], needle: &[u8]) -> bool {
    !needle.is_empty() && line.windows(needle.len()).any(|w| w == needle)
}

/// One automation's newest `limit` records, newest first, with request and
/// response. It reads the file by itself, a line at a time, so the UI can call
/// it while the engine appends; only this automation's lines are parsed and
/// only the newest `limit` are held. Bad lines are skipped.
#[must_use]
pub fn read_automation(path: &Path, slug: &str, limit: usize) -> Vec<HistoryRecord> {
    let mut newest: VecDeque<HistoryRecord> = VecDeque::new();
    if limit == 0 {
        return Vec::new();
    }
    each_line(path, |line| {
        // A slug is letters, digits and `-`, which JSON writes as they are, so
        // a line without those bytes is another automation's: not worth parsing.
        if !contains(line, slug.as_bytes()) {
            return;
        }
        let Ok(v) = serde_json::from_slice::<Value>(line) else {
            return;
        };
        if v.get("automation").and_then(Value::as_str) != Some(slug) {
            return;
        }
        if let Some(record) = from_json(&v, line, true) {
            if newest.len() == limit {
                newest.pop_back();
            }
            newest.push_front(record);
        }
    });
    newest.into()
}

impl History {
    /// Open the file, reading it once: `each` gets every record, oldest first,
    /// without request and response. Bad lines are skipped.
    #[must_use]
    pub fn load(path: &Path, mut each: impl FnMut(HistoryRecord)) -> Self {
        let mut last_id = 0;
        let (lines, bytes) = each_line(path, |line| {
            let Ok(v) = serde_json::from_slice::<Value>(line) else {
                return;
            };
            if let Some(id) = v.get("id").and_then(Value::as_u64) {
                last_id = last_id.max(id);
            }
            if let Some(record) = from_json(&v, line, false) {
                each(record);
            }
        });
        Self {
            path: path.to_owned(),
            lines,
            bytes,
            last_id,
        }
    }

    #[cfg(test)]
    #[must_use]
    pub fn open(path: &Path) -> Self {
        Self::load(path, |_| {})
    }

    /// An id no earlier entry has, rising with time.
    pub fn next_id(&mut self) -> u64 {
        self.last_id = (self.last_id + 1).max(now_ms().saturating_mul(1000));
        self.last_id
    }

    pub fn append(&mut self, record: &HistoryRecord) -> Result<(), String> {
        if let Some(dir) = self.path.parent() {
            fs::create_dir_all(dir).map_err(|e| e.to_string())?;
        }
        let mut line = to_json(record).to_string();
        line.push('\n');
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)
            .map_err(|e| e.to_string())?;
        file.write_all(line.as_bytes()).map_err(|e| e.to_string())?;
        self.lines += 1;
        self.bytes += line.len() as u64;
        if self.lines > TRIM_AT || self.bytes > MAX_BYTES {
            self.trim();
        }
        Ok(())
    }

    /// Keep the newest lines: at most [`KEEP`], and at most [`KEEP_BYTES`].
    fn trim(&mut self) {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return;
        };
        let all: Vec<&str> = text.lines().collect();
        let mut kept: Vec<&str> = Vec::new();
        let mut size = 0;
        for line in all.iter().rev().take(KEEP) {
            size += line.len() + 1;
            if size > KEEP_BYTES && !kept.is_empty() {
                break;
            }
            kept.push(line);
        }
        kept.reverse();
        let tmp = self.path.with_extension("jsonl.tmp");
        let mut body = kept.join("\n");
        body.push('\n');
        if fs::write(&tmp, &body).is_ok() && fs::rename(&tmp, &self.path).is_ok() {
            self.lines = kept.len();
            self.bytes = body.len() as u64;
        }
    }

    /// Remove the file. If that fails the file and the counts stay as they are.
    pub fn clear(&mut self) -> Result<(), String> {
        match fs::remove_file(&self.path) {
            Ok(()) => {}
            Err(err) if err.kind() == std::io::ErrorKind::NotFound => {}
            Err(err) => return Err(err.to_string()),
        }
        self.lines = 0;
        self.bytes = 0;
        Ok(())
    }

    /// Drop every record of one automation, keeping the rest as they are. The
    /// file is rewritten whole to a temporary file and renamed over the old one,
    /// so a reader never sees half of it; the writer is this same value, so no
    /// append can land in between. Lines that cannot be read are kept.
    pub fn remove_automation(&mut self, slug: &str) -> Result<(), String> {
        let Ok(text) = fs::read_to_string(&self.path) else {
            return Ok(());
        };
        let mut kept = String::new();
        let (mut lines, mut dropped) = (0, 0);
        for line in text.lines() {
            let mine = serde_json::from_str::<Value>(line)
                .ok()
                .and_then(|v| {
                    v.get("automation")
                        .and_then(Value::as_str)
                        .map(|a| a == slug)
                })
                .unwrap_or(false);
            if mine {
                dropped += 1;
            } else {
                kept.push_str(line);
                kept.push('\n');
                lines += 1;
            }
        }
        if dropped == 0 {
            return Ok(());
        }
        let tmp = self.path.with_extension("jsonl.tmp");
        fs::write(&tmp, &kept).map_err(|e| e.to_string())?;
        fs::rename(&tmp, &self.path).map_err(|e| e.to_string())?;
        self.lines = lines;
        self.bytes = kept.len() as u64;
        Ok(())
    }

    /// The newest `limit` records, newest first, without request and response.
    #[cfg(test)]
    #[must_use]
    pub fn recent(&self, limit: usize) -> Vec<HistoryRecord> {
        let mut all = Vec::new();
        let _ = Self::load(&self.path, |record| all.push(record));
        all.reverse();
        all.truncate(limit);
        all
    }

    /// One automation's records, as [`read_automation`] gives them.
    #[cfg(test)]
    #[must_use]
    pub fn for_automation(&self, slug: &str, limit: usize) -> Vec<HistoryRecord> {
        read_automation(&self.path, slug, limit)
    }
}

/// The first `SCREEN_CHARS` characters.
#[must_use]
pub fn truncate(text: &str) -> String {
    text.chars().take(SCREEN_CHARS).collect()
}

/// At most [`BODY_CAP`] bytes, cut at a character and marked.
#[must_use]
pub fn cap_body(text: &str) -> String {
    if text.len() <= BODY_CAP {
        return text.to_owned();
    }
    let mut end = BODY_CAP;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    format!(
        "{}\n...[truncated, {} more bytes]",
        &text[..end],
        text.len() - end
    )
}

fn to_json(r: &HistoryRecord) -> Value {
    let mut v = json!({
        "id": r.id,
        "time_ms": r.time_ms, "project": r.project, "terminal": r.terminal_id,
        "terminal_name": r.terminal_name, "automation": r.automation,
        "answer": r.answer, "confidence": r.confidence, "outcome": r.outcome.word(),
        "message": r.message, "latency_ms": r.latency_ms,
        "input_tokens": r.input_tokens, "output_tokens": r.output_tokens,
        "screen": truncate(&r.screen),
    });
    if let Some(request) = &r.request {
        v["request"] = json!(cap_body(request));
    }
    if let Some(response) = &r.response {
        v["response"] = json!(cap_body(response));
    }
    v
}

/// A stable id for a line that has none (written before ids existed).
fn legacy_id(line: &[u8]) -> u64 {
    line.iter().fold(0xcbf2_9ce4_8422_2325_u64, |hash, byte| {
        (hash ^ u64::from(*byte)).wrapping_mul(0x0100_0000_01b3)
    })
}

fn from_json(v: &Value, line: &[u8], bodies: bool) -> Option<HistoryRecord> {
    let text = |f: &str| v.get(f).and_then(Value::as_str).map(str::to_owned);
    let num = |f: &str| v.get(f).and_then(Value::as_u64).unwrap_or(0);
    Some(HistoryRecord {
        id: v
            .get("id")
            .and_then(Value::as_u64)
            .unwrap_or_else(|| legacy_id(line)),
        time_ms: num("time_ms"),
        project: text("project").unwrap_or_default(),
        terminal_id: text("terminal")?,
        terminal_name: text("terminal_name"),
        automation: text("automation").unwrap_or_default(),
        answer: text("answer"),
        confidence: v.get("confidence").and_then(Value::as_f64),
        outcome: AutomationOutcome::from_word(v.get("outcome")?.as_str()?)?,
        message: text("message"),
        latency_ms: num("latency_ms"),
        input_tokens: num("input_tokens"),
        output_tokens: num("output_tokens"),
        screen: text("screen").unwrap_or_default(),
        request: if bodies { text("request") } else { None },
        response: if bodies { text("response") } else { None },
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn record(n: u64) -> HistoryRecord {
        HistoryRecord {
            time_ms: n,
            project: "p".into(),
            terminal_id: "claude".into(),
            terminal_name: None,
            automation: "a".into(),
            answer: Some("yes".into()),
            confidence: Some(0.9),
            outcome: AutomationOutcome::Fired,
            message: None,
            latency_ms: 5,
            input_tokens: 1,
            output_tokens: 0,
            screen: "x".repeat(2000),
            ..HistoryRecord::default()
        }
    }

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("connecto-hist-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn records_round_trip_newest_first_with_truncated_screens() {
        let d = dir("rt");
        let mut h = History::open(&d.join("history.jsonl"));
        h.append(&record(1)).unwrap();
        h.append(&record(2)).unwrap();
        let got = h.recent(10);
        assert_eq!(got.iter().map(|r| r.time_ms).collect::<Vec<_>>(), [2, 1]);
        assert_eq!(got[0].screen.chars().count(), SCREEN_CHARS);
        assert_eq!(got[0].outcome, AutomationOutcome::Fired);
        h.clear().unwrap();
        assert!(h.recent(10).is_empty());
        // Clearing what is not there is fine; a file that cannot be removed is told.
        h.clear().unwrap();
        fs::create_dir_all(d.join("history.jsonl")).unwrap();
        assert!(h.clear().is_err());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn one_pass_gives_the_counts_the_last_id_and_every_record_oldest_first() {
        let d = dir("load");
        let path = d.join("history.jsonl");
        let mut h = History::open(&path);
        let mut last = 0;
        for n in 0..5 {
            let mut r = record(n);
            r.id = h.next_id();
            last = r.id;
            r.request = Some("req".into());
            h.append(&r).unwrap();
        }
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n\xff\xfe not text\n")
            .unwrap();
        let mut seen = Vec::new();
        let again = History::load(&path, |r| seen.push(r));
        assert_eq!(
            seen.iter().map(|r| r.time_ms).collect::<Vec<_>>(),
            [0, 1, 2, 3, 4]
        );
        assert!(seen.iter().all(|r| r.request.is_none()));
        assert_eq!((again.last_id, again.lines), (last, 7));
        assert_eq!(again.bytes, fs::metadata(&path).unwrap().len());
        // A missing file is an empty history.
        let mut records = 0;
        let none = History::load(&d.join("nope.jsonl"), |_| records += 1);
        assert_eq!(
            (none.last_id, none.lines, none.bytes, records),
            (0, 0, 0, 0)
        );
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn one_automations_history_is_its_newest_records_only() {
        let d = dir("for");
        let path = d.join("history.jsonl");
        let mut h = History::open(&path);
        for n in 0..10 {
            let mut r = record(n);
            r.automation = if n % 2 == 0 { "a" } else { "ab" }.into();
            r.request = Some(format!("request {n}"));
            // Another automation's record may name this one in its text.
            r.message = Some("about a and ab".into());
            h.append(&r).unwrap();
        }
        let got = read_automation(&path, "a", 3);
        assert_eq!(got.iter().map(|r| r.time_ms).collect::<Vec<_>>(), [8, 6, 4]);
        assert_eq!(got[0].request.as_deref(), Some("request 8"));
        assert_eq!(read_automation(&path, "ab", 100).len(), 5);
        assert!(read_automation(&path, "b", 100).is_empty());
        assert!(read_automation(&path, "a", 0).is_empty());
        assert!(read_automation(&d.join("nope.jsonl"), "a", 5).is_empty());
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn the_file_is_bounded() {
        let d = dir("bound");
        let path = d.join("history.jsonl");
        let mut h = History::open(&path);
        for n in 0..=(TRIM_AT as u64 + 1) {
            h.append(&record(n)).unwrap();
        }
        let lines = fs::read_to_string(&path).unwrap().lines().count();
        assert!(lines <= KEEP + 1, "{lines}");
        assert_eq!(h.recent(1)[0].time_ms, TRIM_AT as u64 + 1);
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn ids_are_stable_bodies_are_capped_and_the_file_is_bounded_by_size() {
        let d = dir("ids");
        let path = d.join("history.jsonl");
        // A line from before ids existed gets the same id every time it is read.
        fs::write(
            &path,
            "{\"time_ms\":1,\"terminal\":\"c\",\"automation\":\"a\",\"outcome\":\"no-match\"}\n",
        )
        .unwrap();
        let first = History::open(&path).recent(1)[0].id;
        assert_eq!(History::open(&path).recent(1)[0].id, first);
        let mut h = History::open(&path);
        let (a, b) = (h.next_id(), h.next_id());
        assert!(b > a);
        let mut big = record(2);
        big.id = a;
        big.request = Some("r".repeat(BODY_CAP * 2));
        big.response = Some("é".repeat(BODY_CAP));
        h.append(&big).unwrap();
        let got = &h.for_automation("a", 5)[0];
        assert_eq!(got.id, a, "the id is stored");
        for body in [
            got.request.as_ref().unwrap(),
            got.response.as_ref().unwrap(),
        ] {
            assert!(body.len() <= BODY_CAP + 64 && body.contains("truncated"));
        }
        assert!(
            h.recent(5)
                .iter()
                .all(|r| r.request.is_none() && r.response.is_none())
        );
        // Lots of big entries cut the file down by size, not just by lines.
        for n in 0..(MAX_BYTES as usize / (2 * BODY_CAP) + 20) {
            let mut r = record(10 + n as u64);
            r.request = Some("x".repeat(BODY_CAP));
            r.response = Some("y".repeat(BODY_CAP));
            h.append(&r).unwrap();
        }
        assert!(fs::metadata(&path).unwrap().len() <= MAX_BYTES + 4 * BODY_CAP as u64);
        assert_eq!(
            h.recent(1)[0].time_ms,
            10 + (MAX_BYTES as usize / (2 * BODY_CAP) + 19) as u64
        );
        let _ = fs::remove_dir_all(d);
    }

    #[test]
    fn removing_an_automation_rewrites_the_file_without_it_and_keeps_the_rest() {
        let d = dir("remove");
        let path = d.join("history.jsonl");
        let mut h = History::open(&path);
        for (n, slug) in ["a", "b", "a", "c"].into_iter().enumerate() {
            let mut r = record(n as u64);
            r.automation = slug.into();
            r.request = Some("req".into());
            h.append(&r).unwrap();
        }
        fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"not json\n")
            .unwrap();
        h.remove_automation("a").unwrap();
        let text = fs::read_to_string(&path).unwrap();
        assert_eq!(text.lines().count(), 3, "b, c and the unreadable line");
        assert!(text.contains("not json") && !text.contains("\"automation\":\"a\""));
        assert!(!d.join("history.jsonl.tmp").exists());
        // Appending carries on after it, and nothing to remove leaves the file be.
        h.append(&record(9)).unwrap();
        let before = fs::read_to_string(&path).unwrap();
        h.remove_automation("zzz").unwrap();
        assert_eq!(fs::read_to_string(&path).unwrap(), before);
        let _ = fs::remove_dir_all(d);
    }
}
