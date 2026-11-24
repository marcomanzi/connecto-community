//! `vault.json`: names, scopes, descriptions, bindings and times. No values.
//!
//! ```json
//! {"version": 1, "secrets": [
//!   {"name": "GITHUB", "scope": "global", "description": "", "updated": 1763500000,
//!    "bindings": [{"http": {"base_url": "https://api.github.com", "header": "Authorization", "prefix": "Bearer "}}]},
//!   {"name": "AWS", "scope": {"project": "0f8bb1bcbbc9990b"}, "updated": 1763500000, "last_used": 1763500100,
//!    "bindings": [{"command": {"programs": ["/opt/homebrew/bin/aws"], "env": "AWS_SECRET_ACCESS_KEY"}}]}
//! ]}
//! ```

use std::path::Path;

use connecto_core::{Binding, ProviderScope, SecretEntry};
use serde_json::{Value, json};

const VERSION: u64 = 1;

/// The entries in the file, `None` when there is no file.
///
/// # Errors
/// When the file cannot be read or is not a vault this version knows. The
/// caller then never writes it.
pub(crate) fn read(path: &Path) -> Result<Option<Vec<SecretEntry>>, String> {
    let text = match std::fs::read_to_string(path) {
        Ok(text) => text,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(err) => return Err(format!("{} could not be read: {err}", path.display())),
    };
    parse(&text).map(Some).map_err(|why| {
        format!(
            "{} is not usable ({why}); it was left as it is",
            path.display()
        )
    })
}

pub(crate) fn write(path: &Path, entries: &[SecretEntry]) -> Result<(), String> {
    let mut text = serde_json::to_string_pretty(&to_json(entries)).unwrap_or_default();
    text.push('\n');
    connecto_secrets::write_private(path, text.as_bytes())
        .map_err(|err| format!("{} could not be written: {err}", path.display()))
}

fn to_json(entries: &[SecretEntry]) -> Value {
    let secrets: Vec<Value> = entries
        .iter()
        .map(|entry| {
            let mut value = json!({
                "name": entry.name,
                "scope": super::scope_json(&entry.scope),
                "description": entry.description,
                "updated": entry.updated_unix,
                "bindings": entry.bindings.iter().map(binding_json).collect::<Vec<_>>(),
            });
            if let Some(used) = entry.last_used_unix {
                value["last_used"] = json!(used);
            }
            value
        })
        .collect();
    json!({ "version": VERSION, "secrets": secrets })
}

fn binding_json(binding: &Binding) -> Value {
    match binding {
        Binding::Http {
            base_url,
            header,
            prefix,
        } => json!({ "http": { "base_url": base_url, "header": header, "prefix": prefix } }),
        Binding::Command { programs, env } => {
            json!({ "command": { "programs": programs, "env": env } })
        }
        Binding::AgenticWorks => json!({ "agentic_works": {} }),
        Binding::GoogleServiceAccount => json!({ "google_service_account": {} }),
        Binding::Ssh => json!({ "ssh": {} }),
    }
}

fn text(value: &Value, field: &str) -> Result<String, String> {
    value
        .get(field)
        .and_then(Value::as_str)
        .map(str::to_owned)
        .ok_or_else(|| format!("`{field}` is missing"))
}

fn parse(text_in: &str) -> Result<Vec<SecretEntry>, String> {
    let value: Value = serde_json::from_str(text_in).map_err(|err| err.to_string())?;
    if value.get("version").and_then(Value::as_u64) != Some(VERSION) {
        return Err("an unknown version".to_owned());
    }
    let list = value
        .get("secrets")
        .and_then(Value::as_array)
        .ok_or("`secrets` is missing")?;
    let mut out = Vec::with_capacity(list.len());
    for item in list {
        let name = text(item, "name")?;
        connecto_core::vault::check_name(&name)?;
        let scope = match item.get("scope") {
            Some(Value::String(word)) if word == "global" => ProviderScope::Global,
            Some(other) => ProviderScope::Project(
                other
                    .get("project")
                    .and_then(Value::as_str)
                    .ok_or("a scope is not understood")?
                    .to_owned(),
            ),
            None => return Err("`scope` is missing".to_owned()),
        };
        let bindings = item
            .get("bindings")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .map(parse_binding)
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        if out
            .iter()
            .any(|seen: &SecretEntry| seen.name == name && seen.scope == scope)
        {
            return Err(format!("{name} is there twice"));
        }
        out.push(SecretEntry {
            name,
            scope,
            description: item
                .get("description")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
            bindings,
            updated_unix: item.get("updated").and_then(Value::as_u64).unwrap_or(0),
            last_used_unix: item.get("last_used").and_then(Value::as_u64),
            public_key: None,
        });
    }
    Ok(out)
}

/// A binding this version does not know, or one that no longer passes the
/// checks, makes the file unusable rather than being dropped on the next
/// write.
fn parse_binding(value: &Value) -> Result<Binding, String> {
    let binding = if let Some(http) = value.get("http") {
        Binding::Http {
            base_url: text(http, "base_url")?,
            header: text(http, "header")?,
            prefix: http
                .get("prefix")
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned(),
        }
    } else if let Some(command) = value.get("command") {
        Binding::Command {
            programs: command
                .get("programs")
                .and_then(Value::as_array)
                .ok_or("`programs` is missing")?
                .iter()
                .map(|p| p.as_str().map(str::to_owned).ok_or("a program is not text"))
                .collect::<Result<Vec<_>, _>>()?,
            env: text(command, "env")?,
        }
    } else if value.get("agentic_works").is_some() {
        Binding::AgenticWorks
    } else if value.get("google_service_account").is_some() {
        Binding::GoogleServiceAccount
    } else if value.get("ssh").is_some() {
        Binding::Ssh
    } else {
        return Err("a binding of an unknown kind".to_owned());
    };
    crate::check_binding(&binding)?;
    Ok(binding)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn entries_survive_a_round_trip() {
        let entries = vec![
            SecretEntry {
                name: "GITHUB".into(),
                scope: ProviderScope::Global,
                description: "for gh".into(),
                bindings: vec![Binding::Http {
                    base_url: "https://api.github.com".into(),
                    header: "Authorization".into(),
                    prefix: "Bearer ".into(),
                }],
                updated_unix: 5,
                last_used_unix: Some(6),
                public_key: None,
            },
            SecretEntry {
                name: "AWS".into(),
                scope: ProviderScope::Project("0f8bb1bcbbc9990b".into()),
                description: String::new(),
                bindings: vec![Binding::Command {
                    programs: vec!["/opt/homebrew/bin/aws".into()],
                    env: "AWS_SECRET_ACCESS_KEY".into(),
                }],
                updated_unix: 7,
                last_used_unix: None,
                public_key: None,
            },
        ];
        let text = serde_json::to_string(&to_json(&entries)).unwrap();
        assert_eq!(parse(&text).unwrap(), entries);
    }

    #[test]
    fn an_unknown_version_or_binding_is_refused() {
        assert!(parse(r#"{"version":2,"secrets":[]}"#).is_err());
        assert!(
            parse(
                r#"{"version":1,"secrets":[{"name":"A","scope":"global","bindings":[{"ftp":{}}]}]}"#
            )
            .is_err()
        );
    }

    #[test]
    fn a_name_twice_in_one_scope_is_refused() {
        let one = r#"{"name":"A","scope":"global"}"#;
        assert!(parse(&format!(r#"{{"version":1,"secrets":[{one},{one}]}}"#)).is_err());
    }
}
