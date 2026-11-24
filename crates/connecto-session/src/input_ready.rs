//! Whether a CLI that has just started (a new terminal, a restart, a resume)
//! can take a typed prompt yet. A paste sent while the TUI is still booting is
//! lost without a trace, so the relay holds a send until the composer shows,
//! and never types into a screen that is waiting for a person.

use std::time::Duration;

use connecto_adapter::{
    ComposerState, Screen, antigravity_composer_state, claude_composer_state, codex_composer_state,
    opencode_composer_state, pi_composer_state,
};
use connecto_core::{Cell, CellFlags};
use connecto_store::Tool;

use crate::utility::blocked_reason;

/// How long a send may wait for the composer. Under the 30 s the bridge
/// handler waits for an outcome.
pub(crate) const HOLD_LIMIT: Duration = Duration::from_secs(25);
/// How long a send may wait for the user to send or clear a draft Connecto
/// cannot move aside. Its sender is answered `waiting` at once, so this is not
/// bound by the handler's wait.
pub(crate) const DRAFT_LIMIT: Duration = Duration::from_secs(300);
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
    /// The user has a draft Connecto cannot move out of the way safely: wait
    /// for them to send or clear it.
    Draft(String),
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
        // The user is typing: give them until the deadline.
        Input::Draft(reason) if held >= limit => Step::Blocked(reason.clone()),
        Input::Draft(_) => Step::Keep,
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

/// Why a send waits, then is refused, while the user has a draft Connecto
/// cannot move: one it could not follow (a history recall, a completion), or
/// text in the input box nobody typed in this terminal.
pub(crate) const DRAFT: &str = "the input box holds a draft Connecto cannot move aside safely: send or clear it in the terminal";

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

/// One row of the screen with only what was typed on it: every faint (dimmed)
/// cell is a space. A tool dims what it suggests rather than what someone
/// typed: a placeholder, a suggested next prompt, a completion's ghost text.
/// A reverse-video cell right before faint text is the tool's own cursor
/// drawn on the suggestion's first letter (Claude Code), so it is a space too.
/// The row keeps one character per cell, like the drawn one.
pub(crate) fn typed_row(cells: &[Cell]) -> String {
    let faint = |cell: &Cell| cell.flags.contains(CellFlags::DIM);
    cells
        .iter()
        .enumerate()
        .map(|(at, cell)| {
            let next = cells[at + 1..]
                .iter()
                .find(|next| !next.flags.contains(CellFlags::WIDE_SPACER));
            let cursor_on_hint = cell.flags.contains(CellFlags::INVERSE) && next.is_some_and(faint);
            if faint(cell) || cursor_on_hint {
                ' '
            } else {
                cell.ch
            }
        })
        .collect()
}

