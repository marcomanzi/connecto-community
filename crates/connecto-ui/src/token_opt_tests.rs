//! The Tokens tab against a fake control.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::sync::{Arc, Mutex};

use connecto_core::tokens::{
    After, Before, CompactEstimate, Loaded, ProjectTokenView, StudyFinding, TokenSettings,
    TokenView, Totals,
};
use egui::{Event, Pos2, RawInput, Rect, Vec2};

use super::*;

const P: &str = "proj";

#[derive(Default)]
struct Fake {
    view: Mutex<TokenView>,
    calls: Mutex<Vec<String>>,
}

impl TokenControl for Fake {
    fn view(&self) -> Arc<TokenView> {
        Arc::new(self.view.lock().unwrap().clone())
    }
    fn open(&self, project_id: &str) {
        self.calls
            .lock()
            .unwrap()
            .push(format!("open {project_id}"));
    }
    fn analyze(&self, _: &str) -> Result<(), String> {
        self.calls.lock().unwrap().push("analyze".into());
        Ok(())
    }
    fn apply(&self, _: &str, plugins: Vec<String>, skills: Vec<String>) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("apply {plugins:?} {skills:?}"));
        Ok(())
    }
    fn undo(&self, _: &str) -> Result<(), String> {
        self.calls.lock().unwrap().push("undo".into());
        Err("the setting could not be saved".into())
    }
    fn set_autocompact(&self, _: &str, tokens: Option<u32>) -> Result<(), String> {
        self.calls
            .lock()
            .unwrap()
            .push(format!("compact {tokens:?}"));
        Ok(())
    }
}

fn proposal(kind: ProposalKind, name: &str, ids: &[&str], preselected: bool) -> Proposal {
    Proposal {
        kind,
        name: name.into(),
        ids: ids.iter().map(|s| (*s).to_owned()).collect(),
        tokens: 1_436,
        evidence: "used 0 times in 23 sessions".into(),
        preselected,
        hint: (!kind.applicable()).then(|| "Use /mcp".to_owned()),
    }
}

fn report() -> TokenReport {
    TokenReport {
        analyzed_ms: 7,
        totals: Totals {
            sessions: 23,
            calls: 900,
            cache_read_share: 96.0,
            median_context: 120_000,
            median_start_context: 42_614,
            reads_above_200k: 41.0,
            ..Totals::default()
        },
        loaded: Some(Loaded {
            skills: 40,
            ..Loaded::default()
        }),
        proposals: vec![
            proposal(
                ProposalKind::Plugin,
                "product-management",
                &["product-management@synced", "product-management@kwp"],
                true,
            ),
            proposal(ProposalKind::Skill, "dataviz", &["dataviz"], true),
            proposal(ProposalKind::Skill, "init", &["init"], false),
            proposal(ProposalKind::McpServer, "claude_ai_Gmail", &[], false),
        ],
        compact: CompactEstimate {
            reads_above_200k: 41.0,
            savings: vec![(150_000, 30.0), (200_000, 22.0), (300_000, 9.0)],
        },
        after: None,
        biggest: Some("started 2026-11-20 10:00 UTC, 800 calls".into()),
    }
}

fn fake(project: ProjectTokenView) -> Arc<Fake> {
    let fake = Fake::default();
    *fake.view.lock().unwrap() = TokenView {
        projects: vec![project],
        available: true,
    };
    Arc::new(fake)
}

fn base() -> ProjectTokenView {
    ProjectTokenView {
        project_id: P.into(),
        ..ProjectTokenView::default()
    }
}

struct H {
    ctx: egui::Context,
    panel: TokenPanel,
    fake: Arc<Fake>,
}

impl H {
    fn new(fake: Arc<Fake>) -> Self {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut h = Self {
            ctx,
            panel: TokenPanel::default(),
            fake,
        };
        h.frame(vec![]);
        h.frame(vec![]);
        h
    }

    fn frame(&mut self, events: Vec<Event>) {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 6000.0))),
            events,
            ..RawInput::default()
        };
        let (panel, fake) = (&mut self.panel, &self.fake);
        let mut output = self.ctx.run_ui(input, |ui| {
            section(ui, panel, fake.as_ref(), P);
        });
        output.textures_delta.clear();
    }

    fn click(&mut self, key: &str) {
        let rect = *self
            .panel
            .marks
            .rects
            .get(key)
            .unwrap_or_else(|| panic!("{key} not drawn"));
        let at = rect.center();
        self.frame(vec![
            Event::PointerMoved(at),
            Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: true,
                modifiers: egui::Modifiers::default(),
            },
            Event::PointerButton {
                pos: at,
                button: egui::PointerButton::Primary,
                pressed: false,
                modifiers: egui::Modifiers::default(),
            },
        ]);
        self.frame(vec![]);
    }

    fn says(&self, part: &str) -> bool {
        self.panel.marks.texts.iter().any(|t| t.contains(part))
    }

    fn calls(&self) -> Vec<String> {
        self.fake.calls.lock().unwrap().clone()
    }
}

