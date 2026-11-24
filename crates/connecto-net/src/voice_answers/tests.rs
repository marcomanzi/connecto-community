#![allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]

use super::*;
use connecto_core::{
    Cell, GridSize, InputRefused, ResizeOutcome, SessionStatus, TerminalSession, ToolKind,
};
use std::cell::Cell as Flag;
use std::sync::{Arc, Mutex};

const CODEX: &str =
    include_str!("../../../connecto-adapter/tests/fixtures/codex/approval_prompt_inline.txt");
const ROWS: [&str; 3] = ["1. Yes, proceed", "2. Yes, and don't", "3. No, and tell"];

/// What runs after each write to a [`Pane`].
type AfterWrite = Box<dyn FnMut(&Pane) + Send>;

/// A terminal: its live bottom (`screen`), what its scrolled view shows
/// (`view`, when scrolled back), its chat, its input line as typed into it
/// (`draft`, None: not known) and as drawn (`drawn`, None: not readable),
/// and what was written to it. It counts its input's writes and checks a
/// conditional write as a real pane does. Arrows move a Codex approval's
/// selection; Enter answers it; a paste goes into a Codex question form's
/// field (redrawn as a real one is) and into the input line.
#[derive(Default)]
struct Pane {
    screen: Mutex<String>,
    view: Mutex<Option<String>>,
    chat: Mutex<Option<ChatHistory>>,
    draft: Mutex<Option<String>>,
    drawn: Mutex<Option<String>>,
    writes: Mutex<u64>,
    /// Rows the terminal soft-wrapped (by index of `screen`'s rows).
    wrapped: Mutex<Vec<usize>>,
    /// A synchronized redraw still held back: so many captures without
    /// `flush` are refused; one with `flush` shows `partial`.
    holding: Mutex<usize>,
    partial: Mutex<Option<String>>,
    flushed: Mutex<bool>,
    typed: Mutex<Vec<Vec<u8>>>,
    /// Run after each write: the screen or chat changing meanwhile.
    after: Mutex<Option<AfterWrite>>,
    /// The screen's generation: it moves whenever what is drawn changes
    /// between two captures, and with [`Pane::redraw`].
    output: Mutex<(u64, String)>,
    /// Enter leaves the dialog on screen (the redraw not come yet).
    enter_keeps: Mutex<bool>,
    /// Run just before a write lands: the screen redrawn after the host's
    /// last look.
    before: Mutex<Option<AfterWrite>>,
}

impl Pane {
    fn new(screen: &str) -> Arc<Self> {
        let pane = Self::default();
        *pane.screen.lock().unwrap() = screen.to_owned();
        Arc::new(pane)
    }

    fn typed(&self) -> Vec<Vec<u8>> {
        self.typed.lock().unwrap().clone()
    }

    /// The screen's generation now: moved if what is drawn changed.
    fn output_now(&self) -> u64 {
        let now = self.screen.lock().unwrap().clone();
        let mut output = self.output.lock().unwrap();
        if output.1 != now {
            output.0 += 1;
            output.1 = now;
        }
        output.0
    }

    /// The program drew something (the dialog went and came back, say)
    /// between two captures.
    fn redraw(&self) {
        self.output.lock().unwrap().0 += 1;
    }

    /// Someone else's key: a write the answer did not make.
    fn poke(&self) {
        *self.writes.lock().unwrap() += 1;
    }

    fn generation(&self) -> InputGeneration {
        InputGeneration {
            epoch: 1,
            writes: *self.writes.lock().unwrap(),
        }
    }

    fn fill(screen: &str, wrapped: &[usize], out: &mut TermSnapshot) {
        let lines: Vec<&str> = screen.lines().collect();
        let cols = 160;
        out.cols = cols;
        out.rows = u16::try_from(lines.len()).unwrap();
        out.modes.bracketed_paste = true;
        out.cells = lines
            .iter()
            .enumerate()
            .flat_map(|(i, l)| {
                let mut row: Vec<Cell> = l
                    .chars()
                    .map(|ch| Cell {
                        ch,
                        ..Cell::default()
                    })
                    .collect();
                row.resize(usize::from(cols), Cell::default());
                if wrapped.contains(&i)
                    && let Some(last) = row.last_mut()
                {
                    last.flags.insert(CellFlags::WRAPPED);
                }
                row
            })
            .collect();
    }

    fn apply(&self, bytes: &[u8]) {
        let mut screen = self.screen.lock().unwrap();
        let at = ROWS
            .iter()
            .position(|r| screen.contains(&format!("› {r}")))
            .unwrap_or(0);
        match bytes {
            b"\x1b[B" | b"\x1b[A" => {
                let to = if bytes == b"\x1b[B" {
                    (at + 1).min(2)
                } else {
                    at.saturating_sub(1)
                };
                *screen = screen
                    .replace(&format!("› {}", ROWS[at]), &format!("  {}", ROWS[at]))
                    .replace(&format!("  {}", ROWS[to]), &format!("› {}", ROWS[to]));
            }
            b"\r" if *self.enter_keeps.lock().unwrap() => {}
            b"\r" => {
                *screen = "› done".into();
                // What was typed is submitted, as the agent gets it.
                let submitted = self
                    .draft
                    .lock()
                    .unwrap()
                    .replace(String::new())
                    .filter(|d| !d.is_empty())
                    .unwrap_or_else(|| "answer".into());
                *self.drawn.lock().unwrap() = Some(String::new());
                if let Some(chat) = self.chat.lock().unwrap().as_mut() {
                    chat.messages.push((true, submitted));
                    chat.revision += 1;
                }
            }
            pasted => {
                // The field shows what was pasted, as a real one does; the
                // input line gets it after what it held.
                // The input line keeps new lines as the draft tracker does.
                let words = String::from_utf8_lossy(pasted)
                    .replace("\x1b[200~", "")
                    .replace("\x1b[201~", "")
                    .replace('\r', "\n");
                *screen = screen.replace("› Type your answer (optional)", &format!("› {words}"));
                if let Some(d) = self.draft.lock().unwrap().as_mut() {
                    d.push_str(&words);
                }
                // A paste over several lines is drawn collapsed, as Claude
                // draws it: what it holds cannot be read back.
                if let Some(d) = self.drawn.lock().unwrap().as_mut() {
                    if words.contains('\n') {
                        *d = format!("[Pasted text #1 +{} lines]", words.matches('\n').count());
                    } else {
                        d.push_str(&words);
                    }
                }
            }
        }
    }
}

