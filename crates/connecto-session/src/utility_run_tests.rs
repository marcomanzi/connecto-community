//! The runner against scripted sessions of both providers: no CLI, no model.

#![allow(clippy::unwrap_used, clippy::panic)]

use std::collections::VecDeque;
use std::sync::atomic::AtomicUsize;

use connecto_core::EventText;
use serde_json::json;

use super::*;

/// A short pace, so a run takes milliseconds.
fn pace() -> Pace {
    Pace {
        poll: Duration::from_millis(5),
        submit_delay: Duration::from_millis(1),
        settle_still: Duration::from_millis(10),
        settle_limit: Duration::from_millis(200),
        ready_stable: Duration::from_millis(10),
        block_grace: Duration::from_millis(100),
        boot_limit: Duration::from_millis(500),
        submit_limit: Duration::from_millis(300),
        fallback_ready: Duration::from_millis(100),
        log_wait: Duration::from_millis(400),
        log_poll: Duration::from_millis(5),
    }
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    Claude,
    Codex,
}

/// What the fake does when Enter is pressed: events to emit, and a log to
/// write (now or a little later).
type OnEnter = Box<dyn Fn(&Fake) + Send + Sync>;

struct Fake {
    kind: Kind,
    profile: PathBuf,
    events: Mutex<VecDeque<ToolEvent>>,
    pasted: AtomicBool,
    submitted: AtomicBool,
    running: AtomicBool,
    sent: Mutex<Vec<Vec<u8>>>,
    on_enter: OnEnter,
    enters: AtomicUsize,
    /// A screen shown instead of the composer.
    shown: Mutex<Option<String>>,
    /// Pastes the composer loses (typed before it takes input).
    lose_pastes: AtomicUsize,
}

impl Fake {
    fn new(kind: Kind, name: &str, on_enter: OnEnter) -> Self {
        let profile =
            std::env::temp_dir().join(format!("mg-utilrun-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&profile);
        std::fs::create_dir_all(&profile).unwrap();
        Self {
            kind,
            profile,
            events: Mutex::new(VecDeque::new()),
            pasted: AtomicBool::new(false),
            submitted: AtomicBool::new(false),
            running: AtomicBool::new(true),
            sent: Mutex::new(Vec::new()),
            on_enter,
            enters: AtomicUsize::new(0),
            shown: Mutex::new(None),
            lose_pastes: AtomicUsize::new(0),
        }
    }

    fn push(&self, event: ToolEvent) {
        self.events.lock().unwrap().push_back(event);
    }

    fn log_path(&self, name: &str) -> PathBuf {
        match self.kind {
            Kind::Claude => self.profile.join("projects/-p").join(name),
            Kind::Codex => self.profile.join("sessions/2026/11/14").join(name),
        }
    }

    fn write_log(&self, name: &str, text: &str) {
        let path = self.log_path(name);
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(path, text).unwrap();
    }

    fn tool(&self) -> &'static dyn UtilityTool {
        match self.kind {
            Kind::Claude => adapter(Tool::Claude).utility().unwrap(),
            Kind::Codex => adapter(Tool::Codex).utility().unwrap(),
        }
    }

    fn talk<'a>(
        &'a self,
        prompt: &'a str,
        strict: bool,
        deadline: Instant,
        cancelled: &'a dyn Fn() -> bool,
    ) -> Talk<'a> {
        Talk {
            conv: self,
            tool: self.tool(),
            name: match self.kind {
                Kind::Claude => "Claude",
                Kind::Codex => "Codex",
            },
            prompt,
            strict,
            deadline,
            cancelled,
            pace: pace(),
        }
    }
}

impl Drop for Fake {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.profile);
    }
}

impl Conversation for Fake {
    fn screen(&self) -> String {
        if let Some(shown) = self.shown.lock().unwrap().clone() {
            return shown;
        }
        let pasted = self.pasted.load(Ordering::SeqCst);
        let submitted = self.submitted.load(Ordering::SeqCst);
        match self.kind {
            Kind::Claude => {
                let typed = if pasted && !submitted {
                    "[Pasted text #1]"
                } else {
                    ""
                };
                format!("─────\n❯ {typed}\n─────\n  ? for shortcuts\n")
            }
            Kind::Codex => {
                let typed = if pasted && !submitted {
                    "[Pasted Content 9000 chars]"
                } else {
                    "Ask Codex to do anything"
                };
                format!("› {typed}\n\n  gpt-6 · ~/p\n")
            }
        }
    }

    fn running(&self) -> bool {
        self.running.load(Ordering::SeqCst)
    }

    fn bracketed_paste(&self) -> bool {
        true
    }

