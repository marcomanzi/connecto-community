//! The token host against synthetic transcripts and a fake study.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::atomic::{AtomicUsize, Ordering};

use connecto_core::tokens::{Confirmation, ProposalKind, TokenControl};

use super::*;

const PROJECT_ID: &str = "p1";

/// One synthetic session: a skill listing, then `calls` calls.
fn transcript(skills: &[&str], calls: usize, start: &str) -> String {
    let names: Vec<String> = skills.iter().map(|s| (*s).to_owned()).collect();
    let content: String = skills
        .iter()
        .map(|s| format!("- {s}: does a thing\n"))
        .collect();
    let mut lines = vec![
        serde_json::json!({"type":"user","timestamp":start,"message":{"content":"hello"}}).to_string(),
        serde_json::json!({"type":"attachment","attachment":{"type":"skill_listing","names":names,"content":content}})
            .to_string(),
    ];
    for i in 0..calls {
        lines.push(
            serde_json::json!({"type":"assistant","timestamp":start,"message":{"id":format!("m{i}"),
                "usage":{"input_tokens":1,"cache_read_input_tokens":40_000 + i * 1000,"cache_creation_input_tokens":100,"output_tokens":5}}})
            .to_string(),
        );
    }
    lines.join("\n")
}

struct Fake {
    project: PathBuf,
    profile: PathBuf,
    reply: Mutex<Result<String, String>>,
    studies: AtomicUsize,
    /// How long a study takes.
    delay: Duration,
}

impl TokenEnv for Fake {
    fn project_path(&self, project_id: &str) -> Option<PathBuf> {
        (project_id == PROJECT_ID).then(|| self.project.clone())
    }
    fn profiles(&self, _project: &Path) -> Vec<PathBuf> {
        vec![self.profile.clone()]
    }
    fn study(&self, _id: &str, _project: &Path, prompt: String) -> Result<String, String> {
        assert!(prompt.contains("<summary>"));
        self.studies.fetch_add(1, Ordering::SeqCst);
        std::thread::sleep(self.delay);
        lock(&self.reply).clone()
    }
}

struct H {
    _dir: tempfile::TempDir,
    data: PathBuf,
    fake: Arc<Fake>,
    host: TokenHandle,
}

fn harness(reply: Result<String, String>) -> H {
    harness_with(reply, Duration::ZERO)
}

fn harness_with(reply: Result<String, String>, delay: Duration) -> H {
    let dir = tempfile::tempdir().unwrap();
    let data = dir.path().join("data");
    let project = dir.path().join("proj");
    let profile = dir.path().join("profile");
    let folder = profile
        .join("projects")
        .join(token_audit::transcript_dir_name(&project));
    std::fs::create_dir_all(&folder).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let stamp = token_audit::format_utc(now_ms() - 3_600_000);
    for i in 0..5 {
        std::fs::write(
            folder.join(format!("s{i}.jsonl")),
            transcript(&["dataviz", "dev-team"], 4, &stamp),
        )
        .unwrap();
    }
    let fake = Arc::new(Fake {
        project,
        profile,
        reply: Mutex::new(reply),
        studies: AtomicUsize::new(0),
        delay,
    });
    let host = TokenHandle(TokenHost::new(
        data.clone(),
        Arc::clone(&fake) as Arc<dyn TokenEnv>,
        None,
        Arc::new(|| {}),
    ));
    H {
        _dir: dir,
        data,
        fake,
        host,
    }
}

