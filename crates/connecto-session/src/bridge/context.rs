//! How much of its context window an agent terminal has used, as `connecto
//! list` shows it: read from the figure the agent's own footer prints at the
//! bottom of its screen, so a coordinator need not read the screen to decide
//! whether a terminal can take more work.
//!
//! Only each tool's exact footer is taken, in its footer's place:
//!
//! - Claude: Connecto's status line, below the input box's bottom rule:
//!   `Opus 5.5 · high  |  [████░░░░] 40% (400k/1M)  |  $1.00`, the counts in
//!   brackets optional or `(max 1M)`. Claude Code's own "until auto-compact"
//!   warning measures the compaction threshold, not the window: not taken.
//! - Antigravity: its status line, below the input box's bottom rule:
//!   `Gemini 4 Neon (Medium) | Ctx: 2.4% | Cost: $0.00`.
//!
//! Codex 0.167's default footer has no figure (`GPT-6.2-Astra low · ~/w ·
//! <thread title> · ← for agents`), and a custom one can hold the user's
//! words, such as a title, so a Codex screen is not read.
//!
//! Anything else, Codex, Pi and OpenCode, an echoed prompt, a download's
//! progress or text being typed, has no figure. The figure is screen text, so it is only
//! shown, never acted on by Connecto.

use connecto_store::Tool;

/// The last rows with text a footer can be in.
const ROWS: usize = 8;

/// Whether `list` looks at this tool's screen at all.
pub(crate) const fn has_footer(tool: Tool) -> bool {
    matches!(tool, Tool::Claude | Tool::Antigravity)
}

/// The used share, rounded up to a whole percent so 30.4 is never 30, from
/// a screen's text: None unless the tool's footer is there with a figure.
pub(crate) fn used_percent(tool: Tool, screen: &str) -> Option<u8> {
    let rows: Vec<&str> = screen.trim_end().lines().collect();
    let tail = &rows[rows.len().saturating_sub(ROWS)..];
    let used = match tool {
        Tool::Claude => below_last_rule(tail).find_map(claude_bar),
        Tool::Antigravity => below_last_rule(tail).find_map(antigravity_ctx),
        _ => None,
    }?;
    if !(0.0..=100.0).contains(&used) {
        return None;
    }
    // Whole percent, rounded up: a coordinator compares it with 30 and 60.
    #[allow(clippy::cast_possible_truncation, clippy::cast_sign_loss)]
    let whole = used.ceil() as u8;
    Some(whole)
}

