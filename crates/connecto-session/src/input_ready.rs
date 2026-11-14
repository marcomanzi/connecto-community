//! Whether a CLI that has just started (a new terminal, a restart, a resume)
//! can take a typed prompt yet. A paste sent while the TUI is still booting is
//! lost without a trace, so the relay holds a send until the composer shows,
//! and never types into a screen that is waiting for a person.

use std::time::Duration;

use connecto_adapter::{ComposerState, claude_composer_state, codex_composer_state};
use connecto_store::Tool;

use crate::utility::blocked_reason;

/// How long a send may wait for the composer. Under the 30 s the bridge
/// handler waits for an outcome.
pub(crate) const HOLD_LIMIT: Duration = Duration::from_secs(25);
/// How long a blocking screen may show before the send is refused with it.
pub(crate) const BLOCK_GRACE: Duration = Duration::from_secs(3);
/// After the tool's own start hook, a screen with no known blocker counts as
/// ready this much later (a status line can hide the footer the screen check
/// reads, and the hook can come a moment before the box is drawn).
pub(crate) const START_SETTLE: Duration = Duration::from_secs(1);

/// What the screen says about typing now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Input {
    Ready,
    /// Still starting.
    Wait,
    /// Working: the composer is not for typing yet, and the tool will say when it is.
    Busy,
    /// Waiting for a person; the reason names the screen.
    Blocked(String),
}

/// What to do with a held send, given what the screen says now.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Step {
    /// Type it now.
    Go,
    /// Keep waiting.
    Keep,
    /// Refuse: the screen waits for a person.
    Blocked(String),
    /// Refuse: the composer never showed.
    TimedOut,
}

/// `held`: how long the send has waited. `blocked_for`: how long the screen
/// has been a blocking one.
pub(crate) fn step(input: &Input, held: Duration, blocked_for: Duration, limit: Duration) -> Step {
    match input {
        // Whatever the screen says now, a send past its deadline is not typed:
        // its handler has given up on it.
        Input::Ready if held >= limit => Step::TimedOut,
        Input::Ready => Step::Go,
        // The overall deadline covers every state that is not ready.
        Input::Blocked(reason) if blocked_for >= BLOCK_GRACE || held >= limit => {
            Step::Blocked(reason.clone())
        }
        Input::Blocked(_) => Step::Keep,
        Input::Wait | Input::Busy if held >= limit => Step::TimedOut,
        Input::Wait | Input::Busy => Step::Keep,
    }
}

fn codex_blocked(screen: &str) -> Option<String> {
    let has = |text: &str| screen.contains(text);
    if has("Do you trust the contents of this directory") {
        Some("Codex is waiting at its folder trust prompt: answer it in the terminal".to_owned())
    } else if has("Sign in with ChatGPT") || has("Provide your own API key") {
        Some("Codex is not logged in for this project".to_owned())
    } else if has("Update available") && has("Skip") {
        Some("Codex is waiting at its update prompt: answer it in the terminal".to_owned())
    } else {
        None
    }
}

/// The reason a send is refused while the input box holds a draft, a menu or a
/// prompt: typing now would join it or answer it.
const OCCUPIED: &str =
    "the input box holds a draft, or a menu or prompt is open: clear it in the terminal";

/// A prompt that waits for a person, named by its bottom line: every prompt
/// ends with an "esc to cancel" hint, and a live screen's bottom line is the
/// prompt's own, so an old prompt higher up does not count.
fn prompt_open(screen: &str) -> Option<String> {
    let bottom = screen
        .lines()
        .map(str::trim)
        .rfind(|line| !line.is_empty())?;
    bottom
        .to_lowercase()
        .contains("esc to cancel")
        .then(|| "waiting at a prompt: answer it in the terminal".to_owned())
}

fn from_state(state: ComposerState) -> Input {
    match state {
        ComposerState::Empty => Input::Ready,
        ComposerState::Busy => Input::Busy,
        ComposerState::Occupied => Input::Blocked(OCCUPIED.to_owned()),
        ComposerState::Booting => Input::Wait,
    }
}

/// Classify the screen text of a terminal, for a send that wants to type now.
pub(crate) fn classify(tool: Tool, screen: &str) -> Input {
    let blocked = match tool {
        Tool::Claude => blocked_reason(screen),
        Tool::Codex => codex_blocked(screen),
        // A shell has no start-up screen to wait for or be blocked by.
        Tool::Shell => None,
    };
    if let Some(reason) = blocked.or_else(|| prompt_open(screen)) {
        return Input::Blocked(reason);
    }
    match tool {
        Tool::Claude => from_state(claude_composer_state(screen)),
        Tool::Codex => from_state(codex_composer_state(screen)),
        Tool::Shell => Input::Ready,
    }
}

/// The last lines of the screen as one short, plain line.
pub(crate) fn excerpt(screen: &str) -> String {
    let lines: Vec<&str> = screen
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect();
    let tail = lines[lines.len().saturating_sub(4)..].join(" | ");
    let clean = connecto_redact::sanitize(&tail).replace('\n', " ");
    let shown: String = clean.chars().take(240).collect();
    if shown.is_empty() {
        "(blank)".to_owned()
    } else {
        shown
    }
}

fn name(tool: Tool) -> &'static str {
    match tool {
        Tool::Claude => "Claude",
        Tool::Codex => "Codex",
        Tool::Shell => "The shell",
    }
}

/// What the sender is told when the composer never showed.
pub(crate) fn timeout_message(tool: Tool, waited: Duration, screen: &str) -> String {
    format!(
        "{} did not show its input box within {} s; screen shows: {}. Nothing was typed.",
        name(tool),
        waited.as_secs(),
        excerpt(screen)
    )
}

