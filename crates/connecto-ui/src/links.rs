//! Web links in the terminal text. Nothing is scanned per frame: the one
//! logical line (a run of wrapped rows) under the pointer is read when the
//! pointer is over the screen, and the click opens what that read found.

use connecto_core::{CellFlags, TermSnapshot};

use crate::selection::CellPos;

/// Rows read above and below the pointer's row to follow a wrapped line.
const WRAP_REACH: u16 = 12;
/// Longest URL opened; anything longer is not a link a person typed or read.
const MAX_URL: usize = 2048;

/// A link found under the pointer: the URL and the cells it covers, as
/// `(row, start, end)` with `end` exclusive.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Link {
    pub(crate) url: String,
    pub(crate) spans: Vec<(u16, u16, u16)>,
}

/// The http(s) link covering `at`, if any.
pub(crate) fn link_at(snapshot: &TermSnapshot, at: CellPos) -> Option<Link> {
    if at.0 >= snapshot.rows || at.1 >= snapshot.cols {
        return None;
    }
    // A repainted remote screen does not carry WRAPPED, so there a URL over two
    // rows is clickable only as its first-row fragment (accepted limitation).
    // Only a row the terminal soft-wrapped goes on in the next one; a line
    // that happens to fill the row and then ends does not.
    let wrapped = |row: u16| {
        snapshot
            .row(row)
            .last()
            .is_some_and(|cell| cell.flags.contains(CellFlags::WRAPPED))
    };
    let mut first = at.0;
    while first > 0 && at.0 - first < WRAP_REACH && wrapped(first - 1) {
        first -= 1;
    }
    let mut last = at.0;
    while last + 1 < snapshot.rows && last - at.0 < WRAP_REACH && wrapped(last) {
        last += 1;
    }
    // The characters of the logical line with the cell each came from.
    let mut chars: Vec<char> = Vec::new();
    let mut cells: Vec<CellPos> = Vec::new();
    for row in first..=last {
        for (col, cell) in snapshot.row(row).iter().enumerate() {
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            chars.push(cell.ch);
            cells.push((row, col as u16));
        }
    }
    let hovered = cells.iter().position(|cell| *cell == at).or_else(|| {
        // The right half of a wide glyph belongs to the glyph before it.
        cells.iter().rposition(|cell| *cell < at)
    })?;
    let (start, end) = url_range(&chars, hovered)?;
    let url: String = chars[start..end].iter().collect();
    let url = openable(&url)?.to_owned();
    let mut spans: Vec<(u16, u16, u16)> = Vec::new();
    for &(row, col) in &cells[start..end] {
        match spans.last_mut() {
            Some(span) if span.0 == row => span.2 = col + 1,
            _ => spans.push((row, col, col + 1)),
        }
    }
    Some(Link { url, spans })
}

/// The `[start, end)` of the URL in `chars` that includes `at`.
fn url_range(chars: &[char], at: usize) -> Option<(usize, usize)> {
    let mut from = 0;
    while from <= at && from < chars.len() {
        let rest = &chars[from..];
        let scheme = [
            "https://".len() * usize::from(starts_with_ci(rest, "https://")),
            "http://".len() * usize::from(starts_with_ci(rest, "http://")),
        ]
        .into_iter()
        .max()
        .unwrap_or(0);
        if scheme == 0 {
            from += 1;
            continue;
        }
        let mut end = from + scheme;
        while end < chars.len() && !stops_url(chars[end]) {
            end += 1;
        }
        let end = from + trimmed_len(&chars[from..end]);
        if end > from + scheme && (from..end).contains(&at) {
            return Some((from, end));
        }
        from = end.max(from + 1);
    }
    None
}

fn starts_with_ci(chars: &[char], prefix: &str) -> bool {
    let mut it = chars.iter();
    prefix
        .chars()
        .all(|p| it.next().is_some_and(|c| c.eq_ignore_ascii_case(&p)))
}

/// Whitespace, controls and the characters that quote or bracket a URL from
/// outside end it.
fn stops_url(c: char) -> bool {
    c.is_whitespace() || c.is_control() || matches!(c, '<' | '>' | '"' | '`' | '\\' | '│' | '┃')
}

/// The length of `url` without the punctuation that belongs to the sentence
/// around it: dots, commas, quotes, and a closing bracket with no opener
/// inside the URL.
fn trimmed_len(url: &[char]) -> usize {
    let mut len = url.len();
    while len > 0 {
        let c = url[len - 1];
        let open = match c {
            ')' => '(',
            ']' => '[',
            '}' => '{',
            '.' | ',' | ';' | ':' | '!' | '?' | '\'' | '*' => {
                len -= 1;
                continue;
            }
            _ => break,
        };
        let body = &url[..len];
        let opened = body.iter().filter(|x| **x == open).count();
        let closed = body.iter().filter(|x| **x == c).count();
        if closed > opened {
            len -= 1;
        } else {
            break;
        }
    }
    len
}

