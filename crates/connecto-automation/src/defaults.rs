//! The automations a new install comes with.
//!
//! They are data: `defaults/<slug>.json`, embedded in the app. Each answers one
//! of the first-run screens Claude Code and Codex show (the welcome screen, the
//! folder trust prompts, the bypass warning, the theme picker, the Chrome
//! prompt), so a newcomer's first terminal gets past them. The files in
//! `defaults/` ship **switched off**, and that stays so for an existing install.
//! A **first install** writes them switched on instead (see [`is_first_install`]):
//! they only act once a Jev key is set, which is the gate that stays as it was.
//!
//! Seeding is once per default and never takes anything from the user:
//! - a default is recorded in `defaults.json` (in the automations data folder)
//!   when it is first seen, whether it was written or the user already had it;
//! - an automation of the same folder name, or for the same agent with the same
//!   trigger phrases, counts as the user's own: nothing is written, no duplicate;
//! - one that was written and later deleted by the user stays deleted;
//! - one the user edited (even to switch it on or off) is never overwritten; one
//!   that is still exactly as it was written is replaced when a later version
//!   ships a new text for it, keeping the on/off state it was written with;
//! - a default added in a later version to an install that already exists is
//!   written switched off.
//!
//! A first install is one seeding pass: `defaults.json` says `first_install`
//! before anything is written, so an interrupted pass resumes as a first install,
//! and the flag is cleared when the pass ends.

use std::collections::BTreeMap;
use std::fs;
use std::path::Path;

use connecto_hook::automation::{Definition, FILE_NAME};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::store;

/// One shipped default.
pub struct Default {
    pub slug: &'static str,
    pub text: &'static str,
}

/// Every default, in the order they are listed.
pub const DEFAULTS: [Default; 6] = [
    Default {
        slug: "accept-claude-welcome",
        text: include_str!("../defaults/accept-claude-welcome.json"),
    },
    Default {
        slug: "accept-claude-dark-theme",
        text: include_str!("../defaults/accept-claude-dark-theme.json"),
    },
    Default {
        slug: "accept-claude-trust-folder",
        text: include_str!("../defaults/accept-claude-trust-folder.json"),
    },
    Default {
        slug: "accept-claude-bypass-warning",
        text: include_str!("../defaults/accept-claude-bypass-warning.json"),
    },
    Default {
        slug: "accept-claude-chrome-prompt",
        text: include_str!("../defaults/accept-claude-chrome-prompt.json"),
    },
    Default {
        slug: "accept-codex-folder-trust",
        text: include_str!("../defaults/accept-codex-folder-trust.json"),
    },
];

/// The groups the defaults are shipped in (`defaults/groups.json`): data, so a
/// later release adds a group, or a group for another agent, by adding an entry.
const GROUPS_JSON: &str = include_str!("../defaults/groups.json");

/// The shipped groups, with all their members named. Members that are not in
/// `present` (the folder names that exist now) are left out, and so is a group
/// left with none.
#[must_use]
pub fn groups(present: &dyn Fn(&str) -> bool) -> Vec<connecto_core::AutomationGroup> {
    let Ok(Value::Array(rows)) = serde_json::from_str::<Value>(GROUPS_JSON) else {
        return Vec::new();
    };
    rows.iter()
        .filter_map(|row| {
            let agent = match row["agent"].as_str()? {
                "claude" => connecto_core::ToolKind::Claude,
                "codex" => connecto_core::ToolKind::Codex,
                _ => return None,
            };
            let members: Vec<String> = row["members"]
                .as_array()?
                .iter()
                .filter_map(|m| m.as_str())
                .filter(|slug| present(slug))
                .map(str::to_owned)
                .collect();
            (!members.is_empty()).then(|| connecto_core::AutomationGroup {
                id: row["id"].as_str().unwrap_or_default().to_owned(),
                title: row["title"].as_str().unwrap_or_default().to_owned(),
                about: row["about"].as_str().unwrap_or_default().to_owned(),
                agent,
                members,
                custom: false,
            })
        })
        .collect()
}

/// The titles of the shipped groups.
#[must_use]
pub fn group_titles() -> Vec<String> {
    groups(&|_| true).into_iter().map(|g| g.title).collect()
}

/// The ids of the shipped groups.
#[must_use]
pub fn group_ids() -> Vec<String> {
    groups(&|_| true).into_iter().map(|g| g.id).collect()
}

