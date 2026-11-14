//! The groups the user makes: a title and the automations in it, kept in
//! `<data>/automations/groups.json`. The groups that ship with the app are
//! data in `defaults/groups.json` and are never stored here.

use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

/// Longest title, in characters.
const MAX_TITLE: usize = 80;

/// Where the user's groups are kept, under the data folder.
pub const GROUPS_FILE: &str = "groups.json";

/// The store as the facade and the engine's thread share it.
pub type SharedGroups = Arc<Mutex<GroupStore>>;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CustomGroup {
    pub id: String,
    pub title: String,
    pub members: Vec<String>,
}

#[derive(Debug, Clone, Default)]
struct Saved {
    /// The number the next group's id is made from; never reused.
    next: u64,
    groups: Vec<CustomGroup>,
}

impl Saved {
    fn parse(text: &str) -> Option<Self> {
        let value: Value = serde_json::from_str(text).ok()?;
        let groups = value["groups"]
            .as_array()?
            .iter()
            .filter_map(|row| {
                Some(CustomGroup {
                    id: row["id"].as_str()?.to_owned(),
                    title: row["title"].as_str()?.to_owned(),
                    members: row["members"]
                        .as_array()
                        .map(|m| {
                            m.iter()
                                .filter_map(|s| s.as_str().map(str::to_owned))
                                .collect()
                        })
                        .unwrap_or_default(),
                })
            })
            .collect();
        Some(Self {
            next: value["next"].as_u64().unwrap_or(0),
            groups,
        })
    }

    fn to_json(&self) -> Value {
        json!({
            "version": 1,
            "next": self.next,
            "groups": self.groups.iter().map(|g| json!({
                "id": g.id, "title": g.title, "members": g.members,
            })).collect::<Vec<_>>(),
        })
    }
}

#[derive(Debug)]
pub struct GroupStore {
    path: PathBuf,
    saved: Saved,
}

/// Lower-cased and trimmed, for comparing titles.
fn key(title: &str) -> String {
    title.trim().to_lowercase()
}

fn clean_title(title: &str) -> Result<String, String> {
    let title = title.trim();
    if title.is_empty() {
        return Err("give the group a name".to_owned());
    }
    if title.chars().count() > MAX_TITLE {
        return Err(format!("keep the group name under {MAX_TITLE} characters"));
    }
    Ok(title.to_owned())
}

impl GroupStore {
    /// Read the file. A missing or unreadable one is an empty store.
    #[must_use]
    pub fn load(path: PathBuf) -> Self {
        let mut saved: Saved = std::fs::read_to_string(&path)
            .ok()
            .and_then(|text| Saved::parse(&text))
            .unwrap_or_default();
        // Ids never repeat, whatever the file says. A counter at the end of
        // its range stays there: making a group then fails, loading does not.
        let floor = saved
            .groups
            .iter()
            .filter_map(|g| g.id.strip_prefix("custom-")?.parse::<u64>().ok())
            .max()
            .map_or(1, |n| n.saturating_add(1));
        saved.next = saved.next.max(floor);
        Self { path, saved }
    }

    #[must_use]
    pub fn shared(path: PathBuf) -> SharedGroups {
        Arc::new(Mutex::new(Self::load(path)))
    }

    /// The groups, in the order they were made.
    #[must_use]
    pub fn groups(&self) -> &[CustomGroup] {
        &self.saved.groups
    }

    /// Apply `change` to a copy, write it, and keep it only when written.
    fn commit<T>(
        &mut self,
        change: impl FnOnce(&mut Saved) -> Result<T, String>,
    ) -> Result<T, String> {
        let mut next = self.saved.clone();
        let out = change(&mut next)?;
        write_atomic(&self.path, &next)
            .map_err(|why| format!("could not save the groups: {why}"))?;
        self.saved = next;
        Ok(out)
    }

