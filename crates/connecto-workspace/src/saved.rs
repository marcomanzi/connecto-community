//! The saved workspace, `<data>/workspace.json` (project-tree-ui R1).
//!
//! Pure data and the file it lives in. The workspace's worker thread is the
//! only caller, so nothing on the UI thread touches the disk.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use connecto_core::{MAX_NAME_CHARS, ProjectGroup, RecentProject};
use serde_json::{Value, json};

/// The file name inside the data folder.
pub(crate) const FILE: &str = "workspace.json";
const VERSION: u64 = 1;
/// Recent projects kept.
pub(crate) const MAX_RECENTS: usize = 10;
/// Open projects kept. A saved list longer than this is cut when read.
const MAX_PROJECTS: usize = 64;
/// Sidebar groups kept.
pub(crate) const MAX_GROUPS: usize = 32;

/// One open project as saved.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct SavedProject {
    pub path: String,
    pub name: Option<String>,
    pub expanded: bool,
    pub active_terminal: Option<String>,
    /// The id of its sidebar group.
    pub group: Option<String>,
    /// It was opened through a link somewhere in its path: kept so that it
    /// is never deleted after a relaunch either.
    pub linked: bool,
}

/// Everything the file holds.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub(crate) struct Saved {
    pub projects: Vec<SavedProject>,
    /// The path of the project shown.
    pub active: Option<String>,
    pub recents: Vec<RecentProject>,
    /// The recents that were open through a link, by path: never deleted
    /// when opened again from the recents.
    pub linked_recents: Vec<String>,
    pub sidebar_collapsed: bool,
    /// The sidebar's project groups, in order.
    pub groups: Vec<ProjectGroup>,
}

/// A group id as Connecto makes them: `group-<n>`.
pub(crate) fn valid_group_id(id: &str) -> bool {
    id.strip_prefix("group-")
        .is_some_and(|n| !n.is_empty() && n.len() <= 6 && n.bytes().all(|b| b.is_ascii_digit()))
}

/// A name that is one line of plain text and short enough, or `None`.
fn name_of(value: &Value) -> Option<String> {
    let name = value.as_str()?.trim();
    (!name.is_empty()
        && name.chars().count() <= MAX_NAME_CHARS
        && !name.chars().any(char::is_control))
    .then(|| name.to_owned())
}

impl Saved {
    pub(crate) fn to_json(&self) -> String {
        let projects: Vec<Value> = self
            .projects
            .iter()
            .map(|project| {
                let mut value = json!({ "path": project.path, "expanded": project.expanded });
                if let Some(name) = &project.name {
                    value["name"] = json!(name);
                }
                if let Some(terminal) = &project.active_terminal {
                    value["active_terminal"] = json!(terminal);
                }
                if let Some(group) = &project.group {
                    value["group"] = json!(group);
                }
                if project.linked {
                    value["linked"] = json!(true);
                }
                value
            })
            .collect();
        let recents: Vec<Value> = self
            .recents
            .iter()
            .map(|recent| {
                let mut value = json!({ "path": recent.path, "name": recent.name });
                if self.linked_recents.contains(&recent.path) {
                    value["linked"] = json!(true);
                }
                value
            })
            .collect();
        let mut value = json!({
            "version": VERSION,
            "projects": projects,
            "recents": recents,
            "sidebar_collapsed": self.sidebar_collapsed,
        });
        if let Some(active) = &self.active {
            value["active"] = json!(active);
        }
        if !self.groups.is_empty() {
            value["groups"] = self
                .groups
                .iter()
                .map(|group| json!({ "id": group.id, "name": group.name }))
                .collect();
        }
        let mut text = serde_json::to_string_pretty(&value).unwrap_or_default();
        text.push('\n');
        text
    }

    /// Read a saved workspace, or say why it is unusable. A field that is
    /// merely odd (a name that is too long) is dropped, not a reason to lose
    /// the list; a file that is not JSON or has another version is.
    pub(crate) fn parse(text: &str) -> Result<Self, String> {
        let value: Value = serde_json::from_str(text).map_err(|err| format!("bad JSON: {err}"))?;
        if value["version"].as_u64() != Some(VERSION) {
            return Err("unknown version".to_owned());
        }
        // Groups first: a project may only name one that is kept.
        let mut groups: Vec<ProjectGroup> = Vec::new();
        for item in value["groups"].as_array().map_or(&[][..], Vec::as_slice) {
            let (Some(id), Some(name)) = (item["id"].as_str(), name_of(&item["name"])) else {
                continue;
            };
            if valid_group_id(id) && !groups.iter().any(|group| group.id == id) {
                groups.push(ProjectGroup {
                    id: id.to_owned(),
                    name,
                });
            }
        }
        groups.truncate(MAX_GROUPS);
        let mut projects: Vec<SavedProject> = Vec::new();
        for item in value["projects"].as_array().ok_or("no projects list")? {
            // One odd entry is dropped; the rest of the workspace is kept.
            let Some(path) = item["path"].as_str() else {
                continue;
            };
            if path.is_empty() || projects.iter().any(|project| project.path == path) {
                continue;
            }
            projects.push(SavedProject {
                path: path.to_owned(),
                name: name_of(&item["name"]),
                expanded: item["expanded"].as_bool().unwrap_or(true),
                active_terminal: item["active_terminal"].as_str().map(str::to_owned),
                group: item["group"]
                    .as_str()
                    .filter(|id| groups.iter().any(|group| group.id == *id))
                    .map(str::to_owned),
                linked: item["linked"].as_bool().unwrap_or(false),
            });
        }
        projects.truncate(MAX_PROJECTS);
        let mut recents: Vec<RecentProject> = Vec::new();
        let mut linked_recents: Vec<String> = Vec::new();
        for item in value["recents"].as_array().map_or(&[][..], Vec::as_slice) {
            let (Some(path), Some(name)) = (item["path"].as_str(), name_of(&item["name"])) else {
                continue;
            };
            if !recents.iter().any(|recent| recent.path == path) {
                if item["linked"].as_bool() == Some(true) {
                    linked_recents.push(path.to_owned());
                }
                recents.push(RecentProject {
                    path: path.to_owned(),
                    name,
                });
            }
        }
        recents.truncate(MAX_RECENTS);
        Ok(Self {
            active: value["active"]
                .as_str()
                .filter(|active| projects.iter().any(|project| project.path == *active))
                .map(str::to_owned),
            projects,
            linked_recents: linked_recents
                .into_iter()
                .filter(|path| recents.iter().any(|recent| recent.path == *path))
                .collect(),
            recents,
            sidebar_collapsed: value["sidebar_collapsed"].as_bool().unwrap_or(false),
            groups,
        })
    }
}