impl TerminalSession for Pane {
    /// The window's view: scrolled back, it shows history.
    fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        let view = self.view.lock().unwrap().clone();
        let screen = view.unwrap_or_else(|| self.screen.lock().unwrap().clone());
        Self::fill(&screen, &[], out);
        SnapshotOutcome::Updated
    }
    /// The live bottom, whatever the view shows.
    fn poll_capture(
        &self,
        _: usize,
        flush: bool,
        screen: &mut TermSnapshot,
        history: &mut TermSnapshot,
    ) -> SnapshotOutcome {
        *history = TermSnapshot::default();
        let mut holding = self.holding.lock().unwrap();
        if *holding > 0 {
            if flush {
                *self.flushed.lock().unwrap() = true;
                let partial = self.partial.lock().unwrap().clone().unwrap_or_default();
                Self::fill(&partial, &[], screen);
                return SnapshotOutcome::Updated;
            }
            *holding -= 1;
            return SnapshotOutcome::Contended;
        }
        let now = self.screen.lock().unwrap().clone();
        Self::fill(&now, &self.wrapped.lock().unwrap(), screen);
        screen.generation = self.output_now();
        SnapshotOutcome::Updated
    }
    fn chat_history(&self) -> Option<Arc<ChatHistory>> {
        self.chat.lock().unwrap().clone().map(Arc::new)
    }
    fn input_generation(&self) -> Option<InputGeneration> {
        Some(self.generation())
    }
    fn send_input(&self, _: Vec<u8>) -> Result<(), InputRefused> {
        panic!("only send_input_if may write")
    }
    /// As a real pane: on the generation, and the input line exactly so,
    /// as typed and as drawn.
    fn send_input_if(
        &self,
        condition: &InputCondition,
        bytes: Vec<u8>,
    ) -> Result<(), InputRefused> {
        if let Some(before) = self.before.lock().unwrap().as_mut() {
            before(self);
        }
        if condition.generation.is_some_and(|g| g != self.generation()) {
            return Err(InputRefused::Stale);
        }
        // As the real pane: the screen still at that generation, at the
        // moment of the enqueue.
        if condition.output.is_some_and(|g| g != self.output_now()) {
            return Err(InputRefused::Stale);
        }
        if let Some(expected) = &condition.composer
            && (self.draft.lock().unwrap().as_ref() != Some(expected)
                || self.drawn.lock().unwrap().as_ref() != Some(expected))
        {
            return Err(InputRefused::Stale);
        }
        if let Some(expected) = &condition.typed
            && self.draft.lock().unwrap().as_ref() != Some(expected)
        {
            return Err(InputRefused::Stale);
        }
        self.apply(&bytes);
        *self.writes.lock().unwrap() += 1;
        self.typed.lock().unwrap().push(bytes);
        if let Some(after) = self.after.lock().unwrap().as_mut() {
            after(self);
        }
        Ok(())
    }
    fn resize(&self, _: GridSize) -> ResizeOutcome {
        unreachable!()
    }
    fn scroll(&self, _: i32) -> bool {
        unreachable!()
    }
    fn status(&self) -> SessionStatus {
        unreachable!()
    }
}

fn entry(id: &str, name: Option<&str>, tool: ToolKind, pane: &Arc<Pane>) -> TerminalEntry {
    TerminalEntry {
        id: id.into(),
        name: name.map(Into::into),
        parent: None,
        asked_by: None,
        tool,
        state: TermActivity::NeedsUser,
        in_turn: true,
        background: Vec::new(),
        finished_turns: 0,
        session: pane.clone(),
        control: None,
        desired_running: true,
        shown_id: None,
        activity: 0,
    }
}

fn typer(t: &TerminalEntry, _: u32, c: &InputCondition, bytes: Vec<u8>) -> Result<(), String> {
    t.session.send_input_if(c, bytes).map_err(|e| e.to_string())
}

/// The project's terminals, as the host reads them at each step, and the
/// dialog instances issued for them.
struct Project {
    terminals: Mutex<Vec<(TerminalEntry, u32)>>,
    dialogs: Arc<Dialogs>,
}

impl Project {
    fn new(terminals: Vec<(TerminalEntry, u32)>) -> Self {
        Self {
            terminals: Mutex::new(terminals),
            dialogs: Arc::default(),
        }
    }
    fn now(&self) -> Vec<(TerminalEntry, u32)> {
        self.terminals.lock().unwrap().clone()
    }
    /// The questions listed now (tokens issued).
    fn list(&self) -> Value {
        questions_with(&self.dialogs, &self.now(), &|| {})
    }
    /// The id and instance token of `wire`'s question, as listed now.
    fn listed(&self, wire: u32) -> (String, String) {
        let v = self.list();
        let row = v["questions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|q| q["terminal"] == wire)
            .unwrap()
            .clone();
        (
            row["id"].as_str().unwrap().to_owned(),
            row["occurrence"].as_str().unwrap().to_owned(),
        )
    }
    fn answer(
        &self,
        wire: u32,
        (id, occurrence): (&str, &str),
        choice: AnswerChoice,
    ) -> Result<Value, String> {
        self.answer_by(wire, (id, occurrence), choice, &typer)
    }
    fn answer_by(
        &self,
        wire: u32,
        (id, occurrence): (&str, &str),
        choice: AnswerChoice,
        typer: Typer<'_>,
    ) -> Result<Value, String> {
        answer_with(
            &self.dialogs,
            &|| self.now(),
            wire,
            id,
            occurrence,
            &choice,
            typer,
            &|| {},
        )
    }
}

const FORM: &str =
    "x\nQuestion 1/1\nWhy?\n› Type your answer (optional)\nenter to submit answer | esc to go back";

#[test]
fn granted_terminals_only_and_each_by_its_wire_id() {
    let reviewer = Pane::new(CODEX);
    let child = Pane::new(CODEX);
    let entries = vec![
        entry("codex-2", Some("reviewer"), ToolKind::Codex, &reviewer),
        entry("codex-9", Some("helper"), ToolKind::Codex, &child),
    ];
    // The child was started after the peer's last list: not granted.
    let granted = granted_terminals(
        entries,
        |id| Some(if id == "codex-2" { 4 } else { 5 }),
        |wire| wire == 4,
    );
    let project = Project::new(granted);
    let v = project.list();
    let rows = v["questions"].as_array().unwrap();
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["terminal"], 4);
    assert_eq!(rows[0]["kind"], "approval");
    assert_eq!(rows[0]["options"][1]["tapOnly"], true);
    assert_eq!(rows[0]["options"][0]["tapOnly"], false);
    assert_eq!(carried_terminals(v.to_string().as_bytes()), [4]);
    let (id, token) = project.listed(4);
    // Its question has the same id, but its wire id is not granted.
    let err = project
        .answer(5, (&id, &token), AnswerChoice::Option(0))
        .unwrap_err();
    assert!(err.contains("not yours"), "{err}");
    assert!(child.typed().is_empty());
}

