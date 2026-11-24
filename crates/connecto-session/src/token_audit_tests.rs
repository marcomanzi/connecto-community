//! Agent Token Optimization: the parser, the figures and the proposals,
//! against synthetic transcripts only.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::io::Cursor;

use serde_json::json;

use super::*;

const FIXTURE: &str = include_str!("../tests/fixtures/token_audit/session.jsonl");

fn fixture() -> Session {
    parse_session("s1", Cursor::new(FIXTURE))
}

/// A session of `calls` calls, each `context` large, starting at `start_ms`,
/// one minute apart, loading `skills`.
fn session(id: &str, start_ms: u64, contexts: &[u64], skills: &[&str]) -> Session {
    let mut lines = Vec::new();
    let names: Vec<String> = skills.iter().map(|s| (*s).to_owned()).collect();
    let content: String = skills
        .iter()
        .map(|s| format!("- {s}: does things\n"))
        .collect();
    lines.push(
        json!({"type":"attachment","attachment":{"type":"skill_listing","names":names,"content":content}})
            .to_string(),
    );
    lines.push(
        json!({"type":"attachment","attachment":{"type":"deferred_tools_delta","addedNames":[]}})
            .to_string(),
    );
    for (i, context) in contexts.iter().enumerate() {
        let ms = start_ms + i as u64 * 60_000;
        lines.push(
            json!({"type":"assistant","timestamp":format_utc(ms),"message":{"id":format!("{id}-{i}"),
                "usage":{"input_tokens":0,"cache_read_input_tokens":context,"cache_creation_input_tokens":0,"output_tokens":10},"content":[]}})
            .to_string(),
        );
    }
    parse_session(id, Cursor::new(lines.join("\n")))
}

#[test]
fn the_fixture_is_read_line_by_line_skipping_what_cannot_matter() {
    let s = fixture();
    // m1 twice is one call; the sidechain and the request log are not calls.
    assert_eq!(s.calls.len(), 4);
    assert_eq!(s.calls[0].output, 120);
    assert_eq!(s.start_context(), 40_003);
    assert_eq!(s.calls[0].write_1h, 40_000);
    // Without the split, a write counts as 1 hour.
    assert_eq!(s.calls[1].write_1h, 500);
    assert_eq!(s.peak(), 41_002);
    assert_eq!(s.first_ms, parse_utc("2026-11-20T10:00:00.000Z"));
    // The boundary and the summary are one compaction.
    assert_eq!(s.compactions, 1);
    // Two hours of pause, then a cold rebuild.
    assert_eq!(s.pauses.len(), 1);
    assert_eq!(s.pauses[0].minutes, 119);
    assert_eq!(s.cold_rebuilds, 1);
    let loaded = s.loaded.as_ref().unwrap();
    assert_eq!(loaded.skills.len(), 4);
    // A description over two lines counts whole.
    assert_eq!(
        loaded.skills["dataviz"],
        "- dataviz: Charts and\nmore charts\n".len()
    );
    assert_eq!(loaded.mcp_tools.len(), 3);
    assert_eq!(loaded.agents.len(), 2);
    assert!(loaded.mcp_instructions.contains_key("easel"));
    assert_eq!(s.used.skills["update-config"], 1);
    assert_eq!(s.used.servers["easel"], 1);
    assert_eq!(s.used.agents["Explore"], 1);
    // Two prompts; the compaction summary is not one.
    assert_eq!(s.turns.len(), 2);
    assert_eq!(s.turns[0].before, 0);
    assert_eq!(s.turns[0].after, 40_802);
    assert_eq!(s.turns[0].tools["Bash"], 1);
    assert_eq!(s.outputs[0].name, "Bash");
    assert_eq!(s.outputs[0].label, "cargo test --workspace");
}

#[test]
fn a_transcript_without_attachments_is_not_recorded() {
    let line =
        json!({"type":"assistant","message":{"id":"a","usage":{"input_tokens":5}}}).to_string();
    let s = parse_session("x", Cursor::new(line));
    assert_eq!(s.calls.len(), 1);
    assert!(s.loaded.is_none());
    assert!(loaded_counts(Some(&s)).is_none());
}

