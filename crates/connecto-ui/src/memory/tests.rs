//! The memory section, driven through egui with a fake [`MemoryControl`].

#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use connecto_core::Redacted;
use connecto_core::memory::{
    Address, Anchor, Annotation, AnnotationId, AnnotationKind, AnnotationOrigin, AnnotationTarget,
    AskEntry, AskRequest, AskState, BootstrapOptions, BootstrapPlan, BootstrapState, Capability,
    CoverageGap, CycleState, DreamReport, Evidence, EvidenceId, EvidenceKind, ExportFormat,
    ExportOutcome, Freshness, LocalDate, LocalTime, MAX_ASKS, MAX_READS, MAX_STATEMENT_BYTES,
    MIN_INPUT_CHARS, Memory, MemoryCard, MemoryControl, MemoryDetail, MemoryId, MemoryKind,
    MemorySettings, MemoryStatus, MemoryView, ModelChoice, NoMemory, ProjectMemoryView, Provider,
    ProviderCapability, ReadAnswer, ReadRequest, ReadResult, ReadState, Relation, RelationKind,
    Room, RoomCount, RunEntry, RunOutcome, Scope, ScopeEntry, SearchHit, SkippedRun, SourceClass,
    SourceCoverage, SourceRef, Standing, StatementRefusal, StatementResult, StatementState,
    StatusCounts, Subject, UserStatement, UserStatementKind, Verification, VersionEntry, Weekday,
};
use egui::{Event, Key, Modifiers, PointerButton, Pos2, RawInput, Rect, Vec2};

use super::browse::BRIEF_EVERY_MS;
use super::fetch::REFRESH_SPACING_MS;
use super::{MemoryPanel, Tab, section};

const P: &str = "p1";
/// Thu 2026-11-12 10:00 UTC.
const NOW: u64 = 1_762_941_600_000;

#[derive(Debug, Clone, PartialEq, Eq)]
enum Call {
    Settings(MemorySettings),
    Plan(BootstrapOptions),
    Bootstrap(BootstrapOptions),
    CancelBootstrap,
    Resume,
    DreamNow,
    Revert(u64),
    Export(ExportFormat),
    Delete,
    Read(ReadRequest),
    Statement(UserStatement),
    Ask(AskRequest),
}

#[derive(Default)]
struct Fake {
    view: Mutex<MemoryView>,
    calls: Mutex<Vec<Call>>,
    refuse: Mutex<Option<String>>,
    /// How the engine settles a statement it accepted; `None`: recorded and
    /// applied at once.
    outcome: Mutex<Option<StatementState>>,
    /// Keep accepted statements pending until `publish` says otherwise.
    hold: std::sync::atomic::AtomicBool,
    next_statement: AtomicU64,
    next_read: AtomicU64,
    /// How many times the entrance was asked for.
    briefs: AtomicU64,
    next_ask: AtomicU64,
}

impl Fake {
    /// Settle statement `id`; a recorded one changes the memory's data.
    fn publish(&self, id: u64, state: StatementState) {
        self.edit(|p| {
            if matches!(state, StatementState::Recorded { .. }) {
                p.open_annotations += 1;
            }
            if let Some(r) = p.statements.iter_mut().find(|r| r.id == id) {
                r.state = state;
            }
        });
    }

    fn last_statement(&self) -> u64 {
        self.next_statement.load(Ordering::Relaxed)
    }

    fn answer(&self, call: Call) -> Result<(), String> {
        self.calls.lock().unwrap().push(call);
        self.refuse.lock().unwrap().clone().map_or(Ok(()), Err)
    }

    fn calls(&self) -> Vec<Call> {
        self.calls.lock().unwrap().clone()
    }

    fn take(&self) -> Vec<Call> {
        std::mem::take(&mut *self.calls.lock().unwrap())
    }

    fn edit(&self, change: impl FnOnce(&mut ProjectMemoryView)) {
        let mut view = self.view.lock().unwrap();
        view.revision += 1;
        change(
            view.projects
                .iter_mut()
                .find(|p| p.project_id == P)
                .unwrap(),
        );
    }

    fn settle_read(&self, id: u64, state: ReadState) {
        self.edit(|p| {
            if let Some(r) = p.reads.iter_mut().find(|r| r.id == id) {
                r.state = state;
            }
        });
    }

    /// Settle ask `id`, as the engine does when the model has answered.
    fn settle_ask(&self, id: u64, state: AskState) {
        self.edit(|p| {
            if let Some(a) = p.asks.iter_mut().find(|a| a.id == id) {
                a.state = state;
            }
        });
    }

    fn last_read(&self) -> u64 {
        self.next_read.load(Ordering::Relaxed)
    }
}

impl MemoryControl for Fake {
    fn view(&self) -> Arc<MemoryView> {
        Arc::new(self.view.lock().unwrap().clone())
    }

    fn set_settings(&self, project_id: &str, settings: MemorySettings) -> Result<(), String> {
        assert_eq!(project_id, P);
        let result = self.answer(Call::Settings(settings.clone()));
        if result.is_ok() {
            self.edit(|p| {
                p.enabled = settings.enabled;
                p.settings = settings;
            });
        }
        result
    }

    fn plan_bootstrap(&self, _: &str, options: BootstrapOptions) -> Result<(), String> {
        self.answer(Call::Plan(options))
    }

    fn bootstrap(&self, _: &str, options: BootstrapOptions) -> Result<(), String> {
        self.answer(Call::Bootstrap(options))
    }

    fn cancel_bootstrap(&self, _: &str) {
        let _ = self.answer(Call::CancelBootstrap);
    }

    fn resume_bootstrap(&self, _: &str) -> Result<(), String> {
        self.answer(Call::Resume)
    }

    fn run_dream_now(&self, _: &str) -> Result<(), String> {
        self.answer(Call::DreamNow)
    }

    fn revert(&self, _: &str, version: u64) -> Result<(), String> {
        self.answer(Call::Revert(version))
    }

    fn export(&self, _: &str, format: ExportFormat) -> Result<(), String> {
        self.answer(Call::Export(format))
    }

    fn request_read(&self, _: &str, request: ReadRequest) -> Result<u64, String> {
        // The entrance is a window read the overview asks for on its own: it is
        // kept apart from the reads the user asks for, so tests can tell them.
        if request == ReadRequest::Brief {
            self.briefs.fetch_add(1, Ordering::Relaxed);
        } else {
            self.answer(Call::Read(request.clone()))?;
        }
        let id = self.next_read.fetch_add(1, Ordering::Relaxed) + 1;
        self.edit(|p| {
            p.reads.insert(
                0,
                ReadResult {
                    id,
                    request,
                    state: ReadState::Pending,
                },
            );
            // As the engine keeps them: the newest MAX_READS answers, no more.
            p.reads.truncate(MAX_READS);
        });
        Ok(id)
    }

    fn user_statement(&self, _: &str, statement: UserStatement) -> Result<u64, String> {
        self.answer(Call::Statement(statement.clone()))?;
        let id = self.next_statement.fetch_add(1, Ordering::Relaxed) + 1;
        let state = if self.hold.load(Ordering::Relaxed) {
            StatementState::Pending
        } else {
            self.outcome
                .lock()
                .unwrap()
                .clone()
                .unwrap_or(StatementState::Recorded {
                    applied: true,
                    version: None,
                    note: None,
                })
        };
        self.edit(|p| {
            p.statements.insert(
                0,
                StatementResult {
                    id,
                    subject: statement.about.clone(),
                    kind: statement.kind,
                    state: StatementState::Pending,
                },
            );
        });
        self.publish(id, state);
        Ok(id)
    }

    fn delete_memory(&self, _: &str) -> Result<(), String> {
        self.answer(Call::Delete)
    }

    fn ask(&self, _: &str, request: AskRequest) -> Result<u64, String> {
        self.answer(Call::Ask(request.clone()))?;
        let id = self.next_ask.fetch_add(1, Ordering::Relaxed) + 1;
        self.edit(|p| {
            p.asks.push(AskEntry {
                id,
                question: request.question.clone(),
                choice: request.choice.clone(),
                asked_ms: NOW,
                state: AskState::Pending,
            });
            let excess = p.asks.len().saturating_sub(MAX_ASKS);
            p.asks.drain(..excess);
        });
        Ok(id)
    }
}

fn capabilities() -> Vec<ProviderCapability> {
    vec![
        ProviderCapability {
            provider: Provider::Claude,
            capability: Capability::Available,
            models: vec!["haiku".into(), "sonnet".into(), "opus".into()],
            efforts: vec!["low".into(), "medium".into(), "high".into()],
        },
        ProviderCapability {
            provider: Provider::Codex,
            capability: Capability::Unavailable {
                reason: "codex is not installed".into(),
            },
            models: Vec::new(),
            efforts: Vec::new(),
        },
    ]
}

fn project(enabled: bool) -> ProjectMemoryView {
    let settings = MemorySettings {
        enabled,
        ..MemorySettings::default()
    };
    ProjectMemoryView {
        project_id: P.into(),
        settings,
        enabled,
        ..ProjectMemoryView::default()
    }
}

fn fake(enabled: bool) -> Arc<Fake> {
    let fake = Fake::default();
    {
        let mut view = fake.view.lock().unwrap();
        view.capabilities = capabilities();
        view.projects = vec![project(enabled)];
    }
    Arc::new(fake)
}

struct H {
    ctx: egui::Context,
    panel: MemoryPanel,
    control: Arc<dyn MemoryControl>,
    /// The clock every frame sees.
    now: u64,
}

impl H {
    fn new(control: Arc<dyn MemoryControl>) -> Self {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut h = Self {
            ctx,
            panel: MemoryPanel::default(),
            control,
            now: NOW,
        };
        h.frame(vec![]);
        h.frame(vec![]);
        h
    }

    fn of(fake: &Arc<Fake>) -> Self {
        Self::new(Arc::clone(fake) as Arc<dyn MemoryControl>)
    }

    fn frame(&mut self, events: Vec<Event>) {
        let input = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 6000.0))),
            events,
            ..RawInput::default()
        };
        let (panel, control, now) = (&mut self.panel, &self.control, self.now);
        let mut output = self.ctx.run_ui(input, |ui| {
            section(ui, panel, control.as_ref(), P, now, 0);
        });
        output.textures_delta.clear();
    }

    fn rect(&self, key: &str) -> Rect {
        let found = self.panel.marks.rects.get(key).copied();
        assert!(
            found.is_some(),
            "{key} not drawn: {:?}",
            self.panel.marks.rects.keys().collect::<Vec<_>>()
        );
        found.unwrap()
    }

    fn click(&mut self, key: &str) {
        self.click_only(key);
        self.frame(vec![]);
    }

    /// Click, with no pass after the one that acts on it.
    fn click_only(&mut self, key: &str) {
        let at = self.rect(key).center();
        self.frame(vec![Event::PointerMoved(at)]);
        self.frame(vec![pointer(at, true)]);
        self.frame(vec![pointer(at, false)]);
    }

    /// Replace a field's text and press Enter.
    fn type_into(&mut self, key: &str, text: &str) {
        self.click(key);
        self.frame(vec![
            key_event(Key::A, Modifiers::COMMAND, true),
            key_event(Key::A, Modifiers::COMMAND, false),
        ]);
        self.frame(vec![Event::Text(text.to_owned())]);
        self.frame(vec![
            key_event(Key::Enter, Modifiers::NONE, true),
            key_event(Key::Enter, Modifiers::NONE, false),
        ]);
        self.frame(vec![]);
    }

    fn drawn(&self, key: &str) -> bool {
        self.panel.marks.rects.contains_key(key)
    }

    fn shows(&self, text: &str) -> bool {
        self.panel.marks.texts.iter().any(|t| t.contains(text))
    }

    /// Where the first line containing `text` was drawn, in drawing order.
    fn line(&self, text: &str) -> usize {
        let found = self.panel.marks.texts.iter().position(|t| t.contains(text));
        assert!(
            found.is_some(),
            "{text:?} not shown: {:#?}",
            self.panel.marks.texts
        );
        found.unwrap()
    }

    fn tab(&mut self, tab: Tab) {
        self.panel.tab = tab;
        self.frame(vec![]);
    }
}

fn pointer(pos: Pos2, pressed: bool) -> Event {
    Event::PointerButton {
        pos,
        button: PointerButton::Primary,
        pressed,
        modifiers: Modifiers::NONE,
    }
}

fn key_event(key: Key, modifiers: Modifiers, pressed: bool) -> Event {
    Event::Key {
        key,
        physical_key: None,
        pressed,
        repeat: false,
        modifiers,
    }
}

fn mid(n: u8) -> MemoryId {
    MemoryId::from_entropy([n; 5])
}

fn source(n: u64, kind: EvidenceKind) -> SourceRef {
    SourceRef {
        evidence: EvidenceId::new(n),
        kind,
    }
}

fn note(n: u8, kind: AnnotationKind, text: &str, about: Option<MemoryId>) -> Annotation {
    Annotation {
        id: AnnotationId::from_entropy([n; 5]),
        seq: u64::from(n),
        created_ms: NOW - 600_000,
        kind,
        target: about.map_or(
            AnnotationTarget::New {
                room: Room::Workshop,
            },
            AnnotationTarget::Memory,
        ),
        text: text.into(),
        scopes: vec![Scope::Project],
        sources: vec![source(40 + u64::from(n), EvidenceKind::UserStatement)],
        verification: Verification::Unverified,
        origin: AnnotationOrigin::Agent {
            terminal: "claude-2".into(),
        },
        memory_kind: None,
        standing: None,
        absorbed_in: None,
    }
}

fn memory_card(n: u8, summary: &str, open: Vec<Annotation>) -> MemoryCard {
    MemoryCard {
        id: mid(n),
        address: Address::parse(&format!("officina/voce-{n}")).unwrap(),
        kind: MemoryKind::Procedure,
        standing: Standing::Documented,
        summary: summary.into(),
        status: MemoryStatus::ToVerify,
        freshness: Freshness {
            last_confirmed_ms: Some(NOW - 2 * 3_600_000),
            anchors_changed: true,
            open_annotations: u32::try_from(open.len()).unwrap(),
        },
        scopes: vec![ScopeEntry::new(Scope::Project)],
        sources: vec![
            (EvidenceKind::UserStatement, 2),
            (EvidenceKind::CodeObservation, 1),
        ],
        open,
    }
}

// ---------- states of the whole section ----------

#[test]
fn without_an_engine_the_section_says_memory_is_not_available() {
    let h = H::new(Arc::new(NoMemory));
    assert!(h.shows("PROJECT MEMORY"));
    assert!(h.shows("not available in this build"));
    assert!(!h.drawn("mem-toggle"));
}

#[test]
fn a_project_not_in_the_view_yet_is_loading() {
    let fake = fake(false);
    fake.view.lock().unwrap().projects.clear();
    let h = H::of(&fake);
    assert!(h.shows("Loading the memory of this project"));
}

#[test]
fn memory_is_off_by_default_and_shows_no_tabs() {
    let fake = fake(false);
    let h = H::of(&fake);
    assert!(h.drawn("mem-toggle"));
    assert!(h.shows("Memory is off for this project."));
    assert!(!h.drawn("mem-tab:activity"));
    assert!(fake.calls().is_empty());
}

#[test]
fn turned_off_it_keeps_export_and_delete_even_with_nothing_published() {
    let fake = fake(false);
    fake.edit(|p| {
        p.open_annotations = 2;
        p.backlog.queued = 5;
    });
    let mut h = H::of(&fake);
    assert!(h.shows("version 0, 0 memories, 2 open notes, 5 queued events"));
    h.click("mem-export");
    h.click("mem-delete");
    h.click("mem-delete-confirm");
    assert_eq!(
        fake.calls(),
        vec![Call::Export(ExportFormat::Both), Call::Delete]
    );
}

// ---------- turning it on ----------

#[test]
fn turning_on_offers_build_or_empty_and_build_previews_the_plan() {
    let fake = fake(false);
    let mut h = H::of(&fake);
    h.click("mem-toggle");
    assert!(
        fake.calls().is_empty(),
        "nothing is saved before the choice"
    );
    assert!(h.shows("How should the memory start?"));
    assert!(h.drawn("mem-enable-build"));
    assert!(h.drawn("mem-enable-empty"));
    h.click("mem-enable-build");
    let on = MemorySettings {
        enabled: true,
        ..MemorySettings::default()
    };
    assert_eq!(
        fake.calls(),
        vec![Call::Settings(on), Call::Plan(BootstrapOptions::default())]
    );
    assert_eq!(h.panel.tab, Tab::Activity);
    assert!(h.shows("INITIAL BUILD"));
    assert!(
        !h.calls_started(&fake),
        "the build waits for the user's start"
    );
}

impl H {
    fn calls_started(&self, fake: &Fake) -> bool {
        fake.calls().iter().any(|c| matches!(c, Call::Bootstrap(_)))
    }
}

#[test]
fn starting_empty_saves_only_the_switch() {
    let fake = fake(false);
    let mut h = H::of(&fake);
    h.click("mem-toggle");
    h.click("mem-enable-empty");
    let calls = fake.calls();
    assert_eq!(calls.len(), 1);
    assert!(matches!(&calls[0], Call::Settings(s) if s.enabled));
    assert_eq!(h.panel.tab, Tab::Memories);
    assert!(h.drawn("mem-tab:memories"));
}

#[test]
fn cancelling_the_choice_leaves_memory_off() {
    let fake = fake(false);
    let mut h = H::of(&fake);
    h.click("mem-toggle");
    h.click("mem-enable-cancel");
    assert!(fake.calls().is_empty());
    assert!(!h.shows("How should the memory start?"));
    assert!(h.shows("Memory is off"));
}

#[test]
fn turning_off_saves_at_once_and_keeps_the_data() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.click("mem-toggle");
    let calls = fake.calls();
    assert!(matches!(&calls[..], [Call::Settings(s)] if !s.enabled));
    assert!(h.shows("the data stays"));
}

#[test]
fn a_refused_command_shows_its_reason() {
    let fake = fake(true);
    *fake.refuse.lock().unwrap() = Some("paused".into());
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    h.click("mem-dream-now");
    assert_eq!(fake.calls(), vec![Call::DreamNow]);
    assert!(h.shows("paused"));
}

// ---------- the two cycles ----------

fn saved(fake: &Fake) -> MemorySettings {
    match fake.calls().last() {
        Some(Call::Settings(s)) => s.clone(),
        other => panic!("no settings saved: {other:?}"),
    }
}

#[test]
fn development_numbers_reach_set_settings() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    h.type_into("mem-dev-group", "300");
    assert_eq!(saved(&fake).development.group_seconds, 300);
    h.type_into("mem-dev-runs", "45");
    let s = saved(&fake);
    assert_eq!(s.development.max_runs_per_hour, 45);
    assert_eq!(s.development.group_seconds, 300, "the first edit stands");
    h.type_into("mem-dev-input", "80,000");
    assert_eq!(saved(&fake).development.max_input_chars, 80_000);
}

#[test]
fn numbers_are_clamped_as_the_contract_does() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    h.type_into("mem-dev-attempts", "999");
    assert_eq!(saved(&fake).development.max_attempts, 20);
    h.type_into("mem-dream-minutes", "0");
    assert_eq!(saved(&fake).dream.max_minutes, 1);
    h.type_into("mem-dream-input", "10");
    assert_eq!(saved(&fake).dream.max_input_chars, MIN_INPUT_CHARS);
    h.type_into("mem-dev-input", "10");
    assert_eq!(saved(&fake).development.max_input_chars, MIN_INPUT_CHARS);
    // The hints name the contract's minimum, not a number of their own.
    let min = super::text::count(u64::from(MIN_INPUT_CHARS));
    assert!(h.shows(&format!("split, never cut ({min}–2,000,000)")));
    assert!(h.shows(&format!("split by room ({min}–5,000,000)")));
    h.type_into("mem-dream-min-new", "5");
    assert_eq!(saved(&fake).dream.min_new_items, 5);
}

#[test]
fn a_bad_number_or_time_is_refused_with_a_reason() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    h.type_into("mem-dev-group", "soon");
    assert!(fake.calls().is_empty());
    assert!(h.shows("Type a whole number from 0 to 3,600."));
    h.type_into("mem-dream-time", "25:00");
    assert!(fake.calls().is_empty());
    assert!(h.shows("HH:MM"));
}

#[test]
fn the_dream_schedule_reaches_set_settings() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    assert!(h.shows("nothing runs while Connecto is closed"));
    h.type_into("mem-dream-time", "03:15");
    assert_eq!(saved(&fake).dream.time, LocalTime::new(3, 15).unwrap());
    h.click("mem-dream-day:sat");
    assert!(saved(&fake).dream.days.contains(&Weekday::Sat));
    h.click("mem-dream-day:mon");
    let days = saved(&fake).dream.days;
    assert!(!days.contains(&Weekday::Mon));
    assert!(days.contains(&Weekday::Sat));
}