/// Whether a shipped group names `slug` as a member.
#[must_use]
pub fn is_member(slug: &str) -> bool {
    groups(&|_| true)
        .iter()
        .any(|g| g.members.iter().any(|m| m == slug))
}

fn hash(text: &str) -> String {
    Sha256::digest(text.as_bytes())
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

/// What `defaults.json` remembers.
#[derive(Debug, Clone, PartialEq)]
struct State {
    /// True while a first install's seeding pass is running (or was interrupted).
    first: bool,
    /// Slug to the hash of the text last written for it. Empty: the user's own.
    seeded: BTreeMap<String, String>,
}

fn load_state(path: &Path) -> State {
    let value = fs::read_to_string(path)
        .ok()
        .and_then(|text| serde_json::from_str::<Value>(&text).ok())
        .unwrap_or(Value::Null);
    let seeded = value["seeded"]
        .as_object()
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| Some((k.clone(), v.as_str()?.to_owned())))
                .collect()
        })
        .unwrap_or_default();
    State {
        first: value["first_install"].as_bool().unwrap_or(false),
        seeded,
    }
}

fn save_state(path: &Path, state: &State) {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let text = serde_json::to_string_pretty(&json!({
        "version": 1,
        "first_install": state.first,
        "seeded": state.seeded,
    }))
    .unwrap_or_default();
    let tmp = path.with_extension("json.tmp");
    if fs::write(&tmp, text + "\n").is_ok() {
        let _ = fs::rename(&tmp, path);
    }
}

const SWITCHED_OFF: &str = "\"enabled\": false";
const SWITCHED_ON: &str = "\"enabled\": true";

/// The text a default is written with: as shipped (off), or switched on.
fn written_text(shipped: &str, on: bool) -> String {
    if on {
        shipped.replacen(SWITCHED_OFF, SWITCHED_ON, 1)
    } else {
        shipped.to_owned()
    }
}

fn enabled_in(text: &str) -> bool {
    Definition::parse_lenient(text).is_ok_and(|def| def.enabled)
}

/// A first install: nothing has been seeded here, and the automations folder
/// holds nothing at all and the data folder no history. Anything that could be
/// an earlier install (a folder, a file, a history) makes it an existing one.
fn is_first_install(root: &Path, state_path: &Path) -> bool {
    let absent = matches!(
        fs::symlink_metadata(state_path),
        Err(err) if err.kind() == std::io::ErrorKind::NotFound
    );
    let empty = match fs::read_dir(root) {
        Ok(mut read) => read.next().is_none(),
        Err(err) => err.kind() == std::io::ErrorKind::NotFound,
    };
    let no_history = state_path
        .parent()
        .is_some_and(|base| matches!(fs::symlink_metadata(base.join(store::HISTORY_FILE)), Err(err) if err.kind() == std::io::ErrorKind::NotFound));
    absent && empty && no_history
}

fn write_default(root: &Path, slug: &str, text: &str) -> std::io::Result<()> {
    let dir = root.join(slug);
    fs::create_dir_all(&dir)?;
    let file = dir.join(FILE_NAME);
    let tmp = dir.join(format!(".{FILE_NAME}.tmp"));
    fs::write(&tmp, text)?;
    fs::rename(&tmp, file)
}

/// Bring `root` (the automations folder) up to date with the defaults, as the
/// module comment says. `state_path` is where what was seeded is remembered.
/// Returns the slugs that were written. Never fails the caller: a default that
/// cannot be written is tried again next time.
pub fn seed(root: &Path, state_path: &Path) -> Vec<String> {
    seed_with(root, state_path, &DEFAULTS)
}