#[test]
fn a_grant_revoked_between_an_arrow_and_enter_stops_before_enter() {
    let pane = Pane::new(CODEX);
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    let grant = Flag::new(true);
    let revoking = |t: &TerminalEntry, wire: u32, c: &InputCondition, bytes: Vec<u8>| {
        if !grant.get() {
            return Err(format!("terminal {wire} is no longer yours"));
        }
        grant.set(false);
        typer(t, wire, c, bytes)
    };
    let err = project
        .answer_by(4, (&id, &token), AnswerChoice::Option(2), &revoking)
        .unwrap_err();
    assert!(err.contains("no longer yours"), "{err}");
    assert_eq!(pane.typed(), [b"\x1b[B".to_vec()]);
}

#[test]
fn a_terminal_dropped_from_the_project_between_keys_gets_no_enter() {
    let pane = Pane::new(CODEX);
    let project = Arc::new(Project::new(vec![(
        entry("codex-2", None, ToolKind::Codex, &pane),
        4,
    )]));
    let (id, token) = project.listed(4);
    let dropping = Arc::clone(&project);
    *pane.after.lock().unwrap() = Some(Box::new(move |_| {
        dropping.terminals.lock().unwrap().clear();
    }));
    assert!(
        project
            .answer(4, (&id, &token), AnswerChoice::Option(2))
            .is_err()
    );
    assert_eq!(pane.typed(), [b"\x1b[B".to_vec()]);
}

#[test]
fn an_approval_in_the_scrolled_view_is_not_the_live_question() {
    // The view shows an old approval; the live bottom shows a prompt.
    let pane = Pane::new("› ready for the next task");
    *pane.view.lock().unwrap() = Some(CODEX.to_owned());
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    assert!(project.list()["questions"].as_array().unwrap().is_empty());
    let old = question::active(ToolKind::Codex, CODEX)
        .unwrap()
        .id(ToolKind::Codex);
    assert!(
        project
            .answer(4, (&old, "d-00"), AnswerChoice::Option(0))
            .is_err()
    );
    assert!(pane.typed().is_empty());
}

// Regression: a capture ended a held-back redraw early (flush) and read
// its half-drawn grid as an actionable dialog.
#[test]
fn a_redraw_still_held_back_is_waited_for_never_read_half_drawn() {
    let pane = Pane::new(CODEX);
    *pane.partial.lock().unwrap() =
        Some(CODEX.replace("touch /tmp/cx_probe_approval_x", "rm -rf ~"));
    *pane.holding.lock().unwrap() = 100;
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    // Still held after a few tries: left out of the list.
    assert!(project.list()["questions"].as_array().unwrap().is_empty());
    assert!(!*pane.flushed.lock().unwrap(), "a held redraw was flushed");
    // Over after a couple of tries: the whole dialog is read.
    *pane.holding.lock().unwrap() = 2;
    let v = project.list();
    assert!(v.to_string().contains("cx_probe_approval_x"));
    assert!(!v.to_string().contains("rm -rf"));
}

#[test]
fn soft_wrapped_rows_are_one_line() {
    // A command the terminal soft-wrapped: its first row fills the width
    // exactly, and the rest goes on the next row.
    let head = format!("  $ touch /tmp/{}", "a".repeat(160 - 15));
    assert_eq!(head.chars().count(), 160);
    let screen = CODEX.replace(
        "  $ touch /tmp/cx_probe_approval_x",
        &format!("{head}\nbcd"),
    );
    let row = screen.lines().position(|l| l == head).unwrap();
    let pane = Pane::new(&screen);
    pane.wrapped.lock().unwrap().push(row);
    let t = entry("codex-2", None, ToolKind::Codex, &pane);
    let q = look(&t).unwrap().question.unwrap();
    let whole = format!("$ touch /tmp/{}bcd", "a".repeat(145));
    assert_eq!(q.operation.as_deref(), Some(whole.as_str()));
    assert_eq!(q.kind(ToolKind::Codex), Kind::Approval);
}

#[test]
fn a_choice_is_taken_only_on_its_instance_of_the_question() {
    let pane = Pane::new(CODEX);
    let project = Project::new(vec![(
        entry("codex-2", Some("reviewer"), ToolKind::Codex, &pane),
        4,
    )]);
    let (id, token) = project.listed(4);
    // Another question's id, a choice not the selected one: zero bytes.
    let err = project
        .answer(4, ("q-000000000000", &token), AnswerChoice::Option(2))
        .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    // A token the host did not issue: zero bytes.
    let err = project
        .answer(4, (&id, "d-0000"), AnswerChoice::Option(2))
        .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    assert!(pane.typed().is_empty());
    // Don't ask again: tap only, zero bytes, whatever the phone asked; the
    // instance is over.
    let err = project
        .answer(4, (&id, &token), AnswerChoice::Option(1))
        .unwrap_err();
    assert!(err.contains("tapped"), "{err}");
    assert!(pane.typed().is_empty());
    // Words to a choice: zero bytes.
    let (id, token) = project.listed(4);
    assert!(
        project
            .answer(4, (&id, &token), AnswerChoice::Text("yes".into()))
            .is_err()
    );
    assert!(pane.typed().is_empty());
    // No: down twice (the selection moves, the instance holds), then Enter.
    let (id, token) = project.listed(4);
    let v = project
        .answer(4, (&id, &token), AnswerChoice::Option(2))
        .unwrap();
    assert_eq!(v["outcome"], "answered");
    assert_eq!(
        pane.typed(),
        [b"\x1b[B".to_vec(), b"\x1b[B".to_vec(), b"\r".to_vec()]
    );
    // Submitted: that instance is over.
    *pane.screen.lock().unwrap() = CODEX.to_owned();
    assert!(
        project
            .answer(4, (&id, &token), AnswerChoice::Option(0))
            .is_err()
    );
    assert_eq!(pane.typed().len(), 3);
}

// Regression: the dialog went away and came back unchanged in the same
// turn, and the old confirmation reached Enter on the new one.
#[test]
fn a_dialog_gone_and_back_is_another_instance() {
    let pane = Pane::new(CODEX);
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    // Gone, as the watcher sees it...
    *pane.screen.lock().unwrap() = "• Working".into();
    project.dialogs.observe();
    // ...and back, unchanged.
    *pane.screen.lock().unwrap() = CODEX.to_owned();
    let err = project
        .answer(4, (&id, &token), AnswerChoice::Option(0))
        .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    assert!(pane.typed().is_empty());
    // Listed again: a new instance, which can be answered.
    let (id, again) = project.listed(4);
    assert_ne!(token, again);
    project
        .answer(4, (&id, &again), AnswerChoice::Option(0))
        .unwrap();
}

