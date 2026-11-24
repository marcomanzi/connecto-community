//! Project-memory capture at the registry's touch points (spec §5.3).
//!
//! Each Claude or Codex terminal's actor holds a [`TerminalTap`] and calls it
//! where it already sees the events: the tool's start, an accepted prompt, a
//! turn end, the session's end. The tap turns them into [`CaptureEvent`]s and
//! offers them to the app's [`CaptureSink`], set once with [`set_sink`].
//!
//! Rules kept here:
//! - **never blocking**: [`CaptureSink::offer`] must return at once (the app's
//!   sink is a bounded channel that counts what it drops); the tap only
//!   copies the event's text, it reads no file;
//! - **every turn**, whatever it did: no filter on diffs, commits or tools
//!   (spec §5.10);
//! - only agent terminals (Claude, Codex) of a project: shells are skipped,
//!   and utility sessions never reach the registry at all.
//!
//! Redaction, deduplication and the internal-event rules belong to the memory
//! engine, which receives raw text.
//!
//! # Who wrote a prompt ([`PromptSource`])
//!
//! A user statement always counts as independent evidence and as approval,
//! so a prompt is [`PromptSource::User`] only when Connecto positively knows
//! a person typed every byte of it in this app's window. That is decided by
//! the session's ledger ([`crate::input_ledger`], the one way into a
//! terminal's PTY): a terminal any other writer touched is quarantined until
//! it is quiet again, and no prompt in quarantine is the user's. The actor
//! hands the tap the verdict of the session whose hook channel the prompt
//! came from. Whether a verdict of the user's is kept at all is the app's
//! [`TerminalCredit`] (with the sink, [`set_sink_with`]); [`set_sink`] keeps
//! none. A `[connecto …]`
//! header always wins: `[connecto notice]` is Connecto's, any other is an
//! agent's.

use std::path::{Path, PathBuf};
use std::sync::{Arc, OnceLock};
use std::time::{SystemTime, UNIX_EPOCH};

use connecto_core::Reply;
use connecto_core::memory::Provider;
use connecto_store::Tool;

use crate::input_ledger::{TerminalCredit, Verdict};

/// What a turn end said about its reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureReply {
    Text(String),
    None,
    /// Too large for the hook: the engine reads it from `log`.
    Dropped,
}

/// Who wrote a prompt, as far as Connecto knows (see the module doc).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PromptSource {
    /// Typed or pasted in this app's window, and nothing else was.
    User,
    /// Typed by Connecto for another agent or an automation (relay, `send`,
    /// `keys`, a bridge message).
    Agent,
    /// Connecto's own notice.
    Connecto,
    /// Not known: never taken as the user.
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CaptureWhat {
    Prompt {
        text: String,
        source: PromptSource,
        /// The hook cut the text (over 64 KiB): it is not the whole prompt.
        truncated: bool,
    },
    TurnEnd {
        reply: CaptureReply,
    },
    SessionEnd {
        how: String,
    },
}

/// One observation of one terminal. Text is raw, as the hook gave it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CaptureEvent {
    pub project_id: String,
    pub project: PathBuf,
    pub terminal: String,
    pub provider: Provider,
    pub tool_session: Option<String>,
    /// Unix milliseconds this session (attempt) started.
    pub session_start_ms: u64,
    /// Codex's turn id, else `#<n>`: the prompt's ordinal in this session.
    pub turn: String,
    pub log: Option<PathBuf>,
    pub what: CaptureWhat,
}

/// Where captured events go. `offer` must never block.
pub trait CaptureSink: Send + Sync {
    fn offer(&self, event: CaptureEvent);
}

static SINK: OnceLock<(Arc<dyn CaptureSink>, TerminalCredit)> = OnceLock::new();

/// Set the app's sink, once, with no prompt ever credited to the user
/// ([`TerminalCredit::Off`]). False if one was already set.
pub fn set_sink(sink: Arc<dyn CaptureSink>) -> bool {
    set_sink_with(sink, TerminalCredit::Off)
}

