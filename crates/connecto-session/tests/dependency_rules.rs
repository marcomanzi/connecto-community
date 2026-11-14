//! V-27: the crate dependency rules of UI-RELAY-POC section 4.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::collections::{BTreeMap, BTreeSet};
use std::process::Command;

use serde_json::Value;

/// Normal dependencies of each workspace crate on other workspace crates.
/// Dev-dependencies are not part of the shipped graph.
fn graph() -> BTreeMap<String, BTreeSet<String>> {
    let cargo = std::env::var_os("CARGO").unwrap_or_else(|| "cargo".into());
    let output = Command::new(cargo)
        .args(["metadata", "--format-version", "1", "--no-deps"])
        .output()
        .unwrap();
    assert!(output.status.success());
    let metadata: Value = serde_json::from_slice(&output.stdout).unwrap();
    metadata["packages"]
        .as_array()
        .unwrap()
        .iter()
        .map(|package| {
            let deps = package["dependencies"]
                .as_array()
                .unwrap()
                .iter()
                .filter(|dep| dep["kind"].is_null())
                .filter_map(|dep| dep["name"].as_str())
                .filter(|name| name.starts_with("connecto-"))
                .map(str::to_owned)
                .collect();
            (package["name"].as_str().unwrap().to_owned(), deps)
        })
        .collect()
}

fn assert_never_depends(graph: &BTreeMap<String, BTreeSet<String>>, from: &str, on: &[&str]) {
    let deps = graph.get(from).unwrap_or_else(|| panic!("no crate {from}"));
    for name in on {
        assert!(!deps.contains(*name), "{from} must not depend on {name}");
    }
}

#[test]
fn only_session_depends_on_pty() {
    let graph = graph();
    let users: Vec<&String> = graph
        .iter()
        .filter(|(_, deps)| deps.contains("connecto-pty"))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(users, ["connecto-session"]);
}

#[test]
fn adapter_and_hook_stay_below_session_and_gate() {
    let graph = graph();
    for name in ["connecto-adapter", "connecto-hook"] {
        assert_never_depends(&graph, name, &["connecto-session", "connecto-gate"]);
    }
}

#[test]
fn only_session_depends_on_gate() {
    let graph = graph();
    let users: Vec<&String> = graph
        .iter()
        .filter(|(_, deps)| deps.contains("connecto-gate"))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(users, ["connecto-session"]);
}

#[test]
fn the_ui_sees_only_core() {
    let graph = graph();
    let deps = graph.get("connecto-ui").unwrap();
    assert_eq!(deps.iter().collect::<Vec<_>>(), ["connecto-core"]);
}

/// AC2: the sandbox crate is gone, and nothing depends on it.
#[test]
fn connecto_contain_is_gone() {
    let graph = graph();
    assert!(!graph.contains_key("connecto-contain"));
    assert!(
        graph
            .values()
            .all(|deps| !deps.contains("connecto-contain"))
    );
}

/// The workspace sits on the session crate and the shared contract. It never
/// reaches a PTY, the Gate or the adapters itself.
#[test]
fn the_workspace_sees_only_session_core_audit_and_store() {
    let graph = graph();
    let deps = graph.get("connecto-workspace").unwrap();
    let allowed: BTreeSet<String> = [
        "connecto-audit",
        "connecto-core",
        "connecto-session",
        "connecto-store",
    ]
    .into_iter()
    .map(str::to_owned)
    .collect();
    assert!(deps.is_subset(&allowed), "{deps:?}");
    assert!(deps.contains("connecto-session") && deps.contains("connecto-core"));
    assert_never_depends(
        &graph,
        "connecto-workspace",
        &["connecto-pty", "connecto-gate", "connecto-ui"],
    );
}

/// Only the app builds a workspace; the UI sees it through the core contract.
#[test]
fn only_the_app_depends_on_the_workspace() {
    let graph = graph();
    let users: Vec<&String> = graph
        .iter()
        .filter(|(_, deps)| deps.contains("connecto-workspace"))
        .map(|(name, _)| name)
        .collect();
    assert_eq!(users, ["connecto-app"]);
}