// Regression: the dialog went and came back between two of the
// watcher's looks, and the old token still reached Enter.
#[test]
fn a_change_the_host_did_not_see_ends_the_instance() {
    let pane = Pane::new(CODEX);
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    // Drawn again in between, the same dialog showing after.
    pane.redraw();
    let err = project
        .answer(4, (&id, &token), AnswerChoice::Option(0))
        .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    assert!(pane.typed().is_empty());
    // The watcher ends it as well.
    let (id, token) = project.listed(4);
    pane.redraw();
    project.dialogs.observe();
    assert!(
        project
            .answer(4, (&id, &token), AnswerChoice::Option(0))
            .is_err()
    );
    assert!(pane.typed().is_empty());
    // Listed again, as it is now: answered.
    let (id, token) = project.listed(4);
    project
        .answer(4, (&id, &token), AnswerChoice::Option(0))
        .unwrap();
    assert_eq!(pane.typed(), [b"\r".to_vec()]);
}

// Regression: a second answer while the first waited for the redraw after
// its Enter submitted the same token again.
#[test]
fn one_token_is_answered_once() {
    // Enter went, the dialog still shows: the token is spent.
    let pane = Pane::new(CODEX);
    *pane.enter_keeps.lock().unwrap() = true;
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    let v = project
        .answer(4, (&id, &token), AnswerChoice::Option(0))
        .unwrap();
    assert_eq!(v["outcome"], "sent");
    assert!(
        project
            .answer(4, (&id, &token), AnswerChoice::Option(0))
            .is_err()
    );
    // Nor is it given a new token while it still shows: listed for a tap.
    project.dialogs.observe();
    let v = project.list();
    assert!(v["questions"][0]["occurrence"].is_null(), "{v}");
    assert_eq!(v["questions"][0]["options"][0]["tapOnly"], true);
    assert_eq!(pane.typed(), [b"\r".to_vec()]);
    // Two answers at once: one claims it, the other is refused.
    let pane = Pane::new(CODEX);
    *pane.enter_keeps.lock().unwrap() = true;
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    let slow = || std::thread::sleep(std::time::Duration::from_millis(2));
    let results: Vec<bool> = std::thread::scope(|scope| {
        let runs: Vec<_> = (0..2)
            .map(|_| {
                scope.spawn(|| {
                    answer_with(
                        &project.dialogs,
                        &|| project.now(),
                        4,
                        &id,
                        &token,
                        &AnswerChoice::Option(2),
                        &typer,
                        &slow,
                    )
                    .is_ok()
                })
            })
            .collect();
        runs.into_iter().map(|r| r.join().unwrap()).collect()
    });
    assert_eq!(results.iter().filter(|ok| **ok).count(), 1, "{results:?}");
    let enters = pane
        .typed()
        .iter()
        .filter(|k| k.as_slice() == b"\r")
        .count();
    assert_eq!(enters, 1);
}

// Regression: the selection moved to a tap-only choice after the host's
// last look and before the Enter's enqueue, and Enter went in.
#[test]
fn a_redraw_just_before_enter_lands_stops_it() {
    let pane = Pane::new(CODEX);
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    *pane.before.lock().unwrap() = Some(Box::new(|p: &Pane| {
        // "Don't ask again" selected now.
        let mut s = p.screen.lock().unwrap();
        *s = s
            .replace("› 1. Yes, proceed", "  1. Yes, proceed")
            .replace("  2. Yes, and don't", "› 2. Yes, and don't");
    }));
    assert!(
        project
            .answer(4, (&id, &token), AnswerChoice::Option(0))
            .is_err()
    );
    assert!(!pane.typed().iter().any(|k| k.as_slice() == b"\r"));
}

// Regression: the answer's first read after its claim took a redraw it had
// not caused for the instance, and Enter went in on it.
#[test]
fn nothing_drawn_after_the_claim_is_taken_before_a_key_causes_it() {
    let pane = Pane::new(CODEX);
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    let session: Arc<dyn TerminalSession> = pane.clone();
    project.dialogs.claim(&token, 4, &id, &session).unwrap();
    // Drawn again, the same dialog and selection showing.
    pane.redraw();
    let err = answer_dialog(
        &project.dialogs,
        &token,
        &|| project.now().first().map(|(t, _)| t.clone()),
        4,
        &id,
        &AnswerChoice::Option(0),
        &typer,
        &|| {},
    )
    .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    assert!(pane.typed().is_empty());
}

// Regression: listing a dialog an answer had claimed gave it a second,
// claimable token.
#[test]
fn a_claimed_dialog_gets_no_second_token() {
    let pane = Pane::new(CODEX);
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    let session: Arc<dyn TerminalSession> = pane.clone();
    project.dialogs.claim(&token, 4, &id, &session).unwrap();
    let v = project.list();
    assert!(v["questions"][0]["occurrence"].is_null(), "{v}");
    assert_eq!(v["questions"][0]["options"][0]["tapOnly"], true);
}

// Regression (decision c): the call's own prompts are several lines,
// drawn collapsed; they were refused, so a call could not ask the project
// or start a task. What Enter submits is known by construction instead:
// an empty line, our paste at G, Enter at exactly G+1.
const PROMPT: &str = "Start task t-1:\nGoal: dark mode\nScope: the theme\nDone when: it switches";

#[test]
fn a_calls_prompt_over_several_lines_is_submitted_though_drawn_collapsed() {
    let (pane, project) = idle_app();
    let v = send(&project, PROMPT).unwrap();
    assert_eq!(v["outcome"], "answered");
    let typed = pane.typed();
    assert_eq!(typed.len(), 2);
    assert!(String::from_utf8_lossy(&typed[0]).contains("Done when: it switches"));
    assert_eq!(typed[1], b"\r");
}

#[test]
fn a_key_typed_between_the_prompts_paste_and_enter_stops_enter() {
    let (pane, project) = idle_app();
    *pane.after.lock().unwrap() = Some(Box::new(|p: &Pane| {
        // Someone types on the Mac right after the paste.
        if p.typed.lock().unwrap().len() == 1 {
            p.poke();
        }
    }));
    let err = send(&project, PROMPT).unwrap_err();
    assert!(err.contains("nothing was submitted"), "{err}");
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
    // Typed just as Enter is written, after the host's last look: the
    // input gate refuses it, Enter being only for exactly G+1.
    let (pane, project) = idle_app();
    *pane.before.lock().unwrap() = Some(Box::new(|p: &Pane| {
        if p.typed.lock().unwrap().len() == 1 {
            p.poke();
        }
    }));
    assert!(send(&project, PROMPT).is_err());
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
}

