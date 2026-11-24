//! A project's own settings (`ProjectSettings`), kept in
//! `<data>/projects/<id>/settings.json` beside its profiles and terminal list.
//!
//! The launch code reads the file each time a terminal starts, so a change
//! takes effect with the next start and nothing has to be told.
//!
//! The file also holds the project's memory settings under the key `memory`
//! ([`load_memory`], [`save_memory`]). Each save rewrites only its own keys
//! and keeps every other key in the file, so neither side drops the other's
//! settings, and keys from a newer build survive.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::{Mutex, PoisonError};

use connecto_core::memory::{DevelopmentSettings, DreamSettings, LocalTime, Provider, Weekday};
use connecto_core::tokens::{Before, TokenSettings};
use connecto_core::{ConfigMode, MemorySettings, ModelChoice, ProjectSettings, ProjectTools};
use serde_json::{Map, Value, json};

pub const FILE: &str = "settings.json";
const VERSION: u64 = 1;

/// `<data>/projects/<id>/settings.json`.
#[must_use]
pub fn path(data_root: &Path, project_id: &str) -> PathBuf {
    data_root.join("projects").join(project_id).join(FILE)
}

/// The key of the memory settings.
pub const MEMORY_KEY: &str = "memory";

fn to_value(settings: &ProjectSettings) -> Value {
    json!({
        "version": VERSION,
        // What an older build reads (one tool or `all`), and the exact set.
        "tools": settings.tools.legacy_word(),
        "tool_list": settings.tools.words(),
        "claude_config": settings.claude_config.word(),
        "codex_config": settings.codex_config.word(),
        "pi_config": settings.pi_config.word(),
        "opencode_config": settings.opencode_config.word(),
        "use_default_package": settings.use_default_package,
        "packages": settings.packages,
    })
}

fn pretty(value: &Value) -> String {
    let mut text = serde_json::to_string_pretty(value).unwrap_or_default();
    text.push('\n');
    text
}

/// The file's text for these settings alone (no other keys).
#[must_use]
pub fn to_json(settings: &ProjectSettings) -> String {
    pretty(&to_value(settings))
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
        // `tool_list` (the exact set) while `tools` is still what that set
        // writes; an older build that changed `tools` since wins.
        tools: ProjectTools::from_saved(
            word("tools"),
            object
                .get("tool_list")
                .and_then(Value::as_array)
                .map(|list| list.iter().filter_map(Value::as_str)),
        )
        .unwrap_or(defaults.tools),
        // A settings file that does not say (an older one) is a project from
        // before System became the default for new projects: it stays isolated.
        claude_config: word("claude_config")
            .and_then(ConfigMode::from_word)
            .unwrap_or(ConfigMode::Isolated),
        codex_config: word("codex_config")
            .and_then(ConfigMode::from_word)
            .unwrap_or(ConfigMode::Isolated),
        // Pi came after System became the default: a file that does not say
        // has never had a Pi profile to keep.
        pi_config: word("pi_config")
            .and_then(ConfigMode::from_word)
            .unwrap_or(defaults.pi_config),
        // So did OpenCode.
        opencode_config: word("opencode_config")
            .and_then(ConfigMode::from_word)
            .unwrap_or(defaults.opencode_config),
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
                    .map(|id| connecto_core::current_package_id(id).to_owned())
                    .collect()
            })
            .unwrap_or_default(),
    })
}

/// Why a terminal of `tool` may not start in the project `project_id`: its
/// saved Tools setting leaves the CLI out. Read from the file each time, so a
/// change counts at once (the published settings may lag). A shell is never
/// refused. The one place every agent terminal's add and start is checked
/// (the registry's add, a pane's start, restart and restore, the bridge):
/// hidden utility sessions (titles, Functional Memory) are internal and are
/// not checked.
#[must_use]
pub fn tool_refusal(
    data_root: &Path,
    project_id: &str,
    tool: connecto_core::ToolKind,
) -> Option<String> {
    if !tool.is_agent() || project_id.is_empty() {
        return None;
    }
    let tools = load(data_root, project_id).tools;
    (!tools.allows(tool)).then(|| {
        let names: Vec<&str> = tools.tools().iter().map(|t| t.name()).collect();
        format!(
            "This project does not use {}: its Tools setting allows only {}. Start one of those instead.",
            tool.name(),
            names.join(", ")
        )
    })
}