    fn send(&self, bytes: Vec<u8>) -> bool {
        if bytes == b"\r" {
            // Enter on an empty composer does nothing.
            if self.pasted.load(Ordering::SeqCst) {
                self.submitted.store(true, Ordering::SeqCst);
                if self.enters.fetch_add(1, Ordering::SeqCst) == 0 {
                    (self.on_enter)(self);
                }
            }
        } else if self.lose_pastes.load(Ordering::SeqCst) > 0 {
            self.lose_pastes.fetch_sub(1, Ordering::SeqCst);
        } else {
            self.pasted.store(true, Ordering::SeqCst);
        }
        self.sent.lock().unwrap().push(bytes);
        true
    }

    fn next_event(&self) -> Option<ToolEvent> {
        self.events.lock().unwrap().pop_front()
    }

    fn hook_ctx(&self) -> Option<HookCtx> {
        Some(HookCtx {
            profile: self.profile.clone(),
            hooks: false,
        })
    }
}

const SESSION: &str = "626d5795-8d96-4bf8-8c00-cd1f8e0148c7";
const THREAD: &str = "01a1a641-e9d8-79a0-8164-6bd15f72cb16";
const TITLE_THREAD: &str = "01a1a642-0dff-7f31-bf9a-f329fb42fdc8";

fn soon() -> Instant {
    Instant::now() + Duration::from_secs(10)
}

fn never() -> bool {
    false
}

fn long_prompt() -> String {
    let lines: Vec<String> = (0..4000)
        .map(|n| format!("{n:05} filler line about the project memory evidence"))
        .collect();
    format!("TASK\n{}\nFINAL WORD", lines.join("\n"))
}

fn claude_started(fake: &Fake) {
    fake.push(ToolEvent::Started {
        tool_session: EventText::new(SESSION.into()),
        turn_log: Some(EventText::new(
            fake.log_path(&format!("{SESSION}.jsonl"))
                .to_string_lossy()
                .into_owned(),
        )),
    });
}

fn claude_turn_end(reply: Reply) -> ToolEvent {
    ToolEvent::TurnEnded {
        input: Vec::new(),
        input_total: 0,
        reply,
        lifecycle_known: true,
        background: Vec::new(),
        thread: None,
        turn: None,
        failure: None,
        continued: false,
    }
}

fn claude_log(prompt: &str, reply: Option<&str>) -> String {
    let user = json!({"type":"user","message":{"role":"user","content":
        format!("\n\n<pasted_content id=\"ab12\">\n{prompt}\n</pasted_content id=\"ab12\">\n")}});
    let mut log = format!("{user}\n");
    if let Some(reply) = reply {
        let record = json!({"type":"assistant","message":{"content":[{"type":"text","text":reply}],"stop_reason":"end_turn"}});
        log.push_str(&format!("{record}\n"));
    }
    log
}

fn text(t: &str) -> Reply {
    Reply::Text(EventText::reply(t.to_owned()))
}

#[test]
fn a_claude_prompt_over_the_hook_limit_is_proven_from_the_transcript() {
    let prompt = long_prompt();
    assert!(prompt.len() > connecto_core::MAX_EVENT_TEXT * 3);
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Claude,
        "claude-long",
        Box::new(move |f: &Fake| {
            // The hook carries only the first 64 KiB.
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            f.write_log(&format!("{SESSION}.jsonl"), &claude_log(&sent, Some("[]")));
            f.push(claude_turn_end(text("[]")));
        }),
    );
    claude_started(&fake);
    let answer = fake.talk(&prompt, true, soon(), &never).run().unwrap();
    assert_eq!(
        answer,
        Answer {
            text: "[]".into(),
            source: ReplySource::Hook
        }
    );
}

#[test]
fn a_claude_prompt_the_transcript_shows_cut_is_refused() {
    let prompt = long_prompt();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Claude,
        "claude-cut",
        Box::new(move |f: &Fake| {
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            f.write_log(
                &format!("{SESSION}.jsonl"),
                &claude_log(&sent[..sent.len() / 2], Some("[]")),
            );
            f.push(claude_turn_end(text("[]")));
        }),
    );
    claude_started(&fake);
    let err = fake.talk(&prompt, true, soon(), &never).run().unwrap_err();
    assert!(
        matches!(err, RunError::PromptCut { tool: "Claude", arrived, sent } if arrived < sent),
        "{err:?}"
    );
}

