//! App-wide settings, `<data>/settings.json`. Read once at startup; written
//! by the workspace's worker thread, never the UI thread.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use connecto_core::{AgentMode, LaunchModes, UtilityEffort, UtilityModel, UtilitySettings};
use serde_json::{Value, json};

/// The file name inside the data folder.
pub(crate) const FILE: &str = "settings.json";
const VERSION: u64 = 1;

/// Everything the file holds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Settings {
    pub utility: UtilitySettings,
    /// The Automations folder. Optional, so files from before it existed (and
    /// builds that do not know it) stay valid with the same version.
    pub automations_root: Option<PathBuf>,
    /// Connecto Home: holds `automations/` and `packages/`. Optional for the same
    /// reason; `automations_root` keeps being read for older installs.
    pub connecto_home: Option<PathBuf>,
    /// How each tool is launched. Optional in the file, so a file from before
    /// this choice keeps today's behaviour (bypass) for both tools.
    pub launch_modes: LaunchModes,
    /// Keep secrets in the system store (the macOS Keychain, Windows
    /// Credential Manager) instead of the secrets file. Off by default;
    /// written only when on.
    pub secrets_in_system_store: bool,
}

impl Settings {
    pub(crate) fn to_json(&self) -> String {
        let mut value = json!({
            "version": VERSION,
            "utility": {
                "model": self.utility.model.cli_value(),
                "effort": self.utility.effort.cli_value(),
            },
        });
        // Written only when it differs from the default, so a file of a user
        // who never touched it stays as it was.
        if self.launch_modes != LaunchModes::default() {
            value["launch_modes"] = json!({
                "claude": self.launch_modes.claude.cli_value(),
                "codex": self.launch_modes.codex.cli_value(),
                "pi": self.launch_modes.pi.cli_value(),
                "antigravity": self.launch_modes.antigravity.cli_value(),
                "opencode": self.launch_modes.opencode.cli_value(),
            });
        }
        if self.secrets_in_system_store {
            value["secrets_in_system_store"] = json!(true);
        }
        if let Some(root) = &self.automations_root {
            value["automations_root"] = json!(root.to_string_lossy());
        }
        if let Some(home) = &self.connecto_home {
            value["connecto_home"] = json!(home.to_string_lossy());
        }
        let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
        text.push('\n');
        text
    }

    /// An unknown or missing model or effort falls back to its default.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|err| err.to_string())?;
        if value.get("version").and_then(Value::as_u64) != Some(VERSION) {
            return Err("an unknown version".to_owned());
        }
        let utility = value.get("utility");
        let field = |name: &str| utility.and_then(|u| u.get(name)).and_then(Value::as_str);
        Ok(Self {
            utility: UtilitySettings {
                model: field("model")
                    .and_then(UtilityModel::from_cli)
                    .unwrap_or_default(),
                effort: field("effort")
                    .and_then(UtilityEffort::from_cli)
                    .unwrap_or_default(),
            },
            automations_root: value
                .get("automations_root")
                .and_then(Value::as_str)
                .filter(|path| crate::platform::absolute(path))
                .map(PathBuf::from),
            // `magneto_home` is the key an install under the earlier name wrote.
            connecto_home: value
                .get("connecto_home")
                .or_else(|| value.get("magneto_home"))
                .and_then(Value::as_str)
                .filter(|path| crate::platform::absolute(path))
                .map(PathBuf::from),
            launch_modes: {
                // An unknown or missing word keeps the default for that tool.
                let word = |tool: &str| {
                    value
                        .pointer(&format!("/launch_modes/{tool}"))
                        .and_then(Value::as_str)
                        .and_then(AgentMode::from_cli)
                        .unwrap_or_default()
                };
                LaunchModes {
                    claude: word("claude"),
                    codex: word("codex"),
                    pi: word("pi"),
                    antigravity: word("antigravity"),
                    opencode: word("opencode"),
                }
            },
            secrets_in_system_store: value
                .get("secrets_in_system_store")
                .and_then(Value::as_bool)
                .unwrap_or(false),
        })
    }
}

/// What loading found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Loaded {
    /// A readable file, or none yet.
    Read(Settings),
    /// The content was unusable. The file moved to `.bad` and the defaults
    /// are used, with the warning.
    Bad { warning: String },
    /// The file could not be read, or could not be moved aside. It may be
    /// sound, so it is never written this run.
    ReadOnly { warning: String },
}

/// The real file.
pub(crate) struct Disk {
    path: PathBuf,
}

impl Disk {
    pub(crate) fn new(data_root: &Path) -> Self {
        Self {
            path: data_root.join(FILE),
        }
    }

