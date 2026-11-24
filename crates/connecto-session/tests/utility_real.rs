//! Utility runs against the real CLIs and the user's own login. Ignored: run by
//! hand, one provider at a time, with
//!
//! ```text
//! CONNECTO_UTILITY_DATA=<scratch data folder> CONNECTO_UTILITY_PROJECT=<a folder
//! the CLI already trusts> cargo test -p connecto-session --test utility_real \
//!   -- --ignored --nocapture claude
//! ```
//!
//! The data folder starts empty: the CLI is copied from the user's install and
//! the project uses the system configuration (ADR 0020), so the run uses the
//! real login. Each test makes one or two small model calls.

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, Instant};

mod common;

use connecto_core::memory::{ModelChoice, Provider};
use connecto_session::launch::{Layout, project_id};
use connecto_session::utility_run::{UtilityJob, UtilityPurpose, UtilityRunner, capability};

fn env_path(name: &str) -> PathBuf {
    PathBuf::from(std::env::var_os(name).unwrap_or_else(|| panic!("set {name}")))
}

fn setup() -> (Layout, PathBuf) {
    let data_root = env_path("CONNECTO_UTILITY_DATA");
    std::fs::create_dir_all(&data_root).unwrap();
    let home = PathBuf::from(std::env::var_os("HOME").unwrap());
    let project = std::fs::canonicalize(env_path("CONNECTO_UTILITY_PROJECT")).unwrap();
    connecto_store::Store::new(&data_root)
        .install_hook(&common::hook_binary())
        .unwrap();
    (Layout { data_root, home }, project)
}

fn run(provider: Provider, model: &str, purpose: UtilityPurpose, prompt: &str) {
    let (layout, project) = setup();
    eprintln!(
        "{provider} capability: {:?}",
        capability(&layout, &project, provider)
    );
    let runner = UtilityRunner::new(layout, Arc::new(|| {}));
    let job = UtilityJob {
        project_id: project_id(&project),
        project,
        choice: ModelChoice::new(provider, model, "low"),
        prompt: prompt.to_owned(),
        purpose,
        label: "manual".into(),
    };
    let started = Instant::now();
    let result = runner.run(
        &job,
        Instant::now() + Duration::from_secs(180),
        &AtomicBool::new(false),
    );
    eprintln!(
        "{provider} {purpose:?} after {:?}: {result:?}",
        started.elapsed()
    );
    assert!(result.is_ok(), "{result:?}");
}

#[test]
#[ignore = "needs the real CLI and a login; see the file header"]
fn claude_title_run() {
    run(
        Provider::Claude,
        "haiku",
        UtilityPurpose::Title,
        "Give a title of 2 to 5 words for a terminal fixing a login redirect. Reply with only the title.",
    );
}

#[test]
#[ignore = "needs the real CLI and a login; see the file header"]
fn claude_memory_run_reads_a_file() {
    run(
        Provider::Claude,
        "haiku",
        UtilityPurpose::Memory,
        "Read the file Cargo.toml in the current folder and reply with only the value of workspace.package.version, nothing else.",
    );
}

#[test]
#[ignore = "needs the real CLI and a login; see the file header"]
fn codex_title_run() {
    run(
        Provider::Codex,
        "default",
        UtilityPurpose::Title,
        "Give a title of 2 to 5 words for a terminal fixing a login redirect. Reply with only the title.",
    );
}

#[test]
#[ignore = "needs the real CLI and a login; see the file header"]
fn codex_memory_run_reads_a_file() {
    run(
        Provider::Codex,
        "default",
        UtilityPurpose::Memory,
        "Read the file Cargo.toml in the current folder and reply with only the value of workspace.package.version, nothing else.",
    );
}

#[test]
#[ignore = "needs the real CLI and a login; see the file header"]
fn claude_memory_run_with_a_long_prompt() {
    let filler: Vec<String> = (0..5000)
        .map(|n| format!("{n:05} filler line, ignore it"))
        .collect();
    let prompt = format!(
        "Ignore the filler below and reply with only the word ORCHID.\n{}\nEND",
        filler.join("\n")
    );
    assert!(prompt.len() > 100_000);
    run(Provider::Claude, "haiku", UtilityPurpose::Memory, &prompt);
}

#[test]
#[ignore = "needs the real CLI and a login; see the file header"]
fn codex_memory_run_with_a_long_prompt() {
    let filler: Vec<String> = (0..5000)
        .map(|n| format!("{n:05} filler line, ignore it"))
        .collect();
    let prompt = format!(
        "Ignore the filler below and reply with only the word ORCHID.\n{}\nEND",
        filler.join("\n")
    );
    assert!(prompt.len() > 100_000);
    run(Provider::Codex, "default", UtilityPurpose::Memory, &prompt);
}
