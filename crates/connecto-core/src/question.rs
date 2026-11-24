//! Isolate the active CLI dialog from the surrounding screen. A finished reply
//! containing a numbered list is ordinary conversation, even if it ends in '?'.
use crate::ToolKind;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Question {
    pub text: String,
    pub accepts_text: bool,
    /// The operation a permission asks for, recovered whole from a layout
    /// this knows (Claude's "Bash command" with its command between the
    /// dotted rules, Codex's `$ command`): what a voice approval names. None
    /// when it could not be recovered: then the permission is tap-only.
    pub operation: Option<String>,
}

fn border(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.len() >= 12
        && trimmed
            .chars()
            .all(|c| matches!(c, '─' | '━' | '╌' | '╭' | '╮' | '╰' | '╯'))
}

fn dotted(line: &str) -> bool {
    let trimmed = line.trim();
    trimmed.chars().count() >= 12 && trimmed.chars().all(|c| c == '╌')
}

/// Claude's Bash permission, from the rows between its solid border and its
/// footer: "Bash command", then the command between two dotted rules, then
/// "Do you want to proceed?". The header and the command, or None.
fn claude_operation(rows: &[&str]) -> Option<String> {
    let header = rows.iter().find(|r| !r.trim().is_empty())?.trim();
    if header != "Bash command" {
        return None;
    }
    let rules: Vec<usize> = rows
        .iter()
        .enumerate()
        .filter(|(_, r)| dotted(r))
        .map(|(i, _)| i)
        .collect();
    let [open, close] = rules[..] else {
        return None;
    };
    let command: Vec<&str> = rows[open + 1..close]
        .iter()
        .map(|r| r.trim())
        .filter(|r| !r.is_empty())
        .collect();
    let asks = rows[close + 1..].iter().find(|r| !r.trim().is_empty())?;
    (!command.is_empty() && asks.trim() == "Do you want to proceed?")
        .then(|| format!("{header}: {}", command.join("\n")))
}

/// Codex's command permission, from its "Would you like to run the
/// following command?" down to its footer: the whole `$ command`, or None.
/// The choices are found from the footer up (blank rows, then a run of
/// numbered rows counting down to "1.", one of them selected, then at least
/// one blank row), so a command whose own text has numbered rows ("1.
/// Monday") is never cut there. What lies between the `$` row and that run
/// is the command.
fn codex_operation(rows: &[&str]) -> Option<String> {
    let mut end = rows.len();
    while end > 0 && rows[end - 1].trim().is_empty() {
        end -= 1;
    }
    // The run of choices, from its last row up to "1.".
    let mut first = end;
    let mut expected: Option<usize> = None;
    let mut selected = 0;
    while first > 0 {
        let (text, marked) = unmarked(rows[first - 1]);
        let Some((n, _)) = numbered(text) else {
            break;
        };
        if expected.is_some_and(|e| n != e) {
            return None;
        }
        selected += usize::from(marked);
        first -= 1;
        if n == 0 {
            expected = None;
            break;
        }
        expected = Some(n - 1);
    }
    // A run that does not reach "1.", has no selected row (or more than
    // one), or has no blank row above it: the boundary is not certain.
    if first == end
        || expected.is_some()
        || numbered(unmarked(rows[first]).0).map(|(n, _)| n) != Some(0)
        || selected != 1
        || first == 0
        || !rows[first - 1].trim().is_empty()
    {
        return None;
    }
    let at = rows[..first]
        .iter()
        .position(|r| r.trim_start().starts_with("$ "))?;
    // Nothing between the question and the command but its details.
    let mut command: Vec<&str> = rows[at..first].iter().map(|r| r.trim_end()).collect();
    while command.last().is_some_and(|r| r.trim().is_empty()) {
        command.pop();
    }
    (!command.is_empty()).then(|| {
        command
            .iter()
            .map(|r| r.trim())
            .collect::<Vec<_>>()
            .join("\n")
    })
}

pub fn active(tool: ToolKind, screen: &str) -> Option<Question> {
    let lines: Vec<_> = screen.trim_end().lines().collect();
    if tool == ToolKind::Codex
        && let Some(start) = lines.iter().rposition(|line| {
            let line = line.trim();
            line.starts_with("Question ")
                && line.split_whitespace().nth(1).is_some_and(|s| {
                    s.split_once('/')
                        .is_some_and(|(i, n)| i.parse::<u16>().is_ok() && n.parse::<u16>().is_ok())
                })
                || line == "Submit with unanswered questions?"
        })
    {
        let panel = &lines[start..];
        let footer = panel
            .iter()
            .rev()
            .take(3)
            .map(|l| l.trim())
            .collect::<Vec<_>>()
            .join(" ");
        if footer.contains("submit answer") || footer.contains("esc to go back") {
            let accepts_text = panel.iter().any(|l| {
                l.trim_start().starts_with("› Type your answer")
                    || l.trim_start().starts_with("› Add notes")
            }) || !panel.iter().any(|l| {
                l.trim_start()
                    .trim_start_matches('›')
                    .trim_start()
                    .starts_with("1.")
            });
            return Some(Question {
                text: panel.join("\n").trim().to_owned(),
                accepts_text,
                operation: None,
            });
        }
    }
    // Recognise the CLI's footer, which sits immediately under the dialog.
    let footer = lines.iter().rposition(|line| {
        let s = line.trim().to_lowercase();
        s.contains("esc")
            && (s.contains("cancel") || s.contains("dismiss"))
            && (s.contains("enter")
                || s.contains("tab")
                || s.contains("select")
                || s.contains("submit"))
    })?;
    // An old dialog above a fresh composer or another turn is no longer actionable.
    if lines[footer + 1..].iter().any(|line| {
        let s = line.trim_start();
        s.starts_with(['❯', '›', '⏺', '•']) || s.starts_with("> ")
    }) {
        return None;
    }
    let footer_text = lines[footer].to_lowercase();
    let accepts_text =
        tool == ToolKind::Pi && footer_text.contains("submit") && !footer_text.contains("select");
    let mut operation = None;
    let start = if tool == ToolKind::Pi {
        lines[..footer]
            .iter()
            .rposition(|s| border(s))?
            .saturating_add(1)
    } else if let Some(index) = lines[..footer].iter().rposition(|s| {
        s.trim_start()
            .starts_with("Would you like to run the following command")
    }) {
        if tool == ToolKind::Codex {
            operation = codex_operation(&lines[index + 1..footer]);
        }
        index
    } else {
        let selected = lines[..footer].iter().rposition(|s| {
            let row = s.trim_start();
            row.starts_with(['❯', '›']) || row.starts_with("> ")
        })?;
        // A Claude approval has a solid border, then the operation, then
        // dotted rules around the command. Keep those details with the choices.
        let framed = lines[..selected]
            .iter()
            .rposition(|s| s.trim_start().starts_with("────"))
            .map(|n| n + 1);
        if tool == ToolKind::Claude
            && let Some(first) = framed
        {
            operation = claude_operation(&lines[first..footer]);
        }
        framed.or_else(|| {
            lines[..selected]
                .iter()
                .rposition(|s| s.trim_end().ends_with('?'))
        })?
    };
    let text = lines[start..=footer]
        .iter()
        .filter(|s| !border(s))
        .copied()
        .collect::<Vec<_>>()
        .join("\n");
    let text = text.trim();
    (!text.is_empty()).then(|| Question {
        text: text.to_owned(),
        accepts_text,
        operation,
    })
}

