//! The whole word under a cell: what a double click selects and what a
//! file path is read from. A word goes on across rows the terminal wrapped,
//! and across rows an agent broke itself: a token (a URL, path, command, key)
//! cut at the agent's right edge and carried on, indented, on the next row
//! (see `connecto_core::copy_text`). The indentation between the two halves
//! is not part of the word.

use connecto_core::copy_text::{is_token_like, wrap_edge};
use connecto_core::{CellFlags, TermSnapshot};

use crate::selection::CellPos;

/// Rows read above and below the pointer's row to follow a broken word.
const REACH: u16 = 12;

/// A word on the screen: its text and its first and last cell.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Word {
    pub(crate) text: String,
    pub(crate) first: CellPos,
    pub(crate) last: CellPos,
    /// The cells it covers, as `(row, start, end)` with `end` exclusive.
    pub(crate) spans: Vec<(u16, u16, u16)>,
}

/// Characters that end a word: blanks, and the quotes, brackets and bars that
/// enclose one.
fn separates(c: char) -> bool {
    c.is_whitespace()
        || c.is_control()
        || matches!(
            c,
            '"' | '\''
                | '`'
                | '<'
                | '>'
                | '('
                | ')'
                | '['
                | ']'
                | '{'
                | '}'
                | '|'
                | ','
                | '│'
                | '┃'
                | '⏺'
                | '⎿'
        )
}

fn blank(snapshot: &TermSnapshot, row: u16, col: usize) -> bool {
    snapshot
        .row(row)
        .get(col)
        .is_none_or(|cell| cell.ch == ' ' || cell.ch == '\0')
}

/// The column after a row's last non-blank cell, 0 for a blank row.
fn content_end(snapshot: &TermSnapshot, row: u16) -> usize {
    let cells = snapshot.row(row);
    (0..cells.len())
        .rev()
        .find(|&col| !blank(snapshot, row, col))
        .map_or(0, |col| col + 1)
}

/// The column of a row's first non-blank cell.
fn indent(snapshot: &TermSnapshot, row: u16) -> Option<usize> {
    (0..usize::from(snapshot.cols)).find(|&col| !blank(snapshot, row, col))
}

/// The last blank-separated word of a row.
fn last_word(snapshot: &TermSnapshot, row: u16) -> String {
    let text: String = snapshot.row(row).iter().map(|cell| cell.ch).collect();
    text.trim_end()
        .rsplit(char::is_whitespace)
        .next()
        .unwrap_or("")
        .to_owned()
}

/// How `row` goes on in the row below it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Next {
    /// The terminal wrapped it: the next row's cells follow straight on.
    Wrapped,
    /// An agent broke a token at its edge: the token goes on after the next
    /// row's indentation.
    Broken,
    Ends,
}

fn next_of(snapshot: &TermSnapshot, row: u16, edge: Option<usize>) -> Next {
    if row + 1 >= snapshot.rows {
        return Next::Ends;
    }
    if snapshot
        .row(row)
        .last()
        .is_some_and(|cell| cell.flags.contains(CellFlags::WRAPPED))
    {
        return Next::Wrapped;
    }
    let Some(edge) = edge else { return Next::Ends };
    let continues = content_end(snapshot, row) >= edge
        && indent(snapshot, row + 1).is_some_and(|col| col > 0)
        && is_token_like(&last_word(snapshot, row));
    if continues { Next::Broken } else { Next::Ends }
}

