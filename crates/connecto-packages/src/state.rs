//! What Connecto remembers about packages between runs, in
//! `<data>/packages/state.json`: the content hash each package had at its
//! current version (so a change bumps the version by one), and which package
//! is the default.

use std::collections::BTreeMap;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde_json::{Map, Value, json};

use crate::package::BUILTIN_ID;

const FILE: &str = "state.json";
const VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Record {
    pub hash: String,
    pub version: u32,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct State {
    pub records: BTreeMap<String, Record>,
    /// `None`: never chosen, so the built-in package. `Some(None)`: no default.
    pub default: Option<Option<String>>,
    /// The file is there but could not be read or set aside: it is never
    /// written over.
    read_only: bool,
}

/// What `State::load` found.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Loaded {
    pub state: State,
    /// Why the saved state was not used, to show the user.
    pub warning: Option<String>,
}

#[must_use]
pub fn path(data_root: &Path) -> PathBuf {
    data_root.join("packages").join(FILE)
}

impl State {
    /// The default package's id.
    #[must_use]
    pub fn default_id(&self) -> Option<&str> {
        match &self.default {
            None => Some(BUILTIN_ID),
            Some(choice) => choice.as_deref(),
        }
    }

    /// Read the saved state. No file is a first run. A file that cannot be
    /// used is not: whether the user chose "no default package" is then
    /// unknown, and guessing "the built-in one" would install it into every
    /// project that uses the default. So nothing is the default until the
    /// user chooses again, and the file is kept aside as `state.json.bad`.
    #[must_use]
    pub fn load(data_root: &Path) -> Loaded {
        let file = path(data_root);
        let unknown = |read_only| Self {
            default: Some(None),
            read_only,
            ..Self::default()
        };
        const CHOOSE: &str =
            "no default package is installed until you choose one in Skills & Plugins";
        match fs::read(&file) {
            Ok(bytes) => {
                if let Some(state) = std::str::from_utf8(&bytes).ok().and_then(Self::parse) {
                    return Loaded {
                        state,
                        warning: None,
                    };
                }
                let bad = file.with_extension("json.bad");
                let moved = fs::rename(&file, &bad);
                Loaded {
                    state: unknown(moved.is_err()),
                    warning: Some(match moved {
                        Ok(()) => format!(
                            "the package state was damaged and was kept as {}: {CHOOSE}",
                            bad.display()
                        ),
                        Err(e) => format!(
                            "the package state {} is damaged and could not be set aside ({e}): {CHOOSE}",
                            file.display()
                        ),
                    }),
                }
            }
            Err(e) if e.kind() == io::ErrorKind::NotFound => Loaded::default(),
            Err(e) => Loaded {
                state: unknown(true),
                warning: Some(format!(
                    "the package state {} could not be read ({e}): {CHOOSE}",
                    file.display()
                )),
            },
        }
    }

    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        if value.get("version")?.as_u64()? != VERSION {
            return None;
        }
        let records = value
            .get("packages")
            .and_then(Value::as_object)
            .map(|map| {
                map.iter()
                    .filter_map(|(id, r)| {
                        Some((
                            id.clone(),
                            Record {
                                hash: r.get("hash")?.as_str()?.to_owned(),
                                version: u32::try_from(r.get("version")?.as_u64()?).ok()?,
                            },
                        ))
                    })
                    .collect()
            })
            .unwrap_or_default();
        let default = match value.get("default") {
            None => None,
            Some(Value::Null) => Some(None),
            Some(Value::String(id)) => Some(Some(id.clone())),
            Some(_) => None,
        };
        Some(Self {
            records,
            default,
            read_only: false,
        })
    }

    pub fn save(&self, data_root: &Path) -> io::Result<()> {
        if self.read_only {
            return Err(io::Error::other(
                "the saved state could not be read at startup and is left as it is",
            ));
        }
        let packages: Map<String, Value> = self
            .records
            .iter()
            .map(|(id, r)| (id.clone(), json!({"hash": r.hash, "version": r.version})))
            .collect();
        let mut value = json!({"version": VERSION, "packages": packages});
        match &self.default {
            None => {}
            Some(None) => value["default"] = Value::Null,
            Some(Some(id)) => value["default"] = json!(id),
        }
        let file = path(data_root);
        if let Some(parent) = file.parent() {
            fs::create_dir_all(parent)?;
        }
        let staged = file.with_extension("json.tmp");
        let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
        text.push('\n');
        fs::write(&staged, text)?;
        fs::rename(&staged, &file)
    }

    /// The version a package has now, given its manifest's version and the
    /// hash of its content. A change of content raises it by one; the
    /// manifest may raise it further (a copied package keeps its number).
    /// Returns the version and whether the record changed.
    pub fn reconcile(&mut self, id: &str, manifest_version: u32, hash: &str) -> (u32, bool) {
        let (version, record) = match self.records.get(id) {
            None => (manifest_version, true),
            Some(r) if r.hash == hash => {
                let v = r.version.max(manifest_version);
                (v, v != r.version)
            }
            Some(r) => (r.version.max(manifest_version).saturating_add(1), true),
        };
        if record {
            self.records.insert(
                id.to_owned(),
                Record {
                    hash: hash.to_owned(),
                    version,
                },
            );
        }
        (version, record)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    #[test]
    fn versions_start_at_the_manifest_and_bump_by_one_per_change() {
        let mut state = State::default();
        assert_eq!(state.reconcile("p", 1, "h1"), (1, true), "first sight");
        assert_eq!(state.reconcile("p", 1, "h1"), (1, false), "no change");
        assert_eq!(state.reconcile("p", 1, "h2"), (2, true), "content changed");
        assert_eq!(state.reconcile("p", 2, "h2"), (2, false));
        assert_eq!(state.reconcile("p", 2, "h3"), (3, true));
        // A copied package that already carries a higher number keeps it.
        assert_eq!(state.reconcile("q", 7, "x"), (7, true));
        assert_eq!(state.reconcile("q", 7, "y"), (8, true));
        // The manifest edited upwards by hand is honoured without a bump.
        assert_eq!(state.reconcile("q", 20, "y"), (20, true));
    }

    #[test]
    fn state_and_the_default_choice_round_trip() {
        let dir = std::env::temp_dir().join(format!("mg-pkgstate-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        assert_eq!(State::load(&dir).state.default_id(), Some("connecto"));
        let mut state = State::default();
        state.reconcile("p", 3, "h");
        state.default = Some(None);
        state.save(&dir).unwrap();
        let back = State::load(&dir).state;
        assert_eq!(back, state);
        assert_eq!(back.default_id(), None, "no default is remembered");
        state.default = Some(Some("p".to_owned()));
        state.save(&dir).unwrap();
        assert_eq!(State::load(&dir).state.default_id(), Some("p"));
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_damaged_or_other_version_state_is_kept_aside_and_nothing_is_the_default() {
        let dir = std::env::temp_dir().join(format!("mg-pkgstate-bad-{}", std::process::id()));
        for text in ["garbage", r#"{"version":99,"default":null}"#] {
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(path(&dir).parent().unwrap()).unwrap();
            fs::write(path(&dir), text).unwrap();
            let loaded = State::load(&dir);
            assert_eq!(
                loaded.state.default_id(),
                None,
                "{text}: not the built-in package"
            );
            let warning = loaded.warning.unwrap();
            assert!(warning.contains("state.json.bad"), "{warning}");
            let bad = path(&dir).with_extension("json.bad");
            assert_eq!(fs::read_to_string(bad).unwrap(), text);
            assert!(!path(&dir).exists());
            // The next save starts a new file, and "no default" is remembered.
            loaded.state.save(&dir).unwrap();
            let again = State::load(&dir);
            assert_eq!(again.warning, None);
            assert_eq!(again.state.default_id(), None);
        }
        let _ = fs::remove_dir_all(&dir);
    }

    #[test]
    fn a_state_file_that_cannot_be_read_is_never_written_over() {
        let dir = std::env::temp_dir().join(format!("mg-pkgstate-deny-{}", std::process::id()));
        let _ = fs::remove_dir_all(&dir);
        // A folder where the file should be: reading it fails, and not with
        // "not found".
        fs::create_dir_all(path(&dir).join("inner")).unwrap();
        let loaded = State::load(&dir);
        assert_eq!(loaded.state.default_id(), None);
        assert!(loaded.warning.unwrap().contains("could not be read"));
        assert!(loaded.state.save(&dir).is_err());
        assert!(path(&dir).join("inner").is_dir(), "left as it was");
        let _ = fs::remove_dir_all(&dir);
    }
}
