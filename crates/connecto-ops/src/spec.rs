//! `op.json`: one operation's description (spec §3). Unknown fields are an
//! error, so a typo is never silently ignored.

use connecto_core::ops::{Danger, OpArg, OpKind};
use serde_json::{Map, Value};

pub const FILE: &str = "op.json";
pub const SCRIPT: &str = "run.sh";
pub const RECIPE: &str = "recipe.md";

/// Longest `op.json`, script and recipe, in bytes.
pub const MAX_SPEC: usize = 16 * 1024;
pub const MAX_SCRIPT: usize = 64 * 1024;
pub const MAX_RECIPE: usize = 8 * 1024;

pub const DEFAULT_TIMEOUT_S: u64 = 600;
pub const MAX_TIMEOUT_S: u64 = 3600;
const MAX_PHRASES: usize = 12;
const MAX_ARGS: usize = 6;

const FIELDS: [&str; 10] = [
    "version",
    "name",
    "description",
    "phrases",
    "kind",
    "danger",
    "confirm",
    "args",
    "timeout_s",
    "$comment",
];

/// A checked `op.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct OpSpec {
    pub name: String,
    pub description: String,
    pub phrases: Vec<String>,
    pub kind: OpKind,
    pub danger: Danger,
    /// As written; see [`OpSpec::asks_first`].
    pub confirm: bool,
    pub args: Vec<OpArg>,
    pub timeout_s: u64,
}

impl OpSpec {
    /// Whether it asks the person before it runs: `confirm`, or a danger
    /// that always asks whatever the file says.
    #[must_use]
    pub fn asks_first(&self) -> bool {
        self.confirm || self.danger.always_asks()
    }

    /// The file that goes with this kind.
    #[must_use]
    pub fn body_file(&self) -> &'static str {
        body_file(self.kind)
    }
}

#[must_use]
pub fn body_file(kind: OpKind) -> &'static str {
    match kind {
        OpKind::Script | OpKind::ScriptLlm => SCRIPT,
        OpKind::Recipe => RECIPE,
    }
}

/// Whether `slug` names an operation folder: 1 to 40 of `a-z 0-9 -`,
/// starting with a letter or digit.
#[must_use]
pub fn valid_slug(slug: &str) -> bool {
    (1..=40).contains(&slug.len())
        && slug
            .bytes()
            .all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-')
        && !slug.starts_with('-')
}

fn one_line(value: &Value, field: &str, min: usize, max: usize) -> Result<String, String> {
    let text = value
        .as_str()
        .ok_or_else(|| format!("`{field}` must be text"))?;
    let text = text.trim();
    let count = text.chars().count();
    if count < min || count > max {
        return Err(format!("`{field}` must be {min} to {max} characters"));
    }
    if text.chars().any(char::is_control) {
        return Err(format!("`{field}` must be one line"));
    }
    Ok(text.to_owned())
}

fn arg(value: &Value, at: usize) -> Result<OpArg, String> {
    let map = value
        .as_object()
        .ok_or_else(|| format!("`args[{at}]` must be an object"))?;
    for key in map.keys() {
        if !matches!(key.as_str(), "name" | "description" | "required") {
            return Err(format!("`args[{at}]` has an unknown field `{key}`"));
        }
    }
    let name = one_line(map.get("name").unwrap_or(&Value::Null), "args.name", 1, 32)?;
    if !name
        .bytes()
        .all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-')
    {
        return Err(format!(
            "`args[{at}].name` may hold only letters, digits, `_` and `-`"
        ));
    }
    let description = match map.get("description") {
        None => String::new(),
        Some(v) => one_line(v, "args.description", 0, 200)?,
    };
    let required = match map.get("required") {
        None => false,
        Some(v) => v
            .as_bool()
            .ok_or_else(|| format!("`args[{at}].required` must be true or false"))?,
    };
    Ok(OpArg {
        name,
        description,
        required,
    })
}

/// Parse and check `op.json`.
///
/// # Errors
///
/// What is wrong, in one sentence.
pub fn parse(text: &str) -> Result<OpSpec, String> {
    if text.len() > MAX_SPEC {
        return Err("op.json is over 16 KiB".to_owned());
    }
    let value: Value =
        serde_json::from_str(text).map_err(|err| format!("op.json is not valid JSON: {err}"))?;
    let map: &Map<String, Value> = value
        .as_object()
        .ok_or("op.json must hold one JSON object")?;
    for key in map.keys() {
        if !FIELDS.contains(&key.as_str()) {
            return Err(format!("op.json has an unknown field `{key}`"));
        }
    }
    let field = |name: &str| map.get(name).unwrap_or(&Value::Null);
    if field("version").as_u64() != Some(1) {
        return Err("`version` must be 1".to_owned());
    }
    let name = one_line(field("name"), "name", 1, 60)?;
    let description = one_line(field("description"), "description", 1, 300)?;
    let phrases = field("phrases")
        .as_array()
        .ok_or("`phrases` must be a list of text")?
        .iter()
        .map(|p| one_line(p, "phrases", 2, 80))
        .collect::<Result<Vec<_>, _>>()?;
    if phrases.is_empty() || phrases.len() > MAX_PHRASES {
        return Err(format!("`phrases` must hold 1 to {MAX_PHRASES} entries"));
    }
    let kind = field("kind")
        .as_str()
        .and_then(OpKind::from_word)
        .ok_or("`kind` must be \"script\", \"script+llm\" or \"recipe\"")?;
    let danger = field("danger")
        .as_str()
        .and_then(Danger::from_word)
        .ok_or("`danger` must be \"none\", \"changes\", \"deploys\" or \"deletes\"")?;
    let confirm = match map.get("confirm") {
        None => false,
        Some(v) => v.as_bool().ok_or("`confirm` must be true or false")?,
    };
    let args = match map.get("args") {
        None => Vec::new(),
        Some(v) => v
            .as_array()
            .ok_or("`args` must be a list")?
            .iter()
            .enumerate()
            .map(|(at, a)| arg(a, at))
            .collect::<Result<Vec<_>, _>>()?,
    };
    if args.len() > MAX_ARGS {
        return Err(format!("`args` may hold at most {MAX_ARGS} entries"));
    }
    let mut seen_optional = false;
    for a in &args {
        if a.required && seen_optional {
            return Err("a required argument cannot follow an optional one".to_owned());
        }
        seen_optional |= !a.required;
    }
    let timeout_s = match map.get("timeout_s") {
        None => DEFAULT_TIMEOUT_S,
        Some(v) => v
            .as_u64()
            .filter(|t| (1..=MAX_TIMEOUT_S).contains(t))
            .ok_or(format!("`timeout_s` must be 1 to {MAX_TIMEOUT_S}"))?,
    };
    Ok(OpSpec {
        name,
        description,
        phrases,
        kind,
        danger,
        confirm,
        args,
        timeout_s,
    })
}