fn settled(h: &H) -> ProjectTokenView {
    let start = Instant::now();
    loop {
        let view = h.host.view();
        if let Some(p) = view.project(PROJECT_ID)
            && p.busy.is_none()
            && p.report.is_some()
            && p.study != StudyState::Running
        {
            return p.clone();
        }
        assert!(start.elapsed() < Duration::from_secs(20), "never settled");
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn an_analysis_reports_proposes_and_studies() {
    let good =
        r#"{"findings":[{"title":"Long session","evidence":"4 calls","advice":"Split topics"}]}"#;
    let h = harness(Ok(good.to_owned()));
    h.host.analyze(PROJECT_ID).unwrap();
    let p = settled(&h);
    let report = p.report.unwrap();
    assert_eq!(report.totals.sessions, 5);
    assert_eq!(report.totals.calls, 20);
    // dataviz is never used in five sessions: proposed and ticked; dev-team
    // is Connecto's.
    let names: Vec<&str> = report.proposals.iter().map(|p| p.name.as_str()).collect();
    assert_eq!(names, ["dataviz"]);
    assert_eq!(report.proposals[0].kind, ProposalKind::Skill);
    assert!(report.proposals[0].preselected);
    assert!(report.biggest.is_some());
    match p.study {
        StudyState::Done(findings) => assert_eq!(findings[0].title, "Long session"),
        other => panic!("{other:?}"),
    }
}

#[test]
fn a_bad_or_failed_study_keeps_the_analysis() {
    for reply in [
        Ok("I cannot do that".to_owned()),
        Err("Claude is not logged in".to_owned()),
    ] {
        let h = harness(reply);
        h.host.analyze(PROJECT_ID).unwrap();
        let p = settled(&h);
        assert_eq!(p.report.unwrap().totals.sessions, 5);
        assert!(
            matches!(p.study, StudyState::Unavailable(_)),
            "{:?}",
            p.study
        );
        assert_eq!(h.fake.studies.load(Ordering::SeqCst), 1);
    }
}

#[test]
fn apply_writes_the_setting_with_before_and_undo_empties_it() {
    let h = harness(Ok(r#"{"findings":[]}"#.to_owned()));
    // Nothing to apply before an analysis.
    assert!(
        h.host
            .apply(PROJECT_ID, vec![], vec!["dataviz".into()])
            .is_err()
    );
    h.host.analyze(PROJECT_ID).unwrap();
    settled(&h);
    h.host
        .apply(PROJECT_ID, vec!["pm@synced".into()], vec!["dataviz".into()])
        .unwrap();
    let saved = project_settings::load_token_optimization(&h.data, PROJECT_ID);
    assert_eq!(saved.plugins_off, ["pm@synced"]);
    assert_eq!(saved.skills_off, ["dataviz"]);
    assert!(saved.applied_at.is_some());
    let before = saved.before.unwrap();
    assert_eq!(before.sessions, 5);
    assert!(before.start_context_median > 40_000);
    // No session since: start a new terminal to measure.
    let view = h.host.view();
    let after = view
        .project(PROJECT_ID)
        .unwrap()
        .report
        .as_ref()
        .unwrap()
        .after
        .clone()
        .unwrap();
    assert_eq!(after.sessions, 0);
    assert_eq!(after.confirmation, Confirmation::NotYet);
    // A second apply adds; nothing is lost.
    h.host
        .apply(PROJECT_ID, vec![], vec!["other".into()])
        .unwrap();
    let saved = project_settings::load_token_optimization(&h.data, PROJECT_ID);
    assert_eq!(saved.skills_off, ["dataviz", "other"]);
    h.host.set_autocompact(PROJECT_ID, Some(200_000)).unwrap();
    assert!(h.host.set_autocompact(PROJECT_ID, Some(123)).is_err());
    h.host.undo(PROJECT_ID).unwrap();
    let saved = project_settings::load_token_optimization(&h.data, PROJECT_ID);
    assert!(saved.plugins_off.is_empty() && saved.skills_off.is_empty());
    assert!(saved.applied_at.is_none());
    // Undo is about the lists: the compaction choice stays.
    assert_eq!(saved.autocompact, Some(200_000));
    assert_eq!(h.host.view().project(PROJECT_ID).unwrap().settings, saved);
}

#[test]
fn an_unknown_project_is_refused() {
    let h = harness(Ok(String::new()));
    assert!(h.host.analyze("nope").is_err());
}

#[test]
fn a_second_analysis_waits_and_cuts_made_meanwhile_stay() {
    let h = harness_with(
        Ok(r#"{"findings":[]}"#.to_owned()),
        Duration::from_millis(400),
    );
    h.host.analyze(PROJECT_ID).unwrap();
    assert!(h.host.analyze(PROJECT_ID).is_err());
    // While the study runs, the compaction choice changes: the view keeps it.
    while h.host.view().project(PROJECT_ID).unwrap().study != StudyState::Running {
        std::thread::sleep(Duration::from_millis(5));
    }
    h.host.set_autocompact(PROJECT_ID, Some(300_000)).unwrap();
    let p = settled(&h);
    assert_eq!(p.settings.autocompact, Some(300_000));
}

#[test]
fn applying_the_same_cuts_again_keeps_the_measurement_start() {
    let h = harness(Ok(r#"{"findings":[]}"#.to_owned()));
    h.host.analyze(PROJECT_ID).unwrap();
    settled(&h);
    h.host
        .apply(PROJECT_ID, vec![], vec!["dataviz".into()])
        .unwrap();
    let first = project_settings::load_token_optimization(&h.data, PROJECT_ID);
    std::thread::sleep(Duration::from_millis(1_100));
    h.host
        .apply(PROJECT_ID, vec![], vec!["dataviz".into()])
        .unwrap();
    let again = project_settings::load_token_optimization(&h.data, PROJECT_ID);
    assert_eq!(first.applied_at, again.applied_at);
    assert_eq!(first, again);
}