/// The word covering `at`, if `at` is on one.
pub(crate) fn word_at(snapshot: &TermSnapshot, at: CellPos) -> Option<Word> {
    if at.0 >= snapshot.rows || at.1 >= snapshot.cols {
        return None;
    }
    let edge = wrap_edge(
        (0..snapshot.rows).map(|row| content_end(snapshot, row)),
        usize::from(snapshot.cols),
    );
    let mut first = at.0;
    while first > 0 && at.0 - first < REACH && next_of(snapshot, first - 1, edge) != Next::Ends {
        first -= 1;
    }
    let mut last = at.0;
    while last - at.0 < REACH && next_of(snapshot, last, edge) != Next::Ends {
        last += 1;
    }
    // The characters of the joined rows with the cell each came from.
    let mut chars: Vec<char> = Vec::new();
    let mut cells: Vec<CellPos> = Vec::new();
    for row in first..=last {
        let skip = if row > first && next_of(snapshot, row - 1, edge) == Next::Broken {
            indent(snapshot, row).unwrap_or(0)
        } else {
            0
        };
        let end = if row < last && next_of(snapshot, row, edge) == Next::Broken {
            content_end(snapshot, row)
        } else {
            usize::from(snapshot.cols)
        };
        for (col, cell) in snapshot.row(row).iter().enumerate().take(end).skip(skip) {
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            chars.push(if cell.ch == '\0' { ' ' } else { cell.ch });
            cells.push((row, col as u16));
        }
    }
    let hovered = cells.iter().position(|cell| *cell == at).or_else(|| {
        // The right half of a wide glyph belongs to the glyph before it.
        cells.iter().rposition(|cell| *cell < at)
    })?;
    if separates(chars[hovered]) {
        return None;
    }
    let mut start = hovered;
    while start > 0 && !separates(chars[start - 1]) {
        start -= 1;
    }
    let mut end = hovered + 1;
    while end < chars.len() && !separates(chars[end]) {
        end += 1;
    }
    // Sentence punctuation after a word is not part of it.
    while end > hovered + 1 && matches!(chars[end - 1], '.' | ':' | ';' | '!' | '?') {
        end -= 1;
    }
    let mut spans: Vec<(u16, u16, u16)> = Vec::new();
    for &(row, col) in &cells[start..end] {
        match spans.last_mut() {
            Some(span) if span.0 == row => span.2 = col + 1,
            _ => spans.push((row, col, col + 1)),
        }
    }
    Some(Word {
        text: chars[start..end].iter().collect(),
        first: cells[start],
        last: cells[end - 1],
        spans,
    })
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use connecto_core::Cell;

    use super::*;

    fn screen(cols: usize, lines: &[&str]) -> TermSnapshot {
        let cells = lines
            .iter()
            .flat_map(|line| {
                format!("{line:<cols$}")
                    .chars()
                    .map(|ch| Cell {
                        ch,
                        ..Cell::default()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        TermSnapshot {
            cols: cols as u16,
            rows: lines.len() as u16,
            cells,
            ..TermSnapshot::default()
        }
    }

    #[test]
    fn a_word_is_taken_whole_without_its_punctuation() {
        let snap = screen(40, &["see src/main.rs:12, then (run) it."]);
        let word = |col| word_at(&snap, (0, col)).map(|w| w.text);
        assert_eq!(word(6).as_deref(), Some("src/main.rs:12"));
        assert_eq!(word(26).as_deref(), Some("run"));
        assert_eq!(word(32).as_deref(), Some("it"));
        assert_eq!(word(3), None, "a blank");
    }

    #[test]
    fn a_token_an_agent_broke_at_its_edge_is_one_word() {
        let lines = [
            "⏺ open https://example.com/very/lo",
            "  ng/path and the rest",
        ];
        let snap = screen(36, &lines);
        let word = word_at(&snap, (1, 3)).unwrap();
        assert_eq!(word.text, "https://example.com/very/long/path");
        assert_eq!(word.first, (0, 7));
        assert_eq!(word.last, (1, 8));
        assert_eq!(word.spans, vec![(0, 7, 34), (1, 2, 9)]);
        // Reached from its first half too.
        assert_eq!(word_at(&snap, (0, 10)).unwrap().text, word.text);
        // The prose after it is its own word.
        assert_eq!(word_at(&snap, (1, 11)).unwrap().text, "and");
    }

    #[test]
    fn plain_rows_that_end_at_the_edge_are_not_joined() {
        // Not indented: plain output, each row its own.
        let snap = screen(20, &["aaaa/bbbb/cccc/dddd", "eeee"]);
        assert_eq!(word_at(&snap, (0, 2)).unwrap().text, "aaaa/bbbb/cccc/dddd");
        // A prose word at the edge does not go on either.
        let snap = screen(20, &["  some words endings", "  next"]);
        assert_eq!(word_at(&snap, (0, 15)).unwrap().text, "endings");
    }
}