#[test]
fn models_and_efforts_come_from_the_capabilities() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    for key in [
        "mem-dev-model:haiku",
        "mem-dev-model:opus",
        "mem-dream-effort:high",
    ] {
        assert!(h.drawn(key), "{key}");
    }
    h.click("mem-dev-model:sonnet");
    assert_eq!(
        saved(&fake).development.choice,
        ModelChoice::new(Provider::Claude, "sonnet", "low")
    );
    h.click("mem-dream-effort:high");
    let s = saved(&fake);
    assert_eq!(
        s.dream.choice,
        ModelChoice::new(Provider::Claude, "sonnet", "high")
    );
    assert_eq!(s.development.choice.model, "sonnet", "cycles stay apart");
}

#[test]
fn an_unavailable_provider_shows_its_reason_and_cannot_be_picked() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    assert!(h.shows("Codex is unavailable: codex is not installed"));
    h.click("mem-dev-provider:codex");
    h.click("mem-dream-provider:codex");
    assert!(fake.calls().is_empty());
}

#[test]
fn an_available_provider_switches_with_its_own_words() {
    let fake = fake(true);
    fake.view.lock().unwrap().capabilities[1] = ProviderCapability {
        provider: Provider::Codex,
        capability: Capability::Available,
        models: vec!["gpt-5.1".into()],
        efforts: vec!["medium".into()],
    };
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    h.click("mem-dream-provider:codex");
    assert_eq!(
        saved(&fake).dream.choice,
        ModelChoice::new(Provider::Codex, "gpt-5.1", "medium")
    );
}

#[test]
fn a_chosen_provider_that_became_unavailable_is_flagged() {
    let fake = fake(true);
    fake.edit(|p| p.settings.dream.choice = ModelChoice::new(Provider::Codex, "gpt-5.1", "medium"));
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    assert!(h.shows("The chosen provider is unavailable (codex is not installed)"));
}

// ---------- the initial build ----------

fn plan(options: BootstrapOptions) -> BootstrapPlan {
    BootstrapPlan {
        options,
        coverage: vec![
            SourceCoverage {
                class: SourceClass::Documentation,
                analysed: 0,
                excluded: 2,
                remaining: 31,
            },
            SourceCoverage {
                class: SourceClass::Code,
                analysed: 0,
                excluded: 7,
                remaining: 412,
            },
        ],
        gaps: vec![CoverageGap::NoConversations],
        chars: 1_234_567,
        runs: 9,
    }
}

#[test]
fn the_build_needs_a_plan_preview_before_it_starts() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    assert!(h.shows("Not started"));
    h.click("mem-boot-start");
    assert!(fake.calls().is_empty(), "no start without a plan");
    h.click("mem-boot-plan");
    assert_eq!(fake.take(), vec![Call::Plan(BootstrapOptions::default())]);
    fake.edit(|p| p.bootstrap.planning = true);
    h.frame(vec![]);
    assert!(h.shows("Working out the plan"));
    fake.edit(|p| {
        p.bootstrap.planning = false;
        p.bootstrap.plan = Some(plan(BootstrapOptions::default()));
    });
    h.frame(vec![]);
    for text in [
        "Documentation and instructions",
        "Relevant code",
        "412",
        "No Connecto conversations for this project",
        "About 1,234,567 characters, in about 9 model runs.",
        "no reliable cost",
    ] {
        assert!(h.shows(text), "{text}");
    }
    assert!(
        !h.panel
            .marks
            .texts
            .iter()
            .any(|t| t.contains('$') || t.contains('€')),
        "no money anywhere"
    );
    h.click("mem-boot-start");
    assert_eq!(
        fake.calls(),
        vec![Call::Bootstrap(BootstrapOptions::default())]
    );
}

#[test]
fn changed_options_need_a_new_plan() {
    let fake = fake(true);
    fake.edit(|p| p.bootstrap.plan = Some(plan(BootstrapOptions::default())));
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    h.click("mem-boot-source:conversations");
    assert!(h.shows("The options changed since the last plan"));
    h.click("mem-boot-start");
    assert!(fake.calls().is_empty());
    h.type_into("mem-boot-exclude", "vendor/, *.lock");
    h.type_into("mem-boot-days", "30");
    h.click("mem-boot-plan");
    match fake.calls().last() {
        Some(Call::Plan(o)) => {
            assert!(!o.sources.contains(&SourceClass::Conversations));
            assert_eq!(o.exclude, vec!["vendor/".to_owned(), "*.lock".to_owned()]);
            assert_eq!(o.history_days, 30);
        }
        other => panic!("{other:?}"),
    }
}

#[test]
fn build_options_are_clamped_to_the_contract_ranges() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    let min = super::text::count(*BootstrapOptions::MAX_INPUT_CHARS.start());
    assert!(h.shows(&format!("characters read in total ({min}–100,000,000)")));
    h.type_into("mem-boot-days", "99999");
    h.type_into("mem-boot-minutes", "0");
    h.type_into("mem-boot-input", "5");
    h.click("mem-boot-plan");
    match fake.calls().last() {
        Some(Call::Plan(o)) => {
            assert_eq!(o.history_days, *BootstrapOptions::HISTORY_DAYS.end());
            assert_eq!(o.max_minutes, *BootstrapOptions::MAX_MINUTES.start());
            assert_eq!(
                o.max_input_chars,
                *BootstrapOptions::MAX_INPUT_CHARS.start()
            );
        }
        other => panic!("{other:?}"),
    }
}

const UNTRUSTED: &str = "claude has not trusted the folder of this project";

fn last_run(cycle: connecto_core::memory::Cycle, outcome: RunOutcome) -> CycleState {
    CycleState {
        cycle,
        running: false,
        last: Some(run(3, cycle, outcome)),
    }
}

#[test]
fn a_held_cycle_is_called_out_in_the_overview_with_what_to_do() {
    use connecto_core::memory::Cycle;
    let fake = fake(true);
    fake.edit(|p| {
        p.cycles = vec![
            last_run(
                Cycle::Development,
                RunOutcome::Held {
                    reason: UNTRUSTED.into(),
                },
            ),
            last_run(
                Cycle::Dream,
                RunOutcome::Failed {
                    reason: "the model timed out".into(),
                },
            ),
        ];
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    let at = h.line("NEEDS ATTENTION");
    assert!(at < h.line("0 memories"), "above everything else");
    let held = h.line(&format!("Development cycle, on hold: {UNTRUSTED}"));
    assert_eq!(h.line(super::HELD_HINT), held + 1, "the hint follows it");
    assert!(h.shows("Dream cycle, last run failed: the model timed out"));
    assert_eq!(
        h.panel
            .marks
            .texts
            .iter()
            .filter(|t| t.as_str() == super::HELD_HINT)
            .count(),
        1,
        "the hint is for the held run only"
    );
}

#[test]
fn nothing_needs_attention_when_runs_went_well() {
    use connecto_core::memory::Cycle;
    let fake = fake(true);
    fake.edit(|p| p.cycles = vec![last_run(Cycle::Development, RunOutcome::NoChange)]);
    let h = H::of(&fake);
    assert!(!h.shows("NEEDS ATTENTION"));
    assert!(!h.shows(super::HELD_HINT));
}

#[test]
fn a_held_or_failed_build_is_called_out_in_the_build_tab() {
    use connecto_core::memory::Cycle;
    let fake = fake(true);
    fake.edit(|p| {
        p.bootstrap.state = BootstrapState::Interrupted;
        p.cycles = vec![last_run(
            Cycle::Bootstrap,
            RunOutcome::Held {
                reason: UNTRUSTED.into(),
            },
        )];
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    assert!(h.shows(&format!("On hold: {UNTRUSTED}")));
    assert!(h.shows(super::HELD_HINT));
    assert!(h.drawn("mem-boot-resume"));
    h.tab(Tab::Activity);
    assert!(h.shows(&format!("Initial build, on hold: {UNTRUSTED}")));
    fake.edit(|p| {
        p.cycles.clear();
        p.bootstrap.state = BootstrapState::Failed {
            reason: "the store is locked".into(),
        };
    });
    h.tab(Tab::Activity);
    assert!(h.shows("Failed: the store is locked"));
    assert!(!h.shows(super::HELD_HINT));
    h.tab(Tab::Activity);
    assert!(h.shows("Initial build failed: the store is locked"));
}

#[test]
fn the_last_error_shows_on_every_tab() {
    let fake = fake(true);
    fake.edit(|p| p.last_error = Some(UNTRUSTED.into()));
    let mut h = H::of(&fake);
    for tab in [
        Tab::Activity,
        Tab::Activity,
        Tab::Settings,
        Tab::Activity,
        Tab::Memories,
    ] {
        h.tab(tab);
        assert!(h.shows(&format!("Last error: {UNTRUSTED}")), "{tab:?}");
    }
}

#[test]
fn a_running_build_shows_progress_and_can_be_cancelled() {
    let fake = fake(true);
    fake.edit(|p| {
        p.bootstrap.id = Some("b-1".into());
        p.bootstrap.state = BootstrapState::Running;
        p.bootstrap.packets_done = 3;
        p.bootstrap.packets_total = 10;
        p.bootstrap.started_ms = Some(NOW - 120_000);
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    assert!(h.shows("Running"));
    assert!(h.shows("3 of 10 packets"));
    assert!(h.shows("Initial build in progress"));
    assert!(!h.drawn("mem-boot-start"), "no second start while it runs");
    h.click("mem-boot-cancel");
    assert_eq!(fake.calls(), vec![Call::CancelBootstrap]);
}

#[test]
fn a_queued_build_says_so() {
    let fake = fake(true);
    fake.edit(|p| p.bootstrap.state = BootstrapState::Queued);
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    assert!(h.shows("Queued"));
    assert!(h.drawn("mem-boot-cancel"));
}

#[test]
fn interrupted_and_failed_builds_resume() {
    for (state, text) in [
        (BootstrapState::Interrupted, "Interrupted"),
        (
            BootstrapState::Failed {
                reason: "the model timed out".into(),
            },
            "Failed: the model timed out",
        ),
    ] {
        let fake = fake(true);
        fake.edit(|p| {
            p.bootstrap.state = state.clone();
            p.bootstrap.packets_done = 4;
            p.bootstrap.packets_total = 9;
        });
        let mut h = H::of(&fake);
        h.tab(Tab::Activity);
        assert!(h.shows(text), "{text}");
        assert!(h.shows("4 of 9 packets"));
        h.click("mem-boot-resume");
        assert_eq!(fake.calls(), vec![Call::Resume], "{text}");
    }
}

#[test]
fn a_completed_build_shows_coverage_and_does_not_claim_completeness() {
    let fake = fake(true);
    fake.edit(|p| {
        p.version = 1;
        p.bootstrap.state = BootstrapState::Completed;
        p.bootstrap.coverage = vec![SourceCoverage {
            class: SourceClass::GitHistory,
            analysed: 120,
            excluded: 4,
            remaining: 30,
        }];
        p.bootstrap.gaps = vec![CoverageGap::InputLimit, CoverageGap::HistoryWindow];
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    for text in [
        "Completed",
        "SOURCE COVERAGE",
        "Recent history",
        "120",
        "Input limit reached",
        "History limited to the chosen window",
        super::build::NOT_EXHAUSTIVE,
    ] {
        assert!(h.shows(text), "{text}");
    }
    assert!(h.shows("Analysed") && h.shows("Excluded") && h.shows("Remaining"));
    assert!(
        h.drawn("mem-boot-start"),
        "a completed build can reconcile again"
    );
}

// ---------- overview: status apart from freshness and backlog ----------

#[test]
fn freshness_and_backlog_are_shown_apart_from_status() {
    let fake = fake(true);
    fake.edit(|p| {
        p.version = 7;
        p.totals = StatusCounts {
            candidate: 3,
            verified: 5,
            to_verify: 1,
            superseded: 0,
            conflict: 2,
        };
        p.open_annotations = 4;
        p.open_contradictions = 1;
        p.considered_through_ms = Some(NOW - 5 * 60_000);
        p.backlog.queued = 12;
        p.backlog.oldest_queued_ms = Some(NOW - 3 * 3_600_000);
        p.backlog.failed_batches = 2;
        p.backlog.dropped_on_full = 6;
        p.backlog.last_failure = Some("the model did not answer".into());
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    let status = h.line("Version 7");
    let fresh = h.line("FRESHNESS AND BACKLOG");
    assert!(status < fresh);
    for text in [
        "Version 7",
        "11 memories",
        "1 to verify",
        "2 in conflict",
        "4 open notes",
        "1 open contradictions",
    ] {
        let at = h.line(text);
        assert!(status <= at && at < fresh, "{text} belongs to status");
    }
    for text in [
        "Considered work through 5 min ago",
        "12 events waiting · oldest 3 h old",
        "2 batches failed",
        "6 events lost",
        "Last failure: the model did not answer",
        "This is not its reliability",
    ] {
        assert!(h.line(text) > fresh, "{text} belongs to freshness");
    }
}

#[test]
fn dream_now_and_export_reach_the_control() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    h.click("mem-dream-now");
    h.tab(Tab::Settings);
    h.click("mem-export:json");
    h.click("mem-export");
    assert_eq!(
        fake.calls(),
        vec![Call::DreamNow, Call::Export(ExportFormat::Json)]
    );
}

#[test]
fn dream_now_waits_for_a_running_build() {
    let fake = fake(true);
    fake.edit(|p| p.bootstrap.state = BootstrapState::Running);
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    h.click("mem-dream-now");
    assert!(fake.calls().is_empty());
}

#[test]
fn the_export_result_shows_its_paths_or_its_error() {
    let fake = fake(true);
    fake.edit(|p| {
        p.last_export = Some(ExportOutcome {
            format: ExportFormat::Both,
            at_ms: NOW - 60_000,
            result: Ok(vec![
                PathBuf::from("/data/p1/memory/exports/memory.json"),
                PathBuf::from("/data/p1/memory/exports/markdown"),
            ]),
        });
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    assert!(h.shows("Exported JSON and Markdown"));
    assert!(h.shows("/data/p1/memory/exports/memory.json"));
    assert!(h.shows("/data/p1/memory/exports/markdown"));
    fake.edit(|p| {
        p.last_export = Some(ExportOutcome {
            format: ExportFormat::Json,
            at_ms: NOW,
            result: Err("disk full".into()),
        });
    });
    h.frame(vec![]);
    assert!(h.shows("Export failed just now: disk full"));
}

#[test]
fn delete_asks_first() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    h.click("mem-delete");
    assert!(fake.calls().is_empty());
    assert!(h.shows("Delete this project's memory?"));
    h.click("mem-delete-keep");
    assert!(!h.shows("Delete this project's memory?"));
    h.click("mem-delete");
    h.click("mem-delete-confirm");
    assert_eq!(fake.calls(), vec![Call::Delete]);
    assert!(!h.shows("Delete this project's memory?"));
}

#[test]
fn the_last_error_is_shown() {
    let fake = fake(true);
    fake.edit(|p| p.last_error = Some("the store is locked".into()));
    let h = H::of(&fake);
    assert!(h.shows("Last error: the store is locked"));
}

// ---------- runs and versions ----------

fn run(id: u64, cycle: connecto_core::memory::Cycle, outcome: RunOutcome) -> RunEntry {
    RunEntry {
        id,
        cycle,
        choice: ModelChoice::new(Provider::Claude, "haiku", "low"),
        started_ms: NOW - 3_600_000,
        duration_ms: (!outcome.is_running()).then_some(185_000),
        chars_in: 48_210,
        chars_out: 1_904,
        outcome,
        recovers: Vec::new(),
    }
}

#[test]
fn the_run_list_shows_every_outcome_and_measures_no_money() {
    use connecto_core::memory::Cycle;
    let fake = fake(true);
    let date = |s: &str| LocalDate::parse(s).unwrap();
    fake.edit(|p| {
        let mut catch_up = run(
            9,
            Cycle::Dream,
            RunOutcome::Published {
                version: 4,
                applied: 12,
                rejected: 1,
            },
        );
        catch_up.recovers = vec![date("2026-11-10"), date("2026-11-11")];
        p.runs = Arc::new(vec![
            run(10, Cycle::Development, RunOutcome::Running),
            catch_up,
            run(
                8,
                Cycle::Development,
                RunOutcome::Annotated {
                    written: 3,
                    rejected: 0,
                },
            ),
            run(7, Cycle::Development, RunOutcome::NoChange),
            run(6, Cycle::Dream, RunOutcome::HeldByPause),
            run(
                11,
                Cycle::Dream,
                RunOutcome::Held {
                    reason: "codex has not trusted the folder".into(),
                },
            ),
            run(5, Cycle::Dream, RunOutcome::Cancelled),
            run(4, Cycle::Bootstrap, RunOutcome::Interrupted),
            run(
                3,
                Cycle::Development,
                RunOutcome::Failed {
                    reason: "API Error: overloaded".into(),
                },
            ),
        ]);
        p.skipped = vec![
            SkippedRun {
                date: date("2026-11-10"),
                recovered_by: Some(9),
            },
            SkippedRun {
                date: date("2026-11-12"),
                recovered_by: None,
            },
        ];
        p.next_dream_ms = Some(NOW + 86_400_000);
        p.cycles = vec![CycleState {
            cycle: Cycle::Development,
            running: true,
            last: None,
        }];
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    for text in [
        "Next dream tomorrow 10:00",
        "Nothing runs while Connecto is closed",
        "Development cycle running now",
        "#10 Development",
        "Running",
        "#9 Dream",
        "Published version 4 · 12 operations applied · 1 turned down by the validator",
        "Also stands for the skipped dates 2026-11-10, 2026-11-11",
        "Wrote 3 notes",
        "Nothing to change",
        "Held by Pause",
        "On hold: codex has not trusted the folder (no attempt used; it resumes once that is solved)",
        "Cancelled",
        "Interrupted",
        "Failed: API Error: overloaded",
        "48,210 characters in · 1,904 characters out",
        "took 3 min 5 s",
        "Claude haiku (low)",
        "2026-11-10 · recovered by run #9",
        "2026-11-12 · not recovered yet",
    ] {
        assert!(h.shows(text), "{text}");
    }
    assert!(
        !h.panel
            .marks
            .texts
            .iter()
            .any(|t| t.contains('$') || t.contains("cost")),
        "no money anywhere"
    );
}

#[test]
fn undoing_a_version_asks_first() {
    use connecto_core::memory::Cycle;
    let fake = fake(true);
    fake.edit(|p| {
        p.version = 3;
        p.versions = Arc::new(vec![
            VersionEntry {
                version: 3,
                published_ms: NOW - 60_000,
                cycle: Cycle::Dream,
                operations: 5,
                reverts: None,
            },
            VersionEntry {
                version: 2,
                published_ms: NOW - 86_400_000,
                cycle: Cycle::Bootstrap,
                operations: 40,
                reverts: None,
            },
        ]);
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    assert!(h.shows("Version 3") && h.shows("Version 2"));
    h.click("mem-revert:3");
    assert!(fake.calls().is_empty());
    assert!(h.shows("Undo version 3?"));
    h.click("mem-revert-keep");
    assert!(!h.shows("Undo version 3?"));
    h.click("mem-revert:2");
    h.click("mem-revert-confirm");
    assert_eq!(fake.calls(), vec![Call::Revert(2)]);
}

// ---------- the memory itself ----------

fn contradiction(about: &MemoryId) -> Annotation {
    let mut a = note(
        7,
        AnnotationKind::Contradiction,
        "The release is signed in CI now, not locally.",
        Some(about.clone()),
    );
    a.origin = AnnotationOrigin::User;
    a
}

fn cards() -> Vec<MemoryCard> {
    let first = mid(1);
    vec![
        memory_card(
            1,
            "Releases are signed on the maintainer's Mac.",
            vec![
                note(
                    3,
                    AnnotationKind::Integration,
                    "Also notarize the DMG.",
                    Some(first.clone()),
                ),
                contradiction(&first),
            ],
        ),
        {
            let mut card = memory_card(2, "Run cargo test before every push.", Vec::new());
            card.scopes.push(ScopeEntry {
                scope: Scope::Branch("ci-old".into()),
                gone: true,
            });
            card
        },
    ]
}

#[test]
fn cards_say_when_a_branch_is_no_longer_present() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-room:officina");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    h.frame(vec![]);
    h.type_into("mem-search-field", "cargo");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Search(vec![SearchHit::Memory(
            cards().remove(1),
        )])),
    );
    h.frame(vec![]);
    assert!(h.shows("1 results"));
}

/// Open the room index of the workshop and answer it.
fn workshop(fake: &Arc<Fake>) -> H {
    fake.edit(|p| {
        p.version = 2;
        p.totals.to_verify = 2;
        p.rooms = vec![RoomCount {
            room: Room::Workshop,
            counts: StatusCounts {
                to_verify: 2,
                ..StatusCounts::default()
            },
        }];
    });
    let mut h = H::of(fake);
    h.tab(Tab::Memories);
    // The default view reads each room once: answer those, and start clean.
    h.frame(vec![]);
    answer_room_reads(fake, None);
    h.frame(vec![]);
    fake.take();
    h
}

#[test]
fn rooms_show_their_counts_and_open_an_index_through_a_read() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    assert!(h.drawn("mem-room:officina"));
    assert!(h.drawn("mem-room:decisioni"));
    assert!(!h.drawn("mem-room:ingresso"), "the entrance holds nothing");
    h.click("mem-room:officina");
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Room(Room::Workshop))]
    );
    assert!(h.shows("Loading…"));
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    h.frame(vec![]);
    assert!(h.shows("Workshop · 2 memories"));
    assert!(h.shows("Releases are signed"));
}

/// A palace with memories in three rooms, on the Memories tab.
fn palace_view(fake: &Arc<Fake>) {
    fake.edit(|p| {
        p.version = 3;
        p.totals = StatusCounts {
            candidate: 1,
            verified: 2,
            to_verify: 1,
            superseded: 1,
            conflict: 0,
        };
        p.rooms = vec![
            RoomCount {
                room: Room::Architecture,
                counts: StatusCounts {
                    candidate: 1,
                    ..StatusCounts::default()
                },
            },
            RoomCount {
                room: Room::Decisions,
                counts: StatusCounts {
                    verified: 2,
                    to_verify: 1,
                    ..StatusCounts::default()
                },
            },
            RoomCount {
                room: Room::Archive,
                counts: StatusCounts {
                    superseded: 1,
                    ..StatusCounts::default()
                },
            },
        ];
    });
}

/// The palace, on the Memories tab, with its default view answered.
fn palace(fake: &Arc<Fake>) -> H {
    palace_view(fake);
    let mut h = H::of(fake);
    h.tab(Tab::Memories);
    // The default view reads each room once: answer those, and start clean.
    h.frame(vec![]);
    answer_room_reads(fake, None);
    h.frame(vec![]);
    fake.take();
    h
}

#[test]
fn with_an_empty_search_the_palace_shows_every_room_with_what_it_holds() {
    let fake = fake(true);
    let h = palace(&fake);
    assert!(h.drawn("mem-search-field"));
    for room in [
        "architettura",
        "officina",
        "decisioni",
        "problemi",
        "lavori-aperti",
        "convenzioni",
        "archivio",
    ] {
        assert!(h.drawn(&format!("mem-room:{room}")), "{room}");
    }
    assert!(
        !h.drawn("mem-room:ingresso"),
        "the entrance has no room to open"
    );
    assert!(
        h.shows("A memory of Architecture"),
        "the default view lists the palace"
    );
    assert!(!h.drawn("mem-back"), "no list is on show");
    assert!(
        only_reads(&fake.calls())
            .iter()
            .all(|r| matches!(r, ReadRequest::Room(_))),
        "the default view only reads rooms"
    );
}

#[test]
fn a_room_lists_its_memories_and_back_returns_to_the_rooms() {
    let fake = fake(true);
    let mut h = palace(&fake);
    h.click("mem-room:decisioni");
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Room(Room::Decisions))]
    );
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    h.frame(vec![]);
    assert!(h.shows("Decisions · 2 memories"));
    assert!(h.shows("Choices already made, with the reasons behind them."));
    assert!(h.shows("Releases are signed"));
    assert!(h.drawn("mem-rooms"));
    h.click("mem-rooms");
    assert!(h.shows("A memory of Architecture"));
    assert!(h.panel.browse.list.is_none());
    assert_eq!(fake.calls().len(), 1, "going back reads nothing");
}

#[test]
fn a_search_filters_the_palace_and_shows_its_results_by_first_line() {
    let fake = fake(true);
    let mut h = palace(&fake);
    h.type_into("mem-search-field", "release");
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Search {
            query: "release".into(),
            room: None,
            scope: None,
            all: false,
        })]
    );
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Search(vec![SearchHit::Memory(
            cards().remove(0),
        )])),
    );
    h.frame(vec![]);
    assert!(h.shows("1 results for “release”"));
    assert!(h.shows("Releases are signed on the maintainer's Mac."));
    assert!(h.drawn("mem-search-clear"));
}