/// Set the app's sink, once, and whether terminal prompts may be credited to
/// the user. False if one was already set.
pub fn set_sink_with(sink: Arc<dyn CaptureSink>, credit: TerminalCredit) -> bool {
    SINK.set((sink, credit)).is_ok()
}

/// The app's sink, if any.
#[must_use]
pub(crate) fn sink() -> Option<&'static dyn CaptureSink> {
    SINK.get().map(|(sink, _)| sink.as_ref())
}

/// Whether terminal prompts may be credited to the user (off until the app
/// says otherwise).
fn credit() -> TerminalCredit {
    SINK.get()
        .map_or(TerminalCredit::Off, |(_, credit)| *credit)
}

fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| u64::try_from(d.as_millis()).unwrap_or(u64::MAX))
}

/// One terminal's capture state, kept by its actor.
pub(crate) struct TerminalTap {
    terminal: String,
    provider: Option<Provider>,
    tool_session: Option<String>,
    session_start_ms: u64,
    prompts: u64,
    turn: String,
    ended: bool,
    /// Whether a prompt may be the user's at all.
    credit: TerminalCredit,
}

impl TerminalTap {
    pub(crate) fn new(terminal: &str, tool: Tool) -> Self {
        let provider = match tool {
            Tool::Claude => Some(Provider::Claude),
            Tool::Codex => Some(Provider::Codex),
            // Project memory knows two providers: a Pi, Antigravity or
            // OpenCode terminal's turns are not captured yet.
            Tool::Pi | Tool::Antigravity | Tool::OpenCode | Tool::Shell => None,
        };
        Self {
            terminal: terminal.to_owned(),
            provider,
            tool_session: None,
            session_start_ms: now_ms(),
            prompts: 0,
            turn: "#0".to_owned(),
            ended: false,
            credit: credit(),
        }
    }

    /// The tool started a session (a new attempt).
    pub(crate) fn started(&mut self, tool_session: &str) {
        self.tool_session = Some(tool_session.to_owned());
        self.session_start_ms = now_ms();
        self.prompts = 0;
        self.ended = false;
    }

    /// Who wrote the prompt just submitted, from the ledger's verdict.
    fn source(text: &str, verdict: Verdict) -> PromptSource {
        let head = text.trim_start();
        if head.starts_with("[connecto notice]") {
            PromptSource::Connecto
        } else if head.starts_with("[connecto ") {
            PromptSource::Agent
        } else {
            match verdict {
                Verdict::User => PromptSource::User,
                Verdict::Foreign => PromptSource::Agent,
                Verdict::Unknown => PromptSource::Unknown,
            }
        }
    }

    fn offer(
        &self,
        sink: Option<&dyn CaptureSink>,
        project: Option<&(String, PathBuf)>,
        log: Option<&Path>,
        what: CaptureWhat,
    ) {
        let (Some(sink), Some((project_id, project)), Some(provider)) =
            (sink, project, self.provider)
        else {
            return;
        };
        sink.offer(CaptureEvent {
            project_id: project_id.clone(),
            project: project.clone(),
            terminal: self.terminal.clone(),
            provider,
            tool_session: self.tool_session.clone(),
            session_start_ms: self.session_start_ms,
            turn: self.turn.clone(),
            log: log.map(Path::to_path_buf),
            what,
        });
    }

    /// A prompt was accepted. `verdict` is the session ledger's for it;
    /// `log` the session log, for a cut prompt.
    pub(crate) fn prompt(
        &mut self,
        sink: Option<&dyn CaptureSink>,
        project: Option<&(String, PathBuf)>,
        text: &connecto_core::EventText,
        turn: Option<&str>,
        verdict: Verdict,
        log: Option<&Path>,
    ) {
        self.prompts += 1;
        self.turn = turn.map_or_else(|| format!("#{}", self.prompts), str::to_owned);
        let source = Self::source(text.expose(), self.credit.apply(verdict));
        self.offer(
            sink,
            project,
            log,
            CaptureWhat::Prompt {
                text: text.expose().clone(),
                source,
                truncated: text.truncated(),
            },
        );
    }

