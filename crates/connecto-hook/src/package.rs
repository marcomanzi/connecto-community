//! A package's `package.json` and its offline validation. Shared by the app's
//! package manager and by `connecto package check`, like [`crate::automation`].
//!
//! A package is a folder `<home>/packages/<id>/` with a `package.json` and the
//! standard tool folders: `.claude/` (skills, agents, commands, hooks,
//! `settings.json`), `.codex/` and `.agents/`.

use std::fmt::Write as _;
use std::path::Path;

use serde_json::{Map, Value, json};

pub const FILE_NAME: &str = "package.json";
/// The folders whose files a package installs into a project.
pub const TOOL_DIRS: [&str; 3] = [".claude", ".codex", ".agents"];
pub const MAX_NAME_CHARS: usize = crate::name::MAX_NAME_CHARS;
pub const MAX_DESCRIPTION_CHARS: usize = 500;
/// The packages built into Connecto: its team workflows, and Game Dev (Godot).
/// A package may extend them though no folder holds them.
pub const BUILTIN_IDS: [&str; 2] = ["connecto", "game-dev"];

/// `package.json`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Manifest {
    pub name: String,
    pub description: String,
    /// Starts at 1. Connecto raises it by one whenever the files change.
    pub version: u32,
    /// The id of the package this one builds on.
    pub extends: Option<String>,
    /// The tools it needs on the machine (ADR 0037). A project cannot add the
    /// package while one is missing.
    pub requires: Vec<crate::requires::Requirement>,
}

/// A package id is its folder name: lowercase letters, digits and `-`.
#[must_use]
pub fn valid_id(id: &str) -> bool {
    crate::name::valid_slug(id)
}

/// A folder name for a package called `name`.
#[must_use]
pub fn slugify(name: &str) -> String {
    crate::name::slugify(name, "package")
}

pub use crate::name::check_name;

impl Manifest {
    /// Every problem found, or the manifest.
    pub fn parse(text: &str) -> Result<Self, Vec<String>> {
        let value: Value =
            serde_json::from_str(text).map_err(|err| vec![format!("not valid JSON: {err}")])?;
        let Some(object) = value.as_object() else {
            return Err(vec!["the file must be a JSON object".to_owned()]);
        };
        let mut errors = Vec::new();
        for key in object.keys() {
            if !["name", "description", "version", "extends", "requires"].contains(&key.as_str()) {
                errors.push(format!("unknown field {key:?}"));
            }
        }
        let name = match object.get("name").and_then(Value::as_str) {
            Some(name) => match check_name(name) {
                Ok(name) => name.to_owned(),
                Err(why) => {
                    errors.push(format!("\"name\": {why}"));
                    String::new()
                }
            },
            None => {
                errors.push("\"name\" is required (text)".to_owned());
                String::new()
            }
        };
        let description = match object.get("description") {
            None => String::new(),
            Some(Value::String(text)) if text.chars().count() <= MAX_DESCRIPTION_CHARS => {
                text.trim().to_owned()
            }
            Some(_) => {
                errors.push(format!(
                    "\"description\" must be text of at most {MAX_DESCRIPTION_CHARS} characters"
                ));
                String::new()
            }
        };
        let version = match object.get("version") {
            None => 1,
            Some(v) => match v.as_u64().and_then(|n| u32::try_from(n).ok()) {
                Some(n) if n >= 1 => n,
                _ => {
                    errors.push("\"version\" must be a whole number, 1 or more".to_owned());
                    1
                }
            },
        };
        let extends = match object.get("extends") {
            None | Some(Value::Null) => None,
            Some(Value::String(id)) if valid_id(id) => Some(id.clone()),
            Some(_) => {
                errors.push(
                    "\"extends\" must be a package id (lowercase letters, digits, `-`)".to_owned(),
                );
                None
            }
        };
        let requires = object
            .get("requires")
            .map(|v| crate::requires::parse_list(v, &mut errors))
            .unwrap_or_default();
        if errors.is_empty() {
            Ok(Self {
                name,
                description,
                version,
                extends,
                requires,
            })
        } else {
            Err(errors)
        }
    }

    #[must_use]
    pub fn to_json(&self) -> Value {
        let mut object = Map::new();
        object.insert("name".into(), json!(self.name));
        object.insert("description".into(), json!(self.description));
        object.insert("version".into(), json!(self.version));
        if let Some(base) = &self.extends {
            object.insert("extends".into(), json!(base));
        }
        if !self.requires.is_empty() {
            let list = self
                .requires
                .iter()
                .map(crate::requires::Requirement::to_json);
            object.insert("requires".into(), Value::Array(list.collect()));
        }
        Value::Object(object)
    }