#[test]
fn a_dialog_come_between_the_prompts_paste_and_enter_stops_enter() {
    let (pane, project) = idle_app();
    if let Some((t, _)) = project.terminals.lock().unwrap().first_mut() {
        t.tool = ToolKind::Codex;
    }
    *pane.after.lock().unwrap() = Some(Box::new(|p: &Pane| {
        *p.screen.lock().unwrap() = CODEX.into();
    }));
    let err = send(&project, PROMPT).unwrap_err();
    assert!(err.contains("nothing was submitted"), "{err}");
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
}

#[test]
fn a_prompt_is_never_pasted_onto_something_already_in_the_line() {
    for (typed, drawn) in [
        (Some(""), Some("restored prompt")),
        (Some("half typed"), Some("half typed")),
        (None, Some("")),
    ] {
        let (pane, project) = idle_app();
        *pane.draft.lock().unwrap() = typed.map(String::from);
        *pane.drawn.lock().unwrap() = drawn.map(String::from);
        assert!(send(&project, PROMPT).is_err());
        assert!(pane.typed().is_empty(), "{typed:?} {drawn:?}");
    }
}

// A chat answer still must be read back exactly: one that would not fit
// on the input line's row is not typed at all.
#[test]
fn a_chat_answer_that_cannot_be_read_back_is_not_typed_at_all() {
    let pane = chat_pane(ASKED);
    let project = app(&pane);
    let (id, occurrence) = chat_row(&project);
    let long = "word ".repeat(40);
    let err = project
        .answer(
            2,
            (&id, &occurrence),
            AnswerChoice::Text(routed(long.trim())),
        )
        .unwrap_err();
    assert!(err.contains("too long to check"), "{err}");
    assert!(pane.typed().is_empty());
}

#[test]
fn a_restarted_terminal_is_not_the_instance() {
    let pane = Pane::new(CODEX);
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    // The same terminal, a new session showing the same dialog.
    let fresh = Pane::new(CODEX);
    *project.terminals.lock().unwrap() = vec![(entry("codex-2", None, ToolKind::Codex, &fresh), 4)];
    assert!(
        project
            .answer(4, (&id, &token), AnswerChoice::Option(0))
            .is_err()
    );
    assert!(pane.typed().is_empty() && fresh.typed().is_empty());
}

#[test]
fn the_same_dialog_on_two_terminals_is_answered_only_where_named() {
    let coder = Pane::new(CODEX);
    let reviewer = Pane::new(CODEX);
    let project = Project::new(vec![
        (entry("codex-1", Some("coder"), ToolKind::Codex, &coder), 3),
        (
            entry("codex-2", Some("reviewer"), ToolKind::Codex, &reviewer),
            4,
        ),
    ]);
    let (id, token) = project.listed(4);
    assert_ne!(token, project.listed(3).1);
    project
        .answer(4, (&id, &token), AnswerChoice::Option(0))
        .unwrap();
    assert_eq!(reviewer.typed(), [b"\r".to_vec()]);
    assert!(coder.typed().is_empty());
}

#[test]
fn a_wrapped_label_or_a_mixed_form_writes_nothing() {
    let wrapped = Pane::new(
        "x\n  Would you like to run the following command?\n  $ rm -rf build\n\n› 1. Yes, proceed (y)\n  2. Yes, and\n       always allow rm (p)\n  3. No, and tell Codex what to do differently (esc)\n  Press enter to confirm or esc to cancel",
    );
    let mixed = Pane::new(
        "x\nQuestion 1/1\nPick one.\n  1. Continue\n  2. Wait\n› Add notes\nenter to submit answer | esc to go back",
    );
    let project = Project::new(vec![
        (entry("codex-1", None, ToolKind::Codex, &wrapped), 3),
        (entry("codex-2", None, ToolKind::Codex, &mixed), 4),
    ]);
    let (w, wt) = project.listed(3);
    assert!(
        project
            .answer(3, (&w, &wt), AnswerChoice::Option(1))
            .is_err()
    );
    let (m, mt) = project.listed(4);
    assert!(
        project
            .answer(4, (&m, &mt), AnswerChoice::Text("later".into()))
            .is_err()
    );
    assert!(wrapped.typed().is_empty());
    assert!(mixed.typed().is_empty());
}

#[test]
fn words_go_to_a_known_field_which_must_show_them_exactly_before_enter() {
    let pane = Pane::new(FORM);
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    // A tab, a new line, or spaces around it are refused before anything.
    for bad in ["a\tb", "a\nb", "", " a"] {
        let (id, token) = project.listed(4);
        assert!(
            project
                .answer(4, (&id, &token), AnswerChoice::Text(bad.into()))
                .is_err()
        );
    }
    assert!(pane.typed().is_empty());
    // The field redraws with the words (repeated spaces kept); the question
    // keeps its id and instance.
    let (id, token) = project.listed(4);
    let v = project
        .answer(
            4,
            (&id, &token),
            AnswerChoice::Text("Because  the tests fail".into()),
        )
        .unwrap();
    assert_eq!(v["outcome"], "answered");
    let typed = pane.typed();
    assert_eq!(typed.len(), 2);
    assert!(String::from_utf8_lossy(&typed[0]).contains("Because  the tests fail"));
    assert_eq!(typed[1], b"\r");
}

// Regression: a value the narrow field wrapped onto a second row was not
// recognised, so the answer was never submitted.
#[test]
fn words_the_field_wraps_onto_a_second_row_are_still_recognised() {
    let pane = Pane::new(FORM);
    *pane.after.lock().unwrap() = Some(Box::new(|p: &Pane| {
        let mut s = p.screen.lock().unwrap();
        *s = s.replace("› two words", "› two\n  words");
    }));
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    project
        .answer(4, (&id, &token), AnswerChoice::Text("two words".into()))
        .unwrap();
    assert_eq!(pane.typed().last().unwrap(), b"\r");
}

// Regression: the field was compared with spaces collapsed, so words
// shown otherwise than given were submitted.
#[test]
fn a_field_showing_other_spacing_than_the_words_gets_no_enter() {
    let pane = Pane::new(FORM);
    *pane.after.lock().unwrap() = Some(Box::new(|p: &Pane| {
        let mut s = p.screen.lock().unwrap();
        *s = s.replace("a  b", "a b");
    }));
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    let err = project
        .answer(4, (&id, &token), AnswerChoice::Text("a  b".into()))
        .unwrap_err();
    assert!(err.contains("nothing was submitted"), "{err}");
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
}

