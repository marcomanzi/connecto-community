//! V-28: the default build carries no test seam. Compiled only when no seam
//! feature is on, because with one on these strings belong in the binary.

#![cfg(not(any(
    feature = "frame-capture",
    feature = "test-endpoints",
    feature = "ui-test-driver"
)))]
#![allow(clippy::unwrap_used)]

use std::fs;

const SEAMS: [&str; 11] = [
    "--fake-model",
    "--pane",
    "FAKE MODEL",
    "CONNECTO_METRICS",
    "CONNECTO_SHOT_DIR",
    "CONNECTO_UI_PICK",
    "CONNECTO_UI_POS",
    "CONNECTO_UI_SCRIPT",
    "REAL-CLI/FAKE-MODEL",
    "ANTHROPIC_BASE_URL",
    r#"{"press":""#,
];

fn contains(haystack: &[u8], needle: &str) -> bool {
    haystack
        .windows(needle.len())
        .any(|window| window == needle.as_bytes())
}

#[test]
fn no_seam_string_is_in_the_binary() {
    let binary = fs::read(env!("CARGO_BIN_EXE_connecto-app")).unwrap();
    assert!(contains(&binary, "Stop all"), "wrong file was read");
    for seam in SEAMS {
        assert!(!contains(&binary, seam), "{seam} is in the default build");
    }
}

/// AC2: nothing in the shipped binary launches a session under a sandbox.
#[test]
fn the_binary_has_no_sandbox_launcher() {
    let binary = fs::read(env!("CARGO_BIN_EXE_connecto-app")).unwrap();
    for text in ["sandbox-exec", "Starting in the sandbox"] {
        assert!(!contains(&binary, text), "{text} is in the default build");
    }
}