    /// The saved settings; the defaults when there is no file. A file that
    /// is there but cannot be read is not a missing one: writing the defaults
    /// over it would lose Connecto Home and the rest.
    pub(crate) fn load(&self) -> Loaded {
        let bytes = match fs::read(&self.path) {
            Ok(bytes) => bytes,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Loaded::Read(Settings::default());
            }
            Err(err) => {
                return Loaded::ReadOnly {
                    warning: format!(
                        "The saved settings could not be read ({err}). The defaults are used and settings are not saved this run."
                    ),
                };
            }
        };
        let parsed = std::str::from_utf8(&bytes)
            .map_err(|err| err.to_string())
            .and_then(Settings::parse);
        match parsed {
            Ok(settings) => Loaded::Read(settings),
            Err(why) => match fs::rename(&self.path, self.path.with_extension("json.bad")) {
                Ok(()) => Loaded::Bad {
                    warning: format!(
                        "The saved settings were unusable ({why}). They were kept as settings.json.bad, and Connecto starts with the defaults."
                    ),
                },
                Err(err) => Loaded::ReadOnly {
                    warning: format!(
                        "The saved settings were unusable ({why}) and could not be moved aside ({err}). The defaults are used and settings are not saved this run."
                    ),
                },
            },
        }
    }

    /// Replace the file atomically.
    pub(crate) fn write(&self, settings: &Settings) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let staged = self.path.with_extension("json.tmp");
        fs::write(&staged, settings.to_json())?;
        crate::platform::replace(&staged, &self.path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    /// An absolute path in this platform's form: `/a/b`, or `C:/a/b` on
    /// Windows, where `/a/b` depends on the current drive.
    fn abs(rest: &str) -> String {
        #[cfg(unix)]
        return format!("/{rest}");
        #[cfg(windows)]
        return format!("C:/{rest}");
    }

    fn dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("mg-settings-{name}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn the_defaults_are_haiku_and_low() {
        let settings = Settings::default().utility;
        assert_eq!(settings.model, UtilityModel::Haiku);
        assert_eq!(settings.effort, UtilityEffort::Low);
    }

    #[test]
    fn settings_round_trip_through_the_file() {
        let root = dir("round");
        let disk = Disk::new(&root);
        assert_eq!(
            disk.load(),
            Loaded::Read(Settings::default()),
            "no file: defaults"
        );
        let chosen = Settings {
            utility: UtilitySettings {
                model: UtilityModel::Opus,
                effort: UtilityEffort::Xhigh,
            },
            automations_root: Some(PathBuf::from(abs("work/automations"))),
            connecto_home: Some(PathBuf::from(abs("work/home"))),
            launch_modes: LaunchModes::default(),
            secrets_in_system_store: true,
        };
        disk.write(&chosen).unwrap();
        assert_eq!(Disk::new(&root).load(), Loaded::Read(chosen));
        assert!(!root.join("settings.json.tmp").exists());
    }

    #[test]
    fn the_automations_folder_is_optional_and_old_files_still_load() {
        // A file from before the field existed.
        let old = r#"{"version":1,"utility":{"model":"opus","effort":"low"}}"#;
        let parsed = Settings::parse(old).unwrap();
        assert_eq!(parsed.automations_root, None);
        assert_eq!(parsed.utility.model, UtilityModel::Opus);
        // Written only when set, and read back.
        assert!(!Settings::default().to_json().contains("automations_root"));
        let set = Settings {
            automations_root: Some(PathBuf::from(abs("a/b"))),
            ..Settings::default()
        };
        assert_eq!(Settings::parse(&set.to_json()).unwrap(), set);
        // A relative path is not a folder choice.
        let bad = r#"{"version":1,"automations_root":"relative"}"#;
        assert_eq!(Settings::parse(bad).unwrap().automations_root, None);
        // The other settings survive a change of folder, and the reverse.
        let root = dir("auto");
        let disk = Disk::new(&root);
        disk.write(&set).unwrap();
        assert_eq!(disk.load(), Loaded::Read(set));
    }

    #[test]
    fn the_earlier_installs_home_key_is_still_read() {
        let old = format!(r#"{{"version":1,"magneto_home":"{}"}}"#, abs("old/home"));
        let parsed = Settings::parse(&old).unwrap();
        assert_eq!(parsed.connecto_home, Some(PathBuf::from(abs("old/home"))));
        let both = format!(
            r#"{{"version":1,"magneto_home":"{}","connecto_home":"{}"}}"#,
            abs("old"),
            abs("new")
        );
        assert_eq!(
            Settings::parse(&both).unwrap().connecto_home,
            Some(PathBuf::from(abs("new")))
        );
        // Saved again, it is under the new key only.
        let text = parsed.to_json();
        assert!(text.contains("connecto_home") && !text.contains("magneto_home"));
    }

    #[test]
    fn connecto_home_is_optional_absolute_and_kept_beside_the_old_folder() {
        let old = format!(r#"{{"version":1,"automations_root":"{}"}}"#, abs("a"));
        let parsed = Settings::parse(&old).unwrap();
        assert_eq!(parsed.connecto_home, None);
        assert_eq!(parsed.automations_root, Some(PathBuf::from(abs("a"))));
        assert!(!Settings::default().to_json().contains("connecto_home"));
        let rel = r#"{"version":1,"connecto_home":"relative"}"#;
        assert_eq!(Settings::parse(rel).unwrap().connecto_home, None);
        let set = Settings {
            connecto_home: Some(PathBuf::from(abs("h"))),
            launch_modes: LaunchModes::default(),
            automations_root: Some(PathBuf::from(abs("h/automations"))),
            ..Settings::default()
        };
        assert_eq!(Settings::parse(&set.to_json()).unwrap(), set);
    }

    #[test]
    fn odd_values_fall_back_and_a_bad_file_is_kept_aside() {
        let odd = r#"{"version":1,"utility":{"model":"gpt","effort":"max"}}"#;
        let parsed = Settings::parse(odd).unwrap().utility;
        assert_eq!(parsed.model, UtilityModel::Haiku);
        assert_eq!(parsed.effort, UtilityEffort::Max);
        let root = dir("bad");
        // Not JSON, and not text at all: both are content, so both go aside.
        for bad in [&b"not json"[..], &[0xff, 0xfe, 0x00][..]] {
            fs::write(root.join(FILE), bad).unwrap();
            let loaded = Disk::new(&root).load();
            assert!(
                matches!(&loaded, Loaded::Bad { warning } if warning.contains("settings.json.bad")),
                "{loaded:?}"
            );
            assert_eq!(fs::read(root.join("settings.json.bad")).unwrap(), bad);
            assert!(!root.join(FILE).exists());
        }
    }

    #[test]
    fn a_file_that_cannot_be_read_is_not_taken_for_a_missing_one() {
        let root = dir("unreadable");
        // A folder where the file should be: the read fails, and not with
        // "not found".
        fs::create_dir_all(root.join(FILE).join("inner")).unwrap();
        let loaded = Disk::new(&root).load();
        assert!(
            matches!(&loaded, Loaded::ReadOnly { warning } if warning.contains("could not be read")),
            "{loaded:?}"
        );
        assert!(root.join(FILE).join("inner").is_dir(), "left as it was");
        assert!(!root.join("settings.json.bad").exists());
    }

    #[test]
    fn launch_modes_are_saved_only_when_changed_and_an_old_file_keeps_the_default() {
        let old = r#"{"version":1,"utility":{"model":"haiku","effort":"low"}}"#;
        let parsed = Settings::parse(old).unwrap();
        assert_eq!(parsed.launch_modes, LaunchModes::default());
        // A file of someone who never chose stays free of the key.
        assert!(!parsed.to_json().contains("launch_modes"));
        let chosen = Settings {
            launch_modes: LaunchModes {
                claude: AgentMode::Auto,
                codex: AgentMode::Prompt,
                pi: AgentMode::Bypass,
                antigravity: AgentMode::Prompt,
                opencode: AgentMode::Auto,
            },
            ..parsed
        };
        let text = chosen.to_json();
        assert!(text.contains("\"claude\": \"auto\"") && text.contains("\"codex\": \"prompt\""));
        assert_eq!(
            Settings::parse(&text).unwrap().launch_modes,
            chosen.launch_modes
        );
    }

    #[test]
    fn an_unknown_launch_mode_word_keeps_the_default_for_that_tool_only() {
        let text = r#"{"version":1,"launch_modes":{"claude":"yolo","codex":"auto"}}"#;
        let parsed = Settings::parse(text).unwrap();
        assert_eq!(parsed.launch_modes.claude, AgentMode::Bypass);
        assert_eq!(parsed.launch_modes.codex, AgentMode::Auto);
    }

    /// A folder chosen on Windows is a drive path, possibly verbatim, with
    /// spaces or letters outside ASCII, or a share; it is kept exactly as
    /// chosen. A path that depends on the current drive is no choice.
    #[cfg(windows)]
    #[test]
    fn windows_folders_are_saved_and_read_back_as_chosen() {
        let root = dir("windows");
        for home in [
            r"C:\Users\me\Connecto Home",
            r"\\?\C:\Users\Jürgen\Ünïcødé ✓",
            r"D:\work",
            r"\\server\share\home",
        ] {
            let set = Settings {
                connecto_home: Some(PathBuf::from(home)),
                automations_root: Some(PathBuf::from(home).join("automations")),
                ..Settings::default()
            };
            Disk::new(&root).write(&set).unwrap();
            assert_eq!(Disk::new(&root).load(), Loaded::Read(set), "{home}");
        }
        for rel in [r"\x", "/x", "C:x"] {
            let text = json!({ "version": 1, "connecto_home": rel }).to_string();
            assert_eq!(Settings::parse(&text).unwrap().connecto_home, None, "{rel}");
        }
    }
}
