//! Project operations (ADR 0031, `docs/specs/project-operations.md`): the
//! files in `.connecto/ops/`, Connecto's state for them, and the hook's
//! local match. Pure logic and files; the app wires it to the bridge, the UI
//! and Jev.

pub mod confirm;
pub mod discovery;
pub mod folder;
pub mod matcher;
pub mod spec;
pub mod state;

use connecto_core::ops::{OpRow, OpStanding};

pub use folder::{Found, Loaded};
pub use state::{PromptOutcome, PromptRecord, RunFrom, RunRecord, State, Store};

/// A week, in milliseconds.
pub const WEEK_MS: u64 = 7 * 24 * 60 * 60 * 1000;
/// How long Later hides the weekly suggestion.
pub const LATER_MS: u64 = 3 * 24 * 60 * 60 * 1000;
/// Prompts since the last discovery before the weekly suggestion shows.
pub const PROMPTS_FOR_SUGGESTION: u32 = 20;

/// Where a found operation stands, given the approvals.
#[must_use]
pub fn standing(found: &Found, state: &State) -> OpStanding {
    match (&found.loaded, state.approved.get(&found.slug)) {
        (Err(why), _) => OpStanding::Invalid(why.clone()),
        (Ok(loaded), Some(hash)) if *hash == loaded.hash => OpStanding::Approved,
        (Ok(_), Some(_)) => OpStanding::Changed,
        (Ok(_), None) => OpStanding::Proposed,
    }
}

/// The rows the UI shows.
#[must_use]
pub fn rows(
    found: &[Found],
    state: &State,
    runs: &[RunRecord],
    prompts: &[PromptRecord],
) -> Vec<OpRow> {
    found
        .iter()
        .map(|f| {
            let standing = standing(f, state);
            let stats = state::stats(&f.slug, runs, prompts);
            match &f.loaded {
                Ok(loaded) => OpRow {
                    slug: f.slug.clone(),
                    name: loaded.spec.name.clone(),
                    description: loaded.spec.description.clone(),
                    kind: Some(loaded.spec.kind),
                    danger: Some(loaded.spec.danger),
                    asks_first: loaded.spec.asks_first(),
                    standing,
                    intercept: state.intercept.contains(&f.slug),
                    phrases: loaded.spec.phrases.clone(),
                    args: loaded.spec.args.clone(),
                    body: loaded.body.clone(),
                    spec: loaded.spec_text.clone(),
                    hash: loaded.hash.clone(),
                    timeout_s: loaded.spec.timeout_s,
                    stats,
                },
                Err(_) => OpRow {
                    slug: f.slug.clone(),
                    name: f.slug.clone(),
                    description: String::new(),
                    kind: None,
                    danger: None,
                    asks_first: true,
                    standing,
                    intercept: false,
                    phrases: Vec::new(),
                    args: Vec::new(),
                    body: String::new(),
                    spec: String::new(),
                    hash: String::new(),
                    timeout_s: spec::DEFAULT_TIMEOUT_S,
                    stats,
                },
            }
        })
        .collect()
}

/// Prompts recorded since the last discovery (or since operations were
/// first looked at).
#[must_use]
pub fn prompts_since(state: &State, prompts: &[PromptRecord]) -> u32 {
    let from = state.last_discovery_ms.unwrap_or(state.first_seen_ms);
    u32::try_from(prompts.iter().filter(|p| p.at_ms >= from).count()).unwrap_or(u32::MAX)
}

/// Whether the weekly suggestion shows (spec §8): on, not put off with
/// Later, a week since the last discovery, and enough prompts since.
#[must_use]
pub fn discovery_due(state: &State, prompts_since: u32, now_ms: u64) -> bool {
    let from = state.last_discovery_ms.unwrap_or(state.first_seen_ms);
    state.weekly
        && state.discovery_terminal.is_none()
        && state.later_until_ms.is_none_or(|until| now_ms >= until)
        && now_ms.saturating_sub(from) >= WEEK_MS
        && prompts_since >= PROMPTS_FOR_SUGGESTION
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use crate::folder::tests::{SPEC, write_op};

    #[test]
    fn standing_follows_the_hash() {
        let dir = tempfile::tempdir().unwrap();
        write_op(dir.path(), "tests", SPEC, "cargo test\n");
        let mut state = State::new(0);
        let found = folder::scan(dir.path());
        assert_eq!(standing(&found[0], &state), OpStanding::Proposed);
        let hash = found[0].loaded.as_ref().unwrap().hash.clone();
        state.approved.insert("tests".into(), hash);
        assert_eq!(standing(&found[0], &state), OpStanding::Approved);
        write_op(dir.path(), "tests", SPEC, "rm -rf target\n");
        let found = folder::scan(dir.path());
        assert_eq!(standing(&found[0], &state), OpStanding::Changed);
        let rows = rows(&found, &state, &[], &[]);
        assert_eq!(rows[0].name, "Tests");
        assert!(!rows[0].standing.runs());
    }

    #[test]
    fn the_weekly_suggestion_waits_for_a_week_and_prompts() {
        let mut state = State::new(0);
        assert!(!discovery_due(&state, 100, WEEK_MS - 1));
        assert!(!discovery_due(&state, PROMPTS_FOR_SUGGESTION - 1, WEEK_MS));
        assert!(discovery_due(&state, PROMPTS_FOR_SUGGESTION, WEEK_MS));
        state.later_until_ms = Some(WEEK_MS + 10);
        assert!(!discovery_due(&state, 100, WEEK_MS));
        assert!(discovery_due(&state, 100, WEEK_MS + 10));
        state.weekly = false;
        assert!(!discovery_due(&state, 100, 2 * WEEK_MS));
        state.weekly = true;
        state.last_discovery_ms = Some(2 * WEEK_MS);
        assert!(!discovery_due(&state, 100, 2 * WEEK_MS + 1));
        state.discovery_terminal = Some("claude-3".into());
        assert!(!discovery_due(&state, 100, 4 * WEEK_MS));
    }
}
