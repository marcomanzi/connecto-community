//! Which tool session a package's authoring terminal ran, kept beside the
//! package (never inside `package.json`) in `.session.json`, so the
//! conversation can be picked up again.

use std::fs;
use std::path::Path;

use serde_json::{Value, json};

const FILE: &str = ".session.json";

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub terminal: String,
    pub session: String,
    /// The guide version the session was started or resumed under.
    pub guide: Option<u32>,
}

/// Letters, digits, `-` and `_`, starting with a letter or digit: it goes on a
/// command line, so never an option such as `--last`.
fn valid_session_id(id: &str) -> bool {
    id.chars().next().is_some_and(|c| c.is_ascii_alphanumeric())
        && id.len() <= 128
        && id
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

#[must_use]
pub fn read(dir: &Path) -> Option<Record> {
    let value: Value = serde_json::from_str(&fs::read_to_string(dir.join(FILE)).ok()?).ok()?;
    if value.get("tool").and_then(Value::as_str)? != "claude" {
        return None;
    }
    let text = |f: &str| value.get(f).and_then(Value::as_str).map(str::to_owned);
    Some(Record {
        terminal: text("terminal").unwrap_or_default(),
        session: text("session").filter(|id| valid_session_id(id))?,
        guide: value
            .get("guide")
            .and_then(Value::as_u64)
            .and_then(|v| u32::try_from(v).ok()),
    })
}

pub fn write(dir: &Path, record: &Record) -> Result<(), String> {
    if !valid_session_id(&record.session) {
        return Err("not a session id".to_owned());
    }
    let body = json!({
        "version": 1,
        "tool": "claude",
        "terminal": record.terminal,
        "session": record.session,
        "guide": record.guide,
    });
    crate::fsx::write_atomic(dir, FILE, body.to_string().as_bytes(), None)
        .map_err(|e| e.to_string())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_record_round_trips_and_a_bad_id_is_refused() {
        let dir = std::env::temp_dir().join(format!("mg-pkgsession-{}", std::process::id()));
        fs::create_dir_all(&dir).unwrap();
        let record = Record {
            terminal: "claude-2".into(),
            session: "abc-123".into(),
            guide: Some(1),
        };
        write(&dir, &record).unwrap();
        assert_eq!(read(&dir), Some(record));
        let bad = Record {
            session: "--last".into(),
            terminal: String::new(),
            guide: None,
        };
        assert!(write(&dir, &bad).is_err());
        let _ = fs::remove_dir_all(&dir);
    }
}
