//! A host's project groups (the sidebar's labels over its projects), as a
//! peer is told of them ([`crate::wire::Message::GroupsProbe`]): the groups
//! in the host's order, and the group of each project of the connection's
//! latest list. A project the peer was not sent is never named. A group is
//! a label only: folding one is each window's own.

use connecto_core::{ProjectGroup, RemoteGroups};
use serde_json::{Value, json};

/// The most groups, and project memberships, one answer carries.
const MAX_GROUPS: usize = 200;
const MAX_MEMBERS: usize = 2000;
/// The longest group id kept.
const MAX_ID: usize = 64;

/// The groups and the memberships of the projects in `listed`, as JSON.
#[must_use]
pub fn groups_json(groups: &RemoteGroups, listed: &[String]) -> Vec<u8> {
    let list: Vec<Value> = groups
        .groups
        .iter()
        .take(MAX_GROUPS)
        .map(|g| json!({ "id": g.id, "name": g.name }))
        .collect();
    let of: Vec<Value> = groups
        .of
        .iter()
        .filter(|(project, _)| listed.contains(project))
        .take(MAX_MEMBERS)
        .map(|(project, group)| json!([project, group]))
        .collect();
    json!({ "groups": list, "of": of }).to_string().into_bytes()
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= MAX_ID
        && id
            .bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

/// The groups as sent: names cleaned for display, ids checked, a group
/// named twice or a membership of a group not sent left out.
#[must_use]
pub fn groups_from(bytes: &[u8]) -> RemoteGroups {
    let Ok(v) = serde_json::from_slice::<Value>(bytes) else {
        return RemoteGroups::default();
    };
    let mut groups: Vec<ProjectGroup> = Vec::new();
    for g in v
        .get("groups")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_GROUPS)
    {
        let (Some(id), Some(name)) = (
            g.get("id").and_then(Value::as_str),
            g.get("name").and_then(Value::as_str),
        ) else {
            continue;
        };
        if valid_id(id) && !groups.iter().any(|seen| seen.id == id) {
            groups.push(ProjectGroup {
                id: id.to_owned(),
                name: crate::remote_terminals::display_text(name),
            });
        }
    }
    let mut of: Vec<(String, String)> = Vec::new();
    for pair in v
        .get("of")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .take(MAX_MEMBERS)
    {
        let (Some(project), Some(group)) = (
            pair.get(0).and_then(Value::as_str),
            pair.get(1).and_then(Value::as_str),
        ) else {
            continue;
        };
        if crate::remote_terminals::is_canonical_project_id(project)
            && groups.iter().any(|g| g.id == group)
            && !of.iter().any(|(p, _)| p == project)
        {
            of.push((project.to_owned(), group.to_owned()));
        }
    }
    RemoteGroups { groups, of }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;

    const LISTED: &str = "0123456789abcdef";
    const OTHER: &str = "fedcba9876543210";

    #[test]
    fn only_listed_projects_are_named_and_what_is_sent_is_checked() {
        let groups = RemoteGroups {
            groups: vec![
                ProjectGroup {
                    id: "group-2".into(),
                    name: "HARBOR\u{202e}".into(),
                },
                ProjectGroup {
                    id: "group-1".into(),
                    name: "Clients".into(),
                },
            ],
            of: vec![
                (LISTED.into(), "group-2".into()),
                (OTHER.into(), "group-1".into()),
            ],
        };
        let sent = groups_from(&groups_json(&groups, &[LISTED.into()]));
        assert_eq!(
            sent.groups
                .iter()
                .map(|g| g.id.as_str())
                .collect::<Vec<_>>(),
            ["group-2", "group-1"],
            "in the host's order, empty ones too"
        );
        assert_eq!(sent.groups[0].name, "HARBOR");
        assert_eq!(sent.of, vec![(LISTED.to_owned(), "group-2".to_owned())]);
        let bad = br#"{"groups":[{"id":"a b","name":"x"},{"id":"g","name":"G"}],"of":[["0123456789abcdef","missing"]]}"#;
        let sent = groups_from(bad);
        assert_eq!(sent.groups.len(), 1);
        assert!(sent.of.is_empty());
    }
}