/// Check the script or recipe that goes with `spec`.
///
/// # Errors
///
/// What is wrong, in one sentence.
pub fn check_body(spec: &OpSpec, body: &[u8]) -> Result<(), String> {
    let (max, what) = match spec.kind {
        OpKind::Recipe => (MAX_RECIPE, "recipe.md is over 8 KiB"),
        _ => (MAX_SCRIPT, "run.sh is over 64 KiB"),
    };
    if body.len() > max {
        return Err(what.to_owned());
    }
    let text =
        std::str::from_utf8(body).map_err(|_| format!("{} is not UTF-8", spec.body_file()))?;
    if text.trim().is_empty() {
        return Err(format!("{} is empty", spec.body_file()));
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const GOOD: &str = r#"{
      "version": 1,
      "name": "Deploy",
      "description": "Builds the site and deploys it.",
      "phrases": ["deploy", "ship it to production"],
      "kind": "script",
      "danger": "deploys",
      "args": [{"name": "target", "description": "staging or production"}],
      "timeout_s": 900
    }"#;

    #[test]
    fn a_good_file_parses() {
        let spec = parse(GOOD).unwrap();
        assert_eq!(spec.name, "Deploy");
        assert_eq!(spec.kind, OpKind::Script);
        assert_eq!(spec.danger, Danger::Deploys);
        assert!(!spec.confirm);
        assert!(spec.asks_first(), "deploys always asks");
        assert_eq!(spec.args.len(), 1);
        assert!(!spec.args[0].required);
        assert_eq!(spec.timeout_s, 900);
        assert_eq!(spec.body_file(), SCRIPT);
    }

    #[test]
    fn defaults_apply() {
        let spec = parse(
            r#"{"version":1,"name":"Tests","description":"Runs the tests.","phrases":["run the tests"],"kind":"script","danger":"none"}"#,
        )
        .unwrap();
        assert_eq!(spec.timeout_s, DEFAULT_TIMEOUT_S);
        assert!(!spec.asks_first());
        assert!(spec.args.is_empty());
    }

    #[test]
    fn mistakes_are_refused() {
        let base: Value = serde_json::from_str(GOOD).unwrap();
        let with = |key: &str, value: Value| {
            let mut v = base.clone();
            v[key] = value;
            v.to_string()
        };
        for (text, why) in [
            (with("extra", Value::Bool(true)), "unknown field"),
            (with("version", 2.into()), "version"),
            (with("name", "".into()), "name"),
            (with("name", "a\nb".into()), "one line"),
            (with("phrases", serde_json::json!([])), "phrases"),
            (with("phrases", serde_json::json!(["x"])), "phrases"),
            (with("kind", "bash".into()), "kind"),
            (with("danger", "some".into()), "danger"),
            (with("confirm", "yes".into()), "confirm"),
            (with("timeout_s", 0.into()), "timeout_s"),
            (with("timeout_s", 4000.into()), "timeout_s"),
            (
                with(
                    "args",
                    serde_json::json!([{"name":"a"},{"name":"b","required":true}]),
                ),
                "required",
            ),
            (with("args", serde_json::json!([{"name":"a b"}])), "letters"),
            (
                with("args", serde_json::json!([{"name":"a","x":1}])),
                "unknown",
            ),
            ("[]".to_owned(), "object"),
            ("{".to_owned(), "JSON"),
        ] {
            let err = parse(&text).unwrap_err();
            assert!(err.contains(why), "{why}: {err}");
        }
    }

    #[test]
    fn slugs() {
        for good in ["deploy", "run-tests", "a", "0x", "a-b-c"] {
            assert!(valid_slug(good), "{good}");
        }
        for bad in [
            "",
            "-x",
            "Deploy",
            "a_b",
            "a/b",
            "..",
            ".discovery",
            &"a".repeat(41),
        ] {
            assert!(!valid_slug(bad), "{bad}");
        }
    }

    #[test]
    fn bodies() {
        let spec = parse(GOOD).unwrap();
        assert!(check_body(&spec, b"#!/bin/bash\necho hi\n").is_ok());
        assert!(check_body(&spec, b"  \n").is_err());
        assert!(check_body(&spec, &[0xff, 0xfe]).is_err());
        assert!(check_body(&spec, &vec![b'a'; MAX_SCRIPT + 1]).is_err());
    }
}