#[test]
fn a_dropped_claude_reply_is_read_from_the_transcript_written_after_the_hook() {
    let prompt = "Summarize the notes.".to_owned();
    let sent = prompt.clone();
    let fake = Arc::new(Fake::new(
        Kind::Claude,
        "claude-dropped",
        Box::new(move |f: &Fake| {
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            f.write_log(&format!("{SESSION}.jsonl"), &claude_log(&sent, None));
            f.push(claude_turn_end(Reply::Dropped));
        }),
    ));
    claude_started(&fake);
    // The assistant record lands 60 ms after the Stop hook.
    let writer = {
        let fake = Arc::clone(&fake);
        let prompt = prompt.clone();
        thread::spawn(move || {
            while fake.enters.load(Ordering::SeqCst) == 0 {
                thread::sleep(Duration::from_millis(2));
            }
            thread::sleep(Duration::from_millis(60));
            fake.write_log(
                &format!("{SESSION}.jsonl"),
                &claude_log(&prompt, Some("a very long reply")),
            );
        })
    };
    let answer = fake.talk(&prompt, true, soon(), &never).run().unwrap();
    writer.join().unwrap();
    assert_eq!(answer.text, "a very long reply");
    assert_eq!(answer.source, ReplySource::SessionLog);
}

#[test]
fn a_dropped_reply_with_no_readable_log_is_an_error_and_no_text() {
    let prompt = "Summarize the notes.".to_owned();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Claude,
        "claude-nolog",
        Box::new(move |f: &Fake| {
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            f.push(claude_turn_end(Reply::Dropped));
        }),
    );
    claude_started(&fake);
    let err = fake.talk(&prompt, true, soon(), &never).run().unwrap_err();
    assert!(matches!(err, RunError::ReplyUnavailable(_)), "{err:?}");
}

#[test]
fn an_api_error_reply_is_a_failure_not_output() {
    let prompt = "Summarize the notes.".to_owned();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Claude,
        "claude-api",
        Box::new(move |f: &Fake| {
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            f.push(claude_turn_end(text(
                "API Error: Can't reach the API server (ENOTFOUND)",
            )));
        }),
    );
    claude_started(&fake);
    // A title too: it would otherwise become "API Error Can't reach the".
    for strict in [true, false] {
        fake.enters.store(0, Ordering::SeqCst);
        fake.submitted.store(false, Ordering::SeqCst);
        let err = fake
            .talk(&prompt, strict, soon(), &never)
            .run()
            .unwrap_err();
        assert!(
            matches!(err, RunError::Provider(ref why) if why.starts_with("API Error")),
            "{err:?}"
        );
    }
}

#[test]
fn a_run_with_no_turn_end_times_out_at_its_deadline() {
    let fake = Fake::new(Kind::Claude, "claude-timeout", Box::new(|_: &Fake| {}));
    let started = Instant::now();
    let err = fake
        .talk(
            "hi there",
            true,
            Instant::now() + Duration::from_millis(250),
            &never,
        )
        .run()
        .unwrap_err();
    assert!(matches!(err, RunError::Timeout(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_secs(2));
}

#[test]
fn a_cancelled_run_stops_promptly() {
    let fake = Fake::new(Kind::Codex, "codex-cancel", Box::new(|_: &Fake| {}));
    let started = Instant::now();
    let cancelled = || started.elapsed() >= Duration::from_millis(80);
    let err = fake
        .talk("hi there", true, soon(), &cancelled)
        .run()
        .unwrap_err();
    assert_eq!(err, RunError::Cancelled);
    assert!(started.elapsed() < Duration::from_millis(500));
}

fn codex_end(input: &[&str], reply: Reply, thread: &str) -> ToolEvent {
    ToolEvent::TurnEnded {
        input: input
            .iter()
            .map(|t| EventText::new((*t).to_owned()))
            .collect(),
        input_total: input.len(),
        reply,
        lifecycle_known: true,
        background: Vec::new(),
        thread: Some(EventText::new(thread.to_owned())),
        turn: None,
        failure: None,
        continued: false,
    }
}

fn rollout(prompt: &str, tail: &[serde_json::Value]) -> String {
    let ev = |payload: serde_json::Value| {
        json!({"timestamp":"t","type":"event_msg","payload":payload}).to_string()
    };
    let mut lines = vec![ev(json!({"type":"task_started","turn_id":"t1"}))];
    lines.push(ev(json!({"type":"user_message","message":prompt})));
    lines.extend(tail.iter().cloned().map(ev));
    lines.join("\n")
}

fn rollout_name() -> String {
    format!("rollout-2026-11-14T10-00-00-{THREAD}.jsonl")
}

#[test]
fn codex_skips_its_own_title_turn_and_takes_the_real_one() {
    let prompt = "Give a title for this terminal.".to_owned();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Codex,
        "codex-two",
        Box::new(move |f: &Fake| {
            // `notify` fires first for Codex's own title turn.
            f.push(codex_end(
                &["Generate a concise, single-line task title for this"],
                text("Task title"),
                TITLE_THREAD,
            ));
            f.write_log(
                &rollout_name(),
                &rollout(
                    &sent,
                    &[json!({"type":"task_complete","turn_id":"t1","last_agent_message":"Fix login redirect"})],
                ),
            );
            f.push(codex_end(&[&sent], text("Fix login redirect"), THREAD));
        }),
    );
    for strict in [false, true] {
        fake.enters.store(0, Ordering::SeqCst);
        fake.submitted.store(false, Ordering::SeqCst);
        fake.pasted.store(false, Ordering::SeqCst);
        let answer = fake.talk(&prompt, strict, soon(), &never).run().unwrap();
        assert_eq!(answer.text, "Fix login redirect");
        assert_eq!(answer.source, ReplySource::Hook);
    }
}

#[test]
fn a_long_codex_prompt_is_proven_from_the_rollout_and_a_dropped_reply_read_there() {
    let prompt = long_prompt();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Codex,
        "codex-long",
        Box::new(move |f: &Fake| {
            f.write_log(
                &rollout_name(),
                &rollout(
                    &sent,
                    &[json!({"type":"task_complete","turn_id":"t1","last_agent_message":"[{\"n\":1}]"})],
                ),
            );
            f.push(codex_end(&[&sent], Reply::Dropped, THREAD));
        }),
    );
    let answer = fake.talk(&prompt, true, soon(), &never).run().unwrap();
    assert_eq!(answer.text, "[{\"n\":1}]");
    assert_eq!(answer.source, ReplySource::SessionLog);
}