#[test]
fn a_fresh_tab_offers_the_analysis_and_explains() {
    let mut h = H::new(fake(base()));
    assert!(h.says("AGENT TOKEN OPTIMIZATION"));
    assert!(h.says("Analyze past sessions to see"));
    assert!(h.says("Apply a cut to measure"));
    assert_eq!(h.calls()[0], format!("open {P}"));
    h.click("tok-analyze");
    assert!(h.calls().contains(&"analyze".to_owned()));
}

#[test]
fn the_report_shows_and_apply_sends_the_ticked_ids() {
    let mut h = H::new(fake(ProjectTokenView {
        report: Some(report()),
        ..base()
    }));
    assert!(h.says("23 sessions, 900 calls"));
    assert!(h.says("Calls above 200k made 41%"));
    assert!(h.says("LOADED BUT NEVER USED"));
    assert!(h.says("used 0 times in 23 sessions"));
    assert!(h.says("Use /mcp"));
    // Advice has no tick.
    assert!(!h.panel.marks.rects.contains_key("tok-tick:claude_ai_Gmail"));
    assert!(h.says("running terminals keep their setup"));
    // Preselected: the plugin and dataviz; tick init too, untick dataviz.
    h.click("tok-tick:init");
    h.click("tok-tick:dataviz");
    h.click("tok-apply");
    let calls = h.calls();
    assert!(
        calls.contains(
            &r#"apply ["product-management@synced", "product-management@kwp"] ["init"]"#.to_owned()
        ),
        "{calls:?}"
    );
}

#[test]
fn nothing_ticked_cannot_be_applied_and_a_refusal_shows() {
    let mut r = report();
    for p in &mut r.proposals {
        p.preselected = false;
    }
    let mut h = H::new(fake(ProjectTokenView {
        report: Some(r),
        settings: TokenSettings {
            skills_off: vec!["old".into()],
            ..TokenSettings::default()
        },
        ..base()
    }));
    h.click("tok-apply");
    assert!(!h.calls().iter().any(|c| c.starts_with("apply")));
    assert!(h.says("Skills off: old"));
    h.click("tok-undo");
    assert!(h.calls().contains(&"undo".to_owned()));
    assert!(h.says("the setting could not be saved"));
}

#[test]
fn before_after_and_the_study() {
    let mut r = report();
    r.after = Some(After {
        sessions: 2,
        start_context_median: Some(41_178),
        confirmation: Confirmation::Confirmed,
    });
    let h = H::new(fake(ProjectTokenView {
        report: Some(r),
        settings: TokenSettings {
            plugins_off: vec!["pm@synced".into()],
            before: Some(Before {
                start_context_median: 42_614,
                sessions: 10,
            }),
            autocompact: Some(200_000),
            ..TokenSettings::default()
        },
        study: StudyState::Done(vec![StudyFinding {
            title: "Large outputs".into(),
            evidence: "Bash 180k chars".into(),
            advice: "Delegate long test runs".into(),
        }]),
        ..base()
    }));
    assert!(h.says("Before: median start context 43k (10 sessions)."));
    assert!(h.says("After: median start context 42k (2 sessions)."));
    assert!(h.says("confirmed in the transcript"));
    assert!(h.says("Estimated saving: ~22% of reads"));
    assert!(h.says("Large outputs"));
    assert!(h.says("Delegate long test runs"));
}

#[test]
fn no_session_after_and_a_failed_study_are_said() {
    let mut r = report();
    r.after = Some(After {
        sessions: 0,
        start_context_median: None,
        confirmation: Confirmation::NotYet,
    });
    let h = H::new(fake(ProjectTokenView {
        report: Some(r),
        settings: TokenSettings {
            before: Some(Before::default()),
            ..TokenSettings::default()
        },
        study: StudyState::Unavailable("the reply was not the JSON asked for".into()),
        busy: Some("studying the biggest session…".into()),
        ..base()
    }));
    assert!(h.says("start a new terminal to measure"));
    assert!(h.says("Study unavailable: the reply was not the JSON asked for"));
    assert!(h.says("studying the biggest session…"));
}

#[test]
fn compact_earlier_sends_the_choice() {
    let mut h = H::new(fake(ProjectTokenView {
        report: Some(report()),
        ..base()
    }));
    assert!(h.says("Calls above 200k used 41% of your cache reads"));
    assert!(!h.says("Estimated saving"));
    h.click("tok-compact:150000");
    // Auto is on: clicking it again sends nothing.
    h.click("tok-compact:auto");
    assert_eq!(
        h.calls()
            .iter()
            .filter(|c| c.starts_with("compact"))
            .cloned()
            .collect::<Vec<_>>(),
        ["compact Some(150000)"]
    );
}

#[test]
fn without_the_feature_the_tab_says_so() {
    let fake = Arc::new(Fake::default());
    let h = H::new(fake);
    assert!(h.says("not available in this build"));
}

#[test]
fn apply_is_off_when_the_ticks_add_nothing_new() {
    let mut r = report();
    r.proposals.retain(|p| p.name == "dataviz");
    let mut h = H::new(fake(ProjectTokenView {
        report: Some(r),
        settings: TokenSettings {
            skills_off: vec!["dataviz".into()],
            ..TokenSettings::default()
        },
        ..base()
    }));
    h.click("tok-apply");
    assert!(!h.calls().iter().any(|c| c.starts_with("apply")));
}