    /// Make a group; `taken` are the titles of the groups that ship.
    pub fn create(&mut self, title: &str, taken: &[String]) -> Result<String, String> {
        let title = clean_title(title)?;
        self.commit(|s| {
            check_unique(s, &title, None, taken)?;
            let n = s.next.max(1);
            let after = n.checked_add(1).ok_or("no more groups can be made")?;
            if s.groups.iter().any(|g| g.id == format!("custom-{n}")) {
                return Err("no more groups can be made".to_owned());
            }
            let id = format!("custom-{n}");
            s.next = after;
            s.groups.push(CustomGroup {
                id: id.clone(),
                title,
                members: Vec::new(),
            });
            Ok(id)
        })
    }

    pub fn rename(&mut self, id: &str, title: &str, taken: &[String]) -> Result<(), String> {
        let title = clean_title(title)?;
        self.commit(|s| {
            check_unique(s, &title, Some(id), taken)?;
            let group = s
                .groups
                .iter_mut()
                .find(|g| g.id == id)
                .ok_or("no such group")?;
            group.title = title;
            Ok(())
        })
    }

    pub fn delete(&mut self, id: &str) -> Result<(), String> {
        self.commit(|s| {
            let before = s.groups.len();
            s.groups.retain(|g| g.id != id);
            if s.groups.len() == before {
                return Err("no such group".to_owned());
            }
            Ok(())
        })
    }

    /// Put `slug` in group `id` (leaving any other), or in none.
    pub fn set_member(&mut self, slug: &str, id: Option<&str>) -> Result<(), String> {
        self.commit(|s| {
            if let Some(id) = id
                && !s.groups.iter().any(|g| g.id == id)
            {
                return Err("no such group".to_owned());
            }
            for g in &mut s.groups {
                g.members.retain(|m| m != slug);
                if Some(g.id.as_str()) == id {
                    g.members.push(slug.to_owned());
                }
            }
            Ok(())
        })
    }

    /// An automation was deleted. Nothing is written when it was in no group.
    pub fn forget(&mut self, slug: &str) {
        if self
            .saved
            .groups
            .iter()
            .any(|g| g.members.iter().any(|m| m == slug))
        {
            let _ = self.commit(|s| {
                for g in &mut s.groups {
                    g.members.retain(|m| m != slug);
                }
                Ok(())
            });
        }
    }
}

fn check_unique(
    saved: &Saved,
    title: &str,
    except: Option<&str>,
    taken: &[String],
) -> Result<(), String> {
    let wanted = key(title);
    let clash = taken.iter().any(|t| key(t) == wanted)
        || saved
            .groups
            .iter()
            .any(|g| Some(g.id.as_str()) != except && key(&g.title) == wanted);
    if clash {
        Err(format!("there is already a group called \"{title}\""))
    } else {
        Ok(())
    }
}