#[test]
fn the_transcript_folder_is_the_path_with_dashes() {
    assert_eq!(
        transcript_dir_name(Path::new("/Users/x/workspace/connecto")),
        "-Users-x-workspace-connecto"
    );
    assert_eq!(transcript_dir_name(Path::new("/a b/c.d_e")), "-a-b-c-d-e");
}

#[test]
fn only_recent_top_level_transcripts_are_found_and_short_sessions_dropped() {
    let root = tempfile::tempdir().unwrap();
    let project = Path::new("/w/p");
    let dir = root
        .path()
        .join("projects")
        .join(transcript_dir_name(project));
    fs::create_dir_all(dir.join("sub")).unwrap();
    fs::write(dir.join("a.jsonl"), FIXTURE).unwrap();
    fs::write(dir.join("sub").join("agent.jsonl"), FIXTURE).unwrap();
    fs::write(dir.join("notes.txt"), "x").unwrap();
    // Two calls only (m1, then m2 with its MCP use).
    let lines: Vec<&str> = FIXTURE.lines().collect();
    let short = [&lines[..10], &lines[12..13]].concat().join("\n");
    fs::write(dir.join("short.jsonl"), short).unwrap();
    let now = std::time::SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis() as u64;
    let found = find_transcripts(&[root.path().to_owned()], project, now);
    assert_eq!(found.files.len(), 2);
    let read = read_sessions(&found, &|_, _| {});
    assert_eq!(read.kept.len(), 1);
    assert_eq!(read.kept[0].id, "a");
    // The short one's uses still count.
    assert_eq!(read.short_uses.servers["easel"], 1);
    // Forty days later, nothing is recent.
    let later = find_transcripts(&[root.path().to_owned()], project, now + 40 * 86_400_000);
    assert!(later.files.is_empty());
}

#[test]
fn totals_count_shares_and_big_calls() {
    let a = session("a", 1_000, &[40_000, 100_000, 250_000], &[]);
    let b = session("b", 2_000, &[50_000, 60_000, 70_000], &[]);
    let t = totals(&[a, b], &Found::default());
    assert_eq!(t.sessions, 2);
    assert_eq!(t.calls, 6);
    assert_eq!(t.median_start_context, 45_000);
    assert!((t.cache_read_share - 100.0).abs() < 1e-9);
    let reads = 570_000.0;
    assert!((t.reads_above_200k - 250_000.0 * 100.0 / reads).abs() < 1e-9);
    assert!(t.limited.is_none());
    let left = Found {
        left_out: 3,
        ..Found::default()
    };
    assert!(totals(&[], &left).limited.unwrap().contains("3 older"));
}

fn ids(pairs: &[(&str, &[&str])]) -> BTreeMap<String, BTreeSet<String>> {
    pairs
        .iter()
        .map(|(n, ids)| {
            (
                (*n).to_owned(),
                ids.iter().map(|i| (*i).to_owned()).collect(),
            )
        })
        .collect()
}

#[test]
fn proposals_skip_what_was_used_and_connecto_skills() {
    let s = fixture();
    let all = vec![s.clone()];
    let found = proposals(
        Some(&s),
        &all,
        &Used::default(),
        &ids(&[(
            "product-management",
            &[
                "product-management@synced",
                "product-management@knowledge-work-plugins",
            ],
        )]),
    );
    let names: Vec<(&str, ProposalKind)> =
        found.iter().map(|p| (p.name.as_str(), p.kind)).collect();
    // The plugin (never used), dataviz (never used); update-config was used
    // by its command, dev-team is Connecto's; easel was used, Google Drive
    // was not (advice).
    assert!(names.contains(&("product-management", ProposalKind::Plugin)));
    assert!(names.contains(&("dataviz", ProposalKind::Skill)));
    assert!(names.contains(&("claude_ai_Google_Drive", ProposalKind::McpServer)));
    assert!(
        !names
            .iter()
            .any(|(n, _)| *n == "update-config" || *n == "dev-team" || *n == "easel")
    );
    let plugin = found
        .iter()
        .find(|p| p.kind == ProposalKind::Plugin)
        .unwrap();
    assert_eq!(plugin.ids.len(), 2);
    assert_eq!(plugin.evidence, "used 0 times in 1 session");
    // One session: nothing starts ticked.
    assert!(found.iter().all(|p| !p.preselected));
    // Advice carries how to do it and no id.
    let mcp = found
        .iter()
        .find(|p| p.kind == ProposalKind::McpServer)
        .unwrap();
    assert!(mcp.ids.is_empty() && mcp.hint.is_some());
    // Applicable ones first.
    assert!(found[0].kind.applicable());
}