    #[must_use]
    pub fn to_text(&self) -> String {
        let mut text = serde_json::to_string_pretty(&self.to_json()).unwrap_or_default();
        text.push('\n');
        text
    }
}

/// The front matter keys of a Markdown file (`---` block at the top), as
/// `key: value` lines. `None` when there is no block.
fn front_matter(text: &str) -> Option<Vec<(String, String)>> {
    let rest = text.strip_prefix("---\n")?;
    let (block, _) = rest.split_once("\n---")?;
    Some(
        block
            .lines()
            .filter_map(|line| line.split_once(':'))
            .map(|(k, v)| (k.trim().to_owned(), v.trim().to_owned()))
            .collect(),
    )
}

fn files_in(dir: &Path) -> Vec<std::path::PathBuf> {
    let mut found: Vec<_> = std::fs::read_dir(dir)
        .into_iter()
        .flatten()
        .flatten()
        .map(|entry| entry.path())
        .collect();
    found.sort();
    found
}

/// Problems and notes about the layout of a package folder.
fn layout_report(dir: &Path, problems: &mut Vec<String>, notes: &mut Vec<String>) {
    let mut any_files = false;
    for tool_dir in TOOL_DIRS {
        let base = dir.join(tool_dir);
        if !base.is_dir() {
            continue;
        }
        any_files |= !files_in(&base).is_empty();
    }
    if !any_files {
        problems.push("no files: add at least one of .claude/, .codex/ or .agents/".to_owned());
    }
    for skills in [dir.join(".claude/skills"), dir.join(".agents/skills")] {
        for skill in files_in(&skills).into_iter().filter(|p| p.is_dir()) {
            let shown = skill
                .strip_prefix(dir)
                .unwrap_or(&skill)
                .display()
                .to_string();
            match std::fs::read_to_string(skill.join("SKILL.md")) {
                Err(_) => problems.push(format!("{shown}: SKILL.md is missing")),
                Ok(text) => {
                    let keys = front_matter(&text).unwrap_or_default();
                    for need in ["name", "description"] {
                        if !keys.iter().any(|(k, v)| k == need && !v.is_empty()) {
                            problems.push(format!("{shown}/SKILL.md: front matter lacks `{need}`"));
                        }
                    }
                }
            }
        }
    }
    for agents in [dir.join(".claude/agents")] {
        for file in files_in(&agents).into_iter().filter(|p| p.is_file()) {
            let shown = file
                .strip_prefix(dir)
                .unwrap_or(&file)
                .display()
                .to_string();
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            let keys = front_matter(&text).unwrap_or_default();
            if !keys.iter().any(|(k, v)| k == "name" && !v.is_empty()) {
                problems.push(format!("{shown}: front matter lacks `name`"));
            }
        }
    }
    for file in files_in(&dir.join(".codex/agents")) {
        if file.extension().is_some_and(|e| e == "toml") {
            let text = std::fs::read_to_string(&file).unwrap_or_default();
            if !text.lines().any(|l| l.trim_start().starts_with("name")) {
                let shown = file
                    .strip_prefix(dir)
                    .unwrap_or(&file)
                    .display()
                    .to_string();
                problems.push(format!("{shown}: no `name` key"));
            }
        }
    }
    let settings = dir.join(".claude/settings.json");
    if settings.is_file() {
        match std::fs::read_to_string(&settings)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        {
            Some(Value::Object(_)) => notes.push(
                ".claude/settings.json is merged into each project's own file (keys the user set are kept)"
                    .to_owned(),
            ),
            _ => problems.push(".claude/settings.json is not a JSON object".to_owned()),
        }
    }
    if dir.join(".claude/settings.local.json").is_file() {
        notes.push(".claude/settings.local.json is never installed".to_owned());
    }
}