/// What kind of answer a dialog wants: a permission (yes or no to an
/// operation), a choice from a menu, words, or something not recognised
/// well enough to answer by voice ([`Kind::Other`]: shown, tapped only).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Kind {
    Approval,
    Menu,
    Text,
    Other,
}

impl Kind {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Approval => "approval",
            Self::Menu => "menu",
            Self::Text => "text",
            Self::Other => "other",
        }
    }
}

/// One of a dialog's choices, as it shows it: its position from 0, and its
/// words without the selection mark or the number.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Choice {
    pub index: usize,
    pub label: String,
    /// Not positively known to be a one-shot answer, so taken only by a
    /// tap, never by voice: the dialog is not a layout this knows (a
    /// permission whose operation was recovered whole, or Codex's question
    /// form), its choices are not read whole, the label wrapped onto another
    /// row, it allows more than the one operation (always allow, don't ask
    /// again, auto mode, trust...), or, for a permission, it is not one of
    /// the known one-shot answers.
    pub tap_only: bool,
}

/// The marks CLIs put before the selected row.
const MARKS: [char; 4] = ['❯', '›', '→', '>'];

/// `line` without its selection mark, and whether it had one.
fn unmarked(line: &str) -> (&str, bool) {
    let trimmed = line.trim_start();
    match trimmed.strip_prefix(MARKS) {
        Some(rest) if rest.is_empty() || rest.starts_with(' ') => (rest.trim_start(), true),
        _ => (trimmed, false),
    }
}

/// `1. Yes` as (0, "Yes").
fn numbered(text: &str) -> Option<(usize, &str)> {
    let (n, rest) = text.split_once(". ")?;
    let n: usize = n.parse().ok()?;
    (1..=99).contains(&n).then(|| (n - 1, rest.trim()))
}

/// Words of a choice that let more than the one operation through.
#[must_use]
pub fn is_tap_only(label: &str) -> bool {
    let s = label.to_lowercase().replace('’', "'");
    [
        "always",
        "don't ask",
        "do not ask",
        "dont ask",
        "auto mode",
        "auto-accept",
        "auto accept",
        "all edits",
        "session",
        "bypass",
        "yolo",
        "from now on",
        "trust",
        "every time",
        "remember",
        "permanent",
        "policy",
    ]
    .iter()
    .any(|w| s.contains(w))
}

/// A permission's answer known to allow or refuse this one operation and
/// nothing more: the only ones a voice may take.
#[must_use]
pub fn is_one_shot(label: &str) -> bool {
    let l = label.trim().to_lowercase();
    if matches!(
        l.as_str(),
        "yes" | "no" | "yes, proceed" | "yes, proceed (y)" | "no (esc)"
    ) {
        return true;
    }
    // "No, and tell Claude what to do differently (esc)".
    l.strip_prefix("no, and tell ")
        .and_then(|rest| {
            rest.strip_suffix(" what to do differently (esc)")
                .or_else(|| rest.strip_suffix(" what to do differently"))
        })
        .is_some_and(|who| !who.is_empty() && who.chars().all(char::is_alphanumeric))
}

/// Footer rows, under the choices: never one of them.
fn footer_row(line: &str) -> bool {
    let s = line.trim().to_lowercase();
    s.contains("esc") && (s.contains("cancel") || s.contains("dismiss") || s.contains("interrupt"))
        || s.starts_with("↑↓")
        || s.starts_with("press enter")
        || s.starts_with("enter to")
}

/// The numbered choices of a dialog as read: each with its rows, and
/// whether every row from the first choice down is a choice, a blank or the
/// footer (nothing this does not read).
struct Numbered {
    /// (index, label, selected, wrapped, first row, last row)
    rows: Vec<(usize, String, bool, bool, usize, usize)>,
    whole: bool,
}

/// A text field of a dialog: its rows (`first..end` of the dialog's rows)
/// and what is typed in it now ("" for its placeholder).
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Field {
    pub first: usize,
    pub end: usize,
    pub value: String,
}

impl Question {
    /// The dialog's footer row: its last row, by position, when it reads as
    /// a footer. A row elsewhere that happens to say "esc to cancel" (in a
    /// command) is never taken for it.
    fn footer(&self) -> Option<usize> {
        let lines: Vec<&str> = self.text.lines().collect();
        let last = lines.len().checked_sub(1)?;
        footer_row(lines[last]).then_some(last)
    }

