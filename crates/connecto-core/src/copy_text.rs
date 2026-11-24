//! The "clean" copy of terminal text: what a person meant when they selected
//! text that a full-screen program (an agent CLI) broke over rows itself.
//!
//! Such programs end every row with a real line break and indent the rows
//! that follow, so a plain copy of a long URL, path or command comes out cut
//! in pieces with blanks in the middle, and a paragraph comes out as short
//! indented lines. The clean copy puts them back together:
//!
//! - the margin the program draws (its indentation, and a bullet or bar in
//!   front such as `⏺`, `⎿`, `│`) is dropped;
//! - a token cut at the program's right edge (a URL, a path, a command, a
//!   key) is joined to the next row with nothing in between;
//! - a prose row that ended because the next word did not fit is joined to
//!   the next row with one space;
//! - every other break stays: blank lines, list items, short code lines, and
//!   the indentation of lines kept apart (relative to the least indented).
//!
//! Joining needs a continuation row that is indented (the program's margin):
//! plain command output such as a table filling the screen keeps its rows.

/// How far short of the screen's last column a program's right edge may be
/// and still count as an edge it wraps at.
pub const EDGE_SLACK: usize = 8;

/// One logical line of a selection (soft-wrapped rows already joined), as the
/// screen shows it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CopyLine {
    /// The selected characters, trailing blanks dropped.
    pub text: String,
    /// The screen column `text` begins at.
    pub start: usize,
    /// The unselected cells before `start` on the line's first row, which
    /// tell where the program's margin ends when the drag began inside it.
    pub lead: String,
    /// The column just after the last non-blank cell of the line's last row,
    /// selected or not; 0 for a blank row.
    pub end: usize,
}

/// The column a program wraps its text at, given the content end of each row
/// it drew on a screen `cols` wide: the furthest end, when it is near the
/// screen's edge. `None` when no row comes near the edge.
#[must_use]
pub fn wrap_edge(ends: impl IntoIterator<Item = usize>, cols: usize) -> Option<usize> {
    let furthest = ends.into_iter().max()?;
    (furthest > 0 && furthest + EDGE_SLACK >= cols).then_some(furthest)
}

/// Characters that make a word a token (a URL, path, identifier, flag, number
/// or key) rather than a word of prose, which a program cuts anywhere.
#[must_use]
pub fn is_token_like(word: &str) -> bool {
    word.chars()
        .any(|c| c.is_ascii_digit() || "/\\.:_-=?&%#@~+$".contains(c))
        || word.chars().count() >= 24
}

/// Glyphs a program draws in its margin in front of the text: message bullets
/// and the bars of boxes and quotes.
fn is_gutter(c: char) -> bool {
    matches!(c, '⏺' | '⎿' | '●' | '│' | '┃' | '▌' | '▎' | '›' | '❯')
}

/// A line split into its margin and its content.
struct Parsed<'a> {
    /// The screen column the content begins at.
    indent: usize,
    body: &'a str,
    /// The line's `end` from the screen.
    end: usize,
}

fn parse(line: &CopyLine) -> Parsed<'_> {
    // The margin is read over the whole row: the unselected lead, then the
    // selected text. Its blanks and glyphs are never part of the body.
    let lead: Vec<char> = line.lead.chars().collect();
    let mut stream = lead.iter().copied().chain(line.text.chars()).peekable();
    let mut margin: usize = 0;
    while let Some(&c) = stream.peek() {
        if c == ' ' {
            stream.next();
            margin += 1;
            continue;
        }
        if is_gutter(c) {
            let mut ahead = stream.clone();
            ahead.next();
            if matches!(ahead.peek(), Some(' ') | None) {
                stream.next();
                margin += 1;
                continue;
            }
        }
        break;
    }
    // How much of the selected text is margin.
    let skip = margin.saturating_sub(lead.len());
    let rest = line
        .text
        .char_indices()
        .nth(skip)
        .map_or("", |(at, _)| &line.text[at..])
        .trim_start_matches(' ');
    // A box's right bar.
    let body = rest
        .trim_end()
        .strip_suffix(['│', '┃'])
        .map_or(rest.trim_end(), str::trim_end);
    Parsed {
        indent: if line.lead.is_empty() {
            line.start + margin
        } else {
            margin
        },
        body,
        end: line.end,
    }
}

