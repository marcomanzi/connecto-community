//! The models Codex offers, as it lists them itself: `models_cache.json` in
//! its home, which Codex refreshes from its model catalog. Read for the app's
//! model chooser; Codex is never run for it.

use std::path::Path;

use crate::terminal_list::valid_model;

/// The file Codex keeps its model catalog in, in `CODEX_HOME`.
const CACHE: &str = "models_cache.json";

/// The largest cache read.
const MAX_BYTES: u64 = 1024 * 1024;

/// The models Codex shows in its own picker (`visibility` "list"), by its
/// `priority`, from the cache in `codex_home`. `None` when there is no cache,
/// it cannot be read, or it lists none: the caller then has its own list.
pub(crate) fn listed(codex_home: &Path) -> Option<Vec<String>> {
    let path = codex_home.join(CACHE);
    if std::fs::metadata(&path).ok()?.len() > MAX_BYTES {
        return None;
    }
    parse(&std::fs::read_to_string(path).ok()?)
}

fn parse(text: &str) -> Option<Vec<String>> {
    let value: serde_json::Value = serde_json::from_str(text).ok()?;
    let mut listed: Vec<(i64, String)> = value
        .get("models")?
        .as_array()?
        .iter()
        .filter(|model| model.get("visibility").and_then(|v| v.as_str()) == Some("list"))
        .filter_map(|model| {
            let slug = model.get("slug")?.as_str()?;
            // Only a name a launch takes is offered.
            valid_model(slug).then(|| {
                let priority = model
                    .get("priority")
                    .and_then(serde_json::Value::as_i64)
                    .unwrap_or(i64::MAX);
                (priority, slug.to_owned())
            })
        })
        .collect();
    // Stable: equal priorities keep the file's order.
    listed.sort_by_key(|(priority, _)| *priority);
    let mut out: Vec<String> = Vec::with_capacity(listed.len());
    for (_, slug) in listed {
        if !out.contains(&slug) {
            out.push(slug);
        }
    }
    (!out.is_empty()).then_some(out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// The shape Codex 0.166.0 writes, cut down.
    const CACHE_TEXT: &str = r#"{"fetched_at":"2026-11-19T10:00:00Z","client_version":"0.166.0","models":[
        {"slug":"gpt-6.1-sol","display_name":"GPT-6.1-Sol","visibility":"list","priority":5},
        {"slug":"gpt-6.2-astra","display_name":"GPT-6.2-Astra","visibility":"list","priority":2},
        {"slug":"gpt-reserve","display_name":"Reserve","visibility":"hide","priority":4},
        {"slug":"gpt-6.2-sol","display_name":"GPT-6.2-Sol","visibility":"list","priority":1},
        {"slug":"--yolo","visibility":"list","priority":3},
        {"slug":"gpt-5.6-luna","visibility":"list"}
    ]}"#;

    #[test]
    fn listed_models_come_by_priority_without_hidden_or_unlaunchable_ones() {
        assert_eq!(
            parse(CACHE_TEXT).unwrap(),
            [
                "gpt-6.2-sol",
                "gpt-6.2-astra",
                "gpt-6.1-sol",
                "gpt-5.6-luna"
            ]
        );
    }

    #[test]
    fn a_missing_damaged_or_empty_cache_gives_no_list() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(listed(dir.path()), None);
        std::fs::write(dir.path().join(CACHE), "{not json").unwrap();
        assert_eq!(listed(dir.path()), None);
        std::fs::write(dir.path().join(CACHE), r#"{"models":[]}"#).unwrap();
        assert_eq!(listed(dir.path()), None);
        std::fs::write(dir.path().join(CACHE), CACHE_TEXT).unwrap();
        assert_eq!(listed(dir.path()).unwrap()[0], "gpt-6.2-sol");
    }
}