    fn numbered(&self) -> Option<Numbered> {
        let lines: Vec<&str> = self.text.lines().collect();
        let footer = self.footer();
        let mut run: Vec<(usize, String, bool, bool, usize, usize)> = Vec::new();
        let mut best = Vec::new();
        let mut indent = 0;
        for (i, line) in lines.iter().enumerate() {
            if Some(i) == footer {
                break;
            }
            let (text, marked) = unmarked(line);
            if let Some((n, label)) = numbered(text)
                && (n == run.len() || n == 0)
            {
                if n == 0 {
                    run.clear();
                }
                run.push((n, label.to_owned(), marked, false, i, i));
                indent = line.len() - line.trim_start().len();
                continue;
            }
            if !run.is_empty()
                && !text.is_empty()
                && !marked
                && line.len() - line.trim_start().len() > indent + 1
            {
                // A long choice wrapped onto the next row.
                if let Some(last) = run.last_mut() {
                    last.1 = format!("{} {}", last.1, text.trim());
                    last.3 = true;
                    last.5 = i;
                }
                continue;
            }
            if run.len() >= 2 {
                best = std::mem::take(&mut run);
            }
            run.clear();
        }
        if run.len() >= 2 {
            best = run;
        }
        let first = best.first()?.4;
        let covered: std::collections::HashSet<usize> =
            best.iter().flat_map(|r| r.4..=r.5).collect();
        let whole = lines
            .iter()
            .enumerate()
            .skip(first)
            .all(|(i, l)| covered.contains(&i) || l.trim().is_empty() || Some(i) == footer);
        Some(Numbered { rows: best, whole })
    }

    /// A permission this knows: its operation recovered whole, and choices
    /// that say yes and no.
    fn known_approval(&self, tool: ToolKind, n: &Numbered) -> bool {
        matches!(tool, ToolKind::Claude | ToolKind::Codex)
            && self.operation.is_some()
            && Self::is_approval(n.rows.iter().map(|r| r.1.as_str()))
    }

    /// Codex's question form with choices.
    fn known_menu(&self, tool: ToolKind) -> bool {
        tool == ToolKind::Codex && self.text.starts_with("Question ")
    }

    /// Its choices, in order, and which one is selected now. Numbered rows
    /// (the last run that counts 1, 2, 3...), else the rows lined up with
    /// the selected one. None: no choices (a question that takes words, or
    /// one not recognised). Only a layout this knows has choices a voice
    /// may take.
    #[must_use]
    pub fn choices(&self, tool: ToolKind) -> (Vec<Choice>, Option<usize>) {
        if self.field(tool).is_some() {
            return (Vec::new(), None);
        }
        if let Some(n) = self.numbered() {
            let approval = self.known_approval(tool, &n);
            let known = approval || self.known_menu(tool);
            let selected = n.rows.iter().position(|r| r.2);
            let whole = n.whole;
            let choices = n
                .rows
                .into_iter()
                .map(|(index, label, _, wrapped, _, _)| Choice {
                    // Voice only for what is read whole and positively known.
                    tap_only: !known
                        || !whole
                        || wrapped
                        || is_tap_only(&label)
                        || (approval && !is_one_shot(&label)),
                    index,
                    label,
                })
                .collect();
            return (choices, selected);
        }
        // Unnumbered: the rows lined up with the selected one. Where one
        // label ends and the next begins is not known, so none is answered
        // by voice.
        let lines: Vec<&str> = self.text.lines().collect();
        let Some(range) = self.unnumbered_rows() else {
            return (Vec::new(), None);
        };
        let rows: Vec<(String, bool)> = lines[range]
            .iter()
            .map(|l| {
                let (text, marked) = unmarked(l);
                (text.trim().to_owned(), marked)
            })
            .collect();
        // One row alone (a text field's prompt) is not a menu.
        if rows.len() < 2 {
            return (Vec::new(), None);
        }
        let selected = rows.iter().position(|r| r.1);
        let choices = rows
            .into_iter()
            .enumerate()
            .map(|(index, (label, _))| Choice {
                index,
                label,
                tap_only: true,
            })
            .collect();
        (choices, selected)
    }

    /// The rows of unnumbered choices: those lined up with the selected
    /// row (marked, not the footer).
    fn unnumbered_rows(&self) -> Option<std::ops::Range<usize>> {
        let lines: Vec<&str> = self.text.lines().collect();
        let footer = self.footer();
        let at = lines.iter().rposition(|l| unmarked(l).1)?;
        if Some(at) == footer {
            return None;
        }
        let column = |l: &str| {
            let (text, _) = unmarked(l);
            l.chars().count() - text.chars().count()
        };
        let want = column(lines[at]);
        let fits = |i: usize| {
            let (text, _) = unmarked(lines[i]);
            !text.is_empty() && Some(i) != footer && column(lines[i]) == want
        };
        let first = (0..at).rev().find(|&i| !fits(i)).map_or(0, |n| n + 1);
        let last = (at..lines.len()).find(|&i| !fits(i)).unwrap_or(lines.len());
        Some(first..last)
    }

    /// The rows whose selection mark moves: the choices' first rows (and
    /// nothing else: a command or a question keeps every character).
    fn option_rows(&self) -> std::collections::HashSet<usize> {
        match self.numbered() {
            Some(n) => n.rows.iter().map(|r| r.4).collect(),
            None => self
                .unnumbered_rows()
                .map_or_else(Default::default, |r| r.collect()),
        }
    }