// Regression: "Type your answer tomorrow" (someone's words) read as the
// empty placeholder, and the answer was pasted after it.
#[test]
fn a_draft_already_in_the_field_is_left_alone() {
    for draft in ["half typed", "Type your answer tomorrow"] {
        let pane = Pane::new(&FORM.replace("Type your answer (optional)", draft));
        let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
        let (id, token) = project.listed(4);
        let err = project
            .answer(4, (&id, &token), AnswerChoice::Text("words".into()))
            .unwrap_err();
        assert!(err.contains("already typed"), "{err}");
        assert!(pane.typed().is_empty(), "{draft}");
    }
}

#[test]
fn a_question_that_changes_between_the_paste_and_enter_gets_no_enter() {
    let pane = Pane::new(FORM);
    *pane.after.lock().unwrap() = Some(Box::new(|p: &Pane| {
        let mut s = p.screen.lock().unwrap();
        *s = s.replace("Why?", "Delete the branch?");
    }));
    let project = Project::new(vec![(entry("codex-2", None, ToolKind::Codex, &pane), 4)]);
    let (id, token) = project.listed(4);
    let err = project
        .answer(4, (&id, &token), AnswerChoice::Text(routed("yes")))
        .unwrap_err();
    assert!(err.contains("nothing was submitted"), "{err}");
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
}

const ASKED: &str = "Question [t-ab1]. Include archived rows?";

/// An answer to t-ab1's question, routed by name as the phone sends it.
fn routed(words: &str) -> String {
    format!("Answer to Question [t-ab1]: {words}")
}

fn chat_pane(last: &str) -> Arc<Pane> {
    let pane = Pane::new("› ");
    *pane.chat.lock().unwrap() = Some(ChatHistory {
        revision: 7,
        messages: vec![(true, "Run t-ab1".into()), (false, last.into())],
    });
    *pane.draft.lock().unwrap() = Some(String::new());
    *pane.drawn.lock().unwrap() = Some(String::new());
    pane
}

fn app(pane: &Arc<Pane>) -> Project {
    let mut app = entry("claude-1", Some(APP_SESSION), ToolKind::Claude, pane);
    app.state = TermActivity::Idle;
    Project::new(vec![(app, 2)])
}

fn chat_row(project: &Project) -> (String, String) {
    let v = project.list();
    let row = &v["questions"][0];
    assert_eq!(row["chat"], true);
    (
        row["id"].as_str().unwrap().to_owned(),
        row["occurrence"].as_str().unwrap().to_owned(),
    )
}

#[test]
fn a_tasks_question_in_the_chat_is_answered_while_the_chat_ends_on_it() {
    let pane = chat_pane(ASKED);
    let project = app(&pane);
    assert_eq!(project.list()["questions"][0]["task"], "t-ab1");
    let (id, occurrence) = chat_row(&project);
    assert_eq!(id, chat_id(ASKED));
    // The message that asked it, by its absolute place (7 pushed, 2 kept).
    assert_eq!(occurrence, "m6");
    // A choice is not an answer to it.
    assert!(
        project
            .answer(2, (&id, &occurrence), AnswerChoice::Option(0))
            .is_err()
    );
    let v = project
        .answer(
            2,
            (&id, &occurrence),
            AnswerChoice::Text(routed("Only active.")),
        )
        .unwrap();
    assert_eq!(v["outcome"], "answered");
    assert_eq!(pane.typed().len(), 2);
    // Another message now: the same id no longer answers.
    let other = chat_pane("Question [t-ab1]. Delete them too?");
    let err = app(&other)
        .answer(2, (&id, "m6"), AnswerChoice::Text(routed("yes")))
        .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    assert!(other.typed().is_empty());
}

// Regression: the occurrence was the message's index in the kept history,
// so once the history was full (the oldest dropped on each push) an answered
// question asked again got the same handle, and a stale confirmation for
// the first asking typed into the second.
#[test]
fn a_stale_answer_writes_nothing_after_the_history_rolls_over() {
    let filler = |n: usize| -> Vec<(bool, String)> {
        (0..n)
            .map(|i| (i % 2 == 0, format!("message {i}")))
            .collect()
    };
    // A full history (256 kept), ending on the question.
    let mut before = filler(254);
    before.push((true, "Run t-ab1".into()));
    before.push((false, ASKED.into()));
    let pane = Pane::new("› ");
    *pane.chat.lock().unwrap() = Some(ChatHistory {
        revision: 1000,
        messages: before,
    });
    *pane.draft.lock().unwrap() = Some(String::new());
    *pane.drawn.lock().unwrap() = Some(String::new());
    let project = app(&pane);
    let (id, stale) = chat_row(&project);
    // Answered, then asked again: two pushes, the two oldest dropped, so the
    // new asking sits at the same index of the kept list.
    let mut after = filler(252);
    after.push((true, "Run t-ab1".into()));
    after.push((false, ASKED.into()));
    after.push((true, routed("yes")));
    after.push((false, ASKED.into()));
    assert_eq!(after.len(), 256);
    *pane.chat.lock().unwrap() = Some(ChatHistory {
        revision: 1002,
        messages: after,
    });
    let (_, fresh) = chat_row(&project);
    assert_ne!(stale, fresh);
    let err = project
        .answer(2, (&id, &stale), AnswerChoice::Text(routed("yes")))
        .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    assert!(pane.typed().is_empty(), "the stale handle writes no byte");
}

#[test]
fn another_asking_of_the_question_is_another_occurrence() {
    // Asked again later (another message): the earlier asking is not it.
    let pane = chat_pane(ASKED);
    if let Some(chat) = pane.chat.lock().unwrap().as_mut() {
        chat.messages.push((true, "Run t-ab1 again".into()));
        chat.messages.push((false, ASKED.into()));
        chat.revision += 2;
    }
    let err = app(&pane)
        .answer(
            2,
            (&chat_id(ASKED), "m6"),
            AnswerChoice::Text(routed("yes")),
        )
        .unwrap_err();
    assert!(err.contains("changed"), "{err}");
    assert!(pane.typed().is_empty());
}

