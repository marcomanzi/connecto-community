//! A project's own settings (`ProjectSettings`), kept in
//! `<data>/projects/<id>/settings.json` beside its profiles and terminal list.
//!
//! The launch code reads the file each time a terminal starts, so a change
//! takes effect with the next start and nothing has to be told.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use connecto_core::{ConfigMode, ProjectSettings, ProjectTools};
use serde_json::{Value, json};

pub const FILE: &str = "settings.json";
const VERSION: u64 = 1;

/// `<data>/projects/<id>/settings.json`.
#[must_use]
pub fn path(data_root: &Path, project_id: &str) -> PathBuf {
    data_root.join("projects").join(project_id).join(FILE)
}

#[must_use]
pub fn to_json(settings: &ProjectSettings) -> String {
    let value = json!({
        "version": VERSION,
        "tools": settings.tools.word(),
        "claude_config": settings.claude_config.word(),
        "codex_config": settings.codex_config.word(),
        "use_default_package": settings.use_default_package,
        "packages": settings.packages,
    });
    let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
    text.push('\n');
    text
}

/// Unknown or missing fields fall back to their defaults, so a file from a
/// newer or older build still loads.
#[must_use]
pub fn parse(text: &str) -> Option<ProjectSettings> {
    let value: Value = serde_json::from_str(text).ok()?;
    let object = value.as_object()?;
    let defaults = ProjectSettings::default();
    let word = |name: &str| object.get(name).and_then(Value::as_str);
    Some(ProjectSettings {
        tools: word("tools")
            .and_then(ProjectTools::from_word)
            .unwrap_or(defaults.tools),
        // A settings file that does not say (an older one) is a project from
        // before System became the default for new projects: it stays isolated.
        claude_config: word("claude_config")
            .and_then(ConfigMode::from_word)
            .unwrap_or(ConfigMode::Isolated),
        codex_config: word("codex_config")
            .and_then(ConfigMode::from_word)
            .unwrap_or(ConfigMode::Isolated),
        use_default_package: object
            .get("use_default_package")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.use_default_package),
        packages: object
            .get("packages")
            .and_then(Value::as_array)
            .map(|list| {
                list.iter()
                    .filter_map(Value::as_str)
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// The saved settings, or the defaults when there are none or the file is
/// unusable.
#[must_use]
pub fn load(data_root: &Path, project_id: &str) -> ProjectSettings {
    fs::read_to_string(path(data_root, project_id))
        .ok()
        .and_then(|text| parse(&text))
        .unwrap_or_default()
}

/// The file in the data root that says the projects that existed before System
/// became the default have been pinned (see [`pin_existing_projects`]).
const PINNED_MARK: &str = ".config-default-system";

/// Make the default of System apply to new projects only. A project that
/// existed before it (it has a folder under `projects/` with profiles in it) and
/// never saved settings would otherwise read the new default and switch from its
/// isolated profile to the system one with no one asking. Each such project gets
/// a settings file that says Isolated. Runs once per data root, at startup, before
/// anything launches: after the mark is written, a project without a settings file
/// is a new one. Returns how many projects were pinned.
pub fn pin_existing_projects(data_root: &Path) -> usize {
    let mark = data_root.join(PINNED_MARK);
    if mark.exists() {
        return 0;
    }
    let mut pinned = 0;
    if let Ok(projects) = fs::read_dir(data_root.join("projects")) {
        for project in projects.flatten() {
            let id = project.file_name();
            let Some(id) = id.to_str() else { continue };
            let existing = project.path().join("profiles").is_dir();
            if existing && !path(data_root, id).exists() {
                let isolated = ProjectSettings {
                    claude_config: ConfigMode::Isolated,
                    codex_config: ConfigMode::Isolated,
                    ..ProjectSettings::default()
                };
                if save(data_root, id, &isolated).is_ok() {
                    pinned += 1;
                } else {
                    // Not written: try again next start rather than switch it.
                    return pinned;
                }
            }
        }
    }
    let _ = fs::create_dir_all(data_root);
    let _ = fs::write(&mark, "new projects default to the system configuration\n");
    pinned
}

/// Replace the file atomically.
pub fn save(data_root: &Path, project_id: &str, settings: &ProjectSettings) -> io::Result<()> {
    let file = path(data_root, project_id);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let staged = file.with_extension("json.tmp");
    fs::write(&staged, to_json(settings))?;
    fs::rename(&staged, &file)
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn a_new_project_defaults_to_the_system_configuration() {
        let root = dir("new-default");
        let loaded = load(&root, "brand-new");
        assert_eq!(loaded.claude_config, ConfigMode::System);
        assert_eq!(loaded.codex_config, ConfigMode::System);
    }

    #[test]
    fn a_settings_file_that_does_not_say_stays_isolated() {
        let loaded = parse(r#"{"version":1,"tools":"both"}"#).unwrap();
        assert_eq!(loaded.claude_config, ConfigMode::Isolated);
        assert_eq!(loaded.codex_config, ConfigMode::Isolated);
        // What a file says wins.
        let said = parse(r#"{"version":1,"claude_config":"system"}"#).unwrap();
        assert_eq!(said.claude_config, ConfigMode::System);
        assert_eq!(said.codex_config, ConfigMode::Isolated);
    }

    #[test]
    fn existing_projects_keep_their_profiles_and_only_new_ones_get_system() {
        let root = dir("pin");
        // One project that ran before (profiles, no settings file), one that
        // chose System, and one folder with nothing in it.
        fs::create_dir_all(root.join("projects/old/profiles/claude/g1")).unwrap();
        fs::create_dir_all(root.join("projects/chose/profiles/claude/g1")).unwrap();
        save(
            &root,
            "chose",
            &ProjectSettings {
                claude_config: ConfigMode::System,
                ..ProjectSettings::default()
            },
        )
        .unwrap();
        fs::create_dir_all(root.join("projects/empty")).unwrap();
        assert_eq!(pin_existing_projects(&root), 1);
        assert_eq!(load(&root, "old").claude_config, ConfigMode::Isolated);
        assert_eq!(load(&root, "old").codex_config, ConfigMode::Isolated);
        assert_eq!(load(&root, "chose").claude_config, ConfigMode::System);
        assert_eq!(load(&root, "empty").claude_config, ConfigMode::System);
        // Once: a project made after this has no file and is new.
        fs::create_dir_all(root.join("projects/later/profiles/claude/g1")).unwrap();
        assert_eq!(pin_existing_projects(&root), 0);
        assert_eq!(load(&root, "later").claude_config, ConfigMode::System);
        // A fresh data root only gets the mark.
        let fresh = dir("pin-fresh");
        assert_eq!(pin_existing_projects(&fresh), 0);
        assert!(fresh.join(PINNED_MARK).exists());
    }

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-projset-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn no_file_means_the_defaults() {
        let root = dir("none");
        assert_eq!(load(&root, "abc"), ProjectSettings::default());
    }

    #[test]
    fn settings_round_trip_and_odd_values_fall_back() {
        let root = dir("round");
        let chosen = ProjectSettings {
            tools: ProjectTools::Codex,
            claude_config: ConfigMode::System,
            codex_config: ConfigMode::System,
            use_default_package: false,
            packages: vec!["a".to_owned(), "b".to_owned()],
        };
        save(&root, "abc", &chosen).unwrap();
        assert_eq!(load(&root, "abc"), chosen);
        assert!(!root.join("projects/abc/settings.json.tmp").exists());
        let odd = parse(r#"{"tools":"gpt","claude_config":"system"}"#).unwrap();
        assert_eq!(odd.tools, ProjectTools::Both);
        assert_eq!(odd.claude_config, ConfigMode::System);
        assert_eq!(odd.codex_config, ConfigMode::Isolated);
        assert!(odd.use_default_package);
        fs::write(path(&root, "bad"), "{").ok();
        assert_eq!(load(&root, "bad"), ProjectSettings::default());
    }
}