    fn is_approval<'a>(labels: impl Iterator<Item = &'a str> + Clone) -> bool {
        let starts = |w: &str| labels.clone().any(|l| l.to_lowercase().starts_with(w));
        starts("yes") && starts("no")
    }

    /// The text field the dialog shows, when it is one this positively
    /// knows: Codex's question form with no choices ("› Type your answer"),
    /// Pi's input box. A form that mixes choices and a field (Codex's "Add
    /// notes") is none. Its value is read exactly: the field's row after its
    /// mark, and the rows a long value wrapped onto, joined by the space the
    /// wrap took.
    #[must_use]
    pub fn field(&self, tool: ToolKind) -> Option<Field> {
        if !self.accepts_text || self.numbered().is_some() {
            return None;
        }
        let lines: Vec<&str> = self.text.lines().collect();
        if lines.iter().any(|l| {
            l.trim_start()
                .trim_start_matches('›')
                .trim_start()
                .starts_with("Add notes")
        }) {
            return None;
        }
        let end = self.footer().unwrap_or(lines.len());
        let (mark, placeholder) = match tool {
            ToolKind::Codex if lines.first().is_some_and(|l| l.starts_with("Question ")) => (
                '›',
                Some(&["Type your answer (optional)", "Type your answer"][..]),
            ),
            ToolKind::Pi => ('>', None),
            _ => return None,
        };
        let first = lines[..end]
            .iter()
            .rposition(|l| l.trim_start().starts_with(mark))?;
        let after = lines[first].trim_start().strip_prefix(mark)?;
        let mut value = after
            .strip_prefix(' ')
            .unwrap_or(after)
            .trim_end()
            .to_owned();
        // A long value wraps onto indented rows under it, down to the footer.
        for l in &lines[first + 1..end] {
            if l.trim().is_empty() {
                continue;
            }
            if !l.starts_with(' ') {
                return None;
            }
            value = format!("{value} {}", l.trim_start().trim_end());
        }
        // Only the placeholder whole is an empty field: "Type your answer
        // tomorrow" is someone's words.
        if placeholder.is_some_and(|p: &[&str]| p.contains(&value.as_str())) {
            value.clear();
        }
        Some(Field { first, end, value })
    }

    /// What it asks for.
    #[must_use]
    pub fn kind(&self, tool: ToolKind) -> Kind {
        if self.field(tool).is_some() {
            return Kind::Text;
        }
        match self.numbered() {
            Some(n) if self.known_approval(tool, &n) => Kind::Approval,
            Some(_) if self.known_menu(tool) => Kind::Menu,
            _ => Kind::Other,
        }
    }

    /// What it asks, for reading it out: for a permission this knows, its
    /// question and its operation whole; otherwise the dialog without its
    /// choices, field and footer.
    #[must_use]
    pub fn prompt(&self, tool: ToolKind) -> String {
        if self.kind(tool) == Kind::Approval
            && let Some(operation) = &self.operation
        {
            let asks = if tool == ToolKind::Claude {
                "Do you want to proceed?"
            } else {
                "Would you like to run the following command?"
            };
            return format!("{asks}\n{operation}");
        }
        let lines: Vec<&str> = self.text.lines().collect();
        let mut skip = std::collections::HashSet::new();
        if let Some(n) = self.numbered() {
            skip.extend(n.rows.iter().flat_map(|r| r.4..=r.5));
        }
        if let Some(f) = self.field(tool) {
            skip.extend(f.first..f.end);
        }
        skip.extend(self.footer());
        lines
            .iter()
            .enumerate()
            .filter(|(i, l)| !skip.contains(i) && !l.trim().is_empty())
            .map(|(_, l)| l.trim())
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// An id for this dialog, the same whichever choice is selected and
    /// whatever is typed in its field (the whole field, however many rows
    /// it wraps onto, counts as one mark): any other change to it (another
    /// command, another question) changes it.
    #[must_use]
    pub fn id(&self, tool: ToolKind) -> String {
        let field = self.field(tool);
        let options = self.option_rows();
        let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
        let mut eat = |bytes: &[u8]| {
            for b in bytes {
                hash ^= u64::from(*b);
                hash = hash.wrapping_mul(0x0100_0000_01b3);
            }
        };
        eat(tool.label().as_bytes());
        for (i, line) in self.text.lines().enumerate() {
            match &field {
                Some(f) if i == f.first => eat(b"\n\x01field"),
                Some(f) if i > f.first && i < f.end => {}
                // A choice row without its mark (the selection moves);
                // every other row exactly, a leading `>` of a command too.
                _ if options.contains(&i) => {
                    eat(b"\n");
                    eat(unmarked(line).0.trim_end().as_bytes());
                }
                _ => {
                    eat(b"\n");
                    eat(line.trim_end().as_bytes());
                }
            }
        }
        format!("q-{:012x}", hash & 0xffff_ffff_ffff)
    }
}

/// Why [`choose`] took nothing.
#[derive(Debug, PartialEq, Eq)]
pub enum ChooseRefused {
    /// No dialog now, or another one than the id named.
    Changed,
    NoSuchChoice,
    /// The choice allows more than one operation: by tap only.
    TapOnly,
    /// The selection did not move where it should, or the dialog changed
    /// while it moved: nothing was confirmed.
    DidNotMove,
    Input(String),
}

impl std::fmt::Display for ChooseRefused {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(match self {
            Self::Changed => "The question changed or was answered: read it again.",
            Self::NoSuchChoice => "That question has no such choice.",
            Self::TapOnly => {
                "That choice allows more than this one operation: it can only be tapped."
            }
            Self::DidNotMove => "The selection did not move as expected: nothing was chosen.",
            Self::Input(why) => why,
        })
    }
}