/// [`tool_refusal`] for the project in `folder`, by the id its real path
/// gives.
#[must_use]
pub fn tool_refusal_in(
    data_root: &Path,
    folder: &Path,
    tool: connecto_core::ToolKind,
) -> Option<String> {
    if !tool.is_agent() {
        return None;
    }
    let real = fs::canonicalize(folder).unwrap_or_else(|_| folder.to_owned());
    tool_refusal(data_root, &crate::launch::project_id(&real), tool)
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

/// Write the project settings atomically. Other keys already in the file
/// (`memory`, keys from a newer build) are kept. Fails, changing nothing,
/// when the file exists but cannot be read or is not a JSON object.
pub fn save(data_root: &Path, project_id: &str, settings: &ProjectSettings) -> io::Result<()> {
    rewrite(data_root, project_id, |object| {
        merge(object, to_value(settings));
    })
}

/// Serializes the read-modify-write of [`rewrite`] within this process, so two
/// threads saving different keys do not lose each other's change.
static WRITE: Mutex<()> = Mutex::new(());

/// Read the file as an object, let `change` edit it, and replace the file
/// atomically. Only a missing file starts from the default project settings
/// (what [`load`] reads it as): writing only some keys must not change what
/// the others mean (a file without `claude_config` reads as isolated). A file
/// that cannot be read, or is not a JSON object, is an error and is left
/// untouched.
fn rewrite(
    data_root: &Path,
    project_id: &str,
    change: impl FnOnce(&mut Map<String, Value>),
) -> io::Result<()> {
    let _guard = WRITE.lock().unwrap_or_else(PoisonError::into_inner);
    let file = path(data_root, project_id);
    let mut object = match fs::read_to_string(&file) {
        Ok(text) => match serde_json::from_str::<Value>(&text) {
            Ok(Value::Object(object)) => object,
            // Left as it is for the user to repair: overwriting would lose
            // whatever it holds.
            _ => {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "the project's settings.json is not a JSON object; it was left unchanged",
                ));
            }
        },
        Err(error) if error.kind() == io::ErrorKind::NotFound => {
            match to_value(&ProjectSettings::default()) {
                Value::Object(object) => object,
                _ => Map::new(),
            }
        }
        Err(error) => return Err(error),
    };
    change(&mut object);
    if let Some(parent) = file.parent() {
        fs::create_dir_all(parent)?;
    }
    let staged = file.with_extension("json.tmp");
    fs::write(&staged, pretty(&Value::Object(object)))?;
    fs::rename(&staged, &file)
}

/// Put `update`'s keys into `object`; nested objects are merged the same way,
/// so keys only the file knows are kept at every level.
fn merge(object: &mut Map<String, Value>, update: Value) {
    let Value::Object(update) = update else {
        return;
    };
    for (key, value) in update {
        match (object.get_mut(&key), value) {
            (Some(Value::Object(inner)), value @ Value::Object(_)) => merge(inner, value),
            (_, value) => {
                object.insert(key, value);
            }
        }
    }
}

/// The memory settings as stored under [`MEMORY_KEY`] (spec §8.1).
#[must_use]
pub fn memory_to_value(settings: &MemorySettings) -> Value {
    let dev = &settings.development;
    let dream = &settings.dream;
    let days: Vec<&str> = dream.days.iter().map(|d| d.word()).collect();
    json!({
        "enabled": settings.enabled,
        "development": {
            "provider": dev.choice.provider.word(),
            "model": dev.choice.model,
            "effort": dev.choice.effort,
            "group_seconds": dev.group_seconds,
            "max_runs_per_hour": dev.max_runs_per_hour,
            "max_input_chars": dev.max_input_chars,
            "max_attempts": dev.max_attempts,
        },
        "dream": {
            "provider": dream.choice.provider.word(),
            "model": dream.choice.model,
            "effort": dream.choice.effort,
            "days": days,
            "time": dream.time.to_string(),
            "max_minutes": dream.max_minutes,
            "max_input_chars": dream.max_input_chars,
            "min_new_items": dream.min_new_items,
        },
    })
}

/// Read the value under [`MEMORY_KEY`]. Each field that is missing, of the
/// wrong type or invalid falls back to its default on its own; numbers are
/// clamped to their range; unknown fields are ignored. A model choice is
/// taken whole or not at all (a known provider and two valid words), so a
/// model is never paired with another provider's default.
#[must_use]
pub fn parse_memory(value: &Value) -> MemorySettings {
    let defaults = MemorySettings::default();
    let Some(object) = value.as_object() else {
        return defaults;
    };
    let empty = Map::new();
    let section = |name: &str| {
        object
            .get(name)
            .and_then(Value::as_object)
            .unwrap_or(&empty)
    };
    let dev = section("development");
    let dream = section("dream");
    let development = DevelopmentSettings {
        choice: choice(dev, defaults.development.choice.clone()),
        group_seconds: number(dev, "group_seconds", defaults.development.group_seconds),
        max_runs_per_hour: number(
            dev,
            "max_runs_per_hour",
            defaults.development.max_runs_per_hour,
        ),
        max_input_chars: number(dev, "max_input_chars", defaults.development.max_input_chars),
        max_attempts: number(dev, "max_attempts", defaults.development.max_attempts),
    };
    let dream_settings = DreamSettings {
        choice: choice(dream, defaults.dream.choice.clone()),
        days: dream
            .get("days")
            .and_then(Value::as_array)
            .map(|days| {
                days.iter()
                    .filter_map(Value::as_str)
                    .filter_map(Weekday::from_word)
                    .collect()
            })
            .unwrap_or_else(|| defaults.dream.days.clone()),
        time: dream
            .get("time")
            .and_then(Value::as_str)
            .and_then(LocalTime::parse)
            .unwrap_or(defaults.dream.time),
        max_minutes: number(dream, "max_minutes", defaults.dream.max_minutes),
        max_input_chars: number(dream, "max_input_chars", defaults.dream.max_input_chars),
        min_new_items: number(dream, "min_new_items", defaults.dream.min_new_items),
    };
    MemorySettings {
        enabled: object
            .get("enabled")
            .and_then(Value::as_bool)
            .unwrap_or(defaults.enabled),
        development,
        dream: dream_settings,
    }
    .clamped()
}