#[test]
fn an_empty_result_says_so_and_offers_to_clear_the_search() {
    let fake = fake(true);
    let mut h = palace(&fake);
    h.type_into("mem-search-field", "kubernetes");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Search(Vec::new())),
    );
    h.frame(vec![]);
    assert!(h.shows("No memories match “kubernetes”."));
    h.click("mem-clear-search-empty");
    assert!(h.panel.browse.query.is_empty());
    assert!(h.panel.browse.list.is_none());
    assert!(h.drawn("mem-room:decisioni"), "back to the rooms");
    assert_eq!(fake.calls().len(), 1, "clearing reads nothing");
}

#[test]
fn clear_search_returns_to_the_rooms() {
    let fake = fake(true);
    let mut h = palace(&fake);
    h.type_into("mem-search-field", "test");
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Search {
            query: "test".into(),
            room: None,
            scope: None,
            all: false,
        })]
    );
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Search(Vec::new())),
    );
    h.frame(vec![]);
    h.click("mem-search-clear");
    assert!(h.panel.browse.query.is_empty());
    assert!(h.drawn("mem-room:decisioni"));
    assert!(h.shows("A memory of Architecture"));
}

#[test]
fn a_failed_read_says_why_and_can_be_asked_again() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-room:officina");
    fake.settle_read(
        fake.last_read(),
        ReadState::Failed("the store is busy".into()),
    );
    h.frame(vec![]);
    assert!(h.shows("Could not read the memory: the store is busy"));
    h.click("mem-read-again");
    assert_eq!(fake.calls().len(), 2);
    assert!(h.shows("Loading…"));
}

#[test]
fn a_read_that_vanished_is_asked_for_again_once_then_by_hand() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-room:officina");
    fake.take();
    // Evicted while pending: asked for again by itself, once.
    fake.edit(|p| p.reads.clear());
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(
        fake.take(),
        vec![Call::Read(ReadRequest::Room(Room::Workshop))]
    );
    assert!(h.shows("Loading…"));
    // Evicted again: no loop; the user is offered to ask.
    fake.edit(|p| p.reads.clear());
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert_eq!(reads(&fake), 0);
    assert!(h.shows("no longer kept"));
    h.click("mem-read-again");
    assert_eq!(reads(&fake), 1);
    // Once an answer is ready, a later eviction is again recovered by itself.
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    h.frame(vec![]);
    fake.take();
    fake.edit(|p| p.reads.clear());
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(reads(&fake), 1);
}

#[test]
fn the_list_read_evicted_behind_an_open_memory_comes_back_on_back() {
    let fake = fake(true);
    let mut h = opened(&fake);
    let detail_read = fake.last_read();
    fake.edit(|p| p.reads.retain(|r| r.id == detail_read));
    h.frame(vec![]);
    assert_eq!(reads(&fake), 0, "the list is not on show");
    h.click("mem-back");
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Room(Room::Workshop))]
    );
}

/// The contradiction is drawn after its memory's summary and before anything
/// else about it, and before the next memory.
/// On the reading page the contradiction sits under the memory's own text.
fn assert_contradiction_on_page(h: &H) {
    let summary = h.line("Releases are signed");
    let c = h.line("The release is signed in CI now");
    assert!(summary < c, "under the memory's text");
    assert!(h.shows("source: e-47 (User statement)"));
}

fn assert_contradiction_under(h: &H, summary: &str, next: Option<&str>) {
    // A list row: its title, its muted line, then the contradiction under it.
    let at = h.line(summary);
    let c = h.line("Contradicted: The release is signed in CI now");
    assert_eq!(c - 2, at, "right under its memory");
    if let Some(next) = next {
        assert!(c < h.line(next));
    }
}

#[test]
fn a_contradiction_sits_right_under_its_memory_in_the_room_index() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-room:officina");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    h.frame(vec![]);
    assert_contradiction_under(&h, "Releases are signed", Some("Run cargo test"));
}

#[test]
fn standing_status_and_freshness_are_drawn_apart() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-room:officina");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    h.frame(vec![]);
    // One muted line: kind, standing and status in words, then freshness.
    assert!(h.shows("Procedure · Documented · To verify"));
    assert!(h.shows("confirmed 2 h ago"));
}

#[test]
fn search_sends_its_filters_and_shows_contradictions_under_hits() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-filters");
    h.click("mem-search-room:officina");
    h.type_into("mem-search-scope", "branch:main");
    h.click("mem-search-all");
    h.type_into("mem-search-field", "release");
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Search {
            query: "release".into(),
            room: Some(Room::Workshop),
            scope: Some(Scope::Branch("main".into())),
            all: true,
        })]
    );
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Search(vec![
            SearchHit::Memory(cards().remove(0)),
            SearchHit::Annotation(note(9, AnnotationKind::Task, "Automate the release.", None)),
        ])),
    );
    h.frame(vec![]);
    assert!(h.shows("2 results"));
    assert_contradiction_under(&h, "Releases are signed", Some("Automate the release."));
    assert!(h.shows("new note for Workshop"));
}

#[test]
fn a_bad_scope_or_empty_search_is_refused_before_any_read() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-search");
    assert!(h.shows("Type something to search for."));
    h.click("mem-filters");
    h.type_into("mem-search-scope", "repo:x");
    h.type_into("mem-search-field", "release");
    assert!(h.shows("Scope:"));
    assert!(fake.calls().is_empty());
}

fn detail() -> MemoryDetail {
    let id = mid(1);
    let memory = Memory {
        id: id.clone(),
        address: Address::parse("officina/voce-1").unwrap(),
        kind: MemoryKind::Decision,
        standing: Standing::Proposed,
        summary: "Releases are signed on the maintainer's Mac.".into(),
        details: "Use the Developer ID certificate, then notarize.".into(),
        scopes: vec![
            ScopeEntry::new(Scope::Project),
            ScopeEntry {
                scope: Scope::Branch("release-old".into()),
                gone: true,
            },
        ],
        status: MemoryStatus::Conflict,
        freshness: Freshness {
            last_confirmed_ms: None,
            anchors_changed: true,
            open_annotations: 1,
        },
        anchors: vec![Anchor::new("scripts/release.sh", &"ab".repeat(20)).unwrap()],
        sources: vec![
            source(1, EvidenceKind::UserStatement),
            source(2, EvidenceKind::UserStatement),
            source(3, EvidenceKind::AgentClaim),
        ],
        relations: vec![Relation {
            kind: RelationKind::Supersedes,
            target: mid(5),
        }],
        created_ms: NOW - 86_400_000,
        updated_ms: NOW - 3_600_000,
        version: 2,
    };
    let mut absorbed = note(
        4,
        AnnotationKind::Correction,
        "Old note, merged.",
        Some(id.clone()),
    );
    absorbed.absorbed_in = Some(2);
    MemoryDetail {
        memory,
        annotations: vec![contradiction(&id), absorbed],
        evidence: vec![Evidence {
            id: EvidenceId::new(1),
            kind: EvidenceKind::UserStatement,
            project_id: P.into(),
            created_ms: NOW - 7_200_000,
            terminal: Some("claude-1".into()),
            tool_session: None,
            turn: None,
            branch: Some("main".into()),
            head: None,
            log: None,
            files: Vec::new(),
            excerpt: Redacted::assume("we sign on my Mac".into()),
            exposed_to: Vec::new(),
        }],
        redirected_from: None,
        superseded_by: Vec::new(),
    }
}

#[test]
fn opening_a_memory_shows_it_whole_with_its_contradiction_first() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-room:officina");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    h.frame(vec![]);
    let id = mid(1);
    h.click(&format!("mem-open:{id}"));
    assert_eq!(
        fake.calls().last(),
        Some(&Call::Read(ReadRequest::Open(
            connecto_core::memory::Lookup::Memory(id)
        )))
    );
    assert!(h.shows("Loading…"));
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(detail()))),
    );
    h.frame(vec![]);
    assert_contradiction_on_page(&h);
    for text in [
        "Decision",
        "Proposed",
        "Conflict",
        "never confirmed",
        "Use the Developer ID certificate",
        "branch release-old (branch no longer present)",
        "scripts/release.sh @ abababab",
        "An anchored file changed since it was observed",
        "Supersedes m-",
        "User statement (2)",
        "Agent claim (1)",
        "e-1 · 2 h ago · prompt typed in claude-1 · branch main",
        "we sign on my Mac",
        "ABSORBED NOTES",
        "absorbed in version 2",
    ] {
        assert!(h.shows(text), "{text}");
    }
    h.click("mem-back");
    assert!(h.shows("Workshop · 2 memories"));
}

#[test]
fn the_empty_memory_says_how_it_fills() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Memories);
    assert!(h.shows("The palace has no memories yet."));
    assert!(!h.drawn("mem-room:officina"), "no rooms to open yet");
    h.click("mem-go-build");
    assert_eq!(h.panel.tab, Tab::Activity);
}

#[test]
fn while_the_build_runs_the_memory_is_marked_incomplete() {
    let fake = fake(true);
    fake.edit(|p| p.bootstrap.state = BootstrapState::Running);
    let mut h = H::of(&fake);
    h.tab(Tab::Memories);
    assert!(h.shows("Initial build in progress: the memory is not complete yet"));
}

// ---------- the hook in Project Settings ----------

#[test]
fn project_settings_carry_the_memory_section() {
    use crate::forms::Out;
    use crate::project_settings::{self, ProjectSettingsInput, ProjectSettingsSheet, ProjectTab};
    let fake = fake(false);
    let control: Arc<dyn MemoryControl> = Arc::clone(&fake) as Arc<dyn MemoryControl>;
    let packages = connecto_core::PackagesView {
        projects: vec![connecto_core::ProjectPackagesView {
            project_id: P.into(),
            ..connecto_core::ProjectPackagesView::default()
        }],
        ..connecto_core::PackagesView::default()
    };
    let ctx = egui::Context::default();
    let mut sheet = ProjectSettingsSheet::new(P.into());
    let mut out = Out::default();
    for _ in 0..2 {
        let input = ProjectSettingsInput {
            view: &packages,
            project_name: "Quill",
            pending: None,
            now_ms: NOW,
            utc_offset_s: 0,
            memory: control.as_ref(),
            tab: ProjectTab::Memory,
            agents: &connecto_core::NoAgents,
            tokens: &connecto_core::NoTokens,
            functional_memory: &connecto_core::functional_memory::NoFunctionalMemory,
            start: &[],
            ops: &connecto_core::ops::NoOps,
            remote: false,
        };
        let raw = RawInput {
            screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 4000.0))),
            ..RawInput::default()
        };
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 4000.0));
        let mut output = ctx.run_ui(raw, |ui| {
            project_settings::sheet(ui.ctx(), area, &mut sheet, &input, &mut out);
        });
        output.textures_delta.clear();
    }
    assert!(
        sheet
            .memory
            .marks
            .texts
            .iter()
            .any(|t| t == "PROJECT MEMORY")
    );
    assert!(sheet.memory.marks.rects.contains_key("mem-toggle"));
}

/// Project Settings driven frame by frame, with its own marks and actions.
struct Settings {
    ctx: egui::Context,
    sheet: crate::project_settings::ProjectSettingsSheet,
    control: Arc<dyn MemoryControl>,
    packages: connecto_core::PackagesView,
    marks: crate::forms::Marks,
    /// The section on show, kept as the app keeps it across sheets.
    tab: crate::project_settings::ProjectTab,
    /// Functional memory, and whether the project is another machine's.
    functional_memory: Arc<dyn connecto_core::functional_memory::FunctionalMemoryControl>,
    remote: bool,
    /// When the last frame asked to be drawn again.
    repaint: std::time::Duration,
}

impl Settings {
    fn new(fake: &Arc<Fake>) -> Self {
        let ctx = egui::Context::default();
        crate::theme::apply(&ctx);
        let mut s = Self {
            ctx,
            sheet: crate::project_settings::ProjectSettingsSheet::new(P.into()),
            control: Arc::clone(fake) as Arc<dyn MemoryControl>,
            packages: connecto_core::PackagesView {
                projects: vec![connecto_core::ProjectPackagesView {
                    project_id: P.into(),
                    ..connecto_core::ProjectPackagesView::default()
                }],
                ..connecto_core::PackagesView::default()
            },
            marks: crate::forms::Marks::default(),
            tab: crate::project_settings::ProjectTab::default(),
            functional_memory: Arc::new(connecto_core::functional_memory::NoFunctionalMemory),
            remote: false,
            repaint: std::time::Duration::MAX,
        };
        s.frame(vec![]);
        s.frame(vec![]);
        s
    }

    fn frame(&mut self, events: Vec<Event>) -> Vec<crate::project_settings::ProjectSettingsAction> {
        use crate::forms::Out;
        use crate::project_settings::{self, ProjectSettingsInput};
        let input = ProjectSettingsInput {
            view: &self.packages,
            project_name: "Quill",
            pending: None,
            now_ms: NOW,
            utc_offset_s: 0,
            memory: self.control.as_ref(),
            tab: self.tab,
            agents: &connecto_core::NoAgents,
            tokens: &connecto_core::NoTokens,
            functional_memory: self.functional_memory.as_ref(),
            start: &[],
            ops: &connecto_core::ops::NoOps,
            remote: self.remote,
        };
        let area = Rect::from_min_size(Pos2::ZERO, Vec2::new(1200.0, 4000.0));
        let raw = RawInput {
            screen_rect: Some(area),
            events,
            ..RawInput::default()
        };
        let mut out = Out::default();
        // The memory section keeps its own marks: start each pass without any.
        self.sheet.memory.marks = crate::forms::Marks::default();
        let (ctx, sheet) = (&self.ctx, &mut self.sheet);
        let mut output = ctx.run_ui(raw, |ui| {
            project_settings::sheet(ui.ctx(), area, sheet, &input, &mut out);
        });
        output.textures_delta.clear();
        self.repaint = output
            .viewport_output
            .get(&egui::ViewportId::ROOT)
            .map_or(std::time::Duration::MAX, |v| v.repaint_delay);
        self.marks = std::mem::take(&mut out.marks);
        for action in &out.actions {
            if let crate::project_settings::ProjectSettingsAction::SelectTab(tab) = action {
                self.tab = *tab;
            }
        }
        out.actions
    }

    fn drawn(&self, key: &str) -> bool {
        self.marks.rects.contains_key(key) || self.sheet.memory.marks.rects.contains_key(key)
    }

    /// Click a control and return what the clicks asked for.
    fn click(&mut self, key: &str) -> Vec<crate::project_settings::ProjectSettingsAction> {
        let found = self
            .marks
            .rects
            .get(key)
            .or_else(|| self.sheet.memory.marks.rects.get(key))
            .copied();
        assert!(found.is_some(), "{key} not drawn");
        let at = found.unwrap().center();
        let mut actions = self.frame(vec![Event::PointerMoved(at)]);
        actions.extend(self.frame(vec![pointer(at, true)]));
        actions.extend(self.frame(vec![pointer(at, false)]));
        actions.extend(self.frame(vec![]));
        actions
    }
}

/// Leaving Project Memory suspends a bound draft, with its text and without
/// its confirmation, until it is resumed on the Memories tab.
#[test]
fn leaving_project_memory_suspends_a_bound_draft_until_it_is_resumed() {
    use super::statement::{Bound, Composer, Draft};
    let fake = fake(true);
    let mut s = Settings::new(&fake);
    s.click("ptab-memory");
    let d = detail();
    let mut draft = Composer::new(
        UserStatementKind::Correct,
        Some(Bound::memory(&d.memory)),
        "Correct this",
    );
    draft.text = "the corrected text".into();
    draft.confirming = true;
    s.sheet.memory.composer = Some(draft);
    s.click("ptab-tools");
    let c = s.sheet.memory.composer.as_ref().expect("the draft is kept");
    assert!(
        matches!(c.draft, Draft::Suspended { .. }),
        "suspended on leaving"
    );
    assert!(!c.confirming, "the confirmation is dropped");
    assert_eq!(c.text, "the corrected text", "the text is kept");
    s.click("ptab-memory");
    let c = s.sheet.memory.composer.as_ref().expect("still kept");
    assert!(
        matches!(c.draft, Draft::Suspended { .. }),
        "coming back does not resume it by itself"
    );
}