fn write_atomic(path: &Path, saved: &Saved) -> std::io::Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let text = serde_json::to_string_pretty(&saved.to_json()).map_err(std::io::Error::other)?;
    let tmp = path.with_extension("json.tmp");
    std::fs::write(&tmp, text + "\n")?;
    std::fs::rename(&tmp, path).inspect_err(|_| {
        let _ = std::fs::remove_file(&tmp);
    })
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn dir(name: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("connecto-groups-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        d
    }

    #[test]
    fn creates_trimmed_unique_and_persists() {
        let path = dir("create").join(GROUPS_FILE);
        let mut s = GroupStore::load(path.clone());
        let taken = vec!["Accept defaults for Claude".to_owned()];
        let a = s.create("  Mine ", &taken).unwrap();
        let b = s.create("Other", &taken).unwrap();
        assert_ne!(a, b);
        assert_eq!(s.groups()[0].title, "Mine");
        assert!(s.create("", &taken).is_err());
        assert!(s.create("   ", &taken).is_err());
        assert!(s.create("MINE", &taken).is_err());
        assert!(s.create("accept defaults for claude", &taken).is_err());
        assert!(s.create(&"x".repeat(81), &taken).is_err());
        let again = GroupStore::load(path);
        assert_eq!(again.groups(), s.groups());
    }

    #[test]
    fn ids_are_not_reused_after_delete_and_restart() {
        let path = dir("ids").join(GROUPS_FILE);
        let mut s = GroupStore::load(path.clone());
        let a = s.create("A", &[]).unwrap();
        s.delete(&a).unwrap();
        let mut s = GroupStore::load(path);
        let b = s.create("B", &[]).unwrap();
        assert_ne!(a, b);
    }

    #[test]
    fn an_exhausted_counter_loads_and_refuses_to_make_more() {
        let d = dir("overflow");
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join(GROUPS_FILE);
        let max = u64::MAX;
        std::fs::write(
            &path,
            format!(
                r#"{{"next": {max}, "groups": [{{"id": "custom-{max}", "title": "Old", "members": []}}]}}"#
            ),
        )
        .unwrap();
        let mut s = GroupStore::load(path.clone());
        assert_eq!(s.groups().len(), 1);
        assert!(s.create("New", &[]).is_err());
        assert_eq!(s.groups().len(), 1);
        // The other groups still work.
        s.rename(&format!("custom-{max}"), "Renamed", &[]).unwrap();
        // A counter alone at the end of its range is refused the same way.
        std::fs::write(&path, format!(r#"{{"next": {max}, "groups": []}}"#)).unwrap();
        assert!(GroupStore::load(path).create("X", &[]).is_err());
    }

    #[test]
    fn rename_checks_like_create_but_allows_own_title() {
        let mut s = GroupStore::load(dir("rename").join(GROUPS_FILE));
        let a = s.create("A", &[]).unwrap();
        s.create("B", &[]).unwrap();
        assert!(s.rename(&a, "b", &[]).is_err());
        assert!(s.rename(&a, " ", &[]).is_err());
        s.rename(&a, "a", &[]).unwrap();
        s.rename(&a, "  Renamed ", &[]).unwrap();
        assert_eq!(s.groups()[0].title, "Renamed");
        assert!(s.rename("nope", "Z", &[]).is_err());
    }

    #[test]
    fn membership_moves_between_groups_and_forgets() {
        let mut s = GroupStore::load(dir("members").join(GROUPS_FILE));
        let a = s.create("A", &[]).unwrap();
        let b = s.create("B", &[]).unwrap();
        s.set_member("x", Some(&a)).unwrap();
        s.set_member("x", Some(&b)).unwrap();
        assert!(s.groups()[0].members.is_empty());
        assert_eq!(s.groups()[1].members, ["x"]);
        assert!(s.set_member("x", Some("nope")).is_err());
        assert_eq!(s.groups()[1].members, ["x"]);
        s.forget("x");
        assert!(s.groups()[1].members.is_empty());
        s.set_member("y", Some(&a)).unwrap();
        s.set_member("y", None).unwrap();
        assert!(s.groups()[0].members.is_empty());
    }

    #[test]
    fn delete_removes_only_the_group() {
        let mut s = GroupStore::load(dir("delete").join(GROUPS_FILE));
        let a = s.create("A", &[]).unwrap();
        s.set_member("x", Some(&a)).unwrap();
        s.delete(&a).unwrap();
        assert!(s.groups().is_empty());
        assert!(s.delete(&a).is_err());
    }

    #[test]
    fn a_missing_or_corrupt_file_is_an_empty_store() {
        let d = dir("corrupt");
        std::fs::create_dir_all(&d).unwrap();
        let path = d.join(GROUPS_FILE);
        assert!(GroupStore::load(path.clone()).groups().is_empty());
        std::fs::write(&path, "{ not json").unwrap();
        let mut s = GroupStore::load(path.clone());
        assert!(s.groups().is_empty());
        s.create("Fresh", &[]).unwrap();
        assert_eq!(GroupStore::load(path).groups().len(), 1);
    }

    #[test]
    fn a_failed_write_changes_nothing() {
        let d = dir("fail");
        std::fs::create_dir_all(&d).unwrap();
        // The file's place is a folder, so the rename fails.
        let path = d.join(GROUPS_FILE);
        std::fs::create_dir_all(&path).unwrap();
        let mut s = GroupStore::load(path);
        assert!(s.create("A", &[]).is_err());
        assert!(s.groups().is_empty());
    }
}