#[test]
fn a_codex_turn_that_failed_is_a_provider_error() {
    let prompt = "Summarize.".to_owned();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Codex,
        "codex-error",
        Box::new(move |f: &Fake| {
            f.write_log(
                &rollout_name(),
                &rollout(
                    &sent,
                    &[json!({"type":"error","message":"stream disconnected before completion"})],
                ),
            );
            f.push(codex_end(&[&sent], Reply::None, THREAD));
        }),
    );
    let err = fake.talk(&prompt, true, soon(), &never).run().unwrap_err();
    assert!(
        matches!(err, RunError::Provider(ref why) if why.contains("stream disconnected")),
        "{err:?}"
    );
}

#[test]
fn every_agent_cli_has_a_utility_surface_and_a_shell_has_none() {
    for provider in Provider::ALL {
        assert!(
            adapter(tool_of(*provider)).utility().is_some(),
            "{provider}"
        );
    }
    assert!(adapter(Tool::Shell).utility().is_none());
}

// ---------------------------------------------------------------------------
// The runner: budget, cancel-all, the job's checks.

struct Gate {
    open: AtomicBool,
    calls: AtomicUsize,
    reply: Result<Answer, RunError>,
}

impl Engine for Gate {
    fn drive(
        &self,
        _: &UtilityJob,
        deadline: Instant,
        cancelled: &dyn Fn() -> bool,
        _: &dyn Fn(Arc<ManagedPane>),
    ) -> Result<Answer, RunError> {
        self.calls.fetch_add(1, Ordering::SeqCst);
        while !self.open.load(Ordering::SeqCst) {
            if cancelled() {
                return Err(RunError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(RunError::Timeout("no reply".into()));
            }
            thread::sleep(Duration::from_millis(2));
        }
        self.reply.clone()
    }
}

fn gate(reply: Result<Answer, RunError>) -> Arc<Gate> {
    Arc::new(Gate {
        open: AtomicBool::new(false),
        calls: AtomicUsize::new(0),
        reply,
    })
}

fn job() -> UtilityJob {
    let project = std::env::temp_dir();
    let real = std::fs::canonicalize(&project).unwrap();
    UtilityJob {
        project_id: crate::launch::project_id(&real),
        project,
        choice: ModelChoice::new(Provider::Codex, "default", "low"),
        prompt: "a prompt".into(),
        purpose: UtilityPurpose::Memory,
        label: "development".into(),
    }
}

fn wait_for(what: &str, cond: impl Fn() -> bool) {
    let until = Instant::now() + Duration::from_secs(5);
    while !cond() {
        assert!(Instant::now() < until, "timed out waiting for {what}");
        thread::sleep(Duration::from_millis(2));
    }
}

#[test]
fn a_reply_counts_its_characters_and_the_budget_holds_extra_runs_back() {
    let engine = gate(Ok(Answer {
        text: "réponse".into(),
        source: ReplySource::Hook,
    }));
    let runner = Arc::new(UtilityRunner::with_engine(
        Arc::clone(&engine) as Arc<dyn Engine>,
        2,
    ));
    let workers: Vec<_> = (0..3)
        .map(|_| {
            let runner = Arc::clone(&runner);
            thread::spawn(move || runner.run(&job(), soon(), &AtomicBool::new(false)))
        })
        .collect();
    wait_for("two runs", || engine.calls.load(Ordering::SeqCst) == 2);
    thread::sleep(Duration::from_millis(30));
    assert_eq!(engine.calls.load(Ordering::SeqCst), 2, "the third waits");
    assert_eq!(runner.in_flight(), 2);
    engine.open.store(true, Ordering::SeqCst);
    for worker in workers {
        let reply = worker.join().unwrap().unwrap();
        assert_eq!((reply.chars_in, reply.chars_out), (8, 7));
        assert_eq!(reply.source, ReplySource::Hook);
    }
    assert_eq!(engine.calls.load(Ordering::SeqCst), 3);
    assert_eq!(runner.in_flight(), 0);
}

#[test]
fn cancel_all_stops_runs_in_flight_and_not_later_ones() {
    let engine = gate(Ok(Answer {
        text: "ok".into(),
        source: ReplySource::Hook,
    }));
    let runner = Arc::new(UtilityRunner::with_engine(
        Arc::clone(&engine) as Arc<dyn Engine>,
        2,
    ));
    let worker = {
        let runner = Arc::clone(&runner);
        thread::spawn(move || runner.run(&job(), soon(), &AtomicBool::new(false)))
    };
    wait_for("the run", || engine.calls.load(Ordering::SeqCst) == 1);
    runner.cancel_all();
    assert_eq!(worker.join().unwrap(), Err(RunError::Cancelled));
    engine.open.store(true, Ordering::SeqCst);
    assert!(runner.run(&job(), soon(), &AtomicBool::new(false)).is_ok());
    // The job's own flag cancels it too.
    assert_eq!(
        runner.run(&job(), soon(), &AtomicBool::new(true)),
        Err(RunError::Cancelled)
    );
}

#[test]
fn an_unavailable_provider_and_a_bad_job_are_typed_errors() {
    let engine = gate(Err(RunError::Unavailable(
        "Codex is not logged in for this project".into(),
    )));
    engine.open.store(true, Ordering::SeqCst);
    let runner = UtilityRunner::with_engine(Arc::clone(&engine) as Arc<dyn Engine>, 2);
    assert!(matches!(
        runner.run(&job(), soon(), &AtomicBool::new(false)),
        Err(RunError::Unavailable(_))
    ));
    let mut other = job();
    other.project_id = "0000000000000000".into();
    assert!(matches!(
        runner.run(&other, soon(), &AtomicBool::new(false)),
        Err(RunError::LaunchRefused(_))
    ));
    let mut empty = job();
    empty.prompt = "  ".into();
    assert!(matches!(
        runner.run(&empty, soon(), &AtomicBool::new(false)),
        Err(RunError::LaunchRefused(_))
    ));
    assert_eq!(
        engine.calls.load(Ordering::SeqCst),
        1,
        "refused before launch"
    );
}

#[test]
fn a_cli_not_yet_trusted_for_the_project_fails_at_once_with_the_reason() {
    for (kind, screen, name) in [
        (
            Kind::Claude,
            "Accessing workspace:\n /p\n Quick safety check: Is this a project you created or one you trust?\n ❯ 1. Yes, I trust this folder\n   2. No, exit",
            "Claude",
        ),
        (
            Kind::Codex,
            "> You are running Codex in /p\n  Do you trust the contents of this directory?\n› 1. Yes, continue\n  2. No, quit",
            "Codex",
        ),
    ] {
        let fake = Fake::new(kind, &format!("trust-{name}"), Box::new(|_: &Fake| {}));
        *fake.shown.lock().unwrap() = Some(screen.to_owned());
        let started = Instant::now();
        let err = fake
            .talk("a prompt", true, soon(), &never)
            .run()
            .unwrap_err();
        assert!(
            started.elapsed() < Duration::from_millis(200),
            "{name}: not the timeout"
        );
        match err {
            RunError::Untrusted(why) => {
                assert!(
                    why.starts_with(&format!("{name} has not been trusted")),
                    "{why}"
                );
                assert!(why.contains("open a"), "{why}");
            }
            other => panic!("{name}: {other:?}"),
        }
        assert!(
            fake.sent.lock().unwrap().is_empty(),
            "{name}: nothing typed, nothing answered"
        );
    }
}

fn claude_reply_record(id: &str, text: &str, stop: Option<&str>) -> String {
    json!({"type":"assistant","message":{"id":id,"content":[{"type":"text","text":text}],"stop_reason":stop}}).to_string()
}

/// Review finding 1: a memory prompt with 400 characters missing in the middle
/// is refused, though its ending and 95 % of its length arrived. A title keeps
/// the looser check.
#[test]
fn a_memory_prompt_missing_characters_is_refused_and_a_title_keeps_its_check() {
    let prompt: String = (0..2000).map(|n| format!("w{n:04} ")).collect::<String>() + "END";
    let short = format!("{}{}", &prompt[..4000], &prompt[4400..]);
    for strict in [true, false] {
        let got = short.clone();
        let fake = Fake::new(
            Kind::Claude,
            &format!("missing-{strict}"),
            Box::new(move |f: &Fake| {
                f.push(ToolEvent::PromptSubmitted {
                    prompt: EventText::new(got.clone()),
                    turn: None,
                });
                f.push(claude_turn_end(text("[]")));
            }),
        );
        let result = fake.talk(&prompt, strict, soon(), &never).run();
        if strict {
            assert!(
                matches!(result, Err(RunError::PromptCut { .. })),
                "{result:?}"
            );
        } else {
            assert_eq!(result.unwrap().text, "[]");
        }
    }
}

/// Review finding 2: an assistant record with no end mark is not a reply.
#[test]
fn an_unfinished_reply_in_the_log_is_not_returned() {
    let prompt = "Summarize the notes.".to_owned();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Claude,
        "unfinished",
        Box::new(move |f: &Fake| {
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            let log = format!(
                "{}{}\n",
                claude_log(&sent, None),
                claude_reply_record("m1", "[{\"half", None)
            );
            f.write_log(&format!("{SESSION}.jsonl"), &log);
            f.push(claude_turn_end(Reply::Dropped));
        }),
    );
    claude_started(&fake);
    let err = fake.talk(&prompt, true, soon(), &never).run().unwrap_err();
    assert!(matches!(err, RunError::ReplyUnavailable(_)), "{err:?}");
}