#[test]
fn project_settings_has_top_level_tabs_and_opens_on_packages() {
    use crate::project_settings::ProjectTab;
    let fake = fake(true);
    let mut s = Settings::new(&fake);
    for tab in ProjectTab::ALL {
        assert!(s.drawn(tab.key()), "{}", tab.key());
    }
    assert!(s.drawn("use-default"), "packages on show first");
    assert!(!s.drawn("mem-toggle"), "memory waits for its tab");
    assert!(!s.drawn("mem-tab:activity"), "nor its own tabs");
    s.click("ptab-tools");
    assert!(s.drawn("tools:Claude"));
    assert!(!s.drawn("use-default"));
    s.click("ptab-status");
    assert!(s.drawn("refresh"));
    assert!(!s.drawn("tools:Claude"));
    s.click("ptab-memory");
    assert!(s.drawn("mem-toggle"));
    assert!(!s.drawn("refresh"));
    // Its own tabs sit one level down, under the Project Memory tab.
    assert!(s.drawn("mem-tab:activity"));
    assert!(s.drawn("mem-tab:memories"));
    s.click("mem-tab:memories");
    assert!(s.drawn("mem-search-field"));
    assert!(s.drawn("ptab-memory"), "the top level stays");
}

#[test]
fn the_top_level_tabs_are_in_their_order() {
    use crate::project_settings::ProjectTab;
    let labels: Vec<&str> = ProjectTab::ALL.iter().map(|t| t.label()).collect();
    assert_eq!(
        labels,
        [
            "Packages",
            "Tools",
            "Status",
            "Project Memory",
            "Operations",
            "Tokens"
        ]
    );
}

#[test]
fn a_choice_on_the_tools_tab_saves_the_tools() {
    let fake = fake(true);
    let mut s = Settings::new(&fake);
    let looked = s.click("ptab-tools");
    assert!(
        looked.iter().all(|a| matches!(
            a,
            crate::project_settings::ProjectSettingsAction::SelectTab(_)
        )),
        "switching tabs saves nothing"
    );
    let actions = s.click("tools:Codex");
    assert!(
        actions.iter().any(|a| matches!(
            a,
            crate::project_settings::ProjectSettingsAction::Save(settings)
                if !settings.tools.allows(connecto_core::ToolKind::Codex)
                    && settings.tools.len() == 4
        )),
        "the tools choice is saved"
    );
}

// ---------- answers on show follow the memory ----------

/// A memory opened and answered without any note, then left on show.
fn opened(fake: &Arc<Fake>) -> H {
    let mut h = workshop(fake);
    h.click("mem-room:officina");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    h.frame(vec![]);
    h.click(&format!("mem-open:{}", mid(1)));
    let mut quiet = detail();
    quiet.annotations.clear();
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(quiet))),
    );
    h.frame(vec![]);
    h.frame(vec![]);
    fake.take();
    h
}

fn reads(fake: &Fake) -> usize {
    fake.calls()
        .iter()
        .filter(|c| matches!(c, Call::Read(_)))
        .count()
}

#[test]
fn a_contradiction_added_later_reaches_an_open_memory() {
    let fake = fake(true);
    let mut h = opened(&fake);
    assert!(!h.shows("The release is signed in CI now"));
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert_eq!(reads(&fake), 0, "nothing changed, nothing asked");
    fake.edit(|p| {
        p.open_annotations += 1;
        p.open_contradictions += 1;
    });
    h.frame(vec![]);
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Open(
            connecto_core::memory::Lookup::Memory(mid(1))
        ))]
    );
    for _ in 0..5 {
        h.frame(vec![]);
    }
    assert_eq!(
        reads(&fake),
        1,
        "one read per change, while it is pending too"
    );
    assert!(
        !h.shows("Loading…"),
        "the last answer stays on show meanwhile"
    );
    assert!(h.shows("Use the Developer ID certificate"));
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(detail()))),
    );
    h.frame(vec![]);
    assert_contradiction_on_page(&h);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert_eq!(reads(&fake), 1);
}

#[test]
fn a_new_version_refreshes_the_list_and_keeps_nothing_stale() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click("mem-back");
    fake.take();
    fake.edit(|p| p.version += 1);
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Room(Room::Workshop))]
    );
    assert!(
        h.shows("Workshop · 2 memories"),
        "the old index stays meanwhile"
    );
}

#[test]
fn a_refused_refresh_is_not_asked_again_and_again() {
    let fake = fake(true);
    let mut h = opened(&fake);
    *fake.refuse.lock().unwrap() = Some("busy".into());
    fake.edit(|p| p.open_annotations += 1);
    for _ in 0..5 {
        h.frame(vec![]);
    }
    assert_eq!(reads(&fake), 1);
    assert!(h.shows("busy"));
}

#[test]
fn refresh_asks_again_by_hand() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click("mem-refresh");
    assert_eq!(
        fake.calls(),
        vec![Call::Read(ReadRequest::Open(
            connecto_core::memory::Lookup::Memory(mid(1))
        ))]
    );
    assert!(h.drawn("mem-back"), "the memory stays open");
}

#[test]
fn a_change_before_the_next_frame_still_refreshes_the_new_read() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    // The pass that sends the read, then a change before any other pass.
    h.click_only("mem-room:officina");
    assert_eq!(reads(&fake), 1);
    fake.edit(|p| {
        p.open_annotations += 1;
        p.open_contradictions += 1;
    });
    // The first answer comes back from before the change.
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(Vec::new())),
    );
    h.frame(vec![]);
    assert_eq!(reads(&fake), 2, "asked again for the change");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(cards())),
    );
    for _ in 0..4 {
        h.frame(vec![]);
    }
    assert_eq!(reads(&fake), 2, "and only once");
    assert_contradiction_under(&h, "Releases are signed", Some("Run cargo test"));
}

#[test]
fn two_changes_before_either_refresh_keep_the_last_ready_answer() {
    let fake = fake(true);
    let mut h = opened(&fake);
    fake.edit(|p| p.open_annotations += 1);
    h.frame(vec![]);
    let first = fake.last_read();
    fake.edit(|p| p.open_contradictions += 1);
    h.frame(vec![]);
    let second = fake.last_read();
    assert_eq!(reads(&fake), 2, "one read per change");
    for _ in 0..3 {
        h.frame(vec![]);
        assert!(!h.shows("Loading…"));
        assert!(
            h.shows("Use the Developer ID certificate"),
            "the last ready answer stays"
        );
    }
    // The newest answer is shown; a late one from before is ignored.
    fake.settle_read(
        second,
        ReadState::Ready(ReadAnswer::Memory(Box::new(detail()))),
    );
    h.frame(vec![]);
    assert_contradiction_on_page(&h);
    let mut quiet = detail();
    quiet.annotations.clear();
    fake.settle_read(first, ReadState::Ready(ReadAnswer::Memory(Box::new(quiet))));
    h.frame(vec![]);
    assert!(h.shows("The release is signed in CI now"));
    assert_eq!(reads(&fake), 2);
}

#[test]
fn an_answer_that_arrives_out_of_date_is_asked_for_once_more() {
    let fake = fake(true);
    let mut h = opened(&fake);
    fake.edit(|p| p.version += 1);
    h.frame(vec![]);
    assert_eq!(reads(&fake), 1);
    // The data moves again while the refresh is pending; its answer is stale.
    fake.edit(|p| p.open_contradictions += 1);
    let stale = fake.last_read();
    let mut quiet = detail();
    quiet.annotations.clear();
    fake.settle_read(stale, ReadState::Ready(ReadAnswer::Memory(Box::new(quiet))));
    h.frame(vec![]);
    assert_eq!(reads(&fake), 2, "exactly one more");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(detail()))),
    );
    for _ in 0..4 {
        h.frame(vec![]);
    }
    assert_eq!(reads(&fake), 2);
    assert!(h.shows("The release is signed in CI now"));
}

#[test]
fn a_failed_refresh_keeps_the_last_answer_and_waits_for_the_next_change() {
    let fake = fake(true);
    let mut h = opened(&fake);
    fake.edit(|p| p.open_annotations += 1);
    h.frame(vec![]);
    fake.settle_read(
        fake.last_read(),
        ReadState::Failed("the store is busy".into()),
    );
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert_eq!(reads(&fake), 1, "a failure is not retried in a loop");
    assert!(h.shows("Could not read the memory: the store is busy"));
    assert!(
        h.shows("Use the Developer ID certificate"),
        "the last answer stays"
    );
    fake.edit(|p| p.open_annotations += 1);
    h.frame(vec![]);
    assert_eq!(reads(&fake), 2, "the next change asks again");
}

#[test]
fn a_refused_refresh_is_retried_on_the_next_change_only() {
    let fake = fake(true);
    let mut h = opened(&fake);
    *fake.refuse.lock().unwrap() = Some("busy".into());
    fake.edit(|p| p.open_annotations += 1);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert!(
        h.shows("Use the Developer ID certificate"),
        "the last answer stays"
    );
    *fake.refuse.lock().unwrap() = None;
    fake.edit(|p| p.open_annotations += 1);
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(reads(&fake), 2);
}

// ---------- the user's action wins over automatic refreshes ----------

impl H {
    /// Click `key`, running `change` just before the pass that acts on the
    /// click. Returns the calls made in that pass.
    fn click_during(&mut self, fake: &Fake, key: &str, change: impl FnOnce()) -> Vec<Call> {
        let at = self.rect(key).center();
        self.frame(vec![Event::PointerMoved(at)]);
        self.frame(vec![pointer(at, true)]);
        change();
        fake.take();
        self.frame(vec![pointer(at, false)]);
        fake.take()
    }
}

/// Answer every pending read as the engine would.
fn settle_all(fake: &Fake) {
    fake.edit(|p| {
        for r in &mut p.reads {
            if r.state == ReadState::Pending {
                r.state = ReadState::Ready(match &r.request {
                    ReadRequest::Room(_) => ReadAnswer::Room(cards()),
                    ReadRequest::Search { .. } => ReadAnswer::Search(Vec::new()),
                    ReadRequest::Open(_) => ReadAnswer::Memory(Box::new(detail())),
                    ReadRequest::Brief => ReadAnswer::Brief(String::new()),
                });
            }
        }
    });
}

fn only_reads(calls: &[Call]) -> Vec<ReadRequest> {
    calls
        .iter()
        .filter_map(|c| match c {
            Call::Read(r) => Some(r.clone()),
            _ => None,
        })
        .collect()
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Start {
    /// The workshop's index, answered.
    List,
    /// A memory open over it, answered.
    Detail,
    /// The workshop's index, its read failed.
    FailedList,
    /// The index answered, then another tab on show.
    Away,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Background {
    /// The data changes in the very pass that acts on the click.
    Change,
    /// An automatic refresh is in flight, no new change.
    Pending,
    /// An automatic refresh is in flight and the data changes again.
    Both,
}

fn workshop_index() -> ReadRequest {
    ReadRequest::Room(Room::Workshop)
}

fn open_first() -> ReadRequest {
    ReadRequest::Open(connecto_core::memory::Lookup::Memory(mid(1)))
}

fn start(fake: &Arc<Fake>, start: Start) -> H {
    let mut h = match start {
        Start::Detail => opened(fake),
        _ => {
            let mut h = workshop(fake);
            h.click("mem-room:officina");
            let state = if start == Start::FailedList {
                ReadState::Failed("busy".into())
            } else {
                ReadState::Ready(ReadAnswer::Room(cards()))
            };
            fake.settle_read(fake.last_read(), state);
            h.frame(vec![]);
            h
        }
    };
    if start == Start::Away {
        h.click("mem-tab:activity");
    }
    // The table's filter rows are reachable: the filters are open.
    h.panel.browse.filters_open = true;
    h.frame(vec![]);
    fake.take();
    h
}

#[test]
fn the_users_action_wins_and_one_read_at_most_goes_per_frame() {
    // (start, what the user clicks, the reads the click's pass sends when the
    // data changed in it, the reads when only an automatic refresh is in
    // flight, what is on show at the end: a list or a memory, by request)
    let search = ReadRequest::Search {
        query: "release".into(),
        room: None,
        scope: None,
        all: false,
    };
    type Row = (
        Start,
        &'static str,
        Vec<ReadRequest>,
        Vec<ReadRequest>,
        Option<ReadRequest>,
        Option<ReadRequest>,
    );
    let rows: Vec<Row> = vec![
        (
            Start::List,
            "mem-room:decisioni",
            vec![ReadRequest::Room(Room::Decisions)],
            vec![ReadRequest::Room(Room::Decisions)],
            Some(ReadRequest::Room(Room::Decisions)),
            None,
        ),
        (
            Start::List,
            "mem-search",
            vec![search.clone()],
            vec![search.clone()],
            Some(search.clone()),
            None,
        ),
        // A filter changes nothing on show: the due refresh may go.
        (
            Start::List,
            "mem-search-room:decisioni",
            vec![workshop_index()],
            Vec::new(),
            Some(workshop_index()),
            None,
        ),
        (
            Start::List,
            "mem-open:m-aeaqcaib",
            vec![open_first()],
            vec![open_first()],
            Some(workshop_index()),
            Some(open_first()),
        ),
        // Back: the list's refresh, never the closed memory's.
        (
            Start::Detail,
            "mem-back",
            vec![workshop_index()],
            vec![workshop_index()],
            Some(workshop_index()),
            None,
        ),
        (
            Start::Detail,
            "mem-refresh",
            vec![open_first()],
            vec![open_first()],
            Some(workshop_index()),
            Some(open_first()),
        ),
        (
            Start::FailedList,
            "mem-read-again",
            vec![workshop_index()],
            vec![workshop_index()],
            Some(workshop_index()),
            None,
        ),
        // Leaving the tab sends nothing; coming back sends what is due.
        (
            Start::List,
            "mem-tab:activity",
            Vec::new(),
            Vec::new(),
            Some(workshop_index()),
            None,
        ),
        (
            Start::Away,
            "mem-tab:memories",
            vec![workshop_index()],
            Vec::new(),
            Some(workshop_index()),
            None,
        ),
        // Turning memory off: no read beside it.
        (
            Start::List,
            "mem-toggle",
            Vec::new(),
            Vec::new(),
            Some(workshop_index()),
            None,
        ),
    ];
    assert_eq!(mid(1).to_string(), "m-aeaqcaib", "the key in the table");
    for (from, key, on_change, on_pending, list, detail) in rows {
        for background in [Background::Change, Background::Pending, Background::Both] {
            let case = format!("{from:?} {key} {background:?}");
            if from == Start::FailedList && background != Background::Change {
                // With a refresh in flight the failure gives way to
                // "Loading…": there is no "Try again" to click.
                continue;
            }
            let fake = fake(true);
            let mut h = start(&fake, from);
            h.panel.browse.query = "release".into();
            if background != Background::Change && from != Start::Away {
                fake.edit(|p| p.open_annotations += 1);
                h.frame(vec![]);
                assert_eq!(
                    only_reads(&fake.take()).len(),
                    1,
                    "{case}: refresh in flight"
                );
            } else if background != Background::Change {
                // Away from the tab nothing refreshes: the change waits.
                fake.edit(|p| p.open_annotations += 1);
                h.frame(vec![]);
                assert!(fake.take().is_empty(), "{case}");
            }
            let changes = background != Background::Pending;
            let calls = h.click_during(&fake, key, || {
                if changes {
                    fake.edit(|p| p.open_contradictions += 1);
                }
            });
            let reads = only_reads(&calls);
            let expected = if changes || from == Start::Away {
                on_change.clone()
            } else {
                on_pending.clone()
            };
            assert_eq!(reads, expected, "{case}: the click's pass");
            assert!(reads.len() <= 1, "{case}: one read at most");
            // Every answer arrives, the superseded ones too; nothing more is
            // asked and the user's choice stays.
            for _ in 0..3 {
                settle_all(&fake);
                h.frame(vec![]);
                assert!(only_reads(&fake.take()).is_empty(), "{case}: settled");
            }
            if key == "mem-toggle" {
                assert!(fake.calls().last().is_none(), "{case}");
                continue;
            }
            if key == "mem-tab:activity" {
                assert_eq!(h.panel.tab, Tab::Activity, "{case}");
                h.tab(Tab::Memories);
                let back = only_reads(&fake.take());
                // Due only if the data changed while away.
                let due = if changes {
                    vec![workshop_index()]
                } else {
                    Vec::new()
                };
                assert_eq!(back, due, "{case}: on return, once at most");
                settle_all(&fake);
                h.frame(vec![]);
            }
            let shown_list = h.panel.browse.list.as_ref().map(|s| s.request.clone());
            let shown_detail = h.panel.browse.detail.as_ref().map(|s| s.request.clone());
            assert_eq!(shown_list, list, "{case}: the list on show");
            assert_eq!(shown_detail, detail, "{case}: the memory on show");
        }
    }
}

#[test]
fn turning_memory_off_sends_no_read_with_it() {
    let fake = fake(true);
    let mut h = start(&fake, Start::List);
    let calls = h.click_during(&fake, "mem-toggle", || {
        fake.edit(|p| p.open_contradictions += 1);
    });
    assert!(
        matches!(&calls[..], [Call::Settings(s)] if !s.enabled),
        "{calls:?}"
    );
}

#[test]
fn back_never_reopens_the_memory_when_its_refresh_lands_later() {
    let fake = fake(true);
    let mut h = opened(&fake);
    fake.edit(|p| p.open_contradictions += 1);
    h.frame(vec![]);
    let detail_refresh = fake.last_read();
    fake.take();
    h.click("mem-back");
    fake.settle_read(
        detail_refresh,
        ReadState::Ready(ReadAnswer::Memory(Box::new(detail()))),
    );
    settle_all(&fake);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert!(h.panel.browse.detail.is_none());
    assert!(!h.drawn("mem-back"));
    assert!(h.shows("Workshop · 2 memories"));
}

#[test]
fn a_refused_room_click_leaves_the_due_refresh_for_the_next_frame() {
    let fake = fake(true);
    let mut h = start(&fake, Start::List);
    *fake.refuse.lock().unwrap() = Some("busy".into());
    let calls = h.click_during(&fake, "mem-room:decisioni", || {
        fake.edit(|p| p.open_contradictions += 1);
    });
    assert_eq!(only_reads(&calls), vec![ReadRequest::Room(Room::Decisions)]);
    *fake.refuse.lock().unwrap() = None;
    h.frame(vec![]);
    assert_eq!(
        only_reads(&fake.take()),
        vec![workshop_index()],
        "set aside, not lost"
    );
    h.frame(vec![]);
    assert!(fake.take().is_empty());
}

#[test]
fn another_projects_settings_start_with_nothing_on_show() {
    let sheet = crate::project_settings::ProjectSettingsSheet::new("other".into());
    assert!(sheet.memory.browse.list.is_none());
    assert!(sheet.memory.browse.detail.is_none());
}

#[test]
fn the_build_explains_what_past_conversations_could_not_tell() {
    let gaps = [CoverageGap::RolesUnknown, CoverageGap::PastUserProvenance];
    let fake = fake(true);
    fake.edit(|p| {
        p.bootstrap.state = BootstrapState::Completed;
        p.bootstrap.gaps = gaps.to_vec();
        p.bootstrap.plan = Some(BootstrapPlan {
            gaps: gaps.to_vec(),
            ..plan(BootstrapOptions::default())
        });
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    for gap in gaps {
        let sentence = format!("• {}", gap.label());
        let shown = h
            .panel
            .marks
            .texts
            .iter()
            .filter(|t| **t == sentence)
            .count();
        assert_eq!(shown, 2, "{gap:?}: in the coverage and in the plan");
    }
}

// ---------- statements made in this window ----------

use super::statement::{self as st, Composer};

impl H {
    /// Click a text field and type into it.
    fn write(&mut self, key: &str, text: &str) {
        self.click(key);
        self.frame(vec![Event::Text(text.to_owned())]);
        self.frame(vec![]);
    }

    fn composer(&self) -> &Composer {
        self.panel.composer.as_ref().expect("a composer is open")
    }

    /// Move the clock on by `ms`, as time passing while the window stays open.
    fn later(&mut self, ms: u64) {
        self.now += ms;
    }
}

fn statements(fake: &Fake) -> Vec<UserStatement> {
    fake.calls()
        .into_iter()
        .filter_map(|c| match c {
            Call::Statement(s) => Some(s),
            _ => None,
        })
        .collect()
}

fn m1() -> Subject {
    Subject::Memory(mid(1))
}

#[test]
fn the_wording_follows_kind_and_standing() {
    // (kind, standing, accept, reject, what the meaning must say)
    let table = [
        (
            MemoryKind::Decision,
            Standing::Proposed,
            "Accept this decision",
            "Reject this decision",
            "accepted decision, verified",
        ),
        (
            MemoryKind::Decision,
            Standing::Accepted,
            "Confirm this decision",
            "Reject this decision",
            "marks it discarded",
        ),
        (
            MemoryKind::Decision,
            Standing::Discarded,
            "Accept this decision after all",
            "Confirm it stays rejected",
            "keeps it discarded",
        ),
        (
            MemoryKind::Convention,
            Standing::Proposed,
            "Accept this convention",
            "Reject this convention",
            "accepted convention, verified",
        ),
        (
            MemoryKind::Work,
            Standing::Planned,
            "Confirm this plan is right",
            "This plan is wrong",
            "It stays a plan",
        ),
        (
            MemoryKind::Work,
            Standing::InProgress,
            "Confirm this description is right",
            "This is wrong",
            "does not change whether the work is in progress or implemented",
        ),
        (
            MemoryKind::Work,
            Standing::Implemented,
            "Confirm this description is right",
            "This is wrong",
            "back to planned",
        ),
        (
            MemoryKind::Fact,
            Standing::Hypothesis,
            "Confirm this is established",
            "This is wrong",
            "established fact, verified",
        ),
        (
            MemoryKind::Problem,
            Standing::Established,
            "Confirm this still holds",
            "This is wrong",
            "back to a hypothesis",
        ),
        (
            MemoryKind::Procedure,
            Standing::Documented,
            "Confirm this procedure is right",
            "This procedure is wrong",
            "its standing (documented) does not change",
        ),
        (
            MemoryKind::Procedure,
            Standing::Exercised,
            "Confirm this procedure is right",
            "This procedure is wrong",
            "its standing (exercised) does not change",
        ),
    ];
    for (kind, standing, accept, reject, says) in table {
        let w = st::for_memory(kind, standing);
        assert!(
            w.meaning.contains(says),
            "{kind:?} {standing:?}: {}",
            w.meaning
        );
        assert_eq!(
            (w.accept.as_str(), w.reject.as_str()),
            (accept, reject),
            "{kind:?} {standing:?}"
        );
        assert!(
            w.correct.starts_with("Correct this "),
            "{kind:?} {standing:?}"
        );
        assert!(!w.meaning.is_empty());
    }
    // Every standing a kind can have is worded; accepting a plan never
    // claims it is implemented.
    for kind in MemoryKind::ALL {
        for standing in Standing::allowed_for(*kind) {
            let w = st::for_memory(*kind, *standing);
            assert!(!w.accept.is_empty() && !w.reject.is_empty());
        }
    }
    // Nothing claims more than the engine does: no procedure or work is
    // said to work, run, be under way or be implemented by a confirmation.
    for kind in [MemoryKind::Work, MemoryKind::Procedure] {
        for standing in Standing::allowed_for(kind) {
            let w = st::for_memory(kind, *standing);
            for claim in ["works", "seen it work", "under way", "is implemented"] {
                assert!(
                    !w.accept.contains(claim),
                    "{kind:?} {standing:?}: {}",
                    w.accept
                );
            }
            assert!(
                w.meaning.contains("verified on your word"),
                "{kind:?} {standing:?}"
            );
        }
    }
    // Every rejection but a decision's or convention's leaves an objection.
    for kind in [
        MemoryKind::Fact,
        MemoryKind::Problem,
        MemoryKind::Work,
        MemoryKind::Procedure,
    ] {
        for standing in Standing::allowed_for(kind) {
            let w = st::for_memory(kind, *standing);
            assert!(w.meaning.contains("in conflict"), "{kind:?} {standing:?}");
            assert!(
                w.meaning.contains("objection left open"),
                "{kind:?} {standing:?}"
            );
        }
    }
    assert_eq!(
        st::for_memory(MemoryKind::Work, Standing::Planned).correct,
        "Correct this plan…"
    );
}

#[test]
fn notes_are_worded_by_what_they_would_become_or_by_their_kind() {
    let c = contradiction(&mid(1));
    let w = st::for_annotation(&c);
    assert_eq!(
        (w.accept.as_str(), w.reject.as_str()),
        ("The contradiction is right", "The contradiction is wrong")
    );
    let mut proposal = note(5, AnnotationKind::Integration, "Use SQLite.", None);
    proposal.memory_kind = Some(MemoryKind::Decision);
    proposal.standing = Some(Standing::Proposed);
    assert_eq!(st::for_annotation(&proposal).accept, "Accept this decision");
    let task = note(6, AnnotationKind::Task, "Sign the DMG.", None);
    let w = st::for_annotation(&task);
    assert_eq!(w.accept, "Accept this task");
    assert!(w.meaning.contains("does not mark it done"));
}

#[test]
fn accepting_an_opened_decision_sends_one_statement_and_refreshes_it() {
    let fake = fake(true);
    let mut h = opened(&fake);
    assert!(h.shows(st::PROVENANCE));
    assert!(h.shows("It is only proposed."));
    let k = st::key(&m1());
    h.click(&format!("mem-say-accept:{k}"));
    assert_eq!(h.composer().title, "Accept this decision");
    assert!(h.shows("Reason (optional)"));
    h.click("mem-say-send");
    assert_eq!(
        statements(&fake),
        vec![UserStatement {
            kind: UserStatementKind::Accept,
            about: Some(m1()),
            seen: Some(detail().memory.seen()),
            room: None,
            text: String::new(),
        }]
    );
    assert!(h.panel.composer.is_none());
    assert!(h.shows("Recorded as your statement."));
    // The engine recorded it: the memory on show is asked for once more.
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(
        only_reads(&fake.calls()),
        vec![ReadRequest::Open(connecto_core::memory::Lookup::Memory(
            mid(1)
        ))]
    );
}

#[test]
fn rejecting_asks_for_a_second_yes_and_carries_the_reason() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-reject:{}", st::key(&m1())));
    h.write("mem-say-text", "we notarize in CI");
    h.click("mem-say-send");
    assert!(statements(&fake).is_empty(), "not before the second yes");
    assert!(h.shows("Record, as your statement: \"Reject this decision\"?"));
    h.click("mem-say-back");
    assert!(!h.composer().confirming);
    assert_eq!(h.composer().text, "we notarize in CI", "the text stays");
    h.click("mem-say-send");
    h.click("mem-say-confirm");
    assert_eq!(
        statements(&fake),
        vec![UserStatement {
            kind: UserStatementKind::Reject,
            about: Some(m1()),
            seen: Some(detail().memory.seen()),
            room: None,
            text: "we notarize in CI".into(),
        }]
    );
}

#[test]
fn a_correction_needs_text_and_is_blocked_until_it_has_some() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-correct:{}", st::key(&m1())));
    assert!(h.shows("What is right instead"));
    h.click("mem-say-send");
    assert!(statements(&fake).is_empty());
    h.write("mem-say-text", "   ");
    assert!(h.shows("the text is empty"));
    h.click("mem-say-send");
    assert!(
        statements(&fake).is_empty(),
        "blank text is blocked locally"
    );
    h.write("mem-say-text", "Signed in CI since 0.1.9.");
    h.click("mem-say-send");
    let sent = statements(&fake);
    assert_eq!(sent.len(), 1);
    assert_eq!(sent[0].kind, UserStatementKind::Correct);
    assert_eq!(sent[0].text, "   Signed in CI since 0.1.9.");
}

#[test]
fn a_refusal_keeps_the_text_and_shows_the_reason() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-correct:{}", st::key(&m1())));
    h.write("mem-say-text", "the port is 8080");
    *fake.refuse.lock().unwrap() = Some("try again when the current run finishes".into());
    h.click("mem-say-send");
    assert_eq!(statements(&fake).len(), 1);
    assert_eq!(h.composer().text, "the port is 8080");
    assert!(h.shows("Not recorded: try again when the current run finishes"));
    assert!(!h.shows("Recorded as your statement."));
}

#[test]
fn text_over_the_limit_is_counted_and_never_sent() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-correct:{}", st::key(&m1())));
    h.panel.composer.as_mut().unwrap().text = "x".repeat(MAX_STATEMENT_BYTES * 9 / 10);
    h.frame(vec![]);
    assert!(h.shows(&format!(
        "of {} bytes",
        super::text::count(MAX_STATEMENT_BYTES as u64)
    )));
    h.panel.composer.as_mut().unwrap().text = "x".repeat(MAX_STATEMENT_BYTES + 3);
    h.frame(vec![]);
    assert!(h.shows("3 too many. Shorten it; it is never cut."));
    h.click("mem-say-send");
    assert!(statements(&fake).is_empty());
    assert_eq!(
        h.composer().text.len(),
        MAX_STATEMENT_BYTES + 3,
        "nothing cut"
    );
}

