//! Discovery (spec §8): the digest of the person's prompts an agent reads,
//! and the goal it is given.

use std::collections::HashMap;
use std::fmt::Write as _;
use std::path::{Path, PathBuf};

use crate::folder::DISCOVERY_DIR;

/// The digest's file name, in `.connecto/ops/.discovery/`.
pub const DIGEST_FILE: &str = "prompts.md";
/// Longest digest, in bytes.
pub const MAX_DIGEST: usize = 200 * 1024;
/// How much of one prompt the digest shows.
const LINE_CHARS: usize = 300;
/// Most operations one discovery proposes.
pub const MAX_PROPOSALS: usize = 8;
const DAY_MS: u64 = 24 * 60 * 60 * 1000;
/// The same words this close together are one prompt seen twice.
const SAME_PROMPT_MS: u64 = 2 * 60 * 1000;

/// One prompt the person typed, and when.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Asked {
    pub at_ms: u64,
    pub text: String,
}

fn key(text: &str) -> String {
    text.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn one_line(text: &str) -> String {
    let flat = text.split_whitespace().collect::<Vec<_>>().join(" ");
    if flat.chars().count() > LINE_CHARS {
        let mut cut: String = flat.chars().take(LINE_CHARS).collect();
        cut.push('…');
        cut
    } else {
        flat
    }
}

/// The digest: the person's prompts, exact repeats grouped with their count,
/// the most repeated and then the most recent first, at most [`MAX_DIGEST`].
#[must_use]
pub fn digest(asked: &[Asked], now_ms: u64) -> String {
    struct Group {
        text: String,
        count: usize,
        last_ms: u64,
    }
    // One prompt can be in the hook's log and in its terminal's own log,
    // with times a little apart: within two minutes it counts once.
    let mut ordered: Vec<&Asked> = asked.iter().collect();
    ordered.sort_by_key(|a| a.at_ms);
    let mut last_kept: HashMap<String, u64> = HashMap::new();
    let mut kept: Vec<&Asked> = Vec::new();
    for a in ordered {
        let k = key(&a.text);
        if k.is_empty() {
            continue;
        }
        if last_kept
            .get(&k)
            .is_some_and(|at| a.at_ms.saturating_sub(*at) < SAME_PROMPT_MS)
        {
            continue;
        }
        last_kept.insert(k, a.at_ms);
        kept.push(a);
    }
    let mut groups: HashMap<String, Group> = HashMap::new();
    for a in kept.iter().copied() {
        let k = key(&a.text);
        let group = groups.entry(k).or_insert_with(|| Group {
            text: a.text.clone(),
            count: 0,
            last_ms: 0,
        });
        group.count += 1;
        if a.at_ms >= group.last_ms {
            group.last_ms = a.at_ms;
            group.text = a.text.clone();
        }
    }
    let mut groups: Vec<Group> = groups.into_values().collect();
    groups.sort_by(|a, b| {
        b.count
            .cmp(&a.count)
            .then(b.last_ms.cmp(&a.last_ms))
            .then_with(|| a.text.cmp(&b.text))
    });
    let mut out = format!(
        "# What the person asked for in this project\n\n{} prompts, {} different. Each line: how many times, how long ago it was last asked, and the prompt (on one line, cut at {LINE_CHARS} characters). Prompts from other agents and from Connecto are left out.\n\n",
        kept.len(),
        groups.len()
    );
    let mut left_out = 0;
    for g in &groups {
        let days = now_ms.saturating_sub(g.last_ms) / DAY_MS;
        let when = match days {
            0 => "today".to_owned(),
            1 => "1 day ago".to_owned(),
            n => format!("{n} days ago"),
        };
        let line = format!("- {}× · {when} · {}\n", g.count, one_line(&g.text));
        if out.len() + line.len() > MAX_DIGEST - 200 {
            left_out += 1;
            continue;
        }
        out.push_str(&line);
    }
    if left_out > 0 {
        let _ = writeln!(out, "\n({left_out} more prompts did not fit.)");
    }
    out
}

/// Write the digest where the agent reads it; returns its path.
///
/// # Errors
///
/// The folder or file could not be written.
pub fn write_digest(project: &Path, text: &str) -> std::io::Result<PathBuf> {
    crate::folder::publish(project, DISCOVERY_DIR, DIGEST_FILE, text.as_bytes())
        .map_err(std::io::Error::other)
}

/// The discovery terminal's first prompt. `existing` are the slugs already
/// there, which it must not overwrite.
#[must_use]
pub fn goal(existing: &[String], digest_path: &str) -> String {
    let existing = if existing.is_empty() {
        "none".to_owned()
    } else {
        existing.join(", ")
    };
    format!(
        r#"Find the routine requests in this project and write each one down as a Connecto project operation, so later it costs no model (or only a small one) instead of a full agent turn.

1. Read {digest_path}: the prompts the person typed in this project, grouped and counted. Look for requests that repeat or are clearly routine: start or restart the dev server, run the tests or the linter, build, commit, push, deploy, release, update dependencies, regenerate code, open a PR.
2. Look at how this project does those things: the README, package.json scripts, Makefile/justfile, Cargo.toml, CI workflows, deploy configuration, scripts/ folders. Prefer the project's own commands.
3. For each one worth it (seen at least twice, or plainly routine), at most {MAX_PROPOSALS} in all, write a folder .connecto/ops/<name>/ (name: lowercase letters, digits and dashes, at most 40). Existing operations: {existing}. Never change or overwrite them: pick another name, or skip.

Each folder holds op.json and either run.sh (kind "script" or "script+llm") or recipe.md (kind "recipe"). op.json, exactly these fields (unknown fields are an error):
{{
  "version": 1,
  "name": "Run the tests",                      // 1-60 characters
  "description": "Runs the unit tests with cargo.", // 1-300 characters, says what it does
  "phrases": ["run the tests", "lancia i test"],   // 1-12 ways the person asks for it, in their words and language
  "kind": "script",                               // "script", "script+llm" or "recipe"
  "danger": "none",                               // "none" reads only, "changes" changes local files, "deploys" reaches anything outside this machine (push, deploy, publish, send), "deletes" removes data
  "confirm": false,                               // true: asks the person first (always so for deploys and deletes)
  "args": [{{"name": "filter", "description": "only tests matching this", "required": false}}],  // optional, at most 6, in order
  "timeout_s": 600                                // optional, 1-3600
}}
(JSON has no comments: leave them out.)

run.sh runs with /bin/bash from the project folder, as `bash -c <script> <name> <args...>`: the arguments are "$1", "$2"...; use paths relative to the project folder (not to the script). Start it with `set -euo pipefail`. It must be safe to run twice, print what it does, fail with a non-zero exit when it fails, and end: to start something long-running (a dev server), start it in the background, for example `nohup npm run dev > .connecto/ops/<name>/server.log 2>&1 &`, print where its log is, and exit: what it left running keeps running (only a script still running at its timeout has everything it started stopped). It holds no secret (use `connecto secret run <NAME> -- <program>` for one). For a step that needs judgement (a commit message, a changelog line) use kind "script+llm" and ask a small model with `connecto op llm`: it reads one prompt on stdin, so put the instruction first and the input after it, for example:
  msg=$( {{ echo "Write a one-line commit message for this diff. Reply with the message only."; git diff --cached; }} | connecto op llm )
A request that cannot be scripted (it needs reading code and deciding) is a "recipe": recipe.md lists the steps an agent follows, short and concrete.

4. Check each one with `connecto op show <name>`: it says what is wrong with the files. You may run a script whose danger is "none" to see that it works; do not run any other. Connecto does not run any of them until the person approves it.
5. End with a short table: name, kind, danger, the prompts it stands for, and why. The person reviews them in Connecto (the project's Operations icon, or Project Settings > Operations)."#
    )
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use std::fs;

    fn asked(at_ms: u64, text: &str) -> Asked {
        Asked {
            at_ms,
            text: text.to_owned(),
        }
    }

    #[test]
    fn repeats_are_grouped_and_come_first() {
        let now = 10 * DAY_MS;
        let text = digest(
            &[
                asked(DAY_MS, "deploy"),
                asked(9 * DAY_MS, "Deploy "),
                asked(10 * DAY_MS, "why is login slow\nand broken"),
                asked(3 * DAY_MS, "deploy"),
            ],
            now,
        );
        assert!(text.contains("4 prompts, 2 different"), "{text}");
        let deploy = text.find("3× · 1 day ago · Deploy").unwrap();
        let login = text
            .find("1× · today · why is login slow and broken")
            .unwrap();
        assert!(deploy < login, "{text}");
    }

    #[test]
    fn one_prompt_in_two_logs_counts_once() {
        let text = digest(
            &[
                asked(DAY_MS, "deploy"),
                asked(DAY_MS + 30_000, "deploy"),
                asked(2 * DAY_MS, "deploy"),
            ],
            3 * DAY_MS,
        );
        assert!(text.contains("2 prompts, 1 different"), "{text}");
        assert!(text.contains("2× · 1 day ago · deploy"), "{text}");
    }

    #[test]
    fn the_digest_is_bounded() {
        let many: Vec<Asked> = (0..5000)
            .map(|i| asked(i, &format!("prompt number {i} {}", "x".repeat(400))))
            .collect();
        let text = digest(&many, 0);
        assert!(text.len() <= MAX_DIGEST, "{}", text.len());
        assert!(text.contains("more prompts did not fit"));
        assert!(text.lines().all(|l| l.chars().count() < 400));
    }

    #[test]
    fn the_goal_names_what_exists_and_the_digest() {
        let goal = goal(&["tests".into()], ".connecto/ops/.discovery/prompts.md");
        assert!(goal.contains("Existing operations: tests."));
        assert!(goal.contains(".connecto/ops/.discovery/prompts.md"));
        assert!(goal.contains("at most 8"));
        let none = super::goal(&[], "p");
        assert!(none.contains("Existing operations: none."));
    }

    #[cfg(unix)]
    #[test]
    fn the_digest_replaces_a_link_and_never_writes_through_it() {
        let dir = tempfile::tempdir().unwrap();
        let victim = dir.path().join("victim.txt");
        fs::write(&victim, "keep").unwrap();
        let discovery = crate::folder::ops_dir(dir.path()).join(DISCOVERY_DIR);
        fs::create_dir_all(&discovery).unwrap();
        std::os::unix::fs::symlink(&victim, discovery.join(DIGEST_FILE)).unwrap();
        let path = write_digest(dir.path(), "new").unwrap();
        assert_eq!(fs::read_to_string(&victim).unwrap(), "keep");
        assert_eq!(fs::read_to_string(path).unwrap(), "new");
    }

    #[test]
    fn the_digest_is_written_under_the_ops_folder() {
        let dir = tempfile::tempdir().unwrap();
        let path = write_digest(dir.path(), "hello").unwrap();
        assert!(path.ends_with(".connecto/ops/.discovery/prompts.md"));
        assert_eq!(fs::read_to_string(path).unwrap(), "hello");
    }
}