/// What loading found.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Loaded {
    /// A readable file, or none yet.
    Read(Saved),
    /// The content was unusable. The file moved to `.bad` and the workspace
    /// starts empty, with the warning.
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

    pub(crate) fn load(&self) -> Loaded {
        let text = match fs::read_to_string(&self.path) {
            Ok(text) => text,
            Err(err) if err.kind() == io::ErrorKind::NotFound => {
                return Loaded::Read(Saved::default());
            }
            Err(err) => {
                return Loaded::ReadOnly {
                    warning: format!(
                        "The saved workspace could not be read ({err}). It is not changed this run."
                    ),
                };
            }
        };
        match Saved::parse(&text) {
            Ok(saved) => Loaded::Read(saved),
            Err(why) => match fs::rename(&self.path, self.path.with_extension("json.bad")) {
                Ok(()) => Loaded::Bad {
                    warning: format!(
                        "The saved workspace was unusable ({why}). It was kept as workspace.json.bad, and Connecto starts with no open projects."
                    ),
                },
                Err(err) => Loaded::ReadOnly {
                    warning: format!(
                        "The saved workspace was unusable ({why}) and could not be moved aside ({err}). It is not changed this run."
                    ),
                },
            },
        }
    }

    /// Replace the file atomically.
    pub(crate) fn write(&self, saved: &Saved) -> io::Result<()> {
        if let Some(parent) = self.path.parent() {
            fs::create_dir_all(parent)?;
        }
        let staged = self.path.with_extension("json.tmp");
        fs::write(&staged, saved.to_json())?;
        crate::platform::replace(&staged, &self.path)
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    fn sample() -> Saved {
        Saved {
            projects: vec![
                SavedProject {
                    path: "/work/a".into(),
                    name: Some("Alpha".into()),
                    expanded: false,
                    active_terminal: Some("claude-2".into()),
                    group: Some("group-1".into()),
                    linked: true,
                },
                SavedProject {
                    path: "/work/b".into(),
                    name: None,
                    expanded: true,
                    active_terminal: None,
                    group: None,
                    linked: false,
                },
            ],
            active: Some("/work/b".into()),
            recents: vec![RecentProject {
                path: "/work/c".into(),
                name: "c".into(),
            }],
            linked_recents: vec!["/work/c".into()],
            sidebar_collapsed: true,
            groups: vec![ProjectGroup {
                id: "group-1".into(),
                name: "Clients".into(),
            }],
        }
    }

    #[test]
    fn a_workspace_round_trips() {
        let saved = sample();
        assert_eq!(Saved::parse(&saved.to_json()).unwrap(), saved);
    }

    #[test]
    fn odd_fields_are_dropped_and_bad_files_are_refused() {
        let text = r#"{"version":1,"active":"/gone","projects":[
            {"path":"/a","name":"two\nlines"},{"path":"/a"},{"path":""}],
            "recents":[{"path":"/r"}]}"#;
        let saved = Saved::parse(text).unwrap();
        assert_eq!(saved.projects.len(), 1, "a duplicate and an empty path go");
        assert_eq!(saved.projects[0].name, None);
        assert!(saved.projects[0].expanded);
        assert_eq!(saved.active, None, "an active project that is not open");
        assert!(saved.recents.is_empty());
        // A group that is not kept is not named by a project either.
        let text = r#"{"version":1,"groups":[{"id":"x","name":"Bad id"},
            {"id":"group-2","name":""},{"id":"group-3","name":"Ok"}],
            "projects":[{"path":"/a","group":"group-2"},{"path":"/b","group":"group-3"}]}"#;
        let saved = Saved::parse(text).unwrap();
        assert_eq!(saved.groups.len(), 1);
        assert_eq!(saved.projects[0].group, None);
        assert_eq!(saved.projects[1].group.as_deref(), Some("group-3"));
        for bad in ["nope", r#"{"version":2,"projects":[]}"#, r#"{"version":1}"#] {
            assert!(Saved::parse(bad).is_err(), "{bad}");
        }
    }
}