fn seed_with(root: &Path, state_path: &Path, defaults: &[Default]) -> Vec<String> {
    let mut state = load_state(state_path);
    let before = state.clone();
    if is_first_install(root, state_path) {
        state.first = true;
        // Saved before anything is written, so an interrupted pass resumes as one.
        save_state(state_path, &state);
    }
    let pass_is_first = state.first;
    let existing = store::scan(root);
    let mut written = Vec::new();
    for default in defaults {
        let folder = root.join(default.slug).join(FILE_NAME);
        if let Some(recorded) = state.seeded.get(default.slug).cloned() {
            // Seen before. Theirs when empty or edited; a deleted one stays
            // deleted; an unedited one follows the shipped text, keeping the
            // on/off state it was written with.
            if recorded.is_empty() {
                continue;
            }
            let Ok(text) = fs::read_to_string(&folder) else {
                continue;
            };
            if hash(&text) != recorded {
                continue;
            }
            let next = written_text(default.text, enabled_in(&text));
            let next_hash = hash(&next);
            if next_hash != recorded && write_default(root, default.slug, &next).is_ok() {
                state.seeded.insert(default.slug.to_owned(), next_hash);
            }
            continue;
        }
        let Ok(wanted) = Definition::parse(default.text) else {
            continue;
        };
        let text = written_text(default.text, pass_is_first);
        // An interrupted first pass may have written this one already: the very
        // same bytes are ours, so it is recorded and not written again.
        if pass_is_first && fs::read_to_string(&folder).ok().as_deref() == Some(text.as_str()) {
            state.seeded.insert(default.slug.to_owned(), hash(&text));
            continue;
        }
        let users_own = existing.iter().any(|entry| {
            entry.slug == default.slug
                || entry
                    .def
                    .as_ref()
                    .is_ok_and(|def| def.agent == wanted.agent && def.trigger == wanted.trigger)
        });
        if users_own {
            // Recorded as theirs: never written, never updated.
            state.seeded.insert(default.slug.to_owned(), String::new());
        } else if write_default(root, default.slug, &text).is_ok() {
            state.seeded.insert(default.slug.to_owned(), hash(&text));
            written.push(default.slug.to_owned());
        }
    }
    // The first install's pass is over: whatever comes later is an existing
    // install's, and a default added in a later version is off.
    state.first = false;
    if state != before || pass_is_first {
        save_state(state_path, &state);
    }
    written
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use super::*;
    use connecto_hook::automation::Agent;
    use std::path::PathBuf;

    struct Dir(PathBuf);
    impl Dir {
        fn new(name: &str) -> Self {
            let nanos = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map_or(0, |d| d.as_nanos());
            let path = std::env::temp_dir().join(format!(
                "connecto-defaults-{name}-{}-{nanos}",
                std::process::id()
            ));
            fs::create_dir_all(&path).unwrap();
            Self(path)
        }
        fn root(&self) -> PathBuf {
            self.0.join("automations")
        }
        fn state(&self) -> PathBuf {
            self.0.join("data/defaults.json")
        }
    }
    impl Drop for Dir {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.0);
        }
    }

    /// The shipped default with its name changed: what a later version ships.
    fn renamed(text: &str, name: &str) -> &'static str {
        let mut value: Value = serde_json::from_str(text).unwrap();
        value["name"] = Value::String(name.to_owned());
        let out = format!("{}\n", serde_json::to_string_pretty(&value).unwrap());
        Box::leak(out.into_boxed_str())
    }

    /// The shipped defaults as a later version ships them, the welcome one renamed.
    fn later_version(welcome_name: &str) -> Vec<Default> {
        DEFAULTS
            .iter()
            .map(|d| Default {
                slug: d.slug,
                text: if d.slug == "accept-claude-welcome" {
                    renamed(d.text, welcome_name)
                } else {
                    d.text
                },
            })
            .collect()
    }

    /// The files as an earlier version left them: shipped, off, recorded as written.
    fn earlier_install(dir: &Dir) {
        let mut state = State {
            first: false,
            seeded: BTreeMap::new(),
        };
        for default in &DEFAULTS {
            write_default(&dir.root(), default.slug, default.text).unwrap();
            state
                .seeded
                .insert(default.slug.to_owned(), hash(default.text));
        }
        save_state(&dir.state(), &state);
    }

    fn enabled_on_disk(dir: &Dir, slug: &str) -> bool {
        let text = fs::read_to_string(dir.root().join(slug).join(FILE_NAME)).unwrap();
        Definition::parse_lenient(&text).unwrap().enabled
    }

    #[test]
    fn every_shipped_default_is_valid_and_ships_off_with_one_switch_to_turn_on() {
        let mut slugs = std::collections::BTreeSet::new();
        for default in &DEFAULTS {
            let def = Definition::parse(default.text)
                .unwrap_or_else(|why| panic!("{}: {why:?}", default.slug));
            assert!(!def.enabled, "{} ships switched off", default.slug);
            assert_eq!(
                default.text.matches(SWITCHED_OFF).count(),
                1,
                "{} has one switch",
                default.slug
            );
            let on = Definition::parse(&written_text(default.text, true))
                .unwrap_or_else(|why| panic!("{} on: {why:?}", default.slug));
            assert!(on.enabled, "{} switched on", default.slug);
            assert_eq!(
                on.trigger, def.trigger,
                "{}: only the switch differs",
                default.slug
            );
            assert!(slugs.insert(default.slug), "{} twice", default.slug);
            for private in ["/Users/", "/home/", "@", "http", "sk-", "Bearer"] {
                assert!(
                    !default.text.contains(private),
                    "{} mentions {private}",
                    default.slug
                );
            }
        }
    }

    #[test]
    fn a_first_install_gets_all_of_them_on_and_a_second_start_changes_nothing() {
        let dir = Dir::new("fresh");
        let first = seed(&dir.root(), &dir.state());
        assert_eq!(first.len(), DEFAULTS.len());
        let entries = store::scan(&dir.root());
        assert_eq!(entries.len(), DEFAULTS.len());
        assert!(
            entries
                .iter()
                .all(|e| e.def.as_ref().is_ok_and(|d| d.enabled))
        );
        let before: Vec<_> = DEFAULTS
            .iter()
            .map(|d| fs::read_to_string(dir.root().join(d.slug).join(FILE_NAME)).unwrap())
            .collect();
        assert!(seed(&dir.root(), &dir.state()).is_empty());
        let after: Vec<_> = DEFAULTS
            .iter()
            .map(|d| fs::read_to_string(dir.root().join(d.slug).join(FILE_NAME)).unwrap())
            .collect();
        assert_eq!(before, after);
        assert!(!load_state(&dir.state()).first, "the pass is over");
    }

    #[test]
    fn a_first_install_writes_no_other_automation_and_a_new_one_of_the_users_is_off() {
        let dir = Dir::new("users-new");
        seed(&dir.root(), &dir.state());
        let slug = store::create(&dir.root(), "My own check", Agent::Claude).unwrap();
        let def = Definition::parse_lenient(
            &fs::read_to_string(dir.root().join(&slug).join(FILE_NAME)).unwrap(),
        )
        .unwrap();
        assert!(!def.enabled, "a new automation keeps the default off");
        assert_eq!(store::scan(&dir.root()).len(), DEFAULTS.len() + 1);
    }

    #[test]
    fn a_first_install_interrupted_part_way_resumes_as_a_first_install() {
        let dir = Dir::new("interrupted");
        // Saved first, then two were written before the pass stopped.
        save_state(
            &dir.state(),
            &State {
                first: true,
                seeded: BTreeMap::new(),
            },
        );
        for default in &DEFAULTS[..2] {
            write_default(&dir.root(), default.slug, &written_text(default.text, true)).unwrap();
        }
        let written = seed(&dir.root(), &dir.state());
        assert_eq!(
            written.len(),
            DEFAULTS.len() - 2,
            "only the rest is written"
        );
        assert_eq!(
            store::scan(&dir.root()).len(),
            DEFAULTS.len(),
            "no duplicate"
        );
        for default in &DEFAULTS {
            assert!(enabled_on_disk(&dir, default.slug), "{} on", default.slug);
        }
        let state = load_state(&dir.state());
        assert!(!state.first);
        assert_eq!(state.seeded.len(), DEFAULTS.len(), "all recorded as ours");
        assert!(seed(&dir.root(), &dir.state()).is_empty());
    }

    #[test]
    fn a_first_install_interrupted_before_anything_was_written_resumes_as_one() {
        let dir = Dir::new("interrupted-early");
        save_state(
            &dir.state(),
            &State {
                first: true,
                seeded: BTreeMap::new(),
            },
        );
        assert_eq!(seed(&dir.root(), &dir.state()).len(), DEFAULTS.len());
        for default in &DEFAULTS {
            assert!(enabled_on_disk(&dir, default.slug), "{} on", default.slug);
        }
    }

    #[test]
    fn anything_that_could_be_an_earlier_install_makes_it_an_existing_one() {
        // A folder of the user's, with no automation in it.
        let dir = Dir::new("existing-folder");
        fs::create_dir_all(dir.root().join("notes")).unwrap();
        assert_eq!(seed(&dir.root(), &dir.state()).len(), DEFAULTS.len());
        for default in &DEFAULTS {
            assert!(!enabled_on_disk(&dir, default.slug), "{} off", default.slug);
        }
        // Only a history from an earlier install, the folder empty.
        let dir = Dir::new("existing-history");
        fs::create_dir_all(dir.root()).unwrap();
        fs::create_dir_all(dir.0.join("data")).unwrap();
        fs::write(dir.0.join("data").join(store::HISTORY_FILE), "").unwrap();
        seed(&dir.root(), &dir.state());
        for default in &DEFAULTS {
            assert!(!enabled_on_disk(&dir, default.slug), "{} off", default.slug);
        }
        // Files at the top of the folder (a guide, say) and no state at all.
        let dir = Dir::new("existing-file");
        fs::create_dir_all(dir.root()).unwrap();
        fs::write(dir.root().join("CLAUDE.md"), "guide").unwrap();
        seed(&dir.root(), &dir.state());
        assert!(!enabled_on_disk(&dir, DEFAULTS[0].slug));
    }

    #[test]
    fn an_existing_install_with_everything_off_stays_off_through_a_shipped_update() {
        let dir = Dir::new("existing-off");
        earlier_install(&dir);
        let state_before = fs::read_to_string(dir.state()).unwrap();
        assert!(seed(&dir.root(), &dir.state()).is_empty());
        assert_eq!(fs::read_to_string(dir.state()).unwrap(), state_before);
        // A later version ships new text for the welcome default: it follows, off.
        let later = later_version("Continue the welcome (v2)");
        seed_with(&dir.root(), &dir.state(), &later);
        for default in &DEFAULTS {
            assert!(!enabled_on_disk(&dir, default.slug), "{} off", default.slug);
        }
        let text = fs::read_to_string(dir.root().join(DEFAULTS[0].slug).join(FILE_NAME)).unwrap();
        assert!(text.contains("Continue the welcome (v2)"), "text updated");
    }

    #[test]
    fn an_existing_install_with_a_mix_keeps_each_state() {
        let dir = Dir::new("existing-mix");
        earlier_install(&dir);
        // Switched on by the user (an edit, so theirs); deleted by the user; left as is.
        let on = DEFAULTS[1].slug;
        let path = dir.root().join(on).join(FILE_NAME);
        let text = fs::read_to_string(&path)
            .unwrap()
            .replace(SWITCHED_OFF, SWITCHED_ON);
        fs::write(&path, &text).unwrap();
        fs::remove_dir_all(dir.root().join(DEFAULTS[2].slug)).unwrap();
        let later = later_version("Continue the welcome (v2)");
        seed_with(&dir.root(), &dir.state(), &later);
        assert_eq!(
            fs::read_to_string(&path).unwrap(),
            text,
            "theirs: as it was"
        );
        assert!(
            !dir.root().join(DEFAULTS[2].slug).exists(),
            "deleted stays deleted"
        );
        assert!(
            !enabled_on_disk(&dir, DEFAULTS[0].slug),
            "untouched: still off"
        );
    }

    #[test]
    fn a_first_install_keeps_its_on_state_through_a_shipped_update_and_a_switch_off_sticks() {
        let dir = Dir::new("on-then-update");
        seed(&dir.root(), &dir.state());
        let v2 = later_version("Continue the welcome (v2)");
        seed_with(&dir.root(), &dir.state(), &v2);
        let welcome = DEFAULTS[0].slug;
        let text = fs::read_to_string(dir.root().join(welcome).join(FILE_NAME)).unwrap();
        assert!(text.contains("Continue the welcome (v2)"), "text updated");
        assert!(enabled_on_disk(&dir, welcome), "first-install state kept");
        // The user switches one off: an edit, so it is theirs from now on.
        let off = DEFAULTS[1].slug;
        let path = dir.root().join(off).join(FILE_NAME);
        let edited = fs::read_to_string(&path)
            .unwrap()
            .replace(SWITCHED_ON, SWITCHED_OFF);
        fs::write(&path, &edited).unwrap();
        let v3 = later_version("Continue the welcome (v3)");
        seed_with(&dir.root(), &dir.state(), &v3);
        assert!(!enabled_on_disk(&dir, off), "stays off");
        assert_eq!(fs::read_to_string(&path).unwrap(), edited, "left alone");
        assert!(
            enabled_on_disk(&dir, welcome),
            "still on after another update"
        );
    }

    #[test]
    fn a_default_added_in_a_later_version_to_an_existing_install_is_off() {
        let dir = Dir::new("later-default");
        seed(&dir.root(), &dir.state());
        let mut value: Value = serde_json::from_str(DEFAULTS[0].text).unwrap();
        value["trigger"] = json!(["A screen nobody has seen"]);
        value["name"] = json!("Brand new");
        let fresh = Box::leak(format!("{value}\n").into_boxed_str());
        let mut list = later_version("Continue the welcome (v2)");
        list.push(Default {
            slug: "brand-new-default",
            text: fresh,
        });
        let written = seed_with(&dir.root(), &dir.state(), &list);
        assert_eq!(written, ["brand-new-default"]);
        assert!(!enabled_on_disk(&dir, "brand-new-default"));
        assert!(
            enabled_on_disk(&dir, DEFAULTS[0].slug),
            "the others keep theirs"
        );
    }

    #[test]
    fn a_default_the_user_deleted_does_not_come_back() {
        let dir = Dir::new("deleted");
        seed(&dir.root(), &dir.state());
        fs::remove_dir_all(dir.root().join("accept-claude-welcome")).unwrap();
        assert!(seed(&dir.root(), &dir.state()).is_empty());
        assert!(!dir.root().join("accept-claude-welcome").exists());
    }

    #[test]
    fn an_edited_default_is_never_overwritten_and_an_untouched_one_follows_updates() {
        let dir = Dir::new("edited");
        seed(&dir.root(), &dir.state());
        // The user switches one off: edited, so theirs.
        let edited = dir.root().join("accept-claude-welcome").join(FILE_NAME);
        let text = fs::read_to_string(&edited)
            .unwrap()
            .replace(SWITCHED_ON, SWITCHED_OFF);
        fs::write(&edited, &text).unwrap();
        // A later version ships new text for two defaults: pretend by changing what was recorded.
        let mut state = load_state(&dir.state());
        state
            .seeded
            .insert("accept-claude-welcome".to_owned(), "old".to_owned());
        state.seeded.insert(
            "accept-codex-folder-trust".to_owned(),
            hash("an older text"),
        );
        save_state(&dir.state(), &state);
        let other = dir.root().join("accept-codex-folder-trust").join(FILE_NAME);
        fs::write(&other, "an older text").unwrap();
        seed(&dir.root(), &dir.state());
        assert_eq!(
            fs::read_to_string(&edited).unwrap(),
            text,
            "edited: left alone"
        );
        let updated = DEFAULTS
            .iter()
            .find(|d| d.slug == "accept-codex-folder-trust")
            .unwrap();
        assert_eq!(
            fs::read_to_string(&other).unwrap(),
            updated.text,
            "untouched: updated"
        );
    }

    #[test]
    fn what_the_user_already_has_is_left_alone_and_never_duplicated() {
        let dir = Dir::new("migrated");
        let root = dir.root();
        // Same folder name, edited.
        fs::create_dir_all(root.join("accept-claude-welcome")).unwrap();
        fs::write(
            root.join("accept-claude-welcome").join(FILE_NAME),
            "{ \"mine\": true }",
        )
        .unwrap();
        // Another folder name, same agent and same trigger phrases as a default
        // (an earlier install's own copy), and switched on.
        let theirs = DEFAULTS
            .iter()
            .find(|d| d.slug == "accept-codex-folder-trust")
            .unwrap();
        let mut value: Value = serde_json::from_str(theirs.text).unwrap();
        value["enabled"] = Value::Bool(true);
        value["name"] = Value::String("Codex Accept Folder".to_owned());
        fs::create_dir_all(root.join("codex-accept-folder")).unwrap();
        let mine = serde_json::to_string(&value).unwrap();
        fs::write(root.join("codex-accept-folder").join(FILE_NAME), &mine).unwrap();
        let written = seed(&root, &dir.state());
        assert!(!written.contains(&"accept-claude-welcome".to_owned()));
        assert!(!written.contains(&"accept-codex-folder-trust".to_owned()));
        assert_eq!(written.len(), DEFAULTS.len() - 2);
        assert_eq!(
            fs::read_to_string(root.join("accept-claude-welcome").join(FILE_NAME)).unwrap(),
            "{ \"mine\": true }"
        );
        assert_eq!(
            fs::read_to_string(root.join("codex-accept-folder").join(FILE_NAME)).unwrap(),
            mine
        );
        assert!(!root.join("accept-codex-folder-trust").exists());
        // Recorded as theirs: a later start still writes nothing.
        assert!(seed(&root, &dir.state()).is_empty());
    }

    #[test]
    fn every_default_is_in_exactly_one_group_of_its_own_agent() {
        let all = groups(&|_| true);
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].title, "Accept defaults for Claude");
        assert_eq!(all[1].title, "Accept defaults for Codex");
        let mut ids = std::collections::BTreeSet::new();
        let mut seen = std::collections::BTreeMap::new();
        for group in &all {
            assert!(ids.insert(group.id.clone()), "{} twice", group.id);
            assert!(!group.about.is_empty());
            for slug in &group.members {
                assert!(
                    seen.insert(slug.clone(), group.id.clone()).is_none(),
                    "{slug} in two groups"
                );
                let default = DEFAULTS.iter().find(|d| d.slug == slug).unwrap();
                let def = Definition::parse(default.text).unwrap();
                let agent = match def.agent {
                    connecto_hook::automation::Agent::Claude => connecto_core::ToolKind::Claude,
                    connecto_hook::automation::Agent::Codex => connecto_core::ToolKind::Codex,
                };
                assert_eq!(agent, group.agent, "{slug} is in a group of another agent");
            }
        }
        for default in &DEFAULTS {
            assert!(
                seen.contains_key(default.slug),
                "{} is in no group",
                default.slug
            );
        }
    }

    #[test]
    fn a_group_lists_only_the_members_that_exist_and_vanishes_when_none_do() {
        let only = |wanted: &'static [&'static str]| move |slug: &str| wanted.contains(&slug);
        let some = groups(&only(&["accept-claude-welcome", "mine-not-shipped"]));
        assert_eq!(some.len(), 1);
        assert_eq!(some[0].members, ["accept-claude-welcome"]);
        assert!(groups(&only(&["mine-not-shipped"])).is_empty());
    }

    #[test]
    fn defaults_seeded_by_an_earlier_version_are_grouped_without_a_write_or_a_reset() {
        let dir = Dir::new("upgrade");
        seed(&dir.root(), &dir.state());
        // The user switched one off (an edit, so it is theirs) and kept their own.
        let on = dir.root().join("accept-claude-welcome").join(FILE_NAME);
        let text = fs::read_to_string(&on)
            .unwrap()
            .replace(SWITCHED_ON, SWITCHED_OFF);
        fs::write(&on, &text).unwrap();
        fs::create_dir_all(dir.root().join("my-own")).unwrap();
        fs::write(
            dir.root().join("my-own").join(FILE_NAME),
            DEFAULTS[0].text.replace("Welcome", "Mine"),
        )
        .unwrap();
        let before: Vec<_> = DEFAULTS
            .iter()
            .map(|d| fs::read_to_string(dir.root().join(d.slug).join(FILE_NAME)).unwrap())
            .collect();
        // The upgrade starts: seeding again writes nothing.
        assert!(seed(&dir.root(), &dir.state()).is_empty());
        let after: Vec<_> = DEFAULTS
            .iter()
            .map(|d| fs::read_to_string(dir.root().join(d.slug).join(FILE_NAME)).unwrap())
            .collect();
        assert_eq!(before, after, "nothing was rewritten");
        assert!(
            after[0].contains(SWITCHED_OFF),
            "the switched-off one stays off"
        );
        // They are all found in their groups by folder name; the user's own is not.
        let exists = |slug: &str| dir.root().join(slug).join(FILE_NAME).exists();
        let all = groups(&exists);
        let claude = all
            .iter()
            .find(|g| g.id == "accept-defaults-claude")
            .unwrap();
        assert_eq!(claude.members.len(), 5);
        assert!(
            all.iter()
                .all(|g| !g.members.contains(&"my-own".to_owned()))
        );
        assert_eq!(
            fs::read_dir(dir.root()).unwrap().count(),
            7,
            "no duplicate was made"
        );
    }
}