/// The report `connecto package check` prints for one package folder, and its
/// exit status.
#[must_use]
pub fn check_dir(dir: &Path) -> (bool, String) {
    let shown = dir.display().to_string();
    let mut out = String::new();
    let text = match std::fs::read_to_string(dir.join(FILE_NAME)) {
        Ok(text) => text,
        Err(err) => return (false, format!("{shown}: cannot read {FILE_NAME}: {err}\n")),
    };
    let manifest = match Manifest::parse(&text) {
        Ok(manifest) => manifest,
        Err(errors) => {
            for error in errors {
                let _ = writeln!(out, "{shown}/{FILE_NAME}: {error}");
            }
            return (false, out);
        }
    };
    let mut problems = Vec::new();
    let mut notes = Vec::new();
    let id = dir.file_name().and_then(|n| n.to_str()).unwrap_or_default();
    if !valid_id(id) {
        problems.push(format!(
            "the folder name {id:?} is not a package id (lowercase letters, digits, `-`)"
        ));
    }
    if manifest.extends.as_deref() == Some(id) {
        problems.push("\"extends\" names the package itself".to_owned());
    }
    if let Some(base) = &manifest.extends {
        let sibling = dir.parent().map(|p| p.join(base));
        if !BUILTIN_IDS.contains(&base.as_str())
            && !sibling.is_some_and(|p| p.join(FILE_NAME).is_file())
        {
            problems.push(format!("\"extends\": no package {base:?} next to this one"));
        }
    }
    layout_report(dir, &mut problems, &mut notes);
    let _ = writeln!(
        out,
        "{shown}: {}{}",
        if problems.is_empty() {
            "ok"
        } else {
            "problems"
        },
        format_args!(" ({:?}, version {})", manifest.name, manifest.version)
    );
    for problem in &problems {
        let _ = writeln!(out, "problem: {problem}");
    }
    for note in &notes {
        let _ = writeln!(out, "note: {note}");
    }
    (problems.is_empty(), out)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dir(name: &str) -> std::path::PathBuf {
        let d = std::env::temp_dir().join(format!("mg-pkgcheck-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    #[test]
    fn a_manifest_needs_a_name_and_rejects_odd_fields() {
        let ok = Manifest::parse(r#"{"name":"Team","version":3,"extends":"base"}"#).unwrap();
        assert_eq!((ok.version, ok.extends.as_deref()), (3, Some("base")));
        assert_eq!(Manifest::parse(r#"{"name":"x"}"#).unwrap().version, 1);
        for bad in [
            "{",
            "[]",
            r#"{"description":"x"}"#,
            r#"{"name":"x","version":0}"#,
            r#"{"name":"x","extends":"../x"}"#,
            r#"{"name":"x","other":1}"#,
        ] {
            assert!(Manifest::parse(bad).is_err(), "{bad}");
        }
        let round = Manifest::parse(&ok.to_text()).unwrap();
        assert_eq!(round, ok);
    }

    #[test]
    fn ids_are_safe_folder_names() {
        assert!(valid_id("a-1") && !valid_id("../x") && !valid_id("A") && !valid_id(""));
        assert_eq!(slugify("My  Team!"), "my-team");
        assert_eq!(slugify("???"), "package");
    }

    #[test]
    fn check_reports_layout_problems() {
        let root = dir("layout");
        let pkg = root.join("team");
        std::fs::create_dir_all(pkg.join(".claude/skills/s")).unwrap();
        std::fs::write(pkg.join(FILE_NAME), r#"{"name":"Team"}"#).unwrap();
        let (ok, text) = check_dir(&pkg);
        assert!(!ok && text.contains("SKILL.md is missing"), "{text}");
        std::fs::write(
            pkg.join(".claude/skills/s/SKILL.md"),
            "---\nname: s\ndescription: does s\n---\nBody\n",
        )
        .unwrap();
        let (ok, text) = check_dir(&pkg);
        assert!(ok, "{text}");
        // The record of how the package was made is the author's, not checked.
        std::fs::write(pkg.join("ORIGIN.md"), "# Origin\n").unwrap();
        let (ok, text) = check_dir(&pkg);
        assert!(ok && !text.contains("ORIGIN"), "{text}");
        std::fs::write(pkg.join(".claude/settings.json"), "[]").unwrap();
        assert!(!check_dir(&pkg).0);
        std::fs::write(pkg.join(".claude/settings.json"), "{}").unwrap();
        std::fs::write(pkg.join(FILE_NAME), r#"{"name":"Team","extends":"nope"}"#).unwrap();
        assert!(!check_dir(&pkg).0, "a missing base");
        std::fs::write(
            pkg.join(FILE_NAME),
            r#"{"name":"Team","extends":"connecto"}"#,
        )
        .unwrap();
        assert!(check_dir(&pkg).0);
        let empty = root.join("empty");
        std::fs::create_dir_all(&empty).unwrap();
        std::fs::write(empty.join(FILE_NAME), r#"{"name":"E"}"#).unwrap();
        assert!(!check_dir(&empty).0, "no files at all");
    }
}