#[test]
fn a_chat_that_changes_between_the_paste_and_enter_gets_no_enter() {
    let pane = chat_pane(ASKED);
    *pane.after.lock().unwrap() = Some(Box::new(|p: &Pane| {
        if let Some(chat) = p.chat.lock().unwrap().as_mut() {
            chat.messages
                .push((false, "Question [t-ab1]. Also drop the table?".into()));
            chat.revision += 1;
        }
    }));
    let project = app(&pane);
    let (id, occurrence) = chat_row(&project);
    let err = project
        .answer(2, (&id, &occurrence), AnswerChoice::Text(routed("yes")))
        .unwrap_err();
    assert!(err.contains("nothing was submitted"), "{err}");
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
}

// Regression: a prompt the agent put back after an interrupt was drawn in
// its input line while the tracker said empty; the answer was pasted into
// it and both were submitted.
#[test]
fn a_draft_in_the_agents_prompt_is_never_submitted_with_the_answer() {
    // Drawn on screen, not typed here: zero bytes.
    let pane = chat_pane(ASKED);
    *pane.drawn.lock().unwrap() = Some("half a thought".into());
    let project = app(&pane);
    let (id, occurrence) = chat_row(&project);
    assert!(
        project
            .answer(2, (&id, &occurrence), AnswerChoice::Text(routed("yes")))
            .is_err()
    );
    assert!(pane.typed().is_empty());
    // Typed on the Mac: zero bytes.
    let pane = chat_pane(ASKED);
    *pane.draft.lock().unwrap() = Some("half a thought".into());
    *pane.drawn.lock().unwrap() = Some("half a thought".into());
    let project = app(&pane);
    assert!(
        project
            .answer(2, (&id, &occurrence), AnswerChoice::Text(routed("yes")))
            .is_err()
    );
    assert!(pane.typed().is_empty());
    // Not known (an uncertain key): zero bytes.
    let pane = chat_pane(ASKED);
    *pane.draft.lock().unwrap() = None;
    let project = app(&pane);
    assert!(
        project
            .answer(2, (&id, &occurrence), AnswerChoice::Text(routed("yes")))
            .is_err()
    );
    assert!(pane.typed().is_empty());
    // Edited on the Mac between the paste and Enter: no Enter.
    let pane = chat_pane(ASKED);
    *pane.after.lock().unwrap() = Some(Box::new(|p: &Pane| {
        p.poke();
        for line in [&p.draft, &p.drawn] {
            if let Some(d) = line.lock().unwrap().as_mut() {
                d.push_str(" and more");
            }
        }
    }));
    let project = app(&pane);
    let err = project
        .answer(2, (&id, &occurrence), AnswerChoice::Text(routed("yes")))
        .unwrap_err();
    assert!(err.contains("nothing was submitted"), "{err}");
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
}

const CLAUDE_APPROVAL: &str =
    include_str!("../../../connecto-adapter/tests/fixtures/claude/approval_prompt_default.txt");

// Regression: a chat answer's paste and Enter went in on whatever the
// screen showed then; a permission dialog drawn just before the Enter
// took it.
#[test]
fn a_dialog_drawn_just_before_a_chat_answers_paste_or_enter_stops_it() {
    // Just before the paste lands: nothing typed.
    let pane = chat_pane(ASKED);
    *pane.before.lock().unwrap() = Some(Box::new(|p: &Pane| {
        if p.typed.lock().unwrap().is_empty() {
            *p.screen.lock().unwrap() = CLAUDE_APPROVAL.into();
        }
    }));
    let project = app(&pane);
    let (id, occurrence) = chat_row(&project);
    assert!(
        project
            .answer(2, (&id, &occurrence), AnswerChoice::Text(routed("yes")))
            .is_err()
    );
    assert!(pane.typed().is_empty());
    // Just before the Enter lands: no Enter.
    let pane = chat_pane(ASKED);
    *pane.before.lock().unwrap() = Some(Box::new(|p: &Pane| {
        if p.typed.lock().unwrap().len() == 1 {
            *p.screen.lock().unwrap() = CLAUDE_APPROVAL.into();
        }
    }));
    let project = app(&pane);
    let (id, occurrence) = chat_row(&project);
    let err = project
        .answer(2, (&id, &occurrence), AnswerChoice::Text(routed("yes")))
        .unwrap_err();
    assert!(err.contains("nothing was submitted"), "{err}");
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
}

fn history(messages: &[(bool, &str)]) -> ChatHistory {
    ChatHistory {
        revision: 9,
        messages: messages
            .iter()
            .map(|(user, text)| (*user, (*text).to_owned()))
            .collect(),
    }
}

fn pending(messages: &[(bool, &str)]) -> Vec<String> {
    pending_chat_questions(&history(messages))
        .into_iter()
        .map(|(task, _)| task)
        .collect()
}

const NEW_TASK: &str = "A task agreed by voice in Connecto Chat.\nThis is a new task, id t-ab2: a new request, not an answer to any \"Question [<id>].\" you asked.";

// Regression: only the chat's last message counted, so a task's question
// could no longer be answered by voice once anything else was said.
#[test]
fn a_tasks_question_stays_pending_across_unrelated_turns() {
    let asked = (false, ASKED);
    // A new task, and the agent's reply about it: still pending.
    assert_eq!(
        pending(&[
            asked,
            (true, NEW_TASK),
            (false, "Queued [t-ab2]. It waits.")
        ]),
        ["t-ab1"]
    );
    // Answered by name, even after other turns.
    assert!(
        pending(&[
            asked,
            (true, NEW_TASK),
            (false, "Queued [t-ab2]."),
            (true, "Answer to Question [t-ab1]: only active rows"),
        ])
        .is_empty()
    );
    // Answered as before: a plain prompt right after it.
    assert!(pending(&[asked, (true, "only active rows")]).is_empty());
    // The task moved on (a report naming it), or asked again instead.
    assert!(
        pending(&[
            asked,
            (true, NEW_TASK),
            (false, "Task complete [t-ab1]. Done.")
        ])
        .is_empty()
    );
    let again = "Question [t-ab1]. Which table?";
    let chat = history(&[asked, (true, NEW_TASK), (false, again)]);
    let p = pending_chat_questions(&chat);
    assert_eq!(p.len(), 1);
    assert_eq!(p[0].1, again);
}

#[test]
fn a_pending_question_is_answered_by_name_after_unrelated_turns() {
    let pane = chat_pane(ASKED);
    if let Some(chat) = pane.chat.lock().unwrap().as_mut() {
        chat.messages.push((true, NEW_TASK.into()));
        chat.messages
            .push((false, "Queued [t-ab2]. It waits.".into()));
        chat.revision += 2;
    }
    let project = app(&pane);
    let (id, occurrence) = chat_row(&project);
    assert_eq!(id, chat_id(ASKED));
    project
        .answer(
            2,
            (&id, &occurrence),
            AnswerChoice::Text(routed("Only active.")),
        )
        .unwrap();
    let typed = pane.typed();
    assert_eq!(typed.len(), 2);
    assert!(
        String::from_utf8_lossy(&typed[0]).contains("Answer to Question [t-ab1]: Only active."),
        "{:?}",
        String::from_utf8_lossy(&typed[0])
    );
    // Answered: no longer listed.
    assert!(project.list()["questions"].as_array().unwrap().is_empty());
}