/// Whether `body` starts a block of its own: a list item, a heading, a
/// quote, a code fence or a table row.
fn starts_block(body: &str) -> bool {
    let mut words = body.split(' ');
    let first = words.next().unwrap_or("");
    let has_more = words.next().is_some();
    matches!(first, "-" | "*" | "+" | "•" | "◦" | ">" | "|") && has_more
        || first.starts_with('#') && first.chars().all(|c| c == '#') && has_more
        || first.starts_with("```")
        || first.starts_with('|')
        || has_more
            && first.len() >= 2
            && first.ends_with(['.', ')'])
            && first[..first.len() - 1].chars().all(|c| c.is_ascii_digit())
}

/// How two consecutive lines go together.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Join {
    Glue,
    Space,
    Break,
}

fn join(prev: &Parsed<'_>, next: &Parsed<'_>, edge: Option<usize>, cols: usize) -> Join {
    if prev.body.is_empty() || next.body.is_empty() || starts_block(next.body) {
        return Join::Break;
    }
    // A URL a full-screen program cut at the screen's last column, carried on
    // at the start of the next row (a login link).
    let tail = prev.body.rsplit(' ').next().unwrap_or("");
    if prev.end >= cols && next.indent == 0 && tail.contains("://") {
        return Join::Glue;
    }
    // The continuation sits in the program's margin, at or a little right of
    // where the line before began (a list item's text is indented under it).
    let continues = next.indent > 0 && next.indent >= prev.indent && next.indent <= prev.indent + 4;
    if !continues {
        return Join::Break;
    }
    let width = edge.unwrap_or(cols);
    if edge.is_some() && prev.end >= width {
        return if is_token_like(tail) {
            Join::Glue
        } else {
            Join::Space
        };
    }
    let head = next.body.split(' ').next().unwrap_or("");
    if prev.end + 1 + head.chars().count() > width {
        Join::Space
    } else {
        Join::Break
    }
}