#[test]
fn a_plugin_used_once_is_not_proposed_and_five_sessions_preselect() {
    let base = fixture();
    let mut used = base.clone();
    used.used
        .skills
        .insert("product-management:brainstorm".to_owned(), 1);
    let mut all = vec![base.clone(); 4];
    all.push(used);
    let found = proposals(
        Some(&base),
        &all,
        &Used::default(),
        &ids(&[("product-management", &["product-management@synced"])]),
    );
    assert!(!found.iter().any(|p| p.kind == ProposalKind::Plugin));
    let dataviz = found.iter().find(|p| p.name == "dataviz").unwrap();
    assert!(dataviz.preselected);
    assert_eq!(dataviz.evidence, "used 0 times in 5 sessions");
    // A plugin with no known id cannot be turned off: not proposed.
    let none = proposals(
        Some(&base),
        std::slice::from_ref(&base),
        &Used::default(),
        &BTreeMap::new(),
    );
    // A use in a session too short for the figures still counts.
    let mut short = Used::default();
    short.skills.insert("dataviz".to_owned(), 1);
    let found = proposals(
        Some(&base),
        std::slice::from_ref(&base),
        &short,
        &ids(&[("product-management", &["product-management@synced"])]),
    );
    assert!(!found.iter().any(|p| p.name == "dataviz"));
    assert!(!none.iter().any(|p| p.kind == ProposalKind::Plugin));
}

#[test]
fn a_plugins_mcp_use_counts_for_the_plugin() {
    let base = fixture();
    let mut used = base.clone();
    used.used
        .servers
        .insert("plugin_product-management_amplitude".to_owned(), 2);
    let found = proposals(
        Some(&base),
        &[base.clone(), used],
        &Used::default(),
        &ids(&[("product-management", &["product-management@synced"])]),
    );
    assert!(!found.iter().any(|p| p.name == "product-management"));
    // Its server is never advice of its own.
    assert!(!found.iter().any(|p| p.name.starts_with("plugin_")));
}

#[test]
fn a_large_memory_file_is_pointed_out() {
    let line = json!({"type":"attachment","attachment":{"type":"instructions","files":[
        {"path":"/p/CLAUDE.md","content":"x".repeat(2_500)},
        {"path":"/p/MEMORY.md","content":"short"}]}})
    .to_string();
    let s = parse_session("m", Cursor::new(line));
    let found = proposals(
        Some(&s),
        std::slice::from_ref(&s),
        &Used::default(),
        &BTreeMap::new(),
    );
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].kind, ProposalKind::MemoryFile);
    assert_eq!(found[0].name, "/p/CLAUDE.md");
    assert_eq!(found[0].tokens, 625);
}

#[test]
fn plugin_ids_come_from_manifests_installed_and_settings() {
    let profile = tempfile::tempdir().unwrap();
    let project = tempfile::tempdir().unwrap();
    let synced = profile.path().join("plugins").join("synced").join("org_1");
    fs::create_dir_all(&synced).unwrap();
    fs::write(
        synced.join("manifest.json"),
        r#"{"plugins":[{"name":"product-management","marketplaceName":"knowledge-work-plugins"}]}"#,
    )
    .unwrap();
    fs::write(
        profile
            .path()
            .join("plugins")
            .join("installed_plugins.json"),
        r#"{"version":2,"plugins":{"context7@claude-plugins-official":[{}]}}"#,
    )
    .unwrap();
    fs::write(
        profile.path().join("settings.json"),
        r#"{"enabledPlugins":{"product-management@knowledge-work-plugins":true,"bad":true}}"#,
    )
    .unwrap();
    fs::create_dir_all(project.path().join(".claude")).unwrap();
    fs::write(
        project.path().join(".claude").join("settings.local.json"),
        r#"{"enabledPlugins":{"local-one@market":true}}"#,
    )
    .unwrap();
    let found = plugin_ids(&[profile.path().to_owned()], project.path());
    assert_eq!(
        found["product-management"],
        [
            "product-management@knowledge-work-plugins",
            "product-management@synced"
        ]
        .into_iter()
        .map(str::to_owned)
        .collect()
    );
    assert!(found.contains_key("context7"));
    assert!(found.contains_key("local-one"));
    assert!(!found.contains_key("bad"));
}