// Regression: the phone read back the words alone while the host typed
// them with the routing lead in front: what is typed must be what was read
// back, so the words arrive already routed, and others are refused.
#[test]
fn a_chat_answer_is_typed_exactly_as_read_back() {
    let pane = chat_pane(ASKED);
    let project = app(&pane);
    let (id, occurrence) = chat_row(&project);
    let err = project
        .answer(
            2,
            (&id, &occurrence),
            AnswerChoice::Text("Only active.".into()),
        )
        .unwrap_err();
    assert!(err.contains("begins with"), "{err}");
    assert!(pane.typed().is_empty());
    let err = project
        .answer(
            2,
            (&id, &occurrence),
            AnswerChoice::Text("Answer to Question [t-zz9]: Only active.".into()),
        )
        .unwrap_err();
    assert!(err.contains("begins with"), "{err}");
    assert!(pane.typed().is_empty());
}

// Regression: a report that only mentions a waiting task's id ("waiting
// for [t-ab1]") closed its question.
#[test]
fn a_report_that_mentions_a_waiting_task_leaves_its_question_pending() {
    assert_eq!(
        pending(&[
            (false, ASKED),
            (true, NEW_TASK),
            (false, "Queued [t-ab2]. Waiting for [t-ab1] to be answered."),
        ]),
        ["t-ab1"]
    );
    // Its own report, by its leading marker, closes it (Italian too).
    assert!(
        pending(&[
            (false, ASKED),
            (true, NEW_TASK),
            (false, "Compito completato [t-ab1].")
        ])
        .is_empty()
    );
}

// Regression: every pending question took the chat's latest revision as its
// occurrence, so a report about another task dropped a proposal for it.
#[test]
fn a_report_about_another_task_leaves_the_questions_occurrence() {
    let pane = chat_pane(ASKED);
    let project = app(&pane);
    let (id, before) = chat_row(&project);
    if let Some(chat) = pane.chat.lock().unwrap().as_mut() {
        chat.messages.push((true, NEW_TASK.into()));
        chat.messages
            .push((false, "Task complete [t-zz1]. Done.".into()));
        chat.revision += 2;
    }
    let (_, after) = chat_row(&project);
    assert_eq!(before, after);
    project
        .answer(
            2,
            (&id, &before),
            AnswerChoice::Text(routed("Only active.")),
        )
        .unwrap();
}

// Regression: a chat question offered words by voice on a tool whose input
// line cannot be read back, so a yes was collected for nothing.
#[test]
fn words_by_voice_only_where_the_input_line_can_be_read_back() {
    for (tool, voice) in [
        (ToolKind::Claude, true),
        (ToolKind::Codex, true),
        (ToolKind::Pi, false),
    ] {
        let pane = chat_pane(ASKED);
        let mut session = entry("claude-1", Some(APP_SESSION), tool, &pane);
        session.state = TermActivity::Idle;
        let v = Project::new(vec![(session, 2)]).list();
        assert_eq!(v["questions"][0]["voiceText"], voice, "{tool:?}");
    }
}

// Regression: the app session's state was read from the entry as listed,
// so a turn that began between the paste and Enter went unseen.
#[test]
fn the_app_session_is_read_as_it_is_now_before_enter() {
    let pane = chat_pane(ASKED);
    let project = Arc::new(app(&pane));
    let turning = Arc::clone(&project);
    *pane.after.lock().unwrap() = Some(Box::new(move |_| {
        if let Some((t, _)) = turning.terminals.lock().unwrap().first_mut() {
            t.state = TermActivity::Working;
        }
    }));
    let (id, occurrence) = chat_row(&project);
    assert!(
        project
            .answer(2, (&id, &occurrence), AnswerChoice::Text(routed("yes")))
            .is_err()
    );
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
}

fn idle_app() -> (Arc<Pane>, Project) {
    let pane = chat_pane("Done: the tests pass.");
    let project = app(&pane);
    (pane, project)
}

fn send(project: &Project, text: &str) -> Result<Value, String> {
    send_prompt_with(&|| project.now(), 2, text, &typer, &|| {})
}

#[test]
fn a_calls_prompt_goes_to_an_idle_app_session_with_an_empty_input_line() {
    let (pane, project) = idle_app();
    let v = send(&project, "Start task t-1: dark mode").unwrap();
    assert_eq!(v["outcome"], "answered");
    let typed = pane.typed();
    assert_eq!(typed.len(), 2);
    assert!(String::from_utf8_lossy(&typed[0]).contains("dark mode"));
    assert_eq!(typed[1], b"\r");
}

// Regression: a task start was delivered into a text question that came
// after the confirmation, and generic send pressed Enter without looking
// again.
#[test]
fn a_calls_prompt_never_goes_into_a_question() {
    // A text question on screen already: zero bytes.
    let (pane, project) = idle_app();
    *pane.screen.lock().unwrap() = FORM.into();
    if let Some((t, _)) = project.terminals.lock().unwrap().first_mut() {
        t.tool = ToolKind::Codex;
    }
    assert!(send(&project, "Start task t-1").is_err());
    assert!(pane.typed().is_empty());
    // One that comes between the paste and Enter: no Enter.
    let (pane, project) = idle_app();
    *pane.after.lock().unwrap() = Some(Box::new(|p: &Pane| {
        *p.screen.lock().unwrap() = CODEX.into();
    }));
    if let Some((t, _)) = project.terminals.lock().unwrap().first_mut() {
        t.tool = ToolKind::Codex;
    }
    let err = send(&project, "Start task t-1").unwrap_err();
    assert!(err.contains("nothing was submitted"), "{err}");
    assert_eq!(pane.typed().len(), 1, "the paste only, no Enter");
    // Busy, or something in the input line: zero bytes.
    let (pane, project) = idle_app();
    if let Some((t, _)) = project.terminals.lock().unwrap().first_mut() {
        t.state = TermActivity::Working;
    }
    assert!(send(&project, "Start task t-1").is_err());
    let (other, project2) = idle_app();
    *other.drawn.lock().unwrap() = Some("restored prompt".into());
    assert!(send(&project2, "Start task t-1").is_err());
    assert!(pane.typed().is_empty() && other.typed().is_empty());
}