fn choice(section: &Map<String, Value>, default: ModelChoice) -> ModelChoice {
    let word = |name: &str| section.get(name).and_then(Value::as_str);
    let provider = word("provider").and_then(Provider::from_word);
    match (provider, word("model"), word("effort")) {
        (Some(provider), Some(model), Some(effort)) => {
            let choice = ModelChoice::new(provider, model, effort);
            if choice.is_valid() { choice } else { default }
        }
        _ => default,
    }
}

/// A whole number; over `u32::MAX` reads as `u32::MAX` (then clamped).
fn number(section: &Map<String, Value>, name: &str, default: u32) -> u32 {
    section
        .get(name)
        .and_then(Value::as_u64)
        .map_or(default, |n| u32::try_from(n).unwrap_or(u32::MAX))
}

/// The memory settings of a project; the defaults (off) when the file or the
/// key is absent or unusable.
#[must_use]
pub fn load_memory(data_root: &Path, project_id: &str) -> MemorySettings {
    fs::read_to_string(path(data_root, project_id))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get(MEMORY_KEY).map(parse_memory))
        .unwrap_or_default()
}

/// Write the memory settings atomically (clamped), keeping every other key
/// of the file, and fields under `memory` that this build does not know.
/// Fails, changing nothing, when the file exists but cannot be read or is not
/// a JSON object.
pub fn save_memory(
    data_root: &Path,
    project_id: &str,
    settings: &MemorySettings,
) -> io::Result<()> {
    let value = memory_to_value(&settings.clone().clamped());
    rewrite(data_root, project_id, |object| {
        merge(object, json!({ MEMORY_KEY: value }));
    })
}

/// The key of the project's own model providers ([`load_providers`]).
pub const PROVIDERS_KEY: &str = "providers";

/// The model providers a project has of its own, used instead of the
/// app-wide ones (Settings > Agents) by its terminals of a tool whose config
/// is Isolated, when it says so. Keys are never here: they are in the secret
/// store, under names of the project's own.
///
/// ```json
/// "providers": {
///   "claude": { "own": true },
///   "pi": { "own": true, "lmstudio_url": "http://localhost:1234", "default_model": "together/x" }
/// }
/// ```
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct ProjectProviders {
    /// Claude terminals use the project's own providers.
    pub claude_own: bool,
    /// Pi terminals use the project's own providers.
    pub pi_own: bool,
    /// The project's LM Studio server for Pi. `None`: the default address;
    /// empty: no local server.
    pub pi_lmstudio_url: Option<String>,
    /// The model a new Pi terminal of the project starts on. `None` leaves
    /// the choice to Pi.
    pub pi_default_model: Option<String>,
}

impl ProjectProviders {
    /// Whether the project's terminals of `tool` use its own providers.
    #[must_use]
    pub const fn own(&self, tool: connecto_core::ToolKind) -> bool {
        match tool {
            connecto_core::ToolKind::Claude => self.claude_own,
            connecto_core::ToolKind::Pi => self.pi_own,
            connecto_core::ToolKind::Codex
            | connecto_core::ToolKind::Antigravity
            | connecto_core::ToolKind::OpenCode
            | connecto_core::ToolKind::Shell => false,
        }
    }
}

/// Read the value under [`PROVIDERS_KEY`]. Anything missing or of the wrong
/// type is the default (not its own).
#[must_use]
pub fn parse_providers(value: &Value) -> ProjectProviders {
    let section = |name: &str| value.get(name).and_then(Value::as_object);
    let own = |name: &str| {
        section(name)
            .and_then(|tool| tool.get("own"))
            .and_then(Value::as_bool)
            .unwrap_or(false)
    };
    let pi_text = |name: &str| {
        section("pi")
            .and_then(|pi| pi.get(name))
            .and_then(Value::as_str)
            .map(str::to_owned)
    };
    ProjectProviders {
        claude_own: own("claude"),
        pi_own: own("pi"),
        pi_lmstudio_url: pi_text("lmstudio_url"),
        pi_default_model: pi_text("default_model"),
    }
}