#[test]
fn a_note_goes_to_a_room_or_about_the_open_memory() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-say-note");
    assert!(!h.drawn("mem-say-about"), "no memory open");
    h.click("mem-say-room:convenzioni");
    h.write("mem-say-text", "We ship on Fridays.");
    h.click("mem-say-send");
    assert_eq!(
        statements(&fake),
        vec![UserStatement {
            kind: UserStatementKind::Note,
            about: None,
            seen: None,
            room: Some(Room::Conventions),
            text: "We ship on Fridays.".into(),
        }]
    );
    // With a memory open, the note is about it unless unticked.
    let fake = fresh();
    let mut h = opened(&fake);
    h.click("mem-say-note");
    assert_eq!(h.composer().about().cloned(), Some(m1()));
    assert!(
        !h.drawn("mem-say-room:convenzioni"),
        "no room with a memory"
    );
    h.write("mem-say-text", "Ask Marco first.");
    h.click("mem-say-send");
    assert_eq!(statements(&fake)[0].about, Some(m1()));
    assert_eq!(statements(&fake)[0].room, None);
}

/// A second enabled fake in a test whose `fake` name is taken.
fn fresh() -> Arc<Fake> {
    fake(true)
}

#[test]
fn unticking_about_offers_a_room_instead() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click("mem-say-note");
    h.click("mem-say-about");
    assert_eq!(h.composer().about().cloned(), None);
    h.click("mem-say-room:decisioni");
    h.write("mem-say-text", "General rule.");
    h.click("mem-say-send");
    let s = &statements(&fake)[0];
    assert_eq!((s.about.clone(), s.room), (None, Some(Room::Decisions)));
}

#[test]
fn an_open_note_under_a_memory_has_its_own_actions() {
    let fake = fake(true);
    let mut h = opened(&fake);
    // Show the full detail, with its contradiction.
    fake.edit(|p| p.open_contradictions += 1);
    h.frame(vec![]);
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(detail()))),
    );
    h.frame(vec![]);
    let c = contradiction(&mid(1));
    let k = st::key(&Subject::Annotation(c.id.clone()));
    assert!(h.drawn(&format!("mem-say-accept:{k}")));
    assert!(h.shows("Say which side is right"));
    h.click(&format!("mem-say-accept:{k}"));
    assert_eq!(h.composer().title, "The contradiction is right");
    h.click("mem-say-send");
    let s = &statements(&fake)[0];
    assert_eq!(s.kind, UserStatementKind::Accept);
    assert_eq!(s.about, Some(Subject::Annotation(c.id)));
    // An absorbed note has no actions.
    let mut absorbed = note(4, AnnotationKind::Correction, "x", Some(mid(1)));
    absorbed.absorbed_in = Some(2);
    let k = st::key(&Subject::Annotation(absorbed.id));
    assert!(!h.drawn(&format!("mem-say-accept:{k}")));
}

#[test]
fn statements_from_this_window_are_labelled_apart_from_terminal_prompts() {
    let mine = {
        let mut a = note(8, AnnotationKind::Integration, "Stated here.", Some(mid(1)));
        a.origin = AnnotationOrigin::User;
        a.verification = Verification::UserStated;
        a
    };
    assert!(st::from_window(&mine));
    let typed = {
        let mut a = mine.clone();
        a.verification = Verification::Unverified;
        a
    };
    assert!(!st::from_window(&typed));
    let _ = mine;
    // A source stated in the window, and one typed in a terminal.
    let fake = fresh();
    let mut h = opened(&fake);
    let mut d = detail();
    let mut window = d.evidence[0].clone();
    window.id = EvidenceId::new(2);
    window.terminal = None;
    d.evidence.push(window);
    fake.edit(|p| p.open_annotations += 1);
    h.frame(vec![]);
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(d))),
    );
    h.frame(vec![]);
    assert!(h.shows(&format!(
        "e-2 · 2 h ago · {} · branch main",
        st::WINDOW_SOURCE
    )));
    assert!(h.shows("e-1 · 2 h ago · prompt typed in claude-1"));
}

/// A run that proposed and kept nothing: the palace is empty, and the build
/// says so in amber, never as a success.
#[test]
fn a_refused_run_reads_as_a_problem_everywhere() {
    let fake = fake(true);
    let reason = "26 proposed, 0 kept: no source in the packet (26)";
    fake.edit(|p| {
        p.cycles = vec![last_run(
            connecto_core::memory::Cycle::Bootstrap,
            RunOutcome::Refused {
                proposed: 26,
                reason: reason.into(),
            },
        )];
        p.bootstrap.state = BootstrapState::Completed;
        p.runs = Arc::new(vec![run(
            3,
            connecto_core::memory::Cycle::Bootstrap,
            RunOutcome::Refused {
                proposed: 26,
                reason: reason.into(),
            },
        )]);
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    assert!(h.shows("Initial build, nothing kept: 26 proposed, 0 kept"));
    h.tab(Tab::Activity);
    assert!(h.shows("Nothing kept: 26 proposed, 0 kept"));
    h.tab(Tab::Activity);
    assert!(h.shows("Finished with nothing kept"));
    assert!(!h.shows("Completed"));
    assert!(h.shows("Last build run kept nothing: 26 proposed"));
    h.tab(Tab::Memories);
    assert!(h.shows("The latest run kept nothing from 26 proposed."));
    assert!(
        h.shows("The reasons were shown when the run finished"),
        "no report kept: no promise of one"
    );
    assert!(!h.drawn("mem-go-report"));
    // The report of this very run is offered.
    fake.edit(|p| p.last_report = Some(Arc::new(DreamReport { run: 3, ..report() })));
    h.frame(vec![]);
    h.click("mem-go-report");
    assert_eq!(h.panel.tab, Tab::Activity);
}

/// The entrance is asked for once per memory version, drawn as text, and has
/// its loading, error and empty states.
#[test]
fn the_entrance_is_asked_for_once_per_version_and_shown_as_text() {
    let fake = fake(true);
    let mut h = palace(&fake);
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 1);
    assert!(h.shows("Brief for agents, from this memory"));
    assert!(h.shows("Loading…"));
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 1, "not every frame");
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief(
            "Project brief\n- **not markup** <b>kept</b>".into(),
        )),
    );
    h.frame(vec![]);
    assert!(h.shows("Project brief"));
    assert!(h.shows("**not markup** <b>kept</b>"), "plain text");
    fake.edit(|p| p.version += 1);
    h.frame(vec![]);
    assert_eq!(
        fake.briefs.load(Ordering::Relaxed),
        1,
        "one batch at a time: nothing in flight may be replaced"
    );
    h.later(REFRESH_SPACING_MS);
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief as it is now".into())),
    );
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(
        fake.briefs.load(Ordering::Relaxed),
        2,
        "a new version asks again, once"
    );
    h.frame(vec![]);
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 2, "and only once");
}

#[test]
fn a_change_while_the_brief_is_in_flight_waits_for_it() {
    let fake = fake(true);
    let mut h = palace(&fake);
    let old = brief_read(&fake);
    fake.edit(|p| p.version += 1);
    h.frame(vec![]);
    assert_eq!(
        fake.briefs.load(Ordering::Relaxed),
        1,
        "nothing new while one is in flight"
    );
    fake.settle_read(
        old,
        ReadState::Ready(ReadAnswer::Brief("the brief of version 3".into())),
    );
    h.frame(vec![]);
    assert!(
        h.shows("the brief of version 3"),
        "the last complete answer stays drawn"
    );
    assert!(h.shows("Refreshing…"));
    h.later(REFRESH_SPACING_MS);
    h.frame(vec![]);
    assert_eq!(
        fake.briefs.load(Ordering::Relaxed),
        2,
        "then one for the latest key"
    );
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief of version 4".into())),
    );
    h.frame(vec![]);
    assert!(h.shows("the brief of version 4"));
}

#[test]
fn the_entrance_is_asked_again_when_any_input_it_depends_on_changes() {
    let fake = fake(true);
    let mut h = palace(&fake);
    let mut asked = fake.briefs.load(Ordering::Relaxed);
    assert_eq!(asked, 1);
    type Change = Box<dyn Fn(&mut ProjectMemoryView)>;
    let changes: Vec<Change> = vec![
        Box::new(|p| p.open_annotations += 1),
        Box::new(|p| p.open_contradictions += 1),
        Box::new(|p| p.considered_through_ms = Some(NOW)),
        Box::new(|p| p.backlog.queued += 1),
        Box::new(|p| p.bootstrap.state = BootstrapState::Running),
        Box::new(|p| p.version += 1),
    ];
    for change in changes {
        // Everything in flight is answered first: one batch at a time.
        answer_room_reads(&fake, None);
        let pending = brief_read(&fake);
        if pending != 0 {
            fake.settle_read(
                pending,
                ReadState::Ready(ReadAnswer::Brief("the brief".into())),
            );
        }
        h.later(REFRESH_SPACING_MS);
        h.frame(vec![]);
        fake.edit(|p| change(p));
        h.frame(vec![]);
        assert_eq!(fake.briefs.load(Ordering::Relaxed), asked + 1);
        asked += 1;
        h.frame(vec![]);
        assert_eq!(
            fake.briefs.load(Ordering::Relaxed),
            asked,
            "once, not every frame"
        );
    }
}

/// While shown, the entrance is asked again about once a minute, and not
/// before.
#[test]
fn the_entrance_is_refetched_about_once_a_minute_while_shown() {
    let fake = fake(true);
    let mut h = palace(&fake);
    h.frame(vec![]);
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 1);
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief".into())),
    );
    h.later(BRIEF_EVERY_MS - 1);
    h.frame(vec![]);
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 1, "not yet a minute");
    h.later(1);
    h.frame(vec![]);
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 2);
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief".into())),
    );
    h.frame(vec![]);
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 2, "then waits again");
}

/// A refresh with the same key keeps the last answer drawn while it is pending.
#[test]
fn a_refresh_keeps_the_last_answer_on_show_while_it_is_pending() {
    let fake = fake(true);
    let mut h = palace(&fake);
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief as it was".into())),
    );
    h.frame(vec![]);
    h.later(BRIEF_EVERY_MS);
    h.frame(vec![]);
    h.frame(vec![]);
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 2);
    assert!(h.shows("the brief as it was"));
    assert!(h.shows("Refreshing…"));
}