/// The clean copy of `lines` taken from a screen `cols` wide. See the module
/// documentation.
#[must_use]
pub fn clean_copy(lines: &[CopyLine], cols: usize) -> String {
    let parsed: Vec<Parsed<'_>> = lines.iter().map(parse).collect();
    let edge = wrap_edge(lines.iter().map(|line| line.end), cols);
    // Lines kept apart keep their indentation relative to the least indented
    // line, measured on the screen (the first line's too, though its own text
    // starts where the drag did).
    let base = parsed
        .iter()
        .filter(|line| !line.body.is_empty())
        .map(|line| line.indent)
        .min()
        .unwrap_or(0);
    let mut out = String::new();
    for (index, line) in parsed.iter().enumerate() {
        // A drag that began in the first line's margin takes its indentation.
        if index == 0 && lines[0].start <= line.indent {
            out.extend(std::iter::repeat_n(' ', line.indent.saturating_sub(base)));
        }
        if index > 0 {
            match join(&parsed[index - 1], line, edge, cols) {
                Join::Glue => {}
                Join::Space => out.push(' '),
                Join::Break => {
                    out.push('\n');
                    if !line.body.is_empty() {
                        out.extend(std::iter::repeat_n(' ', line.indent.saturating_sub(base)));
                    }
                }
            }
        }
        out.push_str(line.body);
    }
    let kept = out.trim_end().len();
    out.truncate(kept);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Lines of a screen `cols` wide, each selected whole from column 0.
    fn copy(cols: usize, rows: &[&str]) -> String {
        let lines: Vec<CopyLine> = rows
            .iter()
            .map(|row| CopyLine {
                text: row.trim_end().to_owned(),
                start: 0,
                lead: String::new(),
                end: row.trim_end().chars().count(),
            })
            .collect();
        clean_copy(&lines, cols)
    }

    #[test]
    fn an_agent_paragraph_with_a_cut_url_comes_back_whole() {
        let rows = [
            "⏺ Open https://example.com/very/long/pa",
            "  th/to/page and then run the release",
            "  build.",
        ];
        assert_eq!(
            copy(42, &rows),
            "Open https://example.com/very/long/path/to/page and then run the release build."
        );
    }

    #[test]
    fn a_long_command_cut_at_the_edge_is_one_line() {
        let rows = [
            "  cargo build --release --features=one,tw",
            "  o,three --target aarch64-apple-darwin",
        ];
        assert_eq!(
            copy(44, &rows),
            "cargo build --release --features=one,two,three --target aarch64-apple-darwin"
        );
    }

    #[test]
    fn short_code_lines_keep_their_breaks_and_relative_indentation() {
        let rows = ["  fn main() {", "      run();", "  }"];
        assert_eq!(copy(80, &rows), "fn main() {\n    run();\n}");
    }

    #[test]
    fn list_items_and_blank_lines_stay_apart() {
        let rows = [
            "  Two things:",
            "",
            "  - first item that is long enough to",
            "    wrap onto a second row",
            "  - second",
        ];
        assert_eq!(
            copy(40, &rows),
            "Two things:\n\n- first item that is long enough to wrap onto a second row\n- second"
        );
    }

    #[test]
    fn plain_output_filling_the_screen_keeps_its_rows() {
        let rows = [
            "root  1  0.0 /sbin/launchd --flag",
            "root  2  0.0 /usr/libexec/logd -x",
        ];
        assert_eq!(copy(34, &rows), rows.join("\n"));
    }

    #[test]
    fn a_box_and_its_bars_are_dropped() {
        let rows = ["│ hello there │", "│ second line │"];
        assert_eq!(copy(80, &rows), "hello there\nsecond line");
    }

    #[test]
    fn one_line_is_trimmed_only() {
        assert_eq!(copy(80, &["  ls -la /tmp  "]), "ls -la /tmp");
        assert_eq!(copy(80, &[""]), "");
    }

    #[test]
    fn a_selection_starting_mid_row_keeps_the_rows_after_it_aligned() {
        let lines = vec![
            CopyLine {
                text: "if x {".to_owned(),
                start: 10,
                lead: "    x = 1;".to_owned(),
                end: 16,
            },
            CopyLine {
                text: "      y();".to_owned(),
                start: 0,
                lead: String::new(),
                end: 10,
            },
            CopyLine {
                text: "    }".to_owned(),
                start: 0,
                lead: String::new(),
                end: 5,
            },
        ];
        assert_eq!(clean_copy(&lines, 80), "if x {\n  y();\n}");
    }

    #[test]
    fn code_selected_from_its_first_column_keeps_its_indentation() {
        assert_eq!(
            copy(80, &["if ready:", "    run()"]),
            "if ready:\n    run()"
        );
    }

    #[test]
    fn indented_code_selected_from_the_margin_keeps_its_first_indentation() {
        assert_eq!(copy(80, &["    run()", "done()"]), "    run()\ndone()");
    }

    #[test]
    fn full_width_records_stay_apart_but_a_cut_login_url_joins() {
        let rows = ["id=1 name=alpha path=/a/b", "id=2 name=beta path=/c/dd"];
        assert_eq!(copy(25, &rows), rows.join("\n"));
        let rows = ["https://login.example/auth?c", "ode=123"];
        assert_eq!(copy(28, &rows), "https://login.example/auth?code=123");
    }

    #[test]
    fn a_url_selected_after_the_words_before_it_still_joins() {
        // A double click on the URL: the selection starts at column 7.
        let lines = vec![
            CopyLine {
                text: "https://a.io/very/long/pa".to_owned(),
                start: 7,
                lead: "\u{23fa} open ".to_owned(),
                end: 32,
            },
            CopyLine {
                text: "  th/x".to_owned(),
                start: 0,
                lead: String::new(),
                end: 6,
            },
        ];
        assert_eq!(clean_copy(&lines, 34), "https://a.io/very/long/path/x");
    }

    #[test]
    fn the_edge_is_found_only_near_the_screens_right_side() {
        assert_eq!(wrap_edge([10, 76, 40], 80), Some(76));
        assert_eq!(wrap_edge([10, 60], 80), None);
        assert_eq!(wrap_edge([], 80), None);
    }

    #[test]
    fn tokens_are_told_from_words() {
        for token in ["https://a.io", "src/main.rs", "--flag", "v1", "a_b", "x=y"] {
            assert!(is_token_like(token), "{token}");
        }
        for word in ["the", "build", "Hello"] {
            assert!(!is_token_like(word), "{word}");
        }
    }
}