/// The rows below the last horizontal rule, the input box's bottom edge
/// (`────`, a label such as `─── bridge ─` allowed): none without one.
fn below_last_rule<'a>(tail: &'a [&'a str]) -> impl Iterator<Item = &'a str> {
    let rule = tail.iter().rposition(|row| {
        let row = row.trim();
        row.starts_with('─') && row.ends_with('─')
    });
    let below = rule.map_or(&[][..], |at| &tail[at + 1..]);
    below.iter().copied()
}

/// `[████░░░░] 40%`, then nothing or ` (…)`, as one `|`-separated part.
fn claude_bar(row: &str) -> Option<f64> {
    row.split('|').find_map(|part| {
        let rest = part.trim().strip_prefix('[')?;
        let (bar, rest) = rest.split_once("] ")?;
        if bar.is_empty() || !bar.chars().all(|c| c == '█' || c == '░') {
            return None;
        }
        let (number, rest) = rest.split_once('%')?;
        if !(rest.is_empty() || (rest.starts_with(" (") && rest.ends_with(')'))) {
            return None;
        }
        whole(number)
    })
}

/// `Ctx: 2.4%`, as one `|`-separated part.
fn antigravity_ctx(row: &str) -> Option<f64> {
    row.split('|').find_map(|part| {
        let number = part.trim().strip_prefix("Ctx: ")?.strip_suffix('%')?;
        decimal(number)
    })
}

fn whole(text: &str) -> Option<f64> {
    if text.is_empty() || !text.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    text.parse().ok()
}

fn decimal(text: &str) -> Option<f64> {
    let (int, frac) = text.split_once('.').unwrap_or((text, "0"));
    whole(int)?;
    whole(frac)?;
    text.parse().ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    const CLAUDE: &str = "\
✻ Worked for 25m 24s

─────────────────────────────────────────── bridge ─
❯
────────────────────────────────────────────────────
  Opus 5.5 · high  |  [████████░░░░░░░░░░░░] 40% (400k/1M)  |  $15.45
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";

    const ANTIGRAVITY: &str = "\
  effort <low|medium|high>.

────────────────────────────────────────────
>
────────────────────────────────────────────
Gemini 4\tNeon (Medium) |\tCtx: 30.4% | Cost: $0.00
";

    #[test]
    fn each_tools_footer() {
        assert_eq!(used_percent(Tool::Claude, CLAUDE), Some(40));
        // Rounded up: 30.4% is over 30.
        assert_eq!(used_percent(Tool::Antigravity, ANTIGRAVITY), Some(31));
    }

    #[test]
    fn claudes_bar_without_token_counts() {
        let max = CLAUDE.replace("40% (400k/1M)", "40% (max 1M)");
        assert_eq!(used_percent(Tool::Claude, &max), Some(40));
        let bare = CLAUDE.replace("40% (400k/1M)", "40%");
        assert_eq!(used_percent(Tool::Claude, &bare), Some(40));
    }

    #[test]
    fn a_footer_is_read_only_for_its_own_tool() {
        assert_eq!(used_percent(Tool::Claude, ANTIGRAVITY), None);
        assert_eq!(used_percent(Tool::Antigravity, CLAUDE), None);
        for tool in [Tool::Codex, Tool::Pi, Tool::OpenCode, Tool::Shell] {
            assert!(!has_footer(tool));
            assert_eq!(used_percent(tool, CLAUDE), None);
        }
    }

    #[test]
    fn an_echoed_prompt_is_not_a_footer() {
        // The figure as the user typed it, echoed in the history above the
        // input box.
        let echoed = "\
> Opus 5.5 · high  |  [████░░░░░░░░░░░░░░░░] 23% (230k/1M)  |  $1
⏺ Noted.

────────────────
❯
────────────────
  ⏵⏵ bypass permissions on (shift+tab to cycle)
";
        assert_eq!(used_percent(Tool::Claude, echoed), None);
    }

    #[test]
    fn the_users_typed_text_is_not_a_footer() {
        let claude = "\
────────────────
❯ [████████████████████] 5% (1k/1M)
────────────────
  Opus 5.5 · high  |  Context: no data
";
        assert_eq!(used_percent(Tool::Claude, claude), None);
        let antigravity = "────\n> Ctx: 5%\n────\nGemini | Cost: $0\n";
        assert_eq!(used_percent(Tool::Antigravity, antigravity), None);
    }

    #[test]
    fn a_download_percentage_is_not_a_footer() {
        let claude = "\
────────────────
❯
────────────────
  Downloaded 20% (20/100)
";
        assert_eq!(used_percent(Tool::Claude, claude), None);
        assert_eq!(
            used_percent(Tool::Antigravity, "────\n50% | Ctx: 2.4.1%\n"),
            None
        );
    }

    /// Codex 0.167's default footer, as it shows after a turn: the thread
    /// title is the user's words, here one that looks like a figure.
    #[test]
    fn a_codex_thread_title_is_not_a_figure() {
        let codex = "\
› Ask Codex to do anything

  GPT-6.2-Astra low · ~/workspace/connecto · 90% context left · ← for agents          ⚠ 1 warning · f2 to view
";
        assert_eq!(used_percent(Tool::Codex, codex), None);
    }

    #[test]
    fn claudes_auto_compact_warning_is_not_window_use() {
        let screen = "\
────────────────
❯
────────────────
  Context left until auto-compact: 7%
";
        assert_eq!(used_percent(Tool::Claude, screen), None);
    }

    #[test]
    fn a_footer_scrolled_away_or_out_of_range_has_no_figure() {
        let old = format!("{CLAUDE}{}", "output\n".repeat(8));
        assert_eq!(used_percent(Tool::Claude, &old), None);
        let over = ANTIGRAVITY.replace("Ctx: 30.4%", "Ctx: 130%");
        assert_eq!(used_percent(Tool::Antigravity, &over), None);
        assert_eq!(used_percent(Tool::Claude, ""), None);
    }
}