/// Takes `choice` of the dialog `id` on a terminal: only while `read` (the
/// terminal's dialog now) still shows that dialog. It moves the selection
/// with the arrow keys one at a time, reads again until the selection is on
/// the choice and the dialog is still the same, and only then presses
/// Enter. `wait` sleeps between reads (a few tens of milliseconds); after
/// `tries` reads without the selection in place nothing is pressed.
///
/// # Errors
/// Why nothing was chosen ([`ChooseRefused`]); with `Input` after the
/// arrows, the selection may have moved, but Enter was not pressed.
pub fn choose(
    tool: crate::ToolKind,
    id: &str,
    choice: usize,
    mut read: impl FnMut() -> Option<Question>,
    mut send: impl FnMut(&[u8]) -> Result<(), String>,
    mut wait: impl FnMut(),
    tries: usize,
) -> Result<(), ChooseRefused> {
    let same = |q: &Option<Question>| q.as_ref().is_some_and(|q| q.id(tool) == id);
    let now = read();
    if !same(&now) {
        return Err(ChooseRefused::Changed);
    }
    let Some(question) = now else {
        return Err(ChooseRefused::Changed);
    };
    let (choices, selected) = question.choices(tool);
    let target = choices.get(choice).ok_or(ChooseRefused::NoSuchChoice)?;
    if target.tap_only {
        return Err(ChooseRefused::TapOnly);
    }
    let Some(mut at) = selected else {
        return Err(ChooseRefused::DidNotMove);
    };
    for _ in 0..choices.len() * 2 {
        if at == choice {
            break;
        }
        send(if at < choice { b"\x1b[B" } else { b"\x1b[A" }).map_err(ChooseRefused::Input)?;
        // Wait for this key to be drawn before the next.
        let mut moved = false;
        for _ in 0..tries {
            wait();
            let now = read();
            if !same(&now) {
                return Err(ChooseRefused::Changed);
            }
            if let Some(now_at) = now.and_then(|q| q.choices(tool).1)
                && now_at != at
            {
                at = now_at;
                moved = true;
                break;
            }
        }
        if !moved {
            return Err(ChooseRefused::DidNotMove);
        }
    }
    // Read once more right before Enter: the same dialog, the choice
    // selected, and still not tap-only.
    let last = read();
    if !same(&last) {
        return Err(ChooseRefused::Changed);
    }
    let (choices, selected) = last.map(|q| q.choices(tool)).unwrap_or_default();
    if selected != Some(choice) || choices.get(choice).is_none_or(|c| c.tap_only) {
        return Err(ChooseRefused::DidNotMove);
    }
    send(b"\r").map_err(ChooseRefused::Input)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    #[test]
    fn only_the_active_approval_is_visible() {
        for (tool, screen) in [
            (
                ToolKind::Claude,
                include_str!(
                    "../../../crates/connecto-adapter/tests/fixtures/claude/approval_prompt_default.txt"
                ),
            ),
            (
                ToolKind::Codex,
                include_str!(
                    "../../../crates/connecto-adapter/tests/fixtures/codex/approval_prompt_inline.txt"
                ),
            ),
        ] {
            let q = active(tool, screen).expect("approval dialog");
            assert!(q.text.contains("touch /tmp/"));
            assert!(!q.text.contains("nothing else"));
            assert!(!q.accepts_text);
            assert!(active(tool, &format!("{screen}\n› New message")).is_none());
        }
    }
    #[test]
    fn codex_question_forms_accept_both_free_text_and_choices() {
        for (body, free) in [
            ("› Type your answer (optional)", true),
            ("› 1. Continue\n  2. Wait", false),
        ] {
            let screen = format!(
                "tool output\nQuestion 1/2 (2 unanswered)\nPick the next step.\n{body}\nenter to submit answer | esc to interrupt"
            );
            let q = active(ToolKind::Codex, &screen).unwrap();
            assert_eq!(q.accepts_text, free);
            assert!(q.text.contains("Pick the next step."));
            assert!(!q.text.contains("tool output"));
        }
    }

    #[test]
    fn pi_selector_and_text_input_keep_the_title_and_exclude_previous_work() {
        for (contents, accepts_text) in [
            ("→ Yes\n  No\n↑↓ navigate  enter select  esc cancel", false),
            ("> \nenter submit  esc cancel", true),
        ] {
            let q = active(
                ToolKind::Pi,
                &format!(
                    "previous tool result\n──────────\nChoose a branch\n{contents}\n──────────"
                ),
            )
            .unwrap();
            assert!(q.text.contains("Choose a branch"));
            assert!(!q.text.contains("previous"));
            assert_eq!(q.accepts_text, accepts_text);
        }
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod choice_tests {
    use super::*;
    use std::cell::RefCell;

    const CLAUDE: &str = include_str!(
        "../../../crates/connecto-adapter/tests/fixtures/claude/approval_prompt_default.txt"
    );
    const CODEX: &str = include_str!(
        "../../../crates/connecto-adapter/tests/fixtures/codex/approval_prompt_inline.txt"
    );
    const TRUST: &str =
        include_str!("../../../crates/connecto-adapter/tests/fixtures/claude/trust_prompt.txt");

    /// The tool a test dialog is read as.
    fn tool_of(q: &Question) -> ToolKind {
        if q.text.contains("Codex")
            || q.text.starts_with("Question ")
            || q.text.starts_with("Would")
        {
            ToolKind::Codex
        } else if q.text.contains('→') {
            ToolKind::Pi
        } else {
            ToolKind::Claude
        }
    }

    fn labels(q: &Question) -> Vec<(String, bool)> {
        q.choices(tool_of(q))
            .0
            .into_iter()
            .map(|c| (c.label, c.tap_only))
            .collect()
    }

    #[test]
    fn approvals_read_as_choices_with_the_broad_ones_tap_only() {
        let q = active(ToolKind::Claude, CLAUDE).unwrap();
        assert_eq!(q.kind(ToolKind::Claude), Kind::Approval);
        assert_eq!(
            labels(&q),
            [
                ("Yes".into(), false),
                (
                    "Yes, and always allow access to /tmp from this project".into(),
                    true
                ),
                (
                    "Yes, and switch to auto mode · auto mode handles these prompts for you".into(),
                    true
                ),
                ("No".into(), false),
            ]
        );
        assert_eq!(q.choices(ToolKind::Claude).1, Some(0));
        assert_eq!(
            q.operation.as_deref(),
            Some("Bash command: touch /tmp/cc_probe_approval_y")
        );

        let q = active(ToolKind::Codex, CODEX).unwrap();
        assert_eq!(q.kind(ToolKind::Codex), Kind::Approval);
        let l = labels(&q);
        assert_eq!(l.len(), 3);
        assert_eq!(l[0], ("Yes, proceed (y)".into(), false));
        assert!(l[1].1, "don't ask again is tap-only: {l:?}");
        assert!(l[2].0.starts_with("No, and tell Codex"));
        assert!(!l[2].1);
        assert_eq!(q.choices(ToolKind::Codex).1, Some(0));
        assert_eq!(
            q.operation.as_deref(),
            Some("$ touch /tmp/cx_probe_approval_x")
        );

        // Unnumbered: where a label ends is not known, so none by voice.
        let q = active(ToolKind::Claude, TRUST).unwrap();
        assert_eq!(
            labels(&q),
            [
                ("No, exit".into(), true),
                ("Yes, I trust this folder".into(), true)
            ]
        );
    }

    #[test]
    fn menus_text_fields_and_unnumbered_selectors() {
        let menu = active(
            ToolKind::Codex,
            "x\nQuestion 1/1\nPick the next step.\n  1. Continue\n› 2. Wait\nenter to submit answer | esc to go back",
        )
        .unwrap();
        assert_eq!(menu.kind(ToolKind::Codex), Kind::Menu);
        assert_eq!(menu.choices(ToolKind::Codex).1, Some(1));
        let text = active(
            ToolKind::Codex,
            "x\nQuestion 1/1\nWhy?\n› Type your answer (optional)\nenter to submit answer | esc to go back",
        )
        .unwrap();
        assert_eq!(text.kind(ToolKind::Codex), Kind::Text);
        let pi = active(
            ToolKind::Pi,
            "──────────\nChoose a branch\n  main\n→ dev\n↑↓ navigate  enter select  esc cancel\n──────────",
        )
        .unwrap();
        assert_eq!(labels(&pi), [("main".into(), true), ("dev".into(), true)]);
        assert_eq!(pi.choices(ToolKind::Pi).1, Some(1));
    }

    #[test]
    fn the_id_ignores_the_selection_but_not_the_command() {
        let q = active(ToolKind::Claude, CLAUDE).unwrap();
        let moved = Question {
            text: q
                .text
                .replace("❯ 1. Yes", "  1. Yes")
                .replace("   4. No", " ❯ 4. No"),
            ..q.clone()
        };
        assert_eq!(moved.choices(ToolKind::Claude).1, Some(3));
        assert_eq!(q.id(ToolKind::Claude), moved.id(ToolKind::Claude));
        let other = Question {
            text: q.text.replace("cc_probe_approval_y", "rm -rf ~"),
            ..q.clone()
        };
        assert_ne!(q.id(ToolKind::Claude), other.id(ToolKind::Claude));
        assert_ne!(q.id(ToolKind::Claude), q.id(ToolKind::Codex));
    }

    fn approval(options: &str) -> Question {
        active(
            ToolKind::Claude,
            &format!(
                "─────────────────────────\n Bash command\n╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌\n rm -rf build\n╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌╌\n Do you want to proceed?\n{options}\n\n Esc to cancel · Tab to amend"
            ),
        )
        .unwrap()
    }

    #[test]
    fn a_wrapped_label_is_never_answered_by_voice() {
        // "Yes, and" whose continuation says "always allow".
        let q = approval(
            " ❯ 1. Yes\n   2. Yes, and\n      always allow rm from this project\n   3. No",
        );
        assert_eq!(
            labels(&q),
            [
                ("Yes".into(), false),
                ("Yes, and always allow rm from this project".into(), true),
                ("No".into(), false),
            ]
        );
        // Wrapped even when the continuation says nothing broad.
        let q = approval(" ❯ 1. Yes, proceed\n      for this command\n   2. No");
        assert!(q.choices(ToolKind::Claude).0[0].tap_only);
        // A menu's wrapped label, with no broad words: where it ends is
        // not known, so it is not taken by voice either.
        let menu = active(
            ToolKind::Codex,
            "x\nQuestion 1/1\nNext?\n› 1. Continue with\n       the migration\n  2. Wait\nenter to submit answer | esc to go back",
        )
        .unwrap();
        assert_eq!(
            labels(&menu),
            [
                ("Continue with the migration".into(), true),
                ("Wait".into(), false)
            ]
        );
    }

    #[test]
    fn a_layout_not_read_whole_or_an_unknown_permission_is_tap_only() {
        // A row under the choices this does not read: nothing by voice.
        let q = approval(" ❯ 1. Yes\n   2. No\n Note: this also deletes caches");
        assert!(q.choices(ToolKind::Claude).0.iter().all(|c| c.tap_only));
        // A permission answer not known to be one-shot.
        let q = approval(" ❯ 1. Yes, for this folder\n   2. No");
        assert_eq!(
            labels(&q),
            [("Yes, for this folder".into(), true), ("No".into(), false)]
        );
        assert!(is_one_shot(
            "No, and tell Claude what to do differently (esc)"
        ));
        assert!(!is_one_shot("No, and tell Claude to always allow it (esc)"));
    }

    #[test]
    fn only_a_known_text_field_takes_words_and_its_value_is_not_the_id() {
        let form = |field: &str| {
            active(
                ToolKind::Codex,
                &format!("x\nQuestion 1/1\nWhy?\n{field}\nenter to submit answer | esc to go back"),
            )
            .unwrap()
        };
        let empty = form("› Type your answer (optional)");
        let typed = form("› Because the tests fail");
        assert_eq!(empty.field(ToolKind::Codex).unwrap().value, "");
        assert_eq!(
            typed.field(ToolKind::Codex).unwrap().value,
            "Because the tests fail"
        );
        assert_eq!(empty.id(ToolKind::Codex), typed.id(ToolKind::Codex));
        assert_eq!(empty.prompt(ToolKind::Codex), "Question 1/1\nWhy?");
        let other = form("› Type your answer (optional)")
            .text
            .replace("Why?", "Where?");
        assert_ne!(
            Question {
                text: other,
                accepts_text: true,
                operation: None,
            }
            .id(ToolKind::Codex),
            empty.id(ToolKind::Codex)
        );
        // Choices and a notes field together: no words by voice.
        let mixed = active(
            ToolKind::Codex,
            "x\nQuestion 1/1\nPick one.\n  1. Continue\n  2. Wait\n› Add notes\nenter to submit answer | esc to go back",
        )
        .unwrap();
        assert!(mixed.accepts_text);
        assert!(mixed.field(ToolKind::Codex).is_none());
        // Claude has no field this knows.
        assert!(
            Question {
                text: "Tell me more\n> \nEsc to cancel · Enter to submit".into(),
                accepts_text: true,
                operation: None,
            }
            .field(ToolKind::Claude)
            .is_none()
        );
    }

    // Regression: Claude's approval clipped to the rows from "Do you want
    // to proceed?" read as a whole permission, with no command.
    #[test]
    fn a_permission_whose_operation_is_not_recovered_is_tap_only() {
        let clipped = CLAUDE[CLAUDE.find(" Do you want to proceed?").unwrap()..].to_owned();
        let q = active(ToolKind::Claude, &clipped).unwrap();
        assert_eq!(q.operation, None);
        assert_eq!(q.kind(ToolKind::Claude), Kind::Other);
        assert!(labels(&q).iter().all(|(_, tap)| *tap), "{:?}", labels(&q));
        // A Codex command that says "esc to cancel": read back whole, and
        // the footer is only the dialog's last row.
        let codex = CODEX.replace(
            "touch /tmp/cx_probe_approval_x\n\n\n›",
            "echo 'press esc to cancel' > /tmp/x\n\n\n›",
        );
        let codex = codex.replace(
            "$ touch /tmp/cx_probe_approval_x",
            "$ echo 'press esc to cancel' > /tmp/x",
        );
        let q = active(ToolKind::Codex, &codex).unwrap();
        assert_eq!(q.kind(ToolKind::Codex), Kind::Approval);
        assert!(
            q.prompt(ToolKind::Codex)
                .contains("$ echo 'press esc to cancel' > /tmp/x"),
            "{}",
            q.prompt(ToolKind::Codex)
        );
        assert!(!labels(&q)[0].1);
    }

    // Regression: an unknown numbered permission ("Allow permanently") was
    // a voice-selectable menu for Claude, OpenCode and Antigravity.
    #[test]
    fn an_unknown_layout_is_tap_only_whatever_the_provider() {
        let screen = "─────────────────────────\n Change permission policy?\n ❯ 1. Allow permanently\n   2. Refuse\n\n Esc to cancel · Enter to select";
        for tool in [ToolKind::Claude, ToolKind::OpenCode, ToolKind::Antigravity] {
            let q = active(tool, screen).unwrap();
            assert_eq!(q.kind(tool), Kind::Other, "{tool:?}");
            assert!(q.choices(tool).0.iter().all(|c| c.tap_only), "{tool:?}");
        }
        // Not even a plain "Yes"/"No" without a recovered operation.
        let screen = "─────────────────────────\n Edit file foo.rs\n Do you want to make this edit?\n ❯ 1. Yes\n   2. No\n\n Esc to cancel · Tab to amend";
        let q = active(ToolKind::Claude, screen).unwrap();
        assert!(q.choices(ToolKind::Claude).0.iter().all(|c| c.tap_only));
    }

    // Regression: a value wrapped onto a second row lost its field (taken
    // for an unnumbered choice) and changed the id.
    #[test]
    fn a_wrapped_field_value_is_read_exactly_and_keeps_the_id() {
        let form = |field: &str| {
            active(
                ToolKind::Codex,
                &format!("x\nQuestion 1/1\nWhy?\n{field}\nenter to submit answer | esc to go back"),
            )
            .unwrap()
        };
        let empty = form("› Type your answer (optional)");
        let one = form("› two words");
        let wrapped = form("› two\n  words");
        let spaced = form("› a  b");
        assert_eq!(wrapped.field(ToolKind::Codex).unwrap().value, "two words");
        assert_eq!(spaced.field(ToolKind::Codex).unwrap().value, "a  b");
        assert_eq!(wrapped.kind(ToolKind::Codex), Kind::Text);
        let id = empty.id(ToolKind::Codex);
        for q in [&one, &wrapped, &spaced] {
            assert_eq!(q.id(ToolKind::Codex), id, "{}", q.text);
        }
    }

    // Regression: a command whose own text has numbered rows ("1. Monday")
    // was cut at its first one, and the read-back ended at "Agenda:".
    #[test]
    fn a_command_with_numbered_rows_is_read_whole_up_to_the_choices() {
        let agenda = "$ echo 'Agenda:\n  1. Monday\n  2. Tuesday'";
        let screen = CODEX.replace("  $ touch /tmp/cx_probe_approval_x", &format!("  {agenda}"));
        let q = active(ToolKind::Codex, &screen).unwrap();
        assert_eq!(
            q.operation.as_deref(),
            Some("$ echo 'Agenda:\n1. Monday\n2. Tuesday'")
        );
        assert_eq!(q.kind(ToolKind::Codex), Kind::Approval);
        assert!(q.prompt(ToolKind::Codex).contains("2. Tuesday'"));
        assert!(!labels(&q)[0].1, "Yes, proceed by voice");
        // No blank row between the command and the choices: where the
        // command ends is not certain, so nothing by voice.
        let tight = screen.replace("Tuesday'\n\n\n›", "Tuesday'\n›");
        let q = active(ToolKind::Codex, &tight).unwrap();
        assert_eq!(q.operation, None, "{}", q.text);
        assert!(labels(&q).iter().all(|(_, tap)| *tap));
    }

    // Regression: "› Type your answer tomorrow" (someone's words starting
    // like the placeholder) read as an empty field.
    #[test]
    fn only_the_whole_placeholder_is_an_empty_field() {
        let form = |field: &str| {
            active(
                ToolKind::Codex,
                &format!(
                    "x\nQuestion 1/1\nWhen?\n{field}\nenter to submit answer | esc to go back"
                ),
            )
            .unwrap()
        };
        for empty in ["› Type your answer (optional)", "› Type your answer"] {
            assert_eq!(form(empty).field(ToolKind::Codex).unwrap().value, "");
        }
        assert_eq!(
            form("› Type your answer tomorrow")
                .field(ToolKind::Codex)
                .unwrap()
                .value,
            "Type your answer tomorrow"
        );
    }

    // Regression: the id stripped a selection mark from every row, so two
    // commands differing by a `>` at the start of a row had the same id.
    #[test]
    fn a_command_row_keeps_its_marks_in_the_id() {
        let with = |row: &str| {
            let screen = CODEX.replace(
                "  $ touch /tmp/cx_probe_approval_x",
                &format!("  $ printf 'a\n{row}'"),
            );
            active(ToolKind::Codex, &screen).unwrap()
        };
        let quoted = with("> b");
        let plain = with("b");
        assert_ne!(quoted.id(ToolKind::Codex), plain.id(ToolKind::Codex));
        // The selection still moves without changing it.
        let moved = Question {
            text: quoted
                .text
                .replace("› 1. Yes", "  1. Yes")
                .replace("  3. No", "› 3. No"),
            ..quoted.clone()
        };
        assert_eq!(moved.choices(ToolKind::Codex).1, Some(2));
        assert_eq!(moved.id(ToolKind::Codex), quoted.id(ToolKind::Codex));
    }

    /// A fake Claude approval that moves its selection with the arrows.
    struct Fake {
        selected: RefCell<usize>,
        command: RefCell<&'static str>,
        typed: RefCell<Vec<Vec<u8>>>,
    }

    impl Fake {
        fn new() -> Self {
            Self {
                selected: RefCell::new(0),
                command: RefCell::new("cc_probe_approval_y"),
                typed: RefCell::default(),
            }
        }
        fn read(&self) -> Option<Question> {
            let mut screen = CLAUDE.replace("cc_probe_approval_y", &self.command.borrow());
            screen = screen.replace("❯ 1. Yes", "  1. Yes");
            let rows = [
                "1. Yes",
                "2. Yes, and always",
                "3. Yes, and switch",
                "4. No",
            ];
            let row = rows[*self.selected.borrow()];
            screen = screen.replacen(&format!("  {row}"), &format!("❯ {row}"), 1);
            active(ToolKind::Claude, &screen)
        }
        fn send(&self, bytes: &[u8]) -> Result<(), String> {
            let mut at = self.selected.borrow_mut();
            match bytes {
                b"\x1b[B" => *at = (*at + 1).min(3),
                b"\x1b[A" => *at = at.saturating_sub(1),
                _ => {}
            }
            self.typed.borrow_mut().push(bytes.to_vec());
            Ok(())
        }
    }

    #[test]
    fn choose_moves_to_the_choice_then_presses_enter() {
        let fake = Fake::new();
        let id = fake.read().unwrap().id(ToolKind::Claude);
        choose(
            ToolKind::Claude,
            &id,
            3,
            || fake.read(),
            |b| fake.send(b),
            || {},
            3,
        )
        .unwrap();
        let typed = fake.typed.borrow();
        assert_eq!(typed.len(), 4);
        assert_eq!(typed[3], b"\r");
        assert!(typed[..3].iter().all(|k| k == b"\x1b[B"));
    }

    #[test]
    fn choose_refuses_a_changed_dialog_and_tap_only_choices() {
        let fake = Fake::new();
        let id = fake.read().unwrap().id(ToolKind::Claude);
        assert_eq!(
            choose(
                ToolKind::Claude,
                &id,
                1,
                || fake.read(),
                |b| fake.send(b),
                || {},
                3
            ),
            Err(ChooseRefused::TapOnly)
        );
        *fake.command.borrow_mut() = "rm -rf ~";
        assert_eq!(
            choose(
                ToolKind::Claude,
                &id,
                0,
                || fake.read(),
                |b| fake.send(b),
                || {},
                3
            ),
            Err(ChooseRefused::Changed)
        );
        assert!(fake.typed.borrow().is_empty());
        // Changed while the selection moves: no Enter.
        let fake = Fake::new();
        let id = fake.read().unwrap().id(ToolKind::Claude);
        let reads = RefCell::new(0);
        let result = choose(
            ToolKind::Claude,
            &id,
            3,
            || {
                *reads.borrow_mut() += 1;
                if *reads.borrow() == 3 {
                    *fake.command.borrow_mut() = "rm -rf ~";
                }
                fake.read()
            },
            |b| fake.send(b),
            || {},
            3,
        );
        assert_eq!(result, Err(ChooseRefused::Changed));
        assert!(!fake.typed.borrow().iter().any(|k| k == b"\r"));
        // A selection that never moves: no Enter.
        let fake = Fake::new();
        let id = fake.read().unwrap().id(ToolKind::Claude);
        assert_eq!(
            choose(
                ToolKind::Claude,
                &id,
                3,
                || fake.read(),
                |_| Ok(()),
                || {},
                3
            ),
            Err(ChooseRefused::DidNotMove)
        );
    }
}