/// What the sender is told when the screen waits for a person.
pub(crate) fn blocked_message(reason: &str, screen: &str) -> String {
    format!(
        "{reason}. Nothing was typed; screen shows: {}",
        excerpt(screen)
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_composer_is_ready_a_boot_screen_waits_and_a_prompt_blocks() {
        // A footer with no composer above it is still booting.
        assert_eq!(classify(Tool::Claude, "? for shortcuts"), Input::Wait);
        assert_eq!(
            classify(Tool::Claude, "bypass permissions on (shift+tab to cycle)"),
            Input::Wait
        );
        let claude_idle = "❯ Try \"how does <filepath> work?\"\n──────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n";
        assert_eq!(classify(Tool::Claude, claude_idle), Input::Ready);
        let claude_draft =
            "❯ hello draft\n──────\n  ⏵⏵ bypass permissions on (shift+tab to cycle)\n";
        assert!(matches!(
            classify(Tool::Claude, claude_draft),
            Input::Blocked(_)
        ));
        let claude_working =
            "❯ \n──────\n  ⏵⏵ bypass permissions on (shift+tab to cycle) · esc to interrupt\n";
        assert_eq!(classify(Tool::Claude, claude_working), Input::Busy);
        assert!(matches!(
            classify(
                Tool::Claude,
                "❯ 1. Yes\n  2. No\n\nEsc to cancel · Tab to amend"
            ),
            Input::Blocked(_)
        ));
        assert_eq!(classify(Tool::Claude, "Welcome back\n"), Input::Wait);
        assert!(matches!(
            classify(
                Tool::Claude,
                "Is this a project you created or one you trust"
            ),
            Input::Blocked(_)
        ));
        let ready = "› Ask Codex to do anything\n\n  model · ~/p\n  ? for shortcuts";
        assert_eq!(classify(Tool::Codex, ready), Input::Ready);
        // A working Codex is busy, not ready and not a prompt.
        let codex_working = "› Write a story\n• Working (4s • esc to interrupt)\n› Ask Codex to do anything\n  GPT-6 · ~/p\n";
        assert_eq!(classify(Tool::Codex, codex_working), Input::Busy);
        // The screen from the report: a named inline terminal in the bypass
        // layout, which has no hint line. It is ready.
        let reported = "  permissions: YOLO mode\n\n  Tip: Use the desktop app.\n\n\
                        › Ask Codex to do anything\n\n  GPT-6.1-Sol default · ~/workspace · backend reviewer\n";
        assert_eq!(classify(Tool::Codex, reported), Input::Ready);
        assert_eq!(classify(Tool::Codex, "Booting\n"), Input::Wait);
        assert_eq!(classify(Tool::Codex, "› Ask Codex\n"), Input::Wait);
        assert!(matches!(
            classify(Tool::Codex, "Do you trust the contents of this directory?"),
            Input::Blocked(_)
        ));
        assert!(matches!(
            classify(Tool::Codex, "Sign in with ChatGPT\n› "),
            Input::Blocked(_)
        ));
    }

    #[test]
    fn a_held_send_goes_when_ready_waits_while_booting_and_gives_up_with_a_reason() {
        let s = Duration::from_secs;
        assert_eq!(step(&Input::Ready, s(0), s(0), HOLD_LIMIT), Step::Go);
        assert_eq!(step(&Input::Wait, s(10), s(0), HOLD_LIMIT), Step::Keep);
        assert_eq!(
            step(&Input::Wait, HOLD_LIMIT, s(0), HOLD_LIMIT),
            Step::TimedOut
        );
        // The limit is the caller's: a shorter one expires sooner.
        assert_eq!(step(&Input::Wait, s(4), s(0), s(4)), Step::TimedOut);
        // Ready does not revive a send past its deadline.
        assert_eq!(step(&Input::Ready, s(4), s(0), s(4)), Step::TimedOut);
        assert_eq!(step(&Input::Ready, s(3), s(0), s(4)), Step::Go);
        // A blocking screen is never typed into: it is waited on briefly,
        // then reported.
        let blocked = Input::Blocked("trust".into());
        assert_eq!(step(&blocked, s(5), s(1), HOLD_LIMIT), Step::Keep);
        assert_eq!(
            step(&blocked, s(5), BLOCK_GRACE, HOLD_LIMIT),
            Step::Blocked("trust".into())
        );
        // A blocker that shows up at the very end is reported at once, not
        // after a further grace.
        assert_eq!(
            step(&blocked, HOLD_LIMIT, s(0), HOLD_LIMIT),
            Step::Blocked("trust".into())
        );
        assert_eq!(
            step(&blocked, HOLD_LIMIT - s(1), s(1), HOLD_LIMIT),
            Step::Keep
        );
    }

    #[test]
    fn the_messages_say_why_and_quote_the_screen_briefly() {
        let screen = "\n  loading\n\n  model · ~/p\n\x1b[31mwhere\x1b[0m\n";
        let text = timeout_message(Tool::Codex, Duration::from_secs(20), screen);
        assert!(text.starts_with("Codex did not show its input box within 20 s; screen shows: "));
        assert!(text.contains("loading | model · ~/p"));
        assert!(!text.contains('\x1b'));
        assert!(excerpt(&"x".repeat(1000)).chars().count() <= 240);
        assert_eq!(excerpt("  \n"), "(blank)");
        let blocked = blocked_message("Codex is not logged in for this project", "Sign in");
        assert!(blocked.contains("Nothing was typed") && blocked.contains("Sign in"));
    }
}