/// Review finding 3: after a tool call, only the final message is the reply.
#[test]
fn a_dropped_reply_after_a_tool_call_is_the_final_message_alone() {
    let prompt = "Read Cargo.toml, then reply [] if nothing is wrong.".to_owned();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Claude,
        "toolturn",
        Box::new(move |f: &Fake| {
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            let ask = json!({"type":"user","message":{"content": sent}});
            let step = json!({"type":"assistant","message":{"id":"m1","content":[{"type":"text","text":"I will read the file."},{"type":"tool_use","id":"t1","name":"Read","input":{}}],"stop_reason":"tool_use"}});
            let result = json!({"type":"user","message":{"content":[{"type":"tool_result","tool_use_id":"t1","content":"[package]"}]}});
            let last = claude_reply_record("m2", "[]", Some("end_turn"));
            f.write_log(
                &format!("{SESSION}.jsonl"),
                &format!("{ask}\n{step}\n{result}\n{last}\n"),
            );
            f.push(claude_turn_end(Reply::Dropped));
        }),
    );
    claude_started(&fake);
    let answer = fake.talk(&prompt, true, soon(), &never).run().unwrap();
    assert_eq!(answer.text, "[]");
    assert_eq!(answer.source, ReplySource::SessionLog);
}

/// Review finding 4: the log read stops at the job's deadline.
#[test]
fn the_log_read_does_not_outlive_the_deadline() {
    let prompt = "Summarize the notes.".to_owned();
    let sent = prompt.clone();
    let fake = Arc::new(Fake::new(
        Kind::Claude,
        "logdeadline",
        Box::new(move |f: &Fake| {
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            f.write_log(&format!("{SESSION}.jsonl"), &claude_log(&sent, None));
            f.push(claude_turn_end(Reply::Dropped));
        }),
    ));
    claude_started(&fake);
    let writer = {
        let fake = Arc::clone(&fake);
        let prompt = prompt.clone();
        thread::spawn(move || {
            while fake.enters.load(Ordering::SeqCst) == 0 {
                thread::sleep(Duration::from_millis(2));
            }
            thread::sleep(Duration::from_millis(250));
            fake.write_log(
                &format!("{SESSION}.jsonl"),
                &claude_log(&prompt, Some("late")),
            );
        })
    };
    let deadline = Instant::now() + Duration::from_millis(150);
    let err = fake
        .talk(&prompt, true, deadline, &never)
        .run()
        .unwrap_err();
    let ended = Instant::now();
    writer.join().unwrap();
    assert!(matches!(err, RunError::Timeout(_)), "{err:?}");
    assert!(
        ended < deadline + Duration::from_millis(60),
        "stopped near the deadline"
    );
}