/// The refusal named is the latest run's: an older build refusal is not
/// described as the last run after a newer one, and no report of another run
/// is offered for it.
#[test]
fn the_latest_run_decides_what_a_refusal_says() {
    let fake = fake(true);
    let refused = |proposed| RunOutcome::Refused {
        proposed,
        reason: format!("{proposed} proposed, 0 kept: no source"),
    };
    fake.edit(|p| {
        p.runs = Arc::new(vec![
            RunEntry {
                started_ms: NOW - 1000,
                ..run(7, connecto_core::memory::Cycle::Dream, RunOutcome::NoChange)
            },
            RunEntry {
                started_ms: NOW - 5000,
                ..run(3, connecto_core::memory::Cycle::Bootstrap, refused(26))
            },
        ]);
        p.bootstrap.state = BootstrapState::Completed;
        p.last_report = Some(Arc::new(report()));
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Memories);
    assert!(
        !h.shows("kept nothing from 26"),
        "an older refusal is not the last run"
    );
    // Now the latest run is a refused dream whose report is another run's.
    fake.edit(|p| {
        p.runs = Arc::new(vec![RunEntry {
            started_ms: NOW,
            ..run(12, connecto_core::memory::Cycle::Dream, refused(4))
        }]);
        p.totals = StatusCounts::default();
    });
    h.frame(vec![]);
    assert!(h.shows("The latest run kept nothing from 4 proposed."));
    assert!(h.shows("belongs to another run"));
    assert!(
        !h.drawn("mem-go-report"),
        "no report is promised for run 12"
    );
}

#[test]
fn a_failed_entrance_says_why_and_can_be_asked_again() {
    let fake = fake(true);
    let mut h = palace(&fake);
    fake.settle_read(
        brief_read(&fake),
        ReadState::Failed("the store is busy".into()),
    );
    h.frame(vec![]);
    assert!(h.shows("Could not load the brief: the store is busy"));
    h.click("mem-brief-again");
    assert_eq!(fake.briefs.load(Ordering::Relaxed), 2);
}

#[test]
fn the_entrance_is_not_asked_for_an_empty_palace_and_hides_behind_a_room() {
    let empty = fake(true);
    let mut h = H::of(&empty);
    h.tab(Tab::Memories);
    assert_eq!(empty.briefs.load(Ordering::Relaxed), 0);
    assert!(!h.shows("Brief for agents"));
    let fake = fake(true);
    let mut h = palace(&fake);
    h.click("mem-room:decisioni");
    h.frame(vec![]);
    assert!(!h.shows("ENTRANCE"), "a room is on show");
}

/// A statement's result reaches its draft while another tab is on show: the
/// poll that runs on every frame does not wait for the Memory tab.
#[test]
fn a_statement_result_arrives_while_another_tab_is_on_show() {
    let fake = fake(true);
    let mut h = sent(&fake);
    fake.publish(
        fake.last_statement(),
        StatementState::Recorded {
            applied: true,
            version: Some(5),
            note: None,
        },
    );
    let ctx = h.ctx.clone();
    crate::memory::poll(&ctx, &mut h.panel, fake.as_ref(), P);
    assert!(h.panel.composer.is_none(), "the draft is settled");
    assert!(
        h.panel
            .said
            .as_deref()
            .is_some_and(|s| s.contains("version 5"))
    );
}

// ---------- the last report, the git note, the delete text ----------

fn report() -> DreamReport {
    DreamReport {
        run: 9,
        version: Some(4),
        at_ms: NOW - 3_600_000,
        rejected: vec!["session Dream 2, operation 3: no source in the packet".into()],
        adjusted: vec![
            "set_status verified lowered to candidate: no independent evidence".into(),
            "standing accepted kept proposed".into(),
        ],
        lowered: vec![m1()],
    }
}

#[test]
fn the_last_report_shows_counts_then_details_with_links() {
    let fake = fake(true);
    fake.edit(|p| p.last_report = Some(Arc::new(report())));
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    // With no run in the list, the report is shown on its own.
    assert!(h.shows("LAST REPORT"));
    assert!(h.shows("Run #9"));
    assert!(h.shows("Proposed: session Dream 2, operation 3"));
    assert!(h.shows("Refused because: no source in the packet"));
    assert!(h.shows("Refused (1)"));
    assert!(h.shows("Kept in a weaker form (2)"));
    assert!(h.shows("Changed: set_status verified lowered to candidate"));
    assert!(h.shows("Why: no independent evidence"));
    assert!(h.shows("standing accepted kept proposed"));
    h.click(&format!("mem-report-open:{}", m1()));
    assert_eq!(
        only_reads(&fake.calls()),
        vec![ReadRequest::Open(connecto_core::memory::Lookup::Memory(
            mid(1)
        ))]
    );
    assert_eq!(h.panel.tab, Tab::Memories);
    assert!(h.panel.browse.detail.is_some());
    // Activity keeps it on show; nothing when there is none.
    h.tab(Tab::Activity);
    assert!(h.shows("LAST REPORT"));
    fake.edit(|p| p.last_report = None);
    h.frame(vec![]);
    assert!(!h.shows("LAST REPORT"));
}

#[test]
fn an_empty_report_says_nothing_was_changed() {
    let fake = fake(true);
    fake.edit(|p| {
        p.last_report = Some(Arc::new(DreamReport {
            run: 3,
            version: None,
            at_ms: NOW,
            ..DreamReport::default()
        }));
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    // A report that refused and kept nothing is not a thing to show.
    assert!(!h.shows("LAST REPORT"));
}

#[test]
fn the_git_note_shows_by_the_coverage_and_in_the_overview() {
    let note = "Not a git repository: no history or local changes are read.";
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    assert!(!h.shows("GIT"));
    fake.edit(|p| p.git_note = Some(note.into()));
    h.frame(vec![]);
    assert!(h.line("GIT") < h.line(note));
}

#[test]
fn the_delete_confirmation_names_the_cli_logs_it_leaves() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    h.click("mem-delete");
    assert!(h.shows(super::overview::DELETE_LEAVES));
    assert!(h.shows("session logs (Claude, Codex)"));
}

// ---------- drafts stay bound to what they are about ----------

use super::statement::Draft;

const SUMMARY: &str = "Releases are signed on the maintainer's Mac.";

fn quiet_detail() -> MemoryDetail {
    let mut d = detail();
    d.annotations.clear();
    d
}

/// The memory on show is answered anew with `d`.
fn reanswer(fake: &Fake, h: &mut H, d: MemoryDetail) {
    fake.edit(|p| p.open_annotations += 1);
    h.frame(vec![]);
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(d))),
    );
    h.frame(vec![]);
    h.frame(vec![]);
}

/// An "Accept this decision" draft with text, on the open memory.
fn drafting(fake: &Arc<Fake>) -> H {
    let mut h = opened(fake);
    h.click(&format!("mem-say-accept:{}", st::key(&m1())));
    h.write("mem-say-text", "approved in the call");
    assert!(h.composer().is_active());
    fake.take();
    h
}

fn suspended(h: &H) -> bool {
    matches!(h.composer().draft, Draft::Suspended { .. })
}

#[test]
fn back_suspends_a_draft_and_resume_brings_it_back_with_its_text() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    h.click("mem-back");
    assert!(suspended(&h));
    assert!(
        !h.drawn("mem-say-send"),
        "not sendable away from its memory"
    );
    assert!(h.shows(&format!(
        "Draft about {}: \"{SUMMARY}\" (proposed decision, as it was)",
        mid(1)
    )));
    assert!(h.shows("Your text: approved in the call"));
    h.click("mem-draft-resume");
    assert_eq!(only_reads(&fake.take()), vec![open_first()]);
    assert!(h.shows("Reopening…"));
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(quiet_detail()))),
    );
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(h.composer().is_active());
    assert_eq!(h.composer().text, "approved in the call");
    h.click("mem-say-send");
    assert_eq!(
        statements(&fake),
        vec![UserStatement {
            kind: UserStatementKind::Accept,
            about: Some(m1()),
            seen: Some(detail().memory.seen()),
            room: None,
            text: "approved in the call".into(),
        }]
    );
}

#[test]
fn opening_another_memory_suspends_and_never_retargets() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    // The open memory links to m5: open it.
    h.click(&format!("mem-open:{}", mid(5)));
    let mut other = quiet_detail();
    other.memory.id = mid(5);
    other.memory.summary = "Another memory.".into();
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(other))),
    );
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(h.shows("Another memory."));
    assert!(suspended(&h));
    assert_eq!(
        h.composer().about(),
        Some(&m1()),
        "still about the first memory"
    );
    // The other memory's actions wait for the draft: nothing is replaced.
    let k5 = st::key(&Subject::Memory(mid(5)));
    h.click(&format!("mem-say-accept:{k5}"));
    assert_eq!(h.composer().about(), Some(&m1()));
    h.click("mem-say-note");
    assert_eq!(h.composer().about(), Some(&m1()));
    assert!(statements(&fake).is_empty());
    h.click("mem-draft-discard");
    assert!(h.panel.composer.is_none());
    h.click(&format!("mem-say-accept:{k5}"));
    assert_eq!(h.composer().about(), Some(&Subject::Memory(mid(5))));
}

#[test]
fn a_room_or_a_search_suspends_a_bound_draft() {
    for key in ["mem-room:decisioni", "mem-search"] {
        let fake = fake(true);
        let mut h = drafting(&fake);
        h.panel.browse.query = "release".into();
        // Back from the memory first: the rooms and the search are beside the list.
        h.click("mem-back");
        h.click(key);
        h.frame(vec![]);
        assert!(suspended(&h), "{key}");
        assert!(!h.drawn("mem-say-send"), "{key}");
        assert_eq!(h.composer().text, "approved in the call", "{key}");
    }
}

#[test]
fn leaving_the_tab_suspends_a_bound_draft() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    h.tab(Tab::Activity);
    assert!(suspended(&h));
    h.tab(Tab::Memories);
    assert!(
        suspended(&h),
        "coming back does not send or resume by itself"
    );
    assert!(h.shows("You left the Memories tab."));
    assert!(h.drawn("mem-draft-resume"));
}

#[test]
fn turning_memory_off_keeps_drafts_for_when_it_is_back() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    h.click("mem-toggle");
    assert!(suspended(&h));
    // On again, in the same sheet.
    h.click("mem-toggle");
    h.click("mem-enable-empty");
    h.tab(Tab::Memories);
    assert!(h.shows("Memory was turned off. Your text is kept."));
    assert_eq!(h.composer().text, "approved in the call");
    assert!(!h.drawn("mem-say-send"));
    // A free note is kept the same way, and resumes in place.
    let fake = fresh();
    let mut h = workshop(&fake);
    h.click("mem-say-note");
    h.write("mem-say-text", "We ship on Fridays.");
    h.click("mem-toggle");
    h.click("mem-toggle");
    h.click("mem-enable-empty");
    h.tab(Tab::Memories);
    assert!(h.shows("Draft note"));
    h.click("mem-draft-resume");
    assert!(h.composer().is_active());
    assert_eq!(h.composer().text, "We ship on Fridays.");
}

#[test]
fn another_project_drops_what_the_panel_kept() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    let control = Arc::clone(&h.control);
    let raw = RawInput {
        screen_rect: Some(Rect::from_min_size(Pos2::ZERO, Vec2::new(1400.0, 6000.0))),
        ..RawInput::default()
    };
    let panel = &mut h.panel;
    let mut output = h.ctx.run_ui(raw, |ui| {
        section(ui, panel, control.as_ref(), "other", NOW, 0);
    });
    output.textures_delta.clear();
    assert!(h.panel.composer.is_none());
    assert!(h.panel.browse.detail.is_none());
}

/// A change made to the memory on show.
type Change = Box<dyn Fn(&mut MemoryDetail)>;

#[test]
fn a_subject_that_changed_under_a_draft_needs_the_action_again() {
    let changes: Vec<(&str, Change, &str)> = vec![
        (
            "standing",
            Box::new(|d: &mut MemoryDetail| d.memory.standing = Standing::Accepted),
            "Confirm this decision",
        ),
        (
            "summary",
            Box::new(|d: &mut MemoryDetail| d.memory.summary = "Releases are signed in CI.".into()),
            "Accept this decision",
        ),
        (
            "kind",
            Box::new(|d: &mut MemoryDetail| d.memory.kind = MemoryKind::Convention),
            "Accept this convention",
        ),
    ];
    for (what, change, again) in changes {
        let fake = fake(true);
        let mut h = drafting(&fake);
        let mut d = quiet_detail();
        change(&mut d);
        let now = st::Snapshot::of_memory(&d.memory);
        reanswer(&fake, &mut h, d);
        assert!(
            matches!(&h.composer().draft, Draft::Suspended { changed: Some(s), .. } if *s == now),
            "{what}"
        );
        assert!(!h.drawn("mem-say-send"), "{what}");
        assert!(
            h.shows("Choose the action again under it; your text is kept."),
            "{what}"
        );
        assert!(statements(&fake).is_empty(), "{what}");
        h.click(&format!("mem-say-accept:{}", st::key(&m1())));
        assert_eq!(h.composer().title, again, "{what}: the new wording");
        assert_eq!(
            h.composer().text,
            "approved in the call",
            "{what}: text kept"
        );
        h.click("mem-say-send");
        assert_eq!(statements(&fake).len(), 1, "{what}");
    }
}

#[test]
fn a_draft_about_a_note_that_closed_is_suspended() {
    let fake = fake(true);
    let mut h = opened(&fake);
    reanswer(&fake, &mut h, detail());
    let c = contradiction(&mid(1));
    let k = st::key(&Subject::Annotation(c.id.clone()));
    h.click(&format!("mem-say-reject:{k}"));
    h.write("mem-say-text", "it is still signed locally");
    let mut d = detail();
    d.annotations.retain(|a| a.id != c.id);
    reanswer(&fake, &mut h, d);
    assert!(h.shows("The note it is about is no longer open."));
    assert!(!h.drawn("mem-say-send"));
    assert!(statements(&fake).is_empty());
}

#[test]
fn a_free_note_follows_navigation() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-say-note");
    h.write("mem-say-text", "We ship on Fridays.");
    h.click("mem-room:decisioni");
    h.frame(vec![]);
    assert!(h.composer().is_active());
    h.tab(Tab::Activity);
    h.tab(Tab::Memories);
    assert!(h.composer().is_active());
    h.click("mem-say-send");
    let s = &statements(&fake)[0];
    assert_eq!(
        (s.about.clone(), s.text.as_str()),
        (None, "We ship on Fridays.")
    );
}

#[test]
fn the_confirmation_is_gated_on_the_checks_too() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-reject:{}", st::key(&m1())));
    h.write("mem-say-text", "no");
    h.click("mem-say-send");
    assert!(h.composer().confirming);
    // The reason is still editable: past the limit, "Yes" does nothing.
    h.panel.composer.as_mut().unwrap().text = "x".repeat(MAX_STATEMENT_BYTES + 1);
    h.frame(vec![]);
    assert!(h.shows("too many"));
    h.click("mem-say-confirm");
    assert!(statements(&fake).is_empty());
    h.panel.composer.as_mut().unwrap().text = "not what we chose".into();
    h.frame(vec![]);
    h.click("mem-say-confirm");
    assert_eq!(statements(&fake).len(), 1);
}

#[test]
fn a_press_sends_once() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-accept:{}", st::key(&m1())));
    // While a send is unanswered, pressing again does nothing.
    h.panel.composer.as_mut().unwrap().sending = true;
    h.frame(vec![]);
    h.click("mem-say-send");
    assert!(statements(&fake).is_empty());
    h.panel.composer.as_mut().unwrap().sending = false;
    h.frame(vec![]);
    let at = h.rect("mem-say-send").center();
    h.click("mem-say-send");
    // The draft is closed once recorded: a second press finds nothing.
    h.frame(vec![Event::PointerMoved(at)]);
    h.frame(vec![pointer(at, true)]);
    h.frame(vec![pointer(at, false)]);
    assert_eq!(statements(&fake).len(), 1);
    // A refusal answers the press: the next press may try again.
    let fake = fresh();
    let mut h = opened(&fake);
    *fake.refuse.lock().unwrap() = Some("busy".into());
    h.click(&format!("mem-say-accept:{}", st::key(&m1())));
    h.click("mem-say-send");
    assert!(!h.composer().sending);
    h.click("mem-say-send");
    assert_eq!(statements(&fake).len(), 2);
}

#[test]
fn a_refused_resume_can_be_tried_again() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    h.click("mem-back");
    *fake.refuse.lock().unwrap() = Some("busy".into());
    h.click("mem-draft-resume");
    assert!(suspended(&h));
    assert!(h.shows("Could not reopen it: busy"));
    assert!(h.drawn("mem-draft-resume"));
    *fake.refuse.lock().unwrap() = None;
    fake.take();
    h.click("mem-draft-resume");
    assert_eq!(only_reads(&fake.take()), vec![open_first()]);
}

#[test]
fn a_note_whose_memory_changed_can_be_renewed_with_its_text() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click("mem-say-note");
    h.write("mem-say-text", "Ask Marco first.");
    let mut d = quiet_detail();
    d.memory.summary = "Releases are signed in CI.".into();
    reanswer(&fake, &mut h, d);
    assert!(suspended(&h));
    assert!(h.shows("Press \"Add a note…\" again"));
    h.click("mem-say-note");
    assert!(h.composer().is_active());
    assert_eq!(h.composer().text, "Ask Marco first.");
    let bound = h.composer().bound.clone().unwrap();
    assert_eq!(bound.snapshot.text, "Releases are signed in CI.");
    h.click("mem-say-send");
    assert_eq!(statements(&fake)[0].about, Some(m1()));
}

#[test]
fn a_failed_refresh_keeps_a_draft_under_the_memory_still_on_show() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    fake.edit(|p| p.open_annotations += 1);
    h.frame(vec![]);
    fake.settle_read(fake.last_read(), ReadState::Failed("busy".into()));
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(h.shows("Could not read the memory: busy"));
    assert!(h.shows(SUMMARY), "the last answer is still drawn");
    assert!(h.composer().is_active());
    h.click("mem-say-send");
    assert_eq!(statements(&fake).len(), 1);
}

// ---------- statements carry what the user saw ----------

#[test]
fn statements_carry_what_was_drawn_when_the_action_was_chosen() {
    // A memory.
    let fake = fake(true);
    let mut h = opened(&fake);
    let drawn = quiet_detail().memory.seen();
    h.click(&format!("mem-say-correct:{}", st::key(&m1())));
    h.write("mem-say-text", "Signed in CI.");
    h.click("mem-say-send");
    assert_eq!(statements(&fake)[0].seen, Some(drawn));
    // A note under a memory: seen with the memory as it was drawn.
    let fake = fresh();
    let mut h = opened(&fake);
    reanswer(&fake, &mut h, detail());
    let shown = detail();
    let c = contradiction(&mid(1));
    let expected = c.seen(Some(&shown.memory));
    h.click(&format!(
        "mem-say-accept:{}",
        st::key(&Subject::Annotation(c.id.clone()))
    ));
    h.click("mem-say-send");
    let s = &statements(&fake)[0];
    assert_eq!(s.seen, Some(expected));
    assert_eq!(s.seen.as_ref().map(|x| &x.subject), s.about.as_ref());
    // A free note carries none.
    let fake = fresh();
    let mut h = workshop(&fake);
    h.click("mem-say-note");
    h.write("mem-say-text", "We ship on Fridays.");
    h.click("mem-say-send");
    assert_eq!(statements(&fake)[0].seen, None);
}

#[test]
fn changed_details_under_the_same_summary_suspend_a_draft() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    let mut d = quiet_detail();
    d.memory.details = "Use the Developer ID certificate in CI.".into();
    reanswer(&fake, &mut h, d.clone());
    assert!(suspended(&h));
    assert!(!h.drawn("mem-say-send"));
    // Choosing the action again binds it to what is drawn now.
    h.click(&format!("mem-say-accept:{}", st::key(&m1())));
    h.click("mem-say-send");
    assert_eq!(statements(&fake)[0].seen, Some(d.memory.seen()));
    // Status too.
    let fake = fresh();
    let mut h = drafting(&fake);
    let mut d = quiet_detail();
    d.memory.status = MemoryStatus::Verified;
    reanswer(&fake, &mut h, d);
    assert!(suspended(&h));
}

#[test]
fn an_engine_refusal_for_a_change_suspends_and_reloads_once() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    *fake.outcome.lock().unwrap() = Some(StatementState::Refused(StatementRefusal::Changed));
    let reads_before = fake.last_read();
    h.click("mem-say-send");
    assert_eq!(statements(&fake).len(), 1);
    assert!(suspended(&h));
    assert_eq!(h.composer().text, "approved in the call");
    assert!(h.shows("choose the action again under it as it is now"));
    assert_eq!(only_reads(&fake.calls()), vec![open_first()], "one reload");
    assert_eq!(fake.last_read(), reads_before + 1, "accepted by the engine");
    *fake.outcome.lock().unwrap() = None;
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert_eq!(only_reads(&fake.calls()).len(), 1, "and only one");
    // The newer content arrives; the draft still waits for the user.
    let mut d = quiet_detail();
    d.memory.standing = Standing::Accepted;
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Memory(Box::new(d.clone()))),
    );
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(suspended(&h));
    assert_eq!(statements(&fake).len(), 1);
    h.click(&format!("mem-say-accept:{}", st::key(&m1())));
    assert_eq!(h.composer().title, "Confirm this decision");
    assert_eq!(h.composer().text, "approved in the call");
    h.click("mem-say-send");
    assert_eq!(statements(&fake)[1].seen, Some(d.memory.seen()));
}

#[test]
fn already_stated_is_said_plainly_and_the_draft_stays_open() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-correct:{}", st::key(&m1())));
    h.write("mem-say-text", "Signed in CI.");
    *fake.outcome.lock().unwrap() = Some(StatementState::Refused(StatementRefusal::AlreadyStated));
    h.click("mem-say-send");
    h.frame(vec![]);
    assert!(h.composer().is_active());
    assert!(h.shows(&StatementRefusal::AlreadyStated.sentence()));
    assert!(h.shows("Change the text, or cancel the draft."));
    assert!(h.drawn("mem-say-send"), "editable and sendable again");
    h.click("mem-say-cancel");
    assert!(h.panel.composer.is_none());
}

// ---------- results arrive later, typed ----------

/// A draft sent with results held back: it waits as "Sending…".
fn sent(fake: &Arc<Fake>) -> H {
    fake.hold.store(true, Ordering::Relaxed);
    let mut h = drafting(fake);
    h.click("mem-say-send");
    assert_eq!(h.composer().pending, Some(fake.last_statement()));
    h
}