/// The project's own model providers; the defaults (none of its own) when
/// the file or the key is absent or unusable.
#[must_use]
pub fn load_providers(data_root: &Path, project_id: &str) -> ProjectProviders {
    fs::read_to_string(path(data_root, project_id))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get(PROVIDERS_KEY).map(parse_providers))
        .unwrap_or_default()
}

/// Write the project's own model providers atomically, keeping every other
/// key of the file and the fields under `providers` this build does not know.
/// Fails, changing nothing, when the file exists but cannot be read or is not
/// a JSON object.
pub fn save_providers(
    data_root: &Path,
    project_id: &str,
    providers: &ProjectProviders,
) -> io::Result<()> {
    rewrite(data_root, project_id, |object| {
        write_providers(object, providers);
    })
}

/// Change some of the project's own model providers: `change` is given what
/// the file holds, read under the same lock as the write, so two changes of
/// different fields (a choice in Settings, an address found in the
/// background) never undo each other. Fails as [`save_providers`] does.
pub fn update_providers(
    data_root: &Path,
    project_id: &str,
    change: impl FnOnce(&mut ProjectProviders),
) -> io::Result<()> {
    rewrite(data_root, project_id, |object| {
        let mut providers = object
            .get(PROVIDERS_KEY)
            .map(parse_providers)
            .unwrap_or_default();
        change(&mut providers);
        write_providers(object, &providers);
    })
}

/// Put `providers` under [`PROVIDERS_KEY`] in the file's object. The
/// sections are copied out and put back whole: what this build does not know
/// stays.
fn write_providers(object: &mut Map<String, Value>, providers: &ProjectProviders) {
    let taken = |map: &Map<String, Value>, name: &str| {
        map.get(name)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default()
    };
    let mut section = taken(object, PROVIDERS_KEY);
    let mut claude = taken(&section, "claude");
    claude.insert("own".to_owned(), json!(providers.claude_own));
    let mut pi = taken(&section, "pi");
    pi.insert("own".to_owned(), json!(providers.pi_own));
    for (name, value) in [
        ("lmstudio_url", &providers.pi_lmstudio_url),
        ("default_model", &providers.pi_default_model),
    ] {
        match value {
            Some(text) => {
                pi.insert(name.to_owned(), json!(text));
            }
            None => {
                pi.remove(name);
            }
        }
    }
    section.insert("claude".to_owned(), Value::Object(claude));
    section.insert("pi".to_owned(), Value::Object(pi));
    object.insert(PROVIDERS_KEY.to_owned(), Value::Object(section));
}

/// The key of the project's Agent Token Optimization settings
/// (docs/specs/token-optimization.md §5.1).
pub const TOKEN_KEY: &str = "token_optimization";

/// Read the value under [`TOKEN_KEY`]. Anything missing or of the wrong type
/// is the default (no cut); list entries that are not strings are skipped.
#[must_use]
pub fn parse_token_optimization(value: &Value) -> TokenSettings {
    let list = |name: &str| -> Vec<String> {
        value
            .get(name)
            .and_then(Value::as_array)
            .map(|items| {
                items
                    .iter()
                    .filter_map(Value::as_str)
                    .filter(|s| !s.is_empty())
                    .map(str::to_owned)
                    .collect()
            })
            .unwrap_or_default()
    };
    let before = value.get("before").and_then(|b| {
        Some(Before {
            start_context_median: b.get("start_context_median")?.as_u64()?,
            sessions: u32::try_from(b.get("sessions")?.as_u64()?).ok()?,
        })
    });
    TokenSettings {
        plugins_off: list("plugins_off"),
        skills_off: list("skills_off"),
        // Claude refuses less than 100k.
        autocompact: value
            .get("autocompact")
            .and_then(Value::as_u64)
            .and_then(|n| u32::try_from(n).ok())
            .filter(|n| *n >= 100_000),
        applied_at: value
            .get("applied_at")
            .and_then(Value::as_str)
            .map(str::to_owned),
        before,
    }
}

/// The project's token optimization; no cut when the file or key is absent
/// or unusable.
#[must_use]
pub fn load_token_optimization(data_root: &Path, project_id: &str) -> TokenSettings {
    fs::read_to_string(path(data_root, project_id))
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .and_then(|value| value.get(TOKEN_KEY).map(parse_token_optimization))
        .unwrap_or_default()
}

