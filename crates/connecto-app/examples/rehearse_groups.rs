//! Rehearses the upgrade of the shipped automations on COPIES:
//! `rehearse_groups <AUTOMATIONS-COPY> <DEFAULTS-STATE-COPY>`. Seeds as an
//! upgraded install would, then lists the groups that result and whether any
//! folder changed. Nothing else is read or written.

#![allow(clippy::expect_used, clippy::print_stdout)]

use std::collections::BTreeMap;
use std::path::{Path, PathBuf};

use connecto_automation::defaults;

fn snapshot(root: &Path) -> BTreeMap<String, String> {
    let mut map = BTreeMap::new();
    for entry in std::fs::read_dir(root).into_iter().flatten().flatten() {
        let file = entry.path().join("automation.json");
        if let Ok(text) = std::fs::read_to_string(&file) {
            map.insert(entry.file_name().to_string_lossy().into_owned(), text);
        }
    }
    map
}

fn main() {
    let mut args = std::env::args().skip(1);
    let root = PathBuf::from(
        args.next()
            .expect("usage: rehearse_groups <AUTOMATIONS> <STATE>"),
    );
    let state = PathBuf::from(
        args.next()
            .expect("usage: rehearse_groups <AUTOMATIONS> <STATE>"),
    );
    let before = snapshot(&root);
    let written = defaults::seed(&root, &state);
    let after = snapshot(&root);
    println!(
        "folders before: {}, after: {}, written by the upgrade: {written:?}",
        before.len(),
        after.len()
    );
    let changed: Vec<_> = before
        .iter()
        .filter(|(k, v)| after.get(*k) != Some(*v))
        .map(|(k, _)| k)
        .collect();
    println!("existing automations rewritten: {changed:?}");
    for group in defaults::groups(&|slug| after.contains_key(slug)) {
        let on = group
            .members
            .iter()
            .filter(|m| {
                after
                    .get(*m)
                    .is_some_and(|t| t.contains("\"enabled\": true"))
            })
            .count();
        println!(
            "group {:?}: {} of {} on; members {:?}",
            group.title,
            on,
            group.members.len(),
            group.members
        );
    }
    let loose: Vec<_> = after
        .keys()
        .filter(|k| !defaults::DEFAULTS.iter().any(|d| d.slug == k.as_str()))
        .collect();
    println!("the user's own, not in any group: {loose:?}");
}