/// Review finding 4: a slow settle never sends Enter after the deadline.
#[test]
fn enter_is_never_sent_after_the_deadline() {
    let fake = Fake::new(Kind::Claude, "settle", Box::new(|_: &Fake| {}));
    let mut talk = fake.talk(
        "a prompt",
        true,
        Instant::now() + Duration::from_millis(80),
        &never,
    );
    talk.pace.settle_still = Duration::from_secs(2);
    talk.pace.settle_limit = Duration::from_secs(5);
    let started = Instant::now();
    let err = talk.run().unwrap_err();
    assert!(matches!(err, RunError::Timeout(_)), "{err:?}");
    assert!(started.elapsed() < Duration::from_millis(300));
    assert!(
        !fake.sent.lock().unwrap().iter().any(|b| b == b"\r"),
        "no Enter after the deadline"
    );
}

fn claude_meta_log() -> String {
    format!(
        "{}\n",
        json!({"type":"user","isMeta":true,"message":{"content":"<system-reminder>x</system-reminder>"}})
    )
}

fn pastes(fake: &Fake) -> usize {
    fake.sent
        .lock()
        .unwrap()
        .iter()
        .filter(|b| *b != b"\r")
        .count()
}

/// The intermittent "typed but never submitted": a paste the composer lost is
/// typed again once, but only on proof: the session log is readable and holds
/// no user message, and no hook came.
#[test]
fn a_lost_paste_proven_by_the_log_is_typed_again_once() {
    let prompt = "Give a title for this terminal.".to_owned();
    let sent = prompt.clone();
    let fake = Fake::new(
        Kind::Claude,
        "lost",
        Box::new(move |f: &Fake| {
            f.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(sent.clone()),
                turn: None,
            });
            f.write_log(
                &format!("{SESSION}.jsonl"),
                &claude_log(&sent, Some("Fix login redirect")),
            );
            f.push(claude_turn_end(text("Fix login redirect")));
        }),
    );
    claude_started(&fake);
    fake.write_log(&format!("{SESSION}.jsonl"), &claude_meta_log());
    fake.lose_pastes.store(1, Ordering::SeqCst);
    let answer = fake.talk(&prompt, true, soon(), &never).run().unwrap();
    assert_eq!(answer.text, "Fix login redirect");
    assert_eq!(pastes(&fake), 2, "typed again once");
}