/// The URL if it is safe to hand to the system opener: http or https only, a
/// host, no control characters or blanks, of sane length.
pub(crate) fn openable(url: &str) -> Option<&str> {
    let lower = url.get(..8).unwrap_or(url).to_ascii_lowercase();
    let after = if lower.starts_with("https://") {
        &url[8..]
    } else if lower.starts_with("http://") {
        &url[7..]
    } else {
        return None;
    };
    let host = after.split(['/', '?', '#']).next().unwrap_or("");
    let ok = !host.is_empty()
        && !host.starts_with('@')
        && url.len() <= MAX_URL
        && !url.chars().any(|c| c.is_control() || c.is_whitespace());
    ok.then_some(url)
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use connecto_core::Cell;

    use super::*;

    /// A screen `cols` wide of the given lines, each padded with blanks. A
    /// line that fills its row soft-wraps into the next, as the terminal marks it.
    fn screen(cols: usize, lines: &[&str]) -> TermSnapshot {
        build(cols, lines, true)
    }

    /// The same text, but every row ended with a line break.
    fn hard(cols: usize, lines: &[&str]) -> TermSnapshot {
        build(cols, lines, false)
    }

    fn build(cols: usize, lines: &[&str], soft: bool) -> TermSnapshot {
        let cells = lines
            .iter()
            .flat_map(|line| {
                let mut row: Vec<Cell> = format!("{line:<cols$}")
                    .chars()
                    .map(|ch| Cell {
                        ch,
                        ..Cell::default()
                    })
                    .collect();
                if soft
                    && line.chars().count() == cols
                    && let Some(last) = row.last_mut()
                {
                    last.flags.insert(CellFlags::WRAPPED);
                }
                row
            })
            .collect();
        TermSnapshot {
            cols: cols as u16,
            rows: lines.len() as u16,
            cells,
            ..TermSnapshot::default()
        }
    }

    fn url(cols: usize, lines: &[&str], at: CellPos) -> Option<String> {
        link_at(&screen(cols, lines), at).map(|l| l.url)
    }

    #[test]
    fn a_plain_url_is_found_anywhere_along_it() {
        let lines = ["see https://example.com/a now"];
        assert_eq!(
            url(40, &lines, (0, 4)).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(
            url(40, &lines, (0, 24)).as_deref(),
            Some("https://example.com/a")
        );
        assert_eq!(url(40, &lines, (0, 2)), None);
        assert_eq!(url(40, &lines, (0, 26)), None);
    }

    #[test]
    fn trailing_punctuation_and_quotes_are_not_part_of_it() {
        for (text, want) in [
            ("go to https://a.io/x.", "https://a.io/x"),
            ("go to https://a.io/x, ok", "https://a.io/x"),
            ("(https://a.io/x)", "https://a.io/x"),
            ("[https://a.io/x]", "https://a.io/x"),
            ("'https://a.io/x'", "https://a.io/x"),
            ("\"https://a.io/x\"", "https://a.io/x"),
            ("<https://a.io/x>", "https://a.io/x"),
            ("https://a.io/x?!", "https://a.io/x"),
        ] {
            let at = (0, text.find("http").unwrap() as u16 + 3);
            assert_eq!(url(40, &[text], at).as_deref(), Some(want), "{text}");
        }
    }

    #[test]
    fn brackets_inside_the_url_stay() {
        let text = "https://en.wikipedia.org/wiki/Rust_(language)";
        assert_eq!(url(60, &[text], (0, 5)).as_deref(), Some(text));
        let text = "(see https://en.wikipedia.org/wiki/Rust_(language))";
        assert_eq!(
            url(60, &[text], (0, 10)).as_deref(),
            Some("https://en.wikipedia.org/wiki/Rust_(language)")
        );
    }

    #[test]
    fn a_url_wrapped_over_rows_is_one_link() {
        let lines = ["open https://exa", "mple.com/path/to", "/page then go"];
        let link = link_at(&screen(16, &lines), (1, 3)).unwrap();
        assert_eq!(link.url, "https://example.com/path/to/page");
        assert_eq!(link.spans, vec![(0, 5, 16), (1, 0, 16), (2, 0, 5)]);
        // The first row is reached from its own part of the link too.
        assert_eq!(link_at(&screen(16, &lines), (0, 6)).unwrap().url, link.url);
    }

    #[test]
    fn a_hard_newline_is_not_a_wrap_even_when_the_url_ends_in_the_last_column() {
        // `https://a.io` is exactly 12 wide: it fills the row and a newline follows.
        let lines = ["https://a.io", "next"];
        assert_eq!(
            link_at(&hard(12, &lines), (0, 3)).map(|l| l.url).as_deref(),
            Some("https://a.io")
        );
        assert_eq!(link_at(&hard(12, &lines), (1, 1)), None);
        // The same cells, soft-wrapped, are one word.
        assert_eq!(
            link_at(&screen(12, &lines), (0, 3))
                .map(|l| l.url)
                .as_deref(),
            Some("https://a.ionext")
        );
    }

    #[test]
    fn a_row_that_is_not_full_does_not_join_the_next() {
        let lines = ["https://a.io/x", "continued"];
        assert_eq!(url(20, &lines, (0, 3)).as_deref(), Some("https://a.io/x"));
    }

    #[test]
    fn other_schemes_are_not_links() {
        for text in [
            "file:///etc/passwd",
            "javascript:alert(1)",
            "ftp://a.io/x",
            "myapp://open?x=1",
        ] {
            assert_eq!(url(30, &[text], (0, 3)), None, "{text}");
        }
        assert_eq!(openable("file:///etc/passwd"), None);
        assert_eq!(openable("javascript:alert(1)"), None);
        assert_eq!(openable("https://"), None);
        assert_eq!(openable("http:///x"), None);
    }

    #[test]
    fn control_characters_and_blanks_are_refused() {
        assert_eq!(openable("https://a.io/\u{1b}[0m"), None);
        assert_eq!(openable("https://a.io/\nx"), None);
        assert_eq!(openable("https://a.io/ x"), None);
        assert_eq!(openable("HTTPS://A.io/x"), Some("HTTPS://A.io/x"));
        assert_eq!(openable("http://a.io"), Some("http://a.io"));
    }

    #[test]
    fn a_point_off_the_screen_finds_nothing() {
        assert_eq!(url(20, &["https://a.io/x"], (5, 0)), None);
        assert_eq!(url(20, &["https://a.io/x"], (0, 30)), None);
    }
}