/// Classify the screen of a terminal, for a send that wants to type now. Faint
/// text in the input box is not a draft.
pub(crate) fn classify(tool: Tool, screen: Screen<'_>) -> Input {
    let composer = screen;
    let screen = screen.drawn;
    let blocked = match tool {
        Tool::Claude => blocked_reason(screen),
        Tool::Codex => codex_blocked(screen),
        // Pi's own questions (a project to trust, a session to fork) end with
        // the "esc to cancel" hint looked for below.
        Tool::Pi => None,
        // Antigravity's screens have not been measured: its sign-in and trust
        // questions are not recognised, and a send waits for its start hook.
        Tool::Antigravity => None,
        // OpenCode asks nothing before its prompt: a sign-in is `/connect`,
        // typed by the user. Its permission prompts are reported by its
        // plugin.
        Tool::OpenCode => None,
        // A shell has no start-up screen to wait for or be blocked by.
        Tool::Shell => None,
    };
    if let Some(reason) = blocked.or_else(|| prompt_open(screen)) {
        return Input::Blocked(reason);
    }
    match tool {
        Tool::Claude => from_state(claude_composer_state(composer)),
        Tool::Codex => from_state(codex_composer_state(composer)),
        Tool::Pi => from_state(pi_composer_state(composer)),
        Tool::Antigravity => from_state(antigravity_composer_state(composer)),
        Tool::OpenCode => from_state(opencode_composer_state(composer)),
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
        Tool::Pi => "Pi",
        Tool::Antigravity => "Antigravity",
        Tool::OpenCode => "OpenCode",
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

    fn classify(tool: Tool, text: &str) -> Input {
        super::classify(tool, Screen::plain(text))
    }

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
        // Regression: a blank first row with a draft on the second was
        // ready, so a held send went into the draft.
        let two_lines = "›\n  existing draft\n\n  model · ~/p\n  ? for shortcuts";
        assert!(matches!(
            classify(Tool::Codex, two_lines),
            Input::Blocked(_)
        ));
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
        // A draft that cannot be moved waits for the user to the deadline,
        // with no grace cut short, and is then refused with its reason.
        let draft = Input::Draft(DRAFT.into());
        assert_eq!(step(&draft, s(20), s(0), HOLD_LIMIT), Step::Keep);
        assert_eq!(
            step(&draft, HOLD_LIMIT, s(0), HOLD_LIMIT),
            Step::Blocked(DRAFT.into())
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

    /// The screen `bytes` draw, as drawn and as typed, through the emulator.
    fn drawn_and_typed(bytes: &[u8]) -> (String, String) {
        use std::sync::Arc;

        use connecto_core::{GridSize, SnapshotOutcome, TermSnapshot};
        use connecto_term::Emulator;

        let size = GridSize { cols: 60, rows: 6 };
        let emulator = Emulator::new(size, Arc::new(|_| {}), Arc::new(|| {}));
        emulator.feed(bytes);
        let mut snapshot = TermSnapshot::default();
        assert_eq!(
            emulator.poll_snapshot(&mut snapshot),
            SnapshotOutcome::Updated
        );
        let mut drawn = String::new();
        let mut typed = String::new();
        for row in 0..snapshot.rows {
            let cells = snapshot.row(row);
            drawn.extend(cells.iter().map(|c| c.ch));
            drawn.push('\n');
            typed.push_str(&typed_row(cells));
            typed.push('\n');
        }
        (drawn, typed)
    }

    fn classify_bytes(tool: Tool, bytes: &[u8]) -> Input {
        let (drawn, typed) = drawn_and_typed(bytes);
        super::classify(
            tool,
            Screen {
                drawn: &drawn,
                typed: &typed,
            },
        )
    }

    const CLAUDE_FOOTER: &str = "\r\n──────\r\n  ⏵⏵ bypass permissions on (shift+tab to cycle)";

    #[test]
    fn a_dimmed_suggestion_in_the_box_is_not_typing() {
        // Claude Code draws a suggested prompt dimmed, with its own cursor in
        // reverse video on the first letter.
        let claude = format!("❯ \x1b[7mr\x1b[27m\x1b[2mun the tests\x1b[22m{CLAUDE_FOOTER}");
        assert_eq!(
            classify_bytes(Tool::Claude, claude.as_bytes()),
            Input::Ready
        );
        let (drawn, typed) = drawn_and_typed(claude.as_bytes());
        assert!(drawn.contains("❯ run the tests"));
        assert_eq!(typed.lines().next().map(str::trim), Some("❯"));
        // Unfocused, the whole suggestion is dimmed.
        let claude = format!("❯ \x1b[2mrun the tests\x1b[22m{CLAUDE_FOOTER}");
        assert_eq!(
            classify_bytes(Tool::Claude, claude.as_bytes()),
            Input::Ready
        );
        // Codex dims its example prompts.
        let codex =
            b"\xe2\x80\xba \x1b[2mExplain this codebase\x1b[22m\r\n\r\n  GPT-6 \xc2\xb7 ~/p";
        assert_eq!(classify_bytes(Tool::Codex, codex), Input::Ready);
    }

    #[test]
    fn typed_text_still_blocks_and_keeps_its_cursor() {
        // Typed text with Claude's reverse-video cursor after it.
        let claude = format!("❯ run the tests\x1b[7m \x1b[27m{CLAUDE_FOOTER}");
        assert!(matches!(
            classify_bytes(Tool::Claude, claude.as_bytes()),
            Input::Blocked(_)
        ));
        // The cursor on a typed letter, with no faint text after it.
        let claude = format!("❯ \x1b[7mr\x1b[27mun{CLAUDE_FOOTER}");
        assert!(matches!(
            classify_bytes(Tool::Claude, claude.as_bytes()),
            Input::Blocked(_)
        ));
        // Typed text with a completion's ghost text after it: only the ghost
        // text and the cursor on it are left out.
        let (_, typed) = drawn_and_typed(b"\xe2\x9d\xaf /com\x1b[7mp\x1b[27m\x1b[2mact\x1b[22m");
        assert_eq!(typed.lines().next().map(str::trim), Some("❯ /com"));
        let codex = b"\xe2\x80\xba hello\r\n\r\n  GPT-6 \xc2\xb7 ~/p";
        assert!(matches!(
            classify_bytes(Tool::Codex, codex),
            Input::Blocked(_)
        ));
    }
}