/// Review round 2, finding 1: an empty composer is no proof. The prompt was
/// taken (the log has it) and the hooks come late: it is never typed again.
#[test]
fn a_prompt_taken_with_late_hooks_is_never_typed_again() {
    let prompt = "Summarize the notes.".to_owned();
    let sent = prompt.clone();
    let fake = Arc::new(Fake::new(
        Kind::Claude,
        "lateack",
        Box::new(move |f: &Fake| {
            f.write_log(&format!("{SESSION}.jsonl"), &claude_log(&sent, None));
        }),
    ));
    claude_started(&fake);
    let late = {
        let fake = Arc::clone(&fake);
        let prompt = prompt.clone();
        thread::spawn(move || {
            while fake.enters.load(Ordering::SeqCst) == 0 {
                thread::sleep(Duration::from_millis(2));
            }
            // Well past the submit limit and the log wait.
            thread::sleep(Duration::from_millis(900));
            fake.write_log(
                &format!("{SESSION}.jsonl"),
                &claude_log(&prompt, Some("[]")),
            );
            fake.push(ToolEvent::PromptSubmitted {
                prompt: EventText::new(prompt.clone()),
                turn: None,
            });
            fake.push(claude_turn_end(text("[]")));
        })
    };
    let answer = fake.talk(&prompt, true, soon(), &never).run().unwrap();
    late.join().unwrap();
    assert_eq!(answer.text, "[]");
    assert_eq!(pastes(&fake), 1, "never a second submission");
}

/// With no readable log (a Codex rollout is found only from its turn end, a
/// Claude transcript does not exist before its first message), nothing proves
/// the paste was lost: the run waits for a late hook, types nothing again, and
/// at the deadline says the submission is uncertain.
#[test]
fn without_a_readable_log_an_unconfirmed_submission_is_uncertain_and_not_retyped() {
    for kind in [Kind::Claude, Kind::Codex] {
        let fake = Fake::new(
            kind,
            &format!("uncertain-{}", kind == Kind::Claude),
            Box::new(|_: &Fake| {}),
        );
        if kind == Kind::Claude {
            claude_started(&fake);
        }
        fake.lose_pastes.store(1, Ordering::SeqCst);
        let deadline = Instant::now() + Duration::from_millis(1500);
        let err = fake
            .talk("a prompt", true, deadline, &never)
            .run()
            .unwrap_err();
        assert!(matches!(err, RunError::SubmitUncertain(_)), "{err:?}");
        assert!(
            Instant::now() >= deadline,
            "it waited for a late hook until the deadline"
        );
        assert_eq!(pastes(&fake), 1);
    }
}