#[test]
fn a_sent_draft_waits_for_its_final_result() {
    let fake = fake(true);
    let mut h = sent(&fake);
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert!(h.shows("Sending…"));
    assert!(h.shows("approved in the call"));
    assert!(!h.drawn("mem-say-text"), "not editable");
    assert!(!h.drawn("mem-say-send"));
    assert!(h.panel.said.is_none());
    fake.take();
    fake.publish(
        fake.last_statement(),
        StatementState::Recorded {
            applied: true,
            version: Some(5),
            note: None,
        },
    );
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(h.panel.composer.is_none());
    assert!(h.shows("Recorded as your statement, in version 5."));
    assert_eq!(only_reads(&fake.take()), vec![open_first()], "one refresh");
}

#[test]
fn a_statement_kept_as_a_note_says_so() {
    let fake = fake(true);
    let mut h = sent(&fake);
    fake.publish(
        fake.last_statement(),
        StatementState::Recorded {
            applied: false,
            version: None,
            note: Some("No independent evidence yet.".into()),
        },
    );
    h.frame(vec![]);
    assert!(h.panel.composer.is_none());
    assert!(h.shows(
        "Recorded as an open note; it will be considered at the next dream. No independent evidence yet."
    ));
}

#[test]
fn a_busy_engine_leaves_the_draft_sendable_with_what_was_seen() {
    let fake = fake(true);
    let mut h = sent(&fake);
    fake.publish(
        fake.last_statement(),
        StatementState::Refused(StatementRefusal::Busy),
    );
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(h.shows("A run is publishing; try again when it finishes."));
    assert!(h.drawn("mem-say-send"));
    fake.hold.store(false, Ordering::Relaxed);
    h.click("mem-say-send");
    let sent = statements(&fake);
    assert_eq!(sent.len(), 2);
    assert_eq!(
        sent[0].seen, sent[1].seen,
        "the same seen; the engine checks again"
    );
}

#[test]
fn other_refusals_are_shown_and_never_retried() {
    for refusal in [
        StatementRefusal::NotFound,
        StatementRefusal::Disabled,
        StatementRefusal::Invalid("the text is empty".into()),
        StatementRefusal::Other("the store is locked".into()),
    ] {
        let fake = fake(true);
        let mut h = sent(&fake);
        fake.publish(
            fake.last_statement(),
            StatementState::Refused(refusal.clone()),
        );
        for _ in 0..4 {
            h.frame(vec![]);
        }
        assert!(h.shows(&refusal.sentence()), "{refusal:?}");
        assert_eq!(h.composer().text, "approved in the call", "{refusal:?}");
        assert_eq!(statements(&fake).len(), 1, "{refusal:?}: not retried");
    }
}

#[test]
fn a_stale_statement_refused_later_suspends_and_reloads_once() {
    let fake = fake(true);
    let mut h = sent(&fake);
    fake.take();
    fake.publish(
        fake.last_statement(),
        StatementState::Refused(StatementRefusal::Changed),
    );
    for _ in 0..4 {
        h.frame(vec![]);
    }
    assert!(suspended(&h));
    assert_eq!(h.composer().text, "approved in the call");
    assert!(h.shows(&StatementRefusal::Changed.sentence()));
    assert_eq!(only_reads(&fake.take()), vec![open_first()], "one reload");
    assert!(!h.drawn("mem-say-send"));
}

#[test]
fn a_result_reaches_a_draft_left_behind() {
    // Success while away: closed, with word of it.
    let fake = fake(true);
    let mut h = sent(&fake);
    h.click("mem-back");
    assert!(suspended(&h));
    assert!(h.shows("Sent: waiting for the answer…"));
    fake.publish(
        fake.last_statement(),
        StatementState::Recorded {
            applied: true,
            version: None,
            note: None,
        },
    );
    h.frame(vec![]);
    assert!(h.panel.composer.is_none());
    assert!(h.shows("Recorded as your statement."));
    // A refusal while on another tab: the suspended card shows it.
    let fake = fresh();
    let mut h = sent(&fake);
    h.tab(Tab::Activity);
    fake.publish(
        fake.last_statement(),
        StatementState::Refused(StatementRefusal::NotFound),
    );
    h.frame(vec![]);
    h.tab(Tab::Memories);
    assert!(suspended(&h));
    assert!(h.shows(&StatementRefusal::NotFound.sentence()));
}

#[test]
fn statements_in_a_row_are_not_confused_and_late_results_are_ignored() {
    let fake = fake(true);
    let mut h = sent(&fake);
    let first = fake.last_statement();
    fake.publish(
        first,
        StatementState::Recorded {
            applied: true,
            version: None,
            note: None,
        },
    );
    h.frame(vec![]);
    assert!(h.panel.composer.is_none());
    // A second statement, on a fresh draft.
    reanswer(&fake, &mut h, quiet_detail());
    h.click(&format!("mem-say-correct:{}", st::key(&m1())));
    h.write("mem-say-text", "Signed in CI.");
    h.click("mem-say-send");
    let second = fake.last_statement();
    assert_ne!(first, second);
    assert_eq!(h.composer().pending, Some(second));
    // A late, different answer for the first changes nothing.
    fake.publish(first, StatementState::Refused(StatementRefusal::Busy));
    h.frame(vec![]);
    assert_eq!(h.composer().pending, Some(second));
    assert!(h.composer().error.is_none());
    fake.publish(
        second,
        StatementState::Recorded {
            applied: false,
            version: None,
            note: None,
        },
    );
    h.frame(vec![]);
    assert!(h.panel.composer.is_none());
}

#[test]
fn a_result_that_never_comes_does_not_strand_the_draft() {
    let fake = fake(true);
    let mut h = sent(&fake);
    fake.edit(|p| p.statements.clear());
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(h.composer().pending.is_none());
    assert!(h.shows("No answer came for your statement"));
    assert!(h.drawn("mem-say-send"), "editable again");
    // The memory going away does the same.
    let fake = fresh();
    let mut h = sent(&fake);
    fake.view.lock().unwrap().projects.clear();
    h.frame(vec![]);
    assert!(h.composer().pending.is_none());
    assert_eq!(h.composer().text, "approved in the call");
    assert!(
        h.composer()
            .error
            .as_deref()
            .is_some_and(|e| e.contains("went away"))
    );
}

// ---------- nothing touches a draft that waits for its result ----------

#[test]
fn nothing_replaces_resumes_or_drops_a_draft_waiting_for_its_result() {
    for outcome in [
        StatementState::Recorded {
            applied: true,
            version: None,
            note: None,
        },
        StatementState::Refused(StatementRefusal::Changed),
    ] {
        let case = format!("{outcome:?}");
        let fake = fake(true);
        let mut h = opened(&fake);
        reanswer(&fake, &mut h, detail());
        fake.hold.store(true, Ordering::Relaxed);
        h.click(&format!("mem-say-accept:{}", st::key(&m1())));
        h.write("mem-say-text", "approved in the call");
        h.click("mem-say-send");
        let id = fake.last_statement();
        assert_eq!(h.composer().pending, Some(id), "{case}");
        assert!(
            !h.drawn("mem-say-cancel"),
            "{case}: no cancel while sending"
        );
        let note = Subject::Annotation(contradiction(&mid(1)).id);
        let presses = [
            format!("mem-say-accept:{}", st::key(&m1())),
            format!("mem-say-reject:{}", st::key(&m1())),
            format!("mem-say-correct:{}", st::key(&m1())),
            format!("mem-say-accept:{}", st::key(&note)),
            format!("mem-say-reject:{}", st::key(&note)),
            format!("mem-say-correct:{}", st::key(&note)),
            "mem-say-note".to_owned(),
        ];
        for key in &presses {
            assert!(h.drawn(key), "{case}: {key} is drawn");
            h.click(key);
            assert_eq!(h.composer().pending, Some(id), "{case}: {key}");
            assert_eq!(h.composer().text, "approved in the call", "{case}: {key}");
            assert_eq!(
                h.composer().kind,
                UserStatementKind::Accept,
                "{case}: {key}"
            );
        }
        // Suspended by navigation, still untouchable.
        h.click("mem-back");
        h.frame(vec![]);
        let reads_before = only_reads(&fake.calls()).len();
        assert!(suspended(&h), "{case}");
        assert!(h.shows(st::WAITING), "{case}");
        h.click("mem-draft-resume");
        h.click("mem-draft-discard");
        h.click("mem-say-note");
        assert_eq!(h.composer().pending, Some(id), "{case}");
        assert!(suspended(&h), "{case}: not resumed");
        assert_eq!(
            only_reads(&fake.calls()).len(),
            reads_before,
            "{case}: Resume asked for nothing"
        );
        assert_eq!(statements(&fake).len(), 1, "{case}: one user_statement");
        // The result, when it comes, is handled as usual.
        fake.publish(id, outcome.clone());
        h.frame(vec![]);
        h.frame(vec![]);
        match outcome {
            StatementState::Refused(_) => {
                assert!(suspended(&h), "{case}");
                assert!(h.composer().pending.is_none(), "{case}");
                assert!(h.shows(&StatementRefusal::Changed.sentence()), "{case}");
                assert_eq!(h.composer().text, "approved in the call", "{case}");
                h.click("mem-draft-discard");
                assert!(h.panel.composer.is_none(), "{case}: free again");
            }
            _ => {
                assert!(h.panel.composer.is_none(), "{case}");
                assert!(h.shows("Recorded as your statement."), "{case}");
            }
        }
        assert_eq!(statements(&fake).len(), 1, "{case}");
    }
}

#[test]
fn corrections_and_notes_say_they_wait_for_the_dream() {
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-correct:{}", st::key(&m1())));
    assert!(h.shows(st::KEPT_FOR_DREAM));
    h.click("mem-say-cancel");
    h.click(&format!("mem-say-accept:{}", st::key(&m1())));
    assert!(!h.shows(st::KEPT_FOR_DREAM));
}

#[test]
fn a_superseded_or_archived_memory_offers_no_verdict() {
    let retire: [(&str, Change); 2] = [
        (
            "superseded",
            Box::new(|d: &mut MemoryDetail| {
                d.memory.status = MemoryStatus::Superseded;
            }),
        ),
        (
            "archived",
            Box::new(|d: &mut MemoryDetail| {
                d.memory.address = Address::parse("archivio/voce-1").unwrap();
            }),
        ),
    ];
    for (word, change) in retire {
        // None, one, several successors.
        for successors in [Vec::new(), vec![mid(9)], vec![mid(7), mid(9)]] {
            let case = format!("{word} {}", successors.len());
            let fake = fake(true);
            let mut h = opened(&fake);
            let mut d = quiet_detail();
            change(&mut d);
            // The memory's own relations are not where its successor is.
            d.memory.relations = vec![Relation {
                kind: RelationKind::Supersedes,
                target: mid(4),
            }];
            d.superseded_by = successors.clone();
            reanswer(&fake, &mut h, d);
            let k = st::key(&m1());
            assert!(!h.drawn(&format!("mem-say-accept:{k}")), "{case}");
            assert!(!h.drawn(&format!("mem-say-reject:{k}")), "{case}");
            assert!(h.drawn(&format!("mem-say-correct:{k}")), "{case}");
            assert!(h.drawn("mem-say-note"), "{case}");
            assert!(
                h.shows(&format!(
                    "This memory is {word}: act on the one that replaced it."
                )),
                "{case}"
            );
            assert!(!h.drawn(&format!("mem-successor:{}", mid(4))), "{case}");
            for id in &successors {
                assert!(h.drawn(&format!("mem-successor:{id}")), "{case}");
            }
            if let Some(last) = successors.last() {
                fake.take();
                h.click(&format!("mem-successor:{last}"));
                assert_eq!(
                    only_reads(&fake.take()),
                    vec![ReadRequest::Open(connecto_core::memory::Lookup::Memory(
                        last.clone()
                    ))],
                    "{case}"
                );
            }
        }
    }
    // A live memory still offers them.
    let fake = fake(true);
    let h = opened(&fake);
    assert!(h.drawn(&format!("mem-say-accept:{}", st::key(&m1()))));
    assert!(!h.shows("act on the one that replaced it"));
}

#[test]
fn the_engines_refusal_for_a_retired_memory_is_shown_if_it_comes() {
    let fake = fake(true);
    let refusal = StatementRefusal::Invalid(
        "this memory is superseded: act on the one that replaced it".into(),
    );
    *fake.outcome.lock().unwrap() = Some(StatementState::Refused(refusal.clone()));
    let mut h = drafting(&fake);
    h.click("mem-say-send");
    h.frame(vec![]);
    assert!(h.shows(&refusal.sentence()));
    assert_eq!(h.composer().text, "approved in the call");
}

#[test]
fn the_statement_limit_is_the_engines() {
    const { assert!(MAX_STATEMENT_BYTES < connecto_core::memory::MAX_NOTE_BYTES) };
    let fake = fake(true);
    let mut h = opened(&fake);
    h.click(&format!("mem-say-correct:{}", st::key(&m1())));
    // Within the note limit but over the statement limit: blocked here.
    h.panel.composer.as_mut().unwrap().text = "x".repeat(MAX_STATEMENT_BYTES + 1);
    h.frame(vec![]);
    assert!(h.shows("1 too many"));
    h.click("mem-say-send");
    assert!(statements(&fake).is_empty());
    h.panel.composer.as_mut().unwrap().text = "x".repeat(MAX_STATEMENT_BYTES);
    h.frame(vec![]);
    h.click("mem-say-send");
    assert_eq!(statements(&fake).len(), 1);
}

#[test]
fn a_judgment_on_a_note_promises_nothing_at_once() {
    // Notes carrying what they would become, and plain ones.
    let mut notes = Vec::new();
    for kind in MemoryKind::ALL {
        for standing in Standing::allowed_for(*kind) {
            let mut a = note(5, AnnotationKind::Integration, "x", Some(mid(1)));
            a.memory_kind = Some(*kind);
            a.standing = Some(*standing);
            notes.push(a);
        }
    }
    for kind in AnnotationKind::ALL {
        notes.push(note(6, *kind, "x", Some(mid(1))));
    }
    for a in notes {
        let w = st::for_annotation(&a);
        let case = format!("{:?} {:?} {:?}", a.kind, a.memory_kind, a.standing);
        assert!(w.meaning.ends_with(st::ON_A_NOTE), "{case}: {}", w.meaning);
        // No promised effect (the standing's own name may appear).
        for promise in [
            "verified on your word",
            "in conflict",
            "makes it",
            "marks it",
            "back to",
        ] {
            assert!(!w.meaning.contains(promise), "{case}: {}", w.meaning);
        }
    }
    // The labels still follow what the note claims.
    let mut proposal = note(5, AnnotationKind::Integration, "Use SQLite.", None);
    proposal.memory_kind = Some(MemoryKind::Decision);
    proposal.standing = Some(Standing::Proposed);
    let w = st::for_annotation(&proposal);
    assert_eq!(w.accept, "Accept this decision");
    assert!(
        w.meaning
            .starts_with("This note proposes a proposed decision.")
    );
    // A memory's own explanation is still its immediate effect.
    assert!(
        !st::for_memory(MemoryKind::Decision, Standing::Proposed)
            .meaning
            .contains(st::ON_A_NOTE)
    );
}

// ---------- Ask: a question to the memory ----------

fn asked(fake: &Arc<Fake>, question: &str) -> H {
    let mut h = H::of(fake);
    h.type_into("mem-ask-field", question);
    h
}

#[test]
fn a_question_is_sent_pending_then_answered_with_its_memories_as_links() {
    use connecto_core::memory::ask_choices;
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.type_into("mem-ask-field", "what did we decide about releases?");
    let sent = fake.take();
    assert_eq!(
        sent,
        vec![Call::Ask(AskRequest {
            question: "what did we decide about releases?".into(),
            choice: ask_choices()[0].clone(),
        })],
        "one question, with the default cheap model"
    );
    assert!(h.panel.ask.text.is_empty(), "the text is cleared once sent");
    h.frame(vec![]);
    assert!(h.shows("Asking…"));
    fake.settle_ask(
        fake.next_ask.load(Ordering::Relaxed),
        AskState::Answered {
            text: "Releases are signed on the maintainer's Mac.".into(),
            used: vec!["officina/voce-1".into()],
            version: 2,
        },
    );
    h.frame(vec![]);
    assert!(h.shows("Releases are signed on the maintainer's Mac."));
    assert!(h.drawn("mem-ask-from:officina/voce-1"));
    h.click("mem-ask-from:officina/voce-1");
    assert_eq!(
        fake.take(),
        vec![Call::Read(ReadRequest::Open(
            connecto_core::memory::Lookup::Address(Address::parse("officina/voce-1").unwrap())
        ))]
    );
    assert!(
        h.panel.browse.detail.is_some(),
        "the memory opens in the reading page"
    );
}

#[test]
fn a_failed_or_refused_question_says_why_and_keeps_its_text() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    *fake.refuse.lock().unwrap() = Some("memory is off for this project".into());
    h.type_into("mem-ask-field", "anything");
    assert!(h.shows("memory is off for this project"));
    assert_eq!(h.panel.ask.text, "anything", "kept to send again");
    *fake.refuse.lock().unwrap() = None;
    h.click("mem-ask");
    let id = fake.next_ask.load(Ordering::Relaxed);
    fake.settle_ask(id, AskState::Failed("the model timed out".into()));
    h.frame(vec![]);
    assert!(h.shows("No answer: the model timed out"));
}

#[test]
fn an_empty_question_is_not_sent() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.click("mem-ask");
    h.type_into("mem-ask-field", "   ");
    assert!(fake.calls().is_empty());
}

#[test]
fn no_third_question_goes_while_two_are_still_waiting() {
    let fake = fake(true);
    let mut h = asked(&fake, "first question");
    fake.take();
    h.type_into("mem-ask-field", "second question");
    assert_eq!(fake.take().len(), 1, "two may wait at once");
    h.type_into("mem-ask-field", "third question");
    assert!(
        fake.take().is_empty(),
        "a third waits its turn: nothing is sent twice"
    );
    assert!(h.shows("Two questions are already waiting for an answer."));
    fake.settle_ask(
        1,
        AskState::Answered {
            text: "done".into(),
            used: Vec::new(),
            version: 1,
        },
    );
    h.frame(vec![]);
    h.type_into("mem-ask-field", "third question");
    assert_eq!(fake.take().len(), 1, "sent once one is answered");
}

#[test]
fn the_answers_are_listed_newest_first() {
    let fake = fake(true);
    let mut h = asked(&fake, "older question");
    fake.settle_ask(
        1,
        AskState::Answered {
            text: "older answer".into(),
            used: Vec::new(),
            version: 1,
        },
    );
    h.type_into("mem-ask-field", "newer question");
    fake.settle_ask(
        2,
        AskState::Answered {
            text: "newer answer".into(),
            used: Vec::new(),
            version: 1,
        },
    );
    h.frame(vec![]);
    assert!(h.line("newer answer") < h.line("older answer"));
}

// ---------- the three tabs ----------

#[test]
fn three_tabs_with_memories_first_and_by_default() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    for key in ["mem-tab:memories", "mem-tab:activity", "mem-tab:settings"] {
        assert!(h.drawn(key), "{key}");
    }
    assert!(!h.drawn("mem-tab:overview"));
    assert_eq!(h.panel.tab, Tab::Memories);
    h.tab(Tab::Activity);
    assert!(h.drawn("mem-dream-now"));
    assert!(!h.drawn("mem-search-field"), "search is the Memories tab's");
}

/// Every setting is on the Settings tab, and a value typed unchanged sends
/// nothing: only a real change is saved.
#[test]
fn settings_are_all_on_one_tab_and_only_real_changes_save() {
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.tab(Tab::Settings);
    for key in [
        "mem-dev-group",
        "mem-dream-time",
        "mem-dream-minutes",
        "mem-export:json",
        "mem-delete",
    ] {
        assert!(h.drawn(key), "{key}");
    }
    let default_group = connecto_core::memory::DevelopmentSettings::default().group_seconds;
    h.type_into("mem-dev-group", &default_group.to_string());
    assert!(fake.calls().is_empty(), "the same value saves nothing");
    h.type_into("mem-dev-group", "120");
    let saved = saved(&fake);
    assert_eq!(saved.development.group_seconds, 120);
}

/// An expandable refusal sits under its run in Activity and opens on request.
#[test]
fn a_run_with_refusals_opens_its_details_in_activity() {
    let fake = fake(true);
    fake.edit(|p| {
        p.runs = Arc::new(vec![run(
            3,
            connecto_core::memory::Cycle::Dream,
            RunOutcome::Refused {
                proposed: 4,
                reason: "4 proposed, 0 kept".into(),
            },
        )]);
        p.last_report = Some(Arc::new(DreamReport {
            run: 3,
            rejected: vec!["session Dream 2, operation 3: no source in the packet".into()],
            ..DreamReport::default()
        }));
    });
    let mut h = H::of(&fake);
    h.tab(Tab::Activity);
    assert!(h.shows("#3 Dream"));
    assert!(h.shows("Nothing kept: 4 proposed, 0 kept"));
    assert!(
        !h.shows("Refused because: no source in the packet"),
        "closed at first"
    );
    h.click("mem-run-details:3");
    assert!(h.shows("Refused because: no source in the packet"));
    h.click("mem-run-details:3");
    assert!(!h.shows("Refused because: no source in the packet"));
}