#[test]
fn before_and_after_the_cuts() {
    let old: Vec<Session> = (0..12)
        .map(|i| {
            session(
                &format!("o{i}"),
                1_000 + i,
                &[40_000 + i * 100, 1, 1],
                &["dataviz"],
            )
        })
        .collect();
    let b = before(&old);
    assert_eq!(b.sessions, 10);
    assert_eq!(b.start_context_median, 40_000 + 650);
    let settings = TokenSettings {
        skills_off: vec!["dataviz".to_owned()],
        plugins_off: vec!["pm@synced".to_owned()],
        ..TokenSettings::default()
    };
    let applied = 10_000_000;
    let none = after(&old, applied, &settings);
    assert_eq!(none.sessions, 0);
    assert_eq!(none.confirmation, Confirmation::NotYet);
    assert!(none.start_context_median.is_none());
    let mut all = old.clone();
    all.push(session("n1", applied + 1_000, &[38_000, 1, 1], &["init"]));
    let one = after(&all, applied, &settings);
    assert_eq!(one.sessions, 1);
    assert_eq!(one.start_context_median, Some(38_000));
    assert_eq!(one.confirmation, Confirmation::Confirmed);
    let mut still = old;
    still.push(session(
        "n2",
        applied + 5_000,
        &[42_000, 1, 1],
        &["dataviz", "pm:x"],
    ));
    assert_eq!(
        after(&still, applied, &settings).confirmation,
        Confirmation::StillLoaded(vec!["dataviz".to_owned(), "pm:x".to_owned()])
    );
}

#[test]
fn compacting_earlier_saves_only_on_long_sessions() {
    let short = session("s", 0, &[40_000, 60_000, 80_000], &[]);
    let e = compact_estimate(std::slice::from_ref(&short));
    assert_eq!(e.savings.len(), 3);
    assert!(e.savings.iter().all(|(_, pct)| *pct == 0.0));
    let long: Vec<u64> = (0..200).map(|i| 100_000 + i * 2_000).collect();
    let e = compact_estimate(&[session("l", 0, &long, &[])]);
    assert!(e.reads_above_200k > 0.0);
    let at = |t: u32| e.savings.iter().find(|(x, _)| *x == t).unwrap().1;
    assert!(at(150_000) > at(300_000));
    assert!(at(150_000) > 0.0 && at(150_000) < 100.0);
}

#[test]
fn the_study_summary_is_bounded_and_redacted() {
    let mut s = fixture();
    s.turns[0].text =
        "use the key sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdefABCDEF now".to_owned();
    let text = study_summary(&s);
    assert!(text.contains("Largest tool outputs"));
    assert!(text.contains("cargo test --workspace"));
    assert!(!text.contains("AbCdEfGhIjKlMnOpQrStUvWxYz0123456789"));
    // Many long prompts: the head and the tail stay, under the limit.
    s.turns = (0..500)
        .map(|i| Turn {
            text: format!("prompt {i} {}", "x".repeat(190)),
            ..Turn::default()
        })
        .collect();
    let text = study_summary(&s);
    assert!(text.chars().count() <= SUMMARY_LIMIT);
    assert!(text.contains("1. ["));
    assert!(text.contains("500. ["));
    assert!(text.contains("prompts left out"));
    let prompt = study_prompt(&text);
    assert!(prompt.contains("JSON only"));
}