    pub(crate) fn turn_end(
        &mut self,
        sink: Option<&dyn CaptureSink>,
        project: Option<&(String, PathBuf)>,
        reply: &Reply,
        turn: Option<&str>,
        log: Option<&Path>,
    ) {
        if let Some(turn) = turn {
            turn.clone_into(&mut self.turn);
        }
        let reply = match reply {
            // A cut reply is not the reply: read it whole from the log.
            Reply::Text(text) if text.truncated() => CaptureReply::Dropped,
            Reply::Text(text) => CaptureReply::Text(text.expose().clone()),
            Reply::None => CaptureReply::None,
            Reply::Dropped => CaptureReply::Dropped,
        };
        self.offer(sink, project, log, CaptureWhat::TurnEnd { reply });
    }

    /// The session exited or the terminal closed; once per session.
    pub(crate) fn ended(
        &mut self,
        sink: Option<&dyn CaptureSink>,
        project: Option<&(String, PathBuf)>,
        how: &str,
        log: Option<&Path>,
    ) {
        if self.ended || self.tool_session.is_none() {
            return;
        }
        self.ended = true;
        self.offer(
            sink,
            project,
            log,
            CaptureWhat::SessionEnd {
                how: how.to_owned(),
            },
        );
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use std::sync::Mutex;

    use connecto_core::EventText;

    use super::*;

    #[derive(Default)]
    struct Collect(Mutex<Vec<CaptureEvent>>);

    impl CaptureSink for Collect {
        fn offer(&self, event: CaptureEvent) {
            self.0.lock().unwrap().push(event);
        }
    }

    fn project() -> (String, PathBuf) {
        ("p1".to_owned(), PathBuf::from("/work/p1"))
    }

    fn text(t: &str) -> EventText {
        EventText::new(t.to_owned())
    }

    #[test]
    fn a_tap_reports_every_turn_and_one_end() {
        let sink = Collect::default();
        let p = project();
        let mut tap = TerminalTap::new("claude-1", Tool::Claude);
        tap.started("sess-a");
        tap.prompt(
            Some(&sink),
            Some(&p),
            &text("plan the release"),
            None,
            Verdict::User,
            None,
        );
        // A planning turn: no diff, no tool use; still captured.
        tap.turn_end(
            Some(&sink),
            Some(&p),
            &Reply::Text(EventText::reply("Here is a plan".to_owned())),
            None,
            None,
        );
        tap.prompt(
            Some(&sink),
            Some(&p),
            &text("go on"),
            None,
            Verdict::User,
            None,
        );
        tap.turn_end(
            Some(&sink),
            Some(&p),
            &Reply::Dropped,
            None,
            Some(Path::new("/l.jsonl")),
        );
        tap.ended(Some(&sink), Some(&p), "exited: 0", None);
        tap.ended(Some(&sink), Some(&p), "the terminal closed", None);
        let got = sink.0.lock().unwrap();
        assert_eq!(got.len(), 5);
        assert_eq!(got[0].turn, "#1");
        assert_eq!(got[1].turn, "#1");
        assert_eq!(got[2].turn, "#2");
        assert_eq!(
            got[3].what,
            CaptureWhat::TurnEnd {
                reply: CaptureReply::Dropped
            }
        );
        assert_eq!(got[3].log.as_deref(), Some(Path::new("/l.jsonl")));
        assert_eq!(got[0].tool_session.as_deref(), Some("sess-a"));
        assert!(matches!(got[4].what, CaptureWhat::SessionEnd { .. }));
    }

    #[test]
    fn codex_turn_ids_and_shells_and_no_project() {
        let sink = Collect::default();
        let p = project();
        let mut codex = TerminalTap::new("codex-1", Tool::Codex);
        codex.started("thread");
        codex.prompt(
            Some(&sink),
            Some(&p),
            &text("hi"),
            Some("turn-9"),
            Verdict::User,
            None,
        );
        assert_eq!(sink.0.lock().unwrap()[0].turn, "turn-9");
        let mut shell = TerminalTap::new("sh", Tool::Shell);
        shell.prompt(
            Some(&sink),
            Some(&p),
            &text("ls"),
            None,
            Verdict::User,
            None,
        );
        let mut lost = TerminalTap::new("c", Tool::Claude);
        lost.prompt(Some(&sink), None, &text("x"), None, Verdict::User, None);
        lost.prompt(None, Some(&p), &text("x"), None, Verdict::User, None);
        assert_eq!(sink.0.lock().unwrap().len(), 1);
    }

    /// The switch: with `Off` no prompt is the user's, whatever the ledger
    /// said; with `Quarantine` the ledger's verdict stands.
    #[test]
    fn terminal_credit_off_never_records_the_user() {
        let sink = Collect::default();
        let p = project();
        for (credit, want) in [
            (TerminalCredit::Off, PromptSource::Unknown),
            (TerminalCredit::Quarantine, PromptSource::User),
        ] {
            let mut tap = TerminalTap::new("claude-1", Tool::Claude);
            tap.credit = credit;
            tap.started("s");
            tap.prompt(
                Some(&sink),
                Some(&p),
                &text("yes"),
                None,
                Verdict::User,
                None,
            );
            let got = sink.0.lock().unwrap().pop().unwrap();
            assert!(
                matches!(got.what, CaptureWhat::Prompt { source, .. } if source == want),
                "{credit:?}"
            );
        }
        // The default, before the app says anything, is `Off`.
        assert_eq!(
            TerminalTap::new("c", Tool::Claude).credit,
            TerminalCredit::Off
        );
        assert_eq!(TerminalCredit::default(), TerminalCredit::Off);
    }

    /// The tap's own rule: headers win over the ledger's verdict.
    #[test]
    fn headers_win_and_verdicts_map_to_sources() {
        use Verdict::{Foreign, Unknown, User};
        assert_eq!(TerminalTap::source("yes", User), PromptSource::User);
        assert_eq!(TerminalTap::source("yes", Foreign), PromptSource::Agent);
        assert_eq!(TerminalTap::source("yes", Unknown), PromptSource::Unknown);
        assert_eq!(
            TerminalTap::source("[connecto notice] resumed", User),
            PromptSource::Connecto
        );
        assert_eq!(
            TerminalTap::source("[connecto from a, request r1.] yes", User),
            PromptSource::Agent
        );
    }

    /// Review finding 7: a prompt the hook cut is marked, never passed as
    /// whole; a cut reply is read from the log instead.
    #[test]
    fn cut_texts_are_marked() {
        let sink = Collect::default();
        let p = project();
        let mut tap = TerminalTap::new("claude-1", Tool::Claude);
        tap.started("s");
        let long = "x".repeat(connecto_core::MAX_EVENT_TEXT + 10);
        tap.prompt(
            Some(&sink),
            Some(&p),
            &EventText::new(long),
            None,
            Verdict::User,
            Some(Path::new("/log")),
        );
        let long_reply = "y".repeat(connecto_core::MAX_REPLY_TEXT + 10);
        tap.turn_end(
            Some(&sink),
            Some(&p),
            &Reply::Text(EventText::reply(long_reply)),
            None,
            None,
        );
        let got = sink.0.lock().unwrap();
        assert!(matches!(
            got[0].what,
            CaptureWhat::Prompt {
                truncated: true,
                ..
            }
        ));
        assert_eq!(got[0].log.as_deref(), Some(Path::new("/log")));
        assert_eq!(
            got[1].what,
            CaptureWhat::TurnEnd {
                reply: CaptureReply::Dropped
            }
        );
    }
}