// ---------- the default view: every memory of the palace ----------

/// The brief's read, while it waits for its answer.
fn brief_read(fake: &Fake) -> u64 {
    fake.view
        .lock()
        .unwrap()
        .projects
        .iter()
        .flat_map(|p| p.reads.iter())
        .find(|r| r.request == ReadRequest::Brief && r.state == ReadState::Pending)
        .map_or(0, |r| r.id)
}

/// The palace on the Memories tab, its default view asked for but not yet
/// answered.
fn palace_unanswered(fake: &Arc<Fake>) -> H {
    palace_view(fake);
    let mut h = H::of(fake);
    h.tab(Tab::Memories);
    h.frame(vec![]);
    h
}

/// The palace's room reads that are still waiting: (request id, room).
fn pending_room_reads(fake: &Fake) -> Vec<(u64, Room)> {
    fake.view
        .lock()
        .unwrap()
        .projects
        .iter()
        .flat_map(|p| p.reads.iter())
        .filter_map(|r| match (&r.request, &r.state) {
            (ReadRequest::Room(room), ReadState::Pending) => Some((r.id, *room)),
            _ => None,
        })
        .collect()
}

/// Answer every waiting room read with one memory of that room; `fail` makes
/// one room's read fail instead.
fn answer_room_reads(fake: &Fake, fail: Option<Room>) {
    answer_room_reads_tagged(fake, fail, "A");
}

/// As [`answer_room_reads`], with each memory's summary starting with `tag`.
fn answer_room_reads_tagged(fake: &Fake, fail: Option<Room>, tag: &str) {
    for (id, room) in pending_room_reads(fake) {
        let state = if fail == Some(room) {
            ReadState::Failed("busy".into())
        } else {
            let n = Room::ALL.iter().position(|r| *r == room).unwrap_or(0) as u8 + 1;
            ReadState::Ready(ReadAnswer::Room(vec![memory_card(
                n,
                &format!("{tag} memory of {}", room.label()),
                Vec::new(),
            )]))
        };
        fake.settle_read(id, state);
    }
}

#[test]
fn the_default_view_lists_memories_from_several_rooms_with_no_interaction() {
    let fake = fake(true);
    let mut h = palace_unanswered(&fake);
    assert_eq!(
        only_reads(&fake.calls()),
        vec![
            ReadRequest::Room(Room::Architecture),
            ReadRequest::Room(Room::Decisions),
            ReadRequest::Room(Room::Archive),
        ],
        "one read per room that holds memories, in room order"
    );
    answer_room_reads(&fake, None);
    h.frame(vec![]);
    assert!(h.shows("A memory of Architecture"));
    assert!(h.shows("A memory of Decisions"));
    assert!(h.shows("A memory of Archive"));
    // Decisions holds three and the answer gave one: the rest is one click away.
    assert!(h.shows("and 2 more"));
    assert!(h.drawn("mem-all-more:decisioni"));
    for _ in 0..3 {
        h.frame(vec![]);
    }
    assert_eq!(
        only_reads(&fake.calls()).len(),
        3,
        "not asked again every frame"
    );
}

/// A change while a batch of room reads is in flight asks for nothing new
/// until that batch has arrived. The last complete list stays drawn, marked
/// as refreshing, and then one batch for the latest key replaces it.
#[test]
fn changes_while_a_batch_is_in_flight_wait_for_it_and_end_on_the_latest_key() {
    let fake = fake(true);
    let mut h = palace_unanswered(&fake);
    assert_eq!(only_reads(&fake.calls()).len(), 3);
    for _ in 0..5 {
        fake.edit(|p| p.version += 1);
        h.frame(vec![]);
        h.later(REFRESH_SPACING_MS);
        h.frame(vec![]);
    }
    assert_eq!(
        only_reads(&fake.calls()).len(),
        3,
        "nothing new while the batch is in flight"
    );
    answer_room_reads_tagged(&fake, None, "OLD");
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief".into())),
    );
    h.frame(vec![]);
    assert!(
        h.shows("OLD memory of Architecture"),
        "the last complete list stays drawn"
    );
    assert!(h.shows("Refreshing…"));
    h.frame(vec![]);
    assert_eq!(
        only_reads(&fake.calls()).len(),
        6,
        "one batch for the latest key"
    );
    answer_room_reads_tagged(&fake, None, "NEW");
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief".into())),
    );
    h.frame(vec![]);
    assert!(h.shows("NEW memory of Architecture"));
    assert!(!h.shows("Refreshing…"), "nothing left in flight");
    assert!(!h.shows("Loading the memories"), "nothing stuck loading");
    h.frame(vec![]);
    assert_eq!(only_reads(&fake.calls()).len(), 6, "and no more");
}

#[test]
fn one_room_failing_does_not_hide_the_others() {
    let fake = fake(true);
    let mut h = palace_unanswered(&fake);
    answer_room_reads(&fake, Some(Room::Decisions));
    h.frame(vec![]);
    assert!(h.shows("A memory of Architecture"));
    assert!(h.shows("A memory of Archive"));
    assert!(!h.shows("A memory of Decisions"));
    assert!(h.shows("Could not read this room: busy"));
    let before = only_reads(&fake.calls()).len();
    h.click("mem-all-retry:decisioni");
    let reads = only_reads(&fake.calls());
    assert_eq!(reads.len(), before + 1, "only that room is read again");
    assert_eq!(reads.last(), Some(&ReadRequest::Room(Room::Decisions)));
}

#[test]
fn selecting_a_room_narrows_and_all_returns_to_everything() {
    let fake = fake(true);
    let mut h = palace(&fake);
    h.frame(vec![]);
    answer_room_reads(&fake, None);
    h.frame(vec![]);
    let before = only_reads(&fake.calls()).len();
    h.click("mem-room:decisioni");
    fake.settle_read(
        fake.last_read(),
        ReadState::Ready(ReadAnswer::Room(vec![memory_card(
            20,
            "Only a decision",
            Vec::new(),
        )])),
    );
    h.frame(vec![]);
    assert!(h.shows("Only a decision"));
    assert!(!h.shows("A memory of Architecture"), "narrowed to the room");
    h.click("mem-rooms");
    assert!(h.shows("A memory of Architecture"), "All is back");
    assert_eq!(
        only_reads(&fake.calls()).len(),
        before + 1,
        "All reads nothing new"
    );
}

/// Typed without Enter, the question goes with one click on Ask: the field is
/// bound to the live draft, so the button sees what is typed.
#[test]
fn a_question_typed_without_enter_is_sent_by_one_click_on_ask() {
    use connecto_core::memory::ask_choices;
    let fake = fake(true);
    let mut h = H::of(&fake);
    h.click("mem-ask-field");
    h.frame(vec![Event::Text("what is the release process?".into())]);
    h.click("mem-ask");
    assert_eq!(
        fake.take(),
        vec![Call::Ask(AskRequest {
            question: "what is the release process?".into(),
            choice: ask_choices()[0].clone(),
        })],
        "exactly one question, with the typed text"
    );
}

/// The same for search: typed without Enter, one click on Search runs it.
#[test]
fn a_search_typed_without_enter_runs_with_one_click() {
    let fake = fake(true);
    let mut h = workshop(&fake);
    h.click("mem-search-field");
    h.frame(vec![Event::Text("release".into())]);
    h.click("mem-search");
    assert_eq!(
        only_reads(&fake.take()),
        vec![ReadRequest::Search {
            query: "release".into(),
            room: None,
            scope: None,
            all: false,
        }]
    );
}

/// A statement's text is bound to the live draft too: sent by one click.
#[test]
fn a_statement_typed_without_blur_is_sent_by_one_click() {
    let fake = fake(true);
    let mut h = drafting(&fake);
    h.click("mem-say-send");
    let sent = fake.take();
    assert_eq!(
        sent.iter()
            .filter(|c| matches!(c, Call::Statement(_)))
            .count(),
        1,
        "one statement, sent at the first click"
    );
}

// ---------- the palace cache: timeouts, partial rooms, the real history ----------

/// Every room that holds memories has one memory: seven rooms in all.
fn seven_rooms(fake: &Arc<Fake>) -> H {
    fake.edit(|p| {
        p.version = 3;
        p.totals = StatusCounts {
            verified: 7,
            ..StatusCounts::default()
        };
        p.rooms = Room::ALL
            .iter()
            .filter(|r| r.holds_memories())
            .map(|r| RoomCount {
                room: *r,
                counts: StatusCounts {
                    verified: 1,
                    ..StatusCounts::default()
                },
            })
            .collect();
    });
    let mut h = H::of(fake);
    h.tab(Tab::Memories);
    h
}

/// Reads that never complete end as failed after the timeout: each room says
/// so and offers its own retry, and a newer key asks again.
#[test]
fn reads_that_never_complete_time_out_and_refreshing_goes_on() {
    use super::fetch::SLOT_TIMEOUT_MS;
    let fake = fake(true);
    let mut h = palace_unanswered(&fake);
    assert_eq!(only_reads(&fake.calls()).len(), 3);
    h.later(SLOT_TIMEOUT_MS);
    h.frame(vec![]);
    assert!(h.shows("No answer in time."));
    assert!(
        h.drawn("mem-all-retry:architettura"),
        "each room offers its own retry"
    );
    // Nothing is stuck: a newer key is asked for, after the spacing.
    fake.edit(|p| p.version += 1);
    h.later(REFRESH_SPACING_MS);
    h.frame(vec![]);
    assert_eq!(
        only_reads(&fake.calls()).len(),
        6,
        "the newer key asks again"
    );
    answer_room_reads(&fake, None);
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief".into())),
    );
    h.frame(vec![]);
    assert!(h.shows("A memory of Architecture"));
    assert!(!h.shows("No answer in time."));
}

/// A room that has answered is drawn at once, while another still loads.
#[test]
fn a_room_that_has_arrived_is_shown_while_another_still_loads() {
    let fake = fake(true);
    let mut h = palace_unanswered(&fake);
    let (id, _) = pending_room_reads(&fake)
        .into_iter()
        .find(|(_, r)| *r == Room::Architecture)
        .expect("architecture is asked for");
    fake.settle_read(
        id,
        ReadState::Ready(ReadAnswer::Room(vec![memory_card(
            1,
            "ARCH memory",
            Vec::new(),
        )])),
    );
    h.frame(vec![]);
    assert!(h.shows("ARCH memory"), "the arrived room is drawn");
    assert!(h.shows("Architecture · 1 memory"));
    assert!(h.shows("Loading…"), "the others say they are loading");
    assert!(!h.shows("Loading the memories…"));
    answer_room_reads(&fake, None);
    h.frame(vec![]);
    assert!(h.shows("A memory of Decisions"));
}

/// Seven populated rooms, refreshed against the engine's eight-entry history:
/// the batch evicts every earlier answer there, and the page still shows each
/// room's memories, never "no longer kept".
#[test]
fn a_refresh_with_seven_populated_rooms_keeps_every_room_on_show() {
    let fake = fake(true);
    let mut h = seven_rooms(&fake);
    h.frame(vec![]);
    answer_room_reads(&fake, None);
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief".into())),
    );
    h.frame(vec![]);
    for room in [
        "Architecture",
        "Workshop",
        "Decisions",
        "Problems",
        "Open work",
        "Conventions",
        "Archive",
    ] {
        assert!(h.shows(&format!("A memory of {room}")), "{room}");
    }
    let before = fake.calls().len();
    fake.edit(|p| p.version += 1);
    h.later(REFRESH_SPACING_MS);
    h.frame(vec![]);
    assert_eq!(
        fake.calls().len() - before,
        7,
        "one request per populated room"
    );
    assert!(h.shows("Refreshing…"));
    assert!(
        !h.shows("no longer kept"),
        "nothing drawn from the evicted history"
    );
    for room in [
        "Architecture",
        "Workshop",
        "Decisions",
        "Problems",
        "Open work",
        "Conventions",
        "Archive",
    ] {
        assert!(h.shows(room), "{room} stays on show while it refreshes");
    }
    answer_room_reads(&fake, None);
    fake.settle_read(
        brief_read(&fake),
        ReadState::Ready(ReadAnswer::Brief("the brief".into())),
    );
    h.frame(vec![]);
    assert!(!h.shows("Refreshing…"));
    assert!(!h.shows("no longer kept"));
}

/// A memory on show is not disturbed by the list's refresh: no room is asked
/// for while the reading page is open, so nothing can evict it.
#[test]
fn a_memory_on_show_is_not_disturbed_by_the_list_refresh() {
    let fake = fake(true);
    let mut h = opened(&fake);
    fake.edit(|p| p.version += 1);
    h.later(REFRESH_SPACING_MS);
    h.frame(vec![]);
    h.frame(vec![]);
    assert!(
        only_reads(&fake.calls())
            .iter()
            .all(|r| !matches!(r, ReadRequest::Room(_))),
        "no room is read while a memory is on show"
    );
    assert!(h.panel.browse.detail.is_some(), "the memory stays on show");
}

/// Functional memory that records Update now, with a status set by the
/// test (idle and built by default).
#[derive(Default)]
struct FakeFunctionalMemory {
    updates: std::sync::Mutex<u32>,
    unknown: std::sync::atomic::AtomicBool,
    /// What the switch and the model were set to.
    autos: std::sync::Mutex<Vec<bool>>,
    models: std::sync::Mutex<Vec<Option<connecto_core::functional_memory::FmModel>>>,
    /// A status of the test's own, in place of the idle one.
    status: std::sync::Mutex<Option<connecto_core::functional_memory::FmStatus>>,
}

fn fm_model(
    provider: connecto_core::memory::Provider,
    model: &str,
) -> connecto_core::functional_memory::FmModel {
    connecto_core::functional_memory::FmModel::new(provider, model)
}

impl connecto_core::functional_memory::FunctionalMemoryControl for FakeFunctionalMemory {
    fn status(&self, _: &str) -> connecto_core::functional_memory::FmStatus {
        if let Some(status) = self.status.lock().unwrap().clone() {
            return status;
        }
        if self.unknown.load(std::sync::atomic::Ordering::SeqCst) {
            return connecto_core::functional_memory::FmStatus {
                state: connecto_core::functional_memory::FmState::Unknown,
                message: Some("Asking that machine for it…".into()),
                ..Default::default()
            };
        }
        connecto_core::functional_memory::FmStatus {
            state: connecto_core::functional_memory::FmState::Idle,
            commit: Some("abc1234def".into()),
            updated_ms: Some(NOW),
            message: None,
            auto: true,
            model: Some(fm_model(connecto_core::memory::Provider::Claude, "sonnet")),
            chosen: None,
            models_known: true,
            model_choice: true,
            models: vec![
                fm_model(connecto_core::memory::Provider::Claude, "sonnet"),
                fm_model(connecto_core::memory::Provider::Codex, "gpt-6.2-sol"),
            ],
        }
    }
    fn update_now(&self, _: &str) -> Result<(), String> {
        *self.updates.lock().unwrap() += 1;
        Ok(())
    }
    fn set_auto(&self, _: &str, on: bool) -> Result<(), String> {
        self.autos.lock().unwrap().push(on);
        Ok(())
    }
    fn set_model(
        &self,
        _: &str,
        model: Option<connecto_core::functional_memory::FmModel>,
    ) -> Result<(), String> {
        self.models.lock().unwrap().push(model);
        Ok(())
    }
}

/// A project viewed from another machine shows its functional memory, and
/// Update now, the automatic switch and the model are asked of that machine
/// as they are here.
#[test]
fn a_remote_projects_functional_memory_can_be_updated_from_here() {
    let fake = fake(false);
    let mut s = Settings::new(&fake);
    let memory = Arc::new(FakeFunctionalMemory::default());
    s.functional_memory = Arc::clone(&memory) as _;
    s.remote = true;
    s.tab = crate::project_settings::ProjectTab::Memory;
    s.frame(vec![]);
    s.frame(vec![]);
    assert!(
        s.marks
            .texts
            .iter()
            .any(|t| t.contains("describes abc1234"))
    );
    s.click("fm-update");
    assert_eq!(*memory.updates.lock().unwrap(), 1);
    assert!(s.drawn("fm-auto"));
    s.click("fm-auto");
    assert_eq!(*memory.autos.lock().unwrap(), [false]);
    assert!(s.drawn("fm-model-default"));
    s.click("fm-model:codex:gpt-6.2-sol");
    assert_eq!(
        *memory.models.lock().unwrap(),
        [Some(fm_model(
            connecto_core::memory::Provider::Codex,
            "gpt-6.2-sol"
        ))]
    );
    assert_eq!(s.sheet.error, None);
}

/// Another machine's card is asked again every few seconds even while
/// idle, and says nothing is known (not "not built") until that machine
/// answers.
#[test]
fn a_remote_card_refreshes_while_idle_and_waits_for_the_first_answer() {
    let fake = fake(false);
    let mut s = Settings::new(&fake);
    let memory = Arc::new(FakeFunctionalMemory::default());
    s.functional_memory = Arc::clone(&memory) as _;
    s.remote = true;
    s.tab = crate::project_settings::ProjectTab::Memory;
    s.frame(vec![]);
    s.frame(vec![]);
    assert!(
        s.repaint <= std::time::Duration::from_secs(5),
        "idle, still asked again: {:?}",
        s.repaint
    );
    memory
        .unknown
        .store(true, std::sync::atomic::Ordering::SeqCst);
    s.frame(vec![]);
    s.frame(vec![]);
    assert!(!s.marks.texts.iter().any(|t| t.contains("Not built yet")));
    assert!(
        s.marks
            .texts
            .iter()
            .any(|t| t.contains("Asking that machine"))
    );
}

/// A chosen model whose CLI cannot run there now stays shown and can be
/// cleared with Default; with none that can run, the card says how to fix
/// it instead of checking forever.
#[test]
fn an_unavailable_choice_can_be_cleared_and_no_cli_is_said() {
    use connecto_core::functional_memory::{FmState, FmStatus};
    use connecto_core::memory::Provider;
    let fake = fake(false);
    let mut s = Settings::new(&fake);
    let memory = Arc::new(FakeFunctionalMemory::default());
    *memory.status.lock().unwrap() = Some(FmStatus {
        state: FmState::Idle,
        model: Some(fm_model(Provider::Claude, "sonnet")),
        chosen: Some(fm_model(Provider::Codex, "gpt-6.2-sol")),
        models: vec![fm_model(Provider::Claude, "sonnet")],
        models_known: true,
        model_choice: true,
        ..FmStatus::default()
    });
    s.functional_memory = Arc::clone(&memory) as _;
    s.tab = crate::project_settings::ProjectTab::Memory;
    s.frame(vec![]);
    s.frame(vec![]);
    assert!(
        s.drawn("fm-model:codex:gpt-6.2-sol"),
        "the choice stays shown"
    );
    assert!(
        s.marks
            .texts
            .iter()
            .any(|t| t.contains("cannot run here now"))
    );
    s.click("fm-model-default");
    assert_eq!(*memory.models.lock().unwrap(), [None]);
    *memory.status.lock().unwrap() = Some(FmStatus {
        state: FmState::None,
        models_known: true,
        model_choice: true,
        ..FmStatus::default()
    });
    s.frame(vec![]);
    s.frame(vec![]);
    assert!(
        s.marks
            .texts
            .iter()
            .any(|t| t.contains("No CLI can run it here"))
    );
    assert!(!s.marks.texts.iter().any(|t| t.contains("checking")));
}

/// Another machine from before the model choice: its card says the model is
/// chosen there once it is updated, and Update now still works.
#[test]
fn an_older_host_offers_no_model_but_still_updates() {
    use connecto_core::functional_memory::{FmState, FmStatus};
    let fake = fake(false);
    let mut s = Settings::new(&fake);
    let memory = Arc::new(FakeFunctionalMemory::default());
    *memory.status.lock().unwrap() = Some(FmStatus {
        state: FmState::Idle,
        auto: true,
        ..FmStatus::default()
    });
    s.functional_memory = Arc::clone(&memory) as _;
    s.remote = true;
    s.tab = crate::project_settings::ProjectTab::Memory;
    s.frame(vec![]);
    s.frame(vec![]);
    assert!(
        s.marks
            .texts
            .iter()
            .any(|t| t.contains("once its Connecto is updated"))
    );
    assert!(!s.marks.texts.iter().any(|t| t.contains("checking")));
    s.click("fm-update");
    assert_eq!(*memory.updates.lock().unwrap(), 1);
}