#[test]
fn the_study_reply_is_checked() {
    let good = r#"Here: {"findings":[{"title":"Big outputs","evidence":"Bash 180k","advice":"Delegate"},{"title":"x"}]}"#;
    let found = parse_study(good).unwrap();
    assert_eq!(found.len(), 1);
    assert_eq!(found[0].title, "Big outputs");
    assert_eq!(parse_study(r#"{"findings":[]}"#).unwrap(), vec![]);
    let many = json!({"findings": (0..9).map(|i| json!({"title":format!("t{i}"),"evidence":"e","advice":"a"})).collect::<Vec<_>>()}).to_string();
    assert_eq!(parse_study(&many).unwrap().len(), 6);
    for bad in [
        "no json",
        "{not json}",
        r#"{"other":1}"#,
        r#"{"findings":[{"title":"only"}]}"#,
        "} {",
    ] {
        assert!(parse_study(bad).is_err(), "{bad}");
    }
}

#[test]
fn times_round_trip() {
    let ms = parse_utc("2026-11-21T23:50:00Z").unwrap();
    assert_eq!(format_utc(ms), "2026-11-21T23:50:00Z");
    assert_eq!(
        parse_utc("2026-11-21T23:50:00.5Z"),
        Some(ms - 50 * 60_000 + 50 * 60_000 + 500)
    );
    assert_eq!(parse_utc("2026-11-22T00:50:00+01:00"), Some(ms));
    assert_eq!(parse_utc("1970-01-01T00:00:00Z"), Some(0));
    assert!(parse_utc("yesterday").is_none());
    assert!(parse_utc("2026-13-01T00:00:00Z").is_none());
}

#[test]
fn connecto_skills_match_the_builtin_package() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../connecto-packages/builtin/claude/.claude/skills");
    let mut found: Vec<String> = fs::read_dir(root)
        .unwrap()
        .flatten()
        .map(|e| e.file_name().to_string_lossy().into_owned())
        .collect();
    found.sort();
    let mut known: Vec<String> = CONNECTO_SKILLS.iter().map(|s| (*s).to_owned()).collect();
    known.sort();
    assert_eq!(found, known);
}

#[test]
fn a_plugin_used_through_a_server_no_longer_loaded_or_an_agent_is_not_proposed() {
    let base = fixture();
    let plugin = ids(&[("product-management", &["product-management@synced"])]);
    for (server, agent) in [
        (Some("plugin_product-management_oldserver"), None),
        (None, Some("product-management:researcher")),
    ] {
        let mut used = base.clone();
        if let Some(server) = server {
            used.used.servers.insert(server.to_owned(), 1);
        }
        if let Some(agent) = agent {
            used.used.agents.insert(agent.to_owned(), 1);
        }
        let found = proposals(
            Some(&base),
            &[base.clone(), used],
            &Used::default(),
            &plugin,
        );
        assert!(!found.iter().any(|p| p.name == "product-management"));
    }
}

#[test]
fn only_a_skill_listing_can_confirm_the_cuts() {
    let settings = TokenSettings {
        skills_off: vec!["dataviz".to_owned()],
        ..TokenSettings::default()
    };
    let mut lines = vec![
        json!({"type":"attachment","timestamp":"2026-11-22T10:00:00Z","attachment":{"type":"instructions","files":[{"path":"/p/CLAUDE.md","content":"x"}]}}).to_string(),
    ];
    for i in 0..3 {
        lines.push(
            json!({"type":"assistant","message":{"id":format!("m{i}"),"usage":{"input_tokens":1}}})
                .to_string(),
        );
    }
    let s = parse_session("i", Cursor::new(lines.join("\n")));
    assert!(s.loaded.is_some());
    let applied = parse_utc("2026-11-22T09:00:00Z").unwrap();
    assert_eq!(
        after(std::slice::from_ref(&s), applied, &settings).confirmation,
        Confirmation::NotRecorded
    );
}

#[test]
fn a_real_compaction_ends_the_simulated_saving() {
    // Up to 250k, a real compaction, then a long tail of 50k calls.
    let mut contexts: Vec<u64> = (0..20).map(|i| 100_000 + i * 8_000).collect();
    let real_at = contexts.len();
    contexts.extend(std::iter::repeat_n(50_000, 300));
    let mut s = session("c", 0, &contexts, &[]);
    let without = compact_estimate(std::slice::from_ref(&s));
    s.compacted_at = vec![real_at];
    let with = compact_estimate(std::slice::from_ref(&s));
    let at = |e: &CompactEstimate| e.savings.iter().find(|(t, _)| *t == 150_000).unwrap().1;
    // The tail is already small in reality: nothing is saved there.
    assert!(at(&with) < at(&without));
    assert!(at(&with) < 5.0, "{}", at(&with));
    // The parser records where compactions were.
    assert_eq!(fixture().compacted_at, vec![4]);
}

#[test]
fn an_excerpt_is_redacted_whole_before_it_is_cut() {
    let secret = "sk-ant-api03-AbCdEfGhIjKlMnOpQrStUvWxYz0123456789abcdefABCDEF";
    let pem = format!(
        "-----BEGIN PRIVATE KEY-----\n{}\n-----END PRIVATE KEY-----",
        "QUJDREVGR0hJSktMTU5PUFFSU1RVVldYWVo0123456789abcdefghijklmnop\n".repeat(40)
    );
    let prompt = format!("{}{secret} after", "x".repeat(PROMPT_CHARS - 20));
    let command = format!("cat <<EOF\n{pem}\nEOF");
    let lines = [
        json!({"type":"user","message":{"content":prompt}}).to_string(),
        json!({"type":"assistant","message":{"id":"a","usage":{"input_tokens":1},
            "content":[{"type":"tool_use","id":"t","name":"Bash","input":{"command":command}}]}}).to_string(),
        json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t","content":"ok"}]}}).to_string(),
    ];
    let s = parse_session("r", Cursor::new(lines.join("\n")));
    let text = study_summary(&s);
    assert!(!text.contains("sk-ant-api03-AbCd"), "{text}");
    assert!(!text.contains("QUJDREVGR0hJSktM"), "{text}");
    assert!(s.turns[0].text.chars().count() <= PROMPT_CHARS + 1);
    assert!(s.outputs[0].label.chars().count() <= LABEL_CHARS + 1);
}

#[test]
fn a_partial_inventory_never_confirms_what_it_cannot_show() {
    let applied = parse_utc("2026-11-22T09:00:00Z").unwrap();
    let plugin_cut = TokenSettings {
        plugins_off: vec!["pm@synced".to_owned()],
        ..TokenSettings::default()
    };
    let run = |attachments: Vec<Value>| {
        let mut lines: Vec<String> = attachments
            .into_iter()
            .map(|a| {
                json!({"type":"attachment","timestamp":"2026-11-22T10:00:00Z","attachment":a})
                    .to_string()
            })
            .collect();
        for i in 0..3 {
            lines.push(json!({"type":"assistant","message":{"id":format!("m{i}"),"usage":{"input_tokens":1}}}).to_string());
        }
        let s = parse_session("p", Cursor::new(lines.join("\n")));
        after(std::slice::from_ref(&s), applied, &plugin_cut).confirmation
    };
    let skills = json!({"type":"skill_listing","names":["init"],"content":"- init: x"});
    let tools = json!({"type":"deferred_tools_delta","addedNames":["mcp__plugin_pm_srv__go"]});
    let no_tools = json!({"type":"deferred_tools_delta","addedNames":[]});
    // Skills listed, tools not recorded: an MCP-only plugin may still load.
    assert_eq!(run(vec![skills.clone()]), Confirmation::NotRecorded);
    // Tools recorded with the plugin's server, no skill listing: still loaded.
    assert_eq!(
        run(vec![tools]),
        Confirmation::StillLoaded(vec!["MCP server plugin_pm_srv".to_owned()])
    );
    // Both recorded, nothing of it: confirmed.
    assert_eq!(run(vec![skills, no_tools]), Confirmation::Confirmed);
}

#[test]
fn a_subagents_compaction_is_not_the_sessions() {
    let lines = [
        json!({"type":"assistant","message":{"id":"a","usage":{"input_tokens":1}}}).to_string(),
        json!({"type":"user","isSidechain":true,"isCompactSummary":true,"message":{"content":"x"}})
            .to_string(),
        json!({"type":"assistant","message":{"id":"b","usage":{"input_tokens":1}}}).to_string(),
    ];
    let s = parse_session("s", Cursor::new(lines.join("\n")));
    assert!(s.compacted_at.is_empty());
    assert_eq!(s.compactions, 0);
}

#[test]
fn a_transcript_over_the_limit_alone_is_read_in_part() {
    let found = Found {
        files: vec![],
        left_out: 0,
        cut: true,
    };
    assert!(
        totals(&[], &found)
            .limited
            .unwrap()
            .contains("first 200 MB")
    );
}