/// Review round 2, finding 2: a memory reply whose hook text fits is still
/// refused when the log shows it was cut at the token limit.
#[test]
fn a_memory_reply_cut_at_the_token_limit_is_refused_even_from_the_hook() {
    let prompt = "Summarize the notes.".to_owned();
    for (strict, ok) in [(true, false), (false, true)] {
        let sent = prompt.clone();
        let fake = Fake::new(
            Kind::Claude,
            &format!("maxtok-{strict}"),
            Box::new(move |f: &Fake| {
                f.push(ToolEvent::PromptSubmitted {
                    prompt: EventText::new(sent.clone()),
                    turn: None,
                });
                let log = format!(
                    "{}{}\n",
                    claude_log(&sent, None),
                    claude_reply_record("m1", "[{\"cut", Some("max_tokens"))
                );
                f.write_log(&format!("{SESSION}.jsonl"), &log);
                f.push(claude_turn_end(text("[{\"cut")));
            }),
        );
        claude_started(&fake);
        let result = fake.talk(&prompt, strict, soon(), &never).run();
        if ok {
            assert_eq!(
                result.unwrap().source,
                ReplySource::Hook,
                "a title as before"
            );
        } else {
            assert!(
                matches!(result, Err(RunError::ReplyUnavailable(_))),
                "{result:?}"
            );
        }
    }
}

/// The same for Codex: a turn the rollout shows interrupted, or not complete,
/// never comes back as a reply, though `notify` carried text.
#[test]
fn a_codex_turn_aborted_or_unfinished_is_not_a_reply() {
    let prompt = "Summarize.".to_owned();
    for (tail, want_provider) in [
        (vec![json!({"type":"turn_aborted","turn_id":"t1"})], true),
        (
            vec![json!({"type":"agent_message","message":"half"})],
            false,
        ),
    ] {
        let sent = prompt.clone();
        let fake = Fake::new(
            Kind::Codex,
            &format!("codex-unfinished-{want_provider}"),
            Box::new(move |f: &Fake| {
                f.write_log(&rollout_name(), &rollout(&sent, &tail));
                f.push(codex_end(&[&sent], text("half"), THREAD));
            }),
        );
        let err = fake.talk(&prompt, true, soon(), &never).run().unwrap_err();
        if want_provider {
            assert!(matches!(err, RunError::Provider(_)), "{err:?}");
        } else {
            assert!(matches!(err, RunError::ReplyUnavailable(_)), "{err:?}");
        }
    }
}

/// Review finding 5: a CLI is available only with a confirmed login, also the
/// first time, when its private copy was just set up from the user's install.
#[test]
fn a_provider_is_available_only_once_its_login_is_confirmed() {
    let copied = || {
        Ok(connecto_store::Installed {
            version: "0.167.1".into(),
            entry: "/data/cli/codex/0.167.1/codex".into(),
        })
    };
    let checked = std::cell::Cell::new(false);
    let logged_out = availability("Codex", copied(), |_| {
        checked.set(true);
        Ok(AuthState::LoggedOut)
    });
    assert!(checked.get(), "the login is asked even at first use");
    assert_eq!(
        logged_out.reason(),
        Some("Codex is not logged in for this project")
    );
    assert!(
        availability("Codex", copied(), |_| Ok(AuthState::LoggedIn {
            method: "chatgpt".into()
        }))
        .is_available()
    );
    assert!(!availability("Codex", copied(), |_| Ok(AuthState::Unknown)).is_available());
    let missing = availability("Codex", Err("Codex is not installed".into()), |_| {
        panic!("no login check without a copy")
    });
    assert_eq!(missing.reason(), Some("Codex is not installed"));
}

/// A memory run never presses Enter a second time: text still in the
/// composer (or a menu that looks like it) gets no keystroke. A title still
/// presses Enter once more, as it always has.
#[test]
fn only_a_title_presses_enter_a_second_time() {
    for strict in [true, false] {
        let fake = Fake::new(
            Kind::Claude,
            &format!("stuck-{strict}"),
            Box::new(|_: &Fake| {}),
        );
        // The composer keeps showing the paste: Enter never takes.
        *fake.shown.lock().unwrap() =
            Some("─────\n❯ [Pasted text #1]\n─────\n  ? for shortcuts\n".into());
        let err = fake
            .talk("a prompt", strict, soon(), &never)
            .run()
            .unwrap_err();
        assert!(
            matches!(err, RunError::Session(ref why) if why.contains("never submitted")),
            "{err:?}"
        );
        let enters = fake
            .sent
            .lock()
            .unwrap()
            .iter()
            .filter(|b| *b == b"\r")
            .count();
        assert_eq!(enters, if strict { 1 } else { 2 }, "strict: {strict}");
    }
}

/// Token optimization §4: the study runs with no tool at all and proves its
/// prompt arrived, as a memory run does.
#[test]
fn a_token_study_has_no_tool_and_is_strict() {
    assert_eq!(UtilityPurpose::TokenStudy.access(), UtilityAccess::Minimal);
    assert!(UtilityPurpose::TokenStudy.strict());
    assert!(!UtilityPurpose::Title.strict());
}