/// Change the project's token optimization: `change` is given what the file
/// holds, read under the same lock as the write. Every other key of the file,
/// and the fields under `token_optimization` this build does not know, stay.
/// Fails, changing nothing, as [`save_providers`] does.
pub fn update_token_optimization(
    data_root: &Path,
    project_id: &str,
    change: impl FnOnce(&mut TokenSettings),
) -> io::Result<()> {
    rewrite(data_root, project_id, |object| {
        let mut settings = object
            .get(TOKEN_KEY)
            .map(parse_token_optimization)
            .unwrap_or_default();
        change(&mut settings);
        let mut section = object
            .get(TOKEN_KEY)
            .and_then(Value::as_object)
            .cloned()
            .unwrap_or_default();
        section.insert("plugins_off".to_owned(), json!(settings.plugins_off));
        section.insert("skills_off".to_owned(), json!(settings.skills_off));
        section.insert("autocompact".to_owned(), json!(settings.autocompact));
        section.insert("applied_at".to_owned(), json!(settings.applied_at));
        section.insert(
            "before".to_owned(),
            settings.before.map_or(Value::Null, |b| {
                json!({ "start_context_median": b.start_context_median, "sessions": b.sessions })
            }),
        );
        object.insert(TOKEN_KEY.to_owned(), Value::Object(section));
    })
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

    #[test]
    fn the_earlier_built_in_package_id_is_read_as_the_current_one() {
        let loaded =
            parse(r#"{"version":1,"packages":["magneto","team"],"use_default_package":true}"#)
                .unwrap();
        assert_eq!(loaded.packages, ["connecto", "team"]);
    }

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-projset-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    const SPEC_EXAMPLE: &str = r#"{
      "enabled": false,
      "development": {
        "provider": "claude", "model": "haiku", "effort": "low",
        "group_seconds": 3600, "max_runs_per_hour": 20,
        "max_input_chars": 60000, "max_attempts": 3
      },
      "dream": {
        "provider": "claude", "model": "sonnet", "effort": "medium",
        "days": ["mon","tue","wed","thu","fri"], "time": "02:30",
        "max_minutes": 30, "max_input_chars": 200000, "min_new_items": 1
      }
    }"#;

    #[test]
    fn the_spec_example_is_the_default_and_writes_back_the_same() {
        let value: Value = serde_json::from_str(SPEC_EXAMPLE).unwrap();
        assert_eq!(parse_memory(&value), MemorySettings::default());
        assert_eq!(memory_to_value(&MemorySettings::default()), value);
    }

    #[test]
    fn an_absent_memory_key_or_file_means_memory_is_off() {
        let root = dir("mem-absent");
        assert_eq!(load_memory(&root, "none"), MemorySettings::default());
        save(&root, "p", &ProjectSettings::default()).unwrap();
        let loaded = load_memory(&root, "p");
        assert_eq!(loaded, MemorySettings::default());
        assert!(!loaded.enabled);
        fs::create_dir_all(root.join("projects/bad")).unwrap();
        fs::write(path(&root, "bad"), "{").unwrap();
        assert_eq!(load_memory(&root, "bad"), MemorySettings::default());
    }

    #[test]
    fn memory_settings_round_trip() {
        let root = dir("mem-round");
        let mut chosen = MemorySettings {
            enabled: true,
            ..MemorySettings::default()
        };
        chosen.development.choice = ModelChoice::new(Provider::Codex, "gpt-5.1", "minimal");
        chosen.development.group_seconds = 0;
        chosen.dream.days = [Weekday::Sat, Weekday::Sun].into_iter().collect();
        chosen.dream.time = LocalTime::new(23, 5).unwrap();
        chosen.dream.min_new_items = 0;
        save_memory(&root, "p", &chosen).unwrap();
        assert_eq!(load_memory(&root, "p"), chosen);
        assert!(!root.join("projects/p/settings.json.tmp").exists());
    }

    #[test]
    fn odd_memory_values_fall_back_one_by_one() {
        let value: Value = serde_json::from_str(
            r#"{
              "enabled": "yes",
              "future_field": 1,
              "development": {
                "provider": "gemini", "model": "pro", "effort": "low",
                "group_seconds": -4, "max_runs_per_hour": 99999999999,
                "max_input_chars": "lots", "max_attempts": 0
              },
              "dream": {
                "provider": "claude", "model": "--tools", "effort": "high",
                "days": ["sun", "funday", 3, "mon"], "time": "2:30",
                "max_minutes": 10
              }
            }"#,
        )
        .unwrap();
        let s = parse_memory(&value);
        let d = MemorySettings::default();
        assert!(!s.enabled);
        // Unknown provider, or a model word that reads as a flag: the whole
        // choice is the default.
        assert_eq!(s.development.choice, d.development.choice);
        assert_eq!(s.dream.choice, d.dream.choice);
        assert_eq!(s.development.group_seconds, d.development.group_seconds);
        assert_eq!(s.development.max_runs_per_hour, 600);
        assert_eq!(s.development.max_input_chars, d.development.max_input_chars);
        assert_eq!(s.development.max_attempts, 1);
        let days: Vec<_> = s.dream.days.iter().map(|w| w.word()).collect();
        assert_eq!(days, ["mon", "sun"]);
        assert_eq!(s.dream.time, d.dream.time);
        assert_eq!(s.dream.max_minutes, 10);
        assert_eq!(s.dream.min_new_items, d.dream.min_new_items);
        assert_eq!(parse_memory(&json!([1, 2])), d);
        // An empty list of days is kept: never on schedule.
        let none = parse_memory(&json!({"dream": {"days": []}}));
        assert!(none.dream.days.is_empty());
    }

    #[test]
    fn each_save_keeps_the_other_keys() {
        let root = dir("mem-keep");
        let file = path(&root, "p");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            r#"{"version":1,"tools":"codex","claude_config":"isolated","from_the_future":{"x":1},
               "memory":{"enabled":true,"newer":"kept","dream":{"time":"04:00","later":true}}}"#,
        )
        .unwrap();
        // Saving the project settings keeps memory and unknown keys.
        let mut project = load(&root, "p");
        assert_eq!(project.tools, ProjectTools::Codex);
        project.packages = vec!["team".to_owned()];
        save(&root, "p", &project).unwrap();
        let memory = load_memory(&root, "p");
        assert!(memory.enabled);
        assert_eq!(memory.dream.time.to_string(), "04:00");
        // Saving memory keeps the project settings, unknown keys at every level.
        let mut memory = memory;
        memory.dream.max_minutes = 45;
        save_memory(&root, "p", &memory).unwrap();
        assert_eq!(load(&root, "p"), project);
        assert_eq!(load_memory(&root, "p"), memory);
        let raw: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(raw["from_the_future"]["x"], 1);
        assert_eq!(raw["memory"]["newer"], "kept");
        assert_eq!(raw["memory"]["dream"]["later"], true);
        assert_eq!(raw["memory"]["dream"]["max_minutes"], 45);
        assert_eq!(raw["claude_config"], "isolated");
    }

    #[test]
    fn saving_memory_first_does_not_change_the_project_defaults() {
        // A file with only `memory` would read as an isolated project.
        let root = dir("mem-first");
        let on = MemorySettings {
            enabled: true,
            ..MemorySettings::default()
        };
        save_memory(&root, "p", &on).unwrap();
        assert_eq!(load(&root, "p"), ProjectSettings::default());
        assert!(load_memory(&root, "p").enabled);
    }

    #[test]
    fn a_malformed_file_is_refused_and_left_as_it_is() {
        let root = dir("mem-malformed");
        fs::create_dir_all(root.join("projects/p")).unwrap();
        for text in ["{", "[1,2]", "\"x\"", ""] {
            fs::write(path(&root, "p"), text).unwrap();
            let on = MemorySettings {
                enabled: true,
                ..MemorySettings::default()
            };
            let error = save_memory(&root, "p", &on).unwrap_err();
            assert_eq!(error.kind(), io::ErrorKind::InvalidData, "{text:?}");
            assert!(save(&root, "p", &ProjectSettings::default()).is_err());
            assert_eq!(fs::read_to_string(path(&root, "p")).unwrap(), text);
        }
    }

    #[test]
    fn a_read_error_other_than_missing_is_returned_and_nothing_is_written() {
        // A directory where the file should be: reading fails, not with
        // NotFound, on every platform.
        let root = dir("mem-unreadable");
        fs::create_dir_all(path(&root, "p")).unwrap();
        let error = save_memory(&root, "p", &MemorySettings::default()).unwrap_err();
        assert_ne!(error.kind(), io::ErrorKind::NotFound);
        assert!(path(&root, "p").is_dir());
        assert!(!root.join("projects/p/settings.json.tmp").exists());
        assert!(save(&root, "p", &ProjectSettings::default()).is_err());
    }

    #[cfg(unix)]
    #[test]
    fn an_unreadable_file_keeps_its_settings() {
        use std::os::unix::fs::PermissionsExt;
        let root = dir("mem-noperm");
        fs::create_dir_all(root.join("projects/p")).unwrap();
        let file = path(&root, "p");
        let original = r#"{"version":1,"claude_config":"isolated","keep":true}"#;
        fs::write(&file, original).unwrap();
        fs::set_permissions(&file, fs::Permissions::from_mode(0o000)).unwrap();
        // Root can read anything: the case does not arise for it.
        if fs::read_to_string(&file).is_err() {
            let result = save_memory(&root, "p", &MemorySettings::default());
            assert_eq!(result.unwrap_err().kind(), io::ErrorKind::PermissionDenied);
        }
        fs::set_permissions(&file, fs::Permissions::from_mode(0o600)).unwrap();
        assert_eq!(fs::read_to_string(&file).unwrap(), original);
    }

    #[test]
    fn saving_memory_clamps() {
        let root = dir("mem-clamp");
        let mut s = MemorySettings::default();
        s.dream.max_minutes = 0;
        save_memory(&root, "p", &s).unwrap();
        assert_eq!(load_memory(&root, "p").dream.max_minutes, 1);
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
            pi_config: ConfigMode::Isolated,
            opencode_config: ConfigMode::Isolated,
            use_default_package: false,
            packages: vec!["a".to_owned(), "b".to_owned()],
        };
        save(&root, "abc", &chosen).unwrap();
        assert_eq!(load(&root, "abc"), chosen);
        assert!(!root.join("projects/abc/settings.json.tmp").exists());
        let odd = parse(r#"{"tools":"gpt","claude_config":"system"}"#).unwrap();
        assert_eq!(odd.tools, ProjectTools::All);
        assert_eq!(odd.claude_config, ConfigMode::System);
        assert_eq!(odd.codex_config, ConfigMode::Isolated);
        // OpenCode came after System became the default.
        assert_eq!(odd.opencode_config, ConfigMode::System);
        assert!(odd.use_default_package);
        fs::write(path(&root, "bad"), "{").ok();
        assert_eq!(load(&root, "bad"), ProjectSettings::default());
    }

    /// A set of tools is saved as `tool_list`, with `tools` saying what an
    /// older build understands (`all` for a set of several, the tool's word
    /// for one). Old files load as the same set; an older build's later
    /// change to `tools` wins over a stale `tool_list`.
    #[test]
    fn a_set_of_tools_is_saved_so_older_builds_still_read_it() {
        use connecto_core::ToolKind;
        let root = dir("tool-set");
        let codex_pi = ProjectTools::from_tools([ToolKind::Codex, ToolKind::Pi]).unwrap();
        let chosen = ProjectSettings {
            tools: codex_pi,
            ..ProjectSettings::default()
        };
        save(&root, "p", &chosen).unwrap();
        assert_eq!(load(&root, "p").tools, codex_pi);
        let raw: Value =
            serde_json::from_str(&fs::read_to_string(path(&root, "p")).unwrap()).unwrap();
        assert_eq!(raw["tools"], "all", "an older build offers a superset");
        assert_eq!(raw["tool_list"], json!(["codex", "pi"]));
        // One tool, or all: the word an older build wrote too.
        let one = parse(&to_json(&ProjectSettings {
            tools: ProjectTools::Pi,
            ..ProjectSettings::default()
        }))
        .unwrap();
        assert_eq!(one.tools, ProjectTools::Pi);
        assert!(to_json(&ProjectSettings::default()).contains(r#""tools": "all""#));
        // Files from older builds.
        for (word, set) in [
            ("claude", ProjectTools::Claude),
            ("opencode", ProjectTools::OpenCode),
            ("all", ProjectTools::All),
            ("both", ProjectTools::All),
        ] {
            let text = format!(r#"{{"tools":"{word}"}}"#);
            assert_eq!(parse(&text).unwrap().tools, set, "{word}");
        }
        // An older build kept `tool_list` (it keeps unknown keys) and saved
        // `tools` unchanged: the set stays. When it chose Claude: Claude.
        let kept = parse(r#"{"tools":"all","tool_list":["codex","pi"]}"#).unwrap();
        assert_eq!(kept.tools, codex_pi);
        let changed = parse(r#"{"tools":"claude","tool_list":["codex","pi"]}"#).unwrap();
        assert_eq!(changed.tools, ProjectTools::Claude);
        // A tool from a newer build, or an empty list: the word.
        let newer = parse(r#"{"tools":"all","tool_list":["codex","zed"]}"#).unwrap();
        assert_eq!(newer.tools, ProjectTools::All);
        let empty = parse(r#"{"tools":"codex","tool_list":[]}"#).unwrap();
        assert_eq!(empty.tools, ProjectTools::Codex);
    }

    #[test]
    fn project_providers_round_trip_and_keep_the_other_keys() {
        let root = dir("providers");
        assert_eq!(load_providers(&root, "p"), ProjectProviders::default());
        let file = path(&root, "p");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            r#"{"version":1,"claude_config":"isolated","from_the_future":1,
               "memory":{"enabled":true},
               "providers":{"later":{"x":1},"pi":{"newer":"kept"}}}"#,
        )
        .unwrap();
        let chosen = ProjectProviders {
            claude_own: true,
            pi_own: true,
            pi_lmstudio_url: Some(String::new()),
            pi_default_model: Some("together/some-model".to_owned()),
        };
        save_providers(&root, "p", &chosen).unwrap();
        assert_eq!(load_providers(&root, "p"), chosen);
        assert!(chosen.own(connecto_core::ToolKind::Claude));
        assert!(!chosen.own(connecto_core::ToolKind::Codex));
        // The project's settings and memory are as they were.
        assert_eq!(load(&root, "p").claude_config, ConfigMode::Isolated);
        assert!(load_memory(&root, "p").enabled);
        // Saving them in turn keeps the providers.
        save(&root, "p", &load(&root, "p")).unwrap();
        assert_eq!(load_providers(&root, "p"), chosen);
        let raw: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(raw["from_the_future"], 1);
        assert_eq!(raw["providers"]["later"]["x"], 1);
        assert_eq!(raw["providers"]["pi"]["newer"], "kept");
        // Cleared fields go; back to the app-wide providers.
        save_providers(&root, "p", &ProjectProviders::default()).unwrap();
        assert_eq!(load_providers(&root, "p"), ProjectProviders::default());
        let raw: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert!(raw["providers"]["pi"].get("default_model").is_none());
        assert_eq!(raw["providers"]["pi"]["newer"], "kept");
    }

    #[test]
    fn odd_provider_values_read_as_the_app_wide_providers() {
        let odd =
            parse_providers(&json!({"claude": {"own": "yes"}, "pi": [1], "codex": {"own": true}}));
        assert_eq!(odd, ProjectProviders::default());
        // A file without the key, or one that is not usable: none of its own.
        let root = dir("providers-odd");
        save(&root, "p", &ProjectSettings::default()).unwrap();
        assert_eq!(load_providers(&root, "p"), ProjectProviders::default());
        fs::write(path(&root, "bad"), "{").ok();
        assert_eq!(load_providers(&root, "bad"), ProjectProviders::default());
        // A malformed file is refused and left as it is.
        fs::create_dir_all(root.join("projects/m")).unwrap();
        fs::write(path(&root, "m"), "[1]").unwrap();
        assert!(save_providers(&root, "m", &ProjectProviders::default()).is_err());
        assert_eq!(fs::read_to_string(path(&root, "m")).unwrap(), "[1]");
    }

    /// Changes of different fields from different threads all land: each is
    /// read and written under the one lock.
    #[test]
    fn concurrent_provider_changes_of_different_fields_never_undo_each_other() {
        let root = dir("providers-race");
        let threads: Vec<_> = (0..8)
            .map(|n| {
                let root = root.clone();
                std::thread::spawn(move || {
                    for _ in 0..20 {
                        update_providers(&root, "p", |saved| match n % 4 {
                            0 => saved.claude_own = true,
                            1 => saved.pi_own = true,
                            2 => saved.pi_lmstudio_url = Some("http://localhost:1235".into()),
                            _ => saved.pi_default_model = Some("together/x".into()),
                        })
                        .unwrap();
                    }
                })
            })
            .collect();
        for thread in threads {
            thread.join().unwrap();
        }
        let saved = load_providers(&root, "p");
        assert!(saved.claude_own && saved.pi_own);
        assert_eq!(
            saved.pi_lmstudio_url.as_deref(),
            Some("http://localhost:1235")
        );
        assert_eq!(saved.pi_default_model.as_deref(), Some("together/x"));
    }

    #[test]
    fn token_optimization_round_trips_and_keeps_unknown_keys() {
        let root = dir("tokens");
        assert_eq!(
            load_token_optimization(&root, "p"),
            TokenSettings::default()
        );
        let file = path(&root, "p");
        fs::create_dir_all(file.parent().unwrap()).unwrap();
        fs::write(
            &file,
            r#"{"version":1,"later":{"a":1},
               "token_optimization":{"newer":"kept","plugins_off":["x@synced",3],"autocompact":5}}"#,
        )
        .unwrap();
        let read = load_token_optimization(&root, "p");
        assert_eq!(read.plugins_off, vec!["x@synced".to_owned()]);
        // Below Claude's minimum: read as the default.
        assert_eq!(read.autocompact, None);
        update_token_optimization(&root, "p", |t| {
            t.skills_off = vec!["dataviz".to_owned()];
            t.autocompact = Some(200_000);
            t.applied_at = Some("2026-11-21T23:50:00Z".to_owned());
            t.before = Some(Before {
                start_context_median: 42_614,
                sessions: 12,
            });
        })
        .unwrap();
        let read = load_token_optimization(&root, "p");
        assert_eq!(read.plugins_off, vec!["x@synced".to_owned()]);
        assert_eq!(read.skills_off, vec!["dataviz".to_owned()]);
        assert_eq!(read.autocompact, Some(200_000));
        assert_eq!(read.before.unwrap().start_context_median, 42_614);
        let raw: Value = serde_json::from_str(&fs::read_to_string(&file).unwrap()).unwrap();
        assert_eq!(raw["later"]["a"], 1);
        assert_eq!(raw[TOKEN_KEY]["newer"], "kept");
        // The other settings still read as they did.
        assert_eq!(
            load(&root, "p"),
            parse(&fs::read_to_string(&file).unwrap()).unwrap()
        );
    }
}
