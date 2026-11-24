//! A screen, written as the escape sequences that draw it again.
//!
//! A client that fell off the output ring cannot catch up from bytes. The host
//! instead describes its current screen: reset, the modes the emulator keeps,
//! every cell with its colours, the cursor and the title. Fed to a fresh
//! emulator this reproduces the visible screen and these modes: alternate
//! screen, application cursor keys, bracketed paste, cursor shape and
//! visibility, title.
//!
//! Mouse reporting (click, drag, motion, SGR and UTF-8 encodings) and focus
//! reporting are carried too, since the emulator tracks them.
//!
//! Scrollback goes too, as far as it fits a frame: the newest history lines are
//! written first and pushed off the top, so they land in the client's history
//! and the client can scroll back into what ran before it attached. A client
//! of any version takes them, since they are ordinary output.
//!
//! What it does not carry, because the emulator does not expose it: the scroll
//! region, tab stops, the origin and autowrap modes, character sets. A
//! full-screen program repaints these on its next frame.

use std::fmt::Write;

use connecto_core::{Cell, CellFlags, CursorShape, Rgb, TermSnapshot};

fn sgr(out: &mut String, fg: Rgb, bg: Rgb, flags: CellFlags, default_bg: Rgb) {
    out.push_str("\x1b[0");
    if flags.contains(CellFlags::BOLD) {
        out.push_str(";1");
    }
    if flags.contains(CellFlags::ITALIC) {
        out.push_str(";3");
    }
    if flags.contains(CellFlags::UNDERLINE) {
        out.push_str(";4");
    }
    if flags.contains(CellFlags::STRIKEOUT) {
        out.push_str(";9");
    }
    let _ = write!(out, ";38;2;{};{};{}", fg.r, fg.g, fg.b);
    if bg != default_bg {
        let _ = write!(out, ";48;2;{};{};{}", bg.r, bg.g, bg.b);
    }
    out.push('m');
}

/// The character a cell is redrawn with. A tab is kept in the cell it was
/// written at, with the cells it skipped after it; written again it would
/// skip them a second time.
fn glyph(cell: &Cell) -> char {
    if cell.ch == '\t' { ' ' } else { cell.ch }
}

/// One history row as output: its cells, then a line break unless the
/// terminal soft-wrapped it (`last` is never), so the wrap is kept. Trailing
/// blanks of an unwrapped row are left out, but a row `continued` from a
/// wrapped one keeps a cell: a bare break would undo the wrap before it, and
/// the row with it.
fn history_row(cells: &[Cell], default_bg: Rgb, last: bool, continued: bool) -> String {
    let wrapped = !last
        && cells
            .last()
            .is_some_and(|c| c.flags.contains(CellFlags::WRAPPED));
    let skipped = |c: &Cell| {
        c.flags.contains(CellFlags::WIDE_SPACER) || c.flags.contains(CellFlags::LEADING_WIDE_SPACER)
    };
    let blank = |c: &Cell| {
        (glyph(c) == ' ' && c.bg == default_bg && c.flags == CellFlags::default()) || skipped(c)
    };
    let used = if wrapped {
        cells.len()
    } else {
        let used = cells.iter().rposition(|c| !blank(c)).map_or(0, |at| at + 1);
        if continued { used.max(1) } else { used }
    };
    let mut out = String::new();
    let mut current: Option<(Rgb, Rgb, CellFlags)> = None;
    let mut coloured = false;
    for cell in cells.iter().take(used) {
        // A leading spacer is left out too: the wide glyph after it wraps
        // again on its own, as it did on the host.
        if skipped(cell) {
            continue;
        }
        let attrs = (cell.fg, cell.bg, cell.flags);
        if current != Some(attrs) {
            sgr(&mut out, cell.fg, cell.bg, cell.flags, default_bg);
            current = Some(attrs);
        }
        coloured |= cell.bg != default_bg;
        out.push(glyph(cell));
    }
    // A row the wrap brought in took the background of the glyph that wrapped
    // it: the blanks left out after the text are put back to the default.
    if continued && coloured && used < cells.len() {
        out.push_str("\x1b[0m\x1b[K");
    }
    if !wrapped {
        // Reset first: a line a break brings in takes the current background.
        if current.is_some() {
            out.push_str("\x1b[0m");
        }
        out.push_str("\r\n");
    }
    out
}

/// The newest rows of `history` whose output fits `budget` bytes, oldest
/// first, then the line feeds that push them all off the top of a screen of
/// `rows` rows into its history, leaving the cursor home on a blank screen.
/// Empty when no row fits.
fn history_text(history: &TermSnapshot, rows: u16, budget: usize) -> String {
    let push = "\n".repeat(usize::from(rows.saturating_sub(1)));
    let Some(mut left) = budget.checked_sub(push.len() + 8) else {
        return String::new();
    };
    let mut kept = Vec::new();
    let wrapped = |row: u16| {
        history
            .row(row)
            .last()
            .is_some_and(|c| c.flags.contains(CellFlags::WRAPPED))
    };
    for row in (0..history.rows).rev() {
        let last = row + 1 == history.rows;
        let continued = row > 0 && wrapped(row - 1);
        let line = history_row(history.row(row), history.default_bg, last, continued);
        let Some(rest) = left.checked_sub(line.len()) else {
            break;
        };
        left = rest;
        kept.push(line);
    }
    if kept.is_empty() {
        return String::new();
    }
    let mut out = String::with_capacity(budget - left);
    out.push_str("\x1b[H");
    for line in kept.iter().rev() {
        out.push_str(line);
    }
    out.push_str(&push);
    out.push_str("\x1b[H");
    out
}

/// The sequences that redraw `snapshot` on a blank terminal of the same size.
#[must_use]
pub fn repaint(snapshot: &TermSnapshot) -> Vec<u8> {
    repaint_with_history(snapshot, None, usize::MAX)
}

/// As [`repaint`], with the newest lines of `history` (rows of the same width,
/// oldest first) put into the terminal's scrollback first, as many as keep the
/// whole under `max_len` bytes. History is left out when its width is not the
/// screen's. A screen that alone goes over `max_len` is drawn as plain text,
/// and as many of its bottom rows as fit, so it still fits a frame. The reset,
/// modes, title and cursor are always written, so `max_len` is kept only when
/// it leaves room for them: a title is at most `MAX_TITLE_CHARS` characters,
/// and the wire's repaint limit has room to spare.
#[must_use]
pub fn repaint_with_history(
    snapshot: &TermSnapshot,
    history: Option<&TermSnapshot>,
    max_len: usize,
) -> Vec<u8> {
    let screen = repaint_screen(snapshot, Paint::Coloured(max_len))
        .unwrap_or_else(|| repaint_screen(snapshot, Paint::Plain(max_len)).unwrap_or_default());
    let Some(history) = history.filter(|h| h.rows > 0 && h.cols == snapshot.cols) else {
        return screen;
    };
    const RESET: &str = "\x1bc\x1b[3J";
    let budget = max_len.saturating_sub(screen.len());
    let lines = history_text(history, snapshot.rows, budget);
    if lines.is_empty() {
        return screen;
    }
    // The screen's own reset leads it; the history goes right after.
    let mut out = Vec::with_capacity(screen.len() + lines.len());
    out.extend_from_slice(RESET.as_bytes());
    out.extend_from_slice(lines.as_bytes());
    out.extend_from_slice(screen.get(RESET.len()..).unwrap_or_default());
    out
}

/// Whether screen row `row` soft-wraps into the next one on the screen.
fn wraps_on(snapshot: &TermSnapshot, row: u16) -> bool {
    row + 1 < snapshot.rows
        && snapshot
            .row(row)
            .last()
            .is_some_and(|c| c.flags.contains(CellFlags::WRAPPED))
}

/// One screen row, in colour. Trailing blanks are left out (the reset cleared
/// them): written, they would be cells with content to the client's screen,
/// which a later resize then reflows unlike the host's. For the same reason a
/// row the terminal soft-wrapped is wrapped again: wrapping is turned on for
/// its last glyph, so the first glyph of the row it `continued` into (which
/// is then not placed, but written where the wrap leaves it) wraps it, and
/// turned off again after that glyph.
fn coloured_row(snapshot: &TermSnapshot, row: u16, continued: bool) -> String {
    let mut out = String::new();
    if !continued {
        let _ = write!(out, "\x1b[{};1H", u32::from(row) + 1);
    }
    let mut current: Option<(Rgb, Rgb, CellFlags)> = None;
    let cells = snapshot.row(row);
    let wrapped = wraps_on(snapshot, row);
    let spacer = |c: &Cell| {
        c.flags.contains(CellFlags::WIDE_SPACER) || c.flags.contains(CellFlags::LEADING_WIDE_SPACER)
    };
    let blank = |c: &Cell| {
        (glyph(c) == ' ' && c.bg == snapshot.default_bg && c.flags == CellFlags::default())
            || spacer(c)
    };
    let used = if wrapped {
        cells.len()
    } else {
        let used = cells.iter().rposition(|c| !blank(c)).map_or(0, |at| at + 1);
        // A continued row takes a glyph, or nothing wraps the row before.
        if continued { used.max(1) } else { used }
    };
    // A leading spacer (a wide glyph that did not fit) is not written: the
    // wide glyph after the wrap makes it again.
    let last_glyph = if wrapped {
        cells.iter().rposition(|c| !spacer(c))
    } else {
        None
    };
    let mut first = continued;
    for (at, cell) in cells.iter().enumerate().take(used) {
        if spacer(cell) {
            continue;
        }
        if Some(at) == last_glyph {
            out.push_str("\x1b[?7h");
        }
        let attrs = (cell.fg, cell.bg, cell.flags);
        if current != Some(attrs) {
            sgr(&mut out, cell.fg, cell.bg, cell.flags, snapshot.default_bg);
            current = Some(attrs);
        }
        out.push(glyph(cell));
        // Wrapping stays on when this glyph also ends a wrapped row: the
        // next row's first glyph must wrap again.
        if first {
            first = false;
            if Some(at) != last_glyph {
                out.push_str("\x1b[?7l");
            }
        }
    }
    out
}

/// One screen row, placed, as plain text without its trailing blanks; empty
/// for a blank row, which the reset already cleared. Concealed text (drawn in
/// its background colour) is written as blanks, so it stays hidden.
fn plain_row(snapshot: &TermSnapshot, row: u16) -> String {
    let cells = snapshot.row(row);
    let shown = |c: &Cell| {
        if c.fg == c.bg { ' ' } else { glyph(c) }
    };
    let used = cells
        .iter()
        .rposition(|c| !c.flags.contains(CellFlags::WIDE_SPACER) && shown(c) != ' ')
        .map_or(0, |at| at + 1);
    if used == 0 {
        return String::new();
    }
    let mut out = String::new();
    let _ = write!(out, "\x1b[{};1H", u32::from(row) + 1);
    for cell in cells.iter().take(used) {
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            continue;
        }
        match shown(cell) {
            // A hidden wide glyph still takes its two columns.
            ' ' if cell.flags.contains(CellFlags::WIDE) => out.push_str("  "),
            ch => out.push(ch),
        }
    }
    out
}

/// The most the end of a screen repaint (reset, cursor) takes.
const SUFFIX_MAX: usize = 64;

/// How a screen is drawn, and in at most how many bytes.
#[derive(Clone, Copy)]
enum Paint {
    /// In colour; given up on (`None`) as soon as it goes over.
    Coloured(usize),
    /// As plain text, leaving out rows from the top as needed.
    Plain(usize),
}

/// The screen as `paint` says, or `None` when in colour it does not fit.
fn repaint_screen(snapshot: &TermSnapshot, paint: Paint) -> Option<Vec<u8>> {
    let mut out =
        String::with_capacity(usize::from(snapshot.cols) * usize::from(snapshot.rows) * 4);
    // Full reset, then clear scrollback.
    out.push_str("\x1bc\x1b[3J");
    if snapshot.modes.alt_screen {
        out.push_str("\x1b[?1049h");
    }
    if snapshot.modes.app_cursor {
        out.push_str("\x1b[?1h");
    }
    if snapshot.modes.bracketed_paste {
        out.push_str("\x1b[?2004h");
    }
    // Mouse and focus reporting: a program that asked for them keeps getting
    // them from a client that joined later. The tracking level is set first,
    // and the encoding after it.
    let m = &snapshot.modes;
    if m.mouse_click {
        out.push_str("\x1b[?1000h");
    }
    if m.mouse_drag {
        out.push_str("\x1b[?1002h");
    }
    if m.mouse_motion {
        out.push_str("\x1b[?1003h");
    }
    if m.mouse_utf8 {
        out.push_str("\x1b[?1005h");
    }
    if m.mouse_sgr {
        out.push_str("\x1b[?1006h");
    }
    if m.focus_reporting {
        out.push_str("\x1b[?1004h");
    }
    if let Some(title) = &snapshot.title {
        let _ = write!(out, "\x1b]2;{title}\x07");
    }
    // Wrapping is off while painting, so a full last column cannot scroll.
    out.push_str("\x1b[?7l\x1b[H");
    match paint {
        Paint::Coloured(budget) => {
            for row in 0..snapshot.rows {
                let continued = row > 0 && wraps_on(snapshot, row - 1);
                out.push_str(&coloured_row(snapshot, row, continued));
                if out.len() > budget {
                    return None;
                }
            }
        }
        Paint::Plain(budget) => {
            // Each row is placed on its own, so rows can be dropped from the
            // top until the rest fits: the bottom, where the cursor and the
            // newest output usually are, is kept.
            let mut left = budget.saturating_sub(out.len() + SUFFIX_MAX);
            let mut kept = Vec::new();
            for row in (0..snapshot.rows).rev() {
                let line = plain_row(snapshot, row);
                let Some(rest) = left.checked_sub(line.len()) else {
                    break;
                };
                left = rest;
                kept.push(line);
            }
            for line in kept.iter().rev() {
                out.push_str(line);
            }
        }
    }
    out.push_str("\x1b[0m\x1b[?7h");
    match snapshot.cursor {
        Some(cursor) => {
            let _ = write!(
                out,
                "\x1b[{};{}H\x1b[{} q\x1b[?25h",
                u32::from(cursor.row) + 1,
                u32::from(cursor.col) + 1,
                match cursor.shape {
                    CursorShape::Block => 2,
                    CursorShape::Underline => 4,
                    CursorShape::Beam => 6,
                }
            );
        }
        None => out.push_str("\x1b[?25l"),
    }
    if matches!(paint, Paint::Coloured(budget) if out.len() > budget) {
        return None;
    }
    Some(out.into_bytes())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::panic)]
mod tests {
    use std::sync::Arc;

    use connecto_core::{GridSize, SnapshotOutcome};
    use connecto_term::Emulator;

    use super::*;

    fn emulator(cols: u16, rows: u16) -> Emulator {
        Emulator::new(GridSize { cols, rows }, Arc::new(|_| {}), Arc::new(|| {}))
    }

    fn snap(e: &Emulator) -> TermSnapshot {
        let mut s = TermSnapshot::default();
        assert_eq!(e.poll_snapshot(&mut s), SnapshotOutcome::Updated);
        s
    }

    fn assert_same_screen(a: &TermSnapshot, b: &TermSnapshot) {
        assert_eq!((a.cols, a.rows), (b.cols, b.rows));
        for row in 0..a.rows {
            for (col, (x, y)) in a.row(row).iter().zip(b.row(row)).enumerate() {
                assert_eq!(x, y, "cell at row {row} col {col}");
            }
        }
        assert_eq!(a.cursor, b.cursor);
        assert_eq!(a.modes, b.modes);
        assert_eq!(a.title, b.title);
    }

    fn history(e: &Emulator) -> TermSnapshot {
        let (mut screen, mut h) = (TermSnapshot::default(), TermSnapshot::default());
        assert!(e.try_capture(usize::MAX, false, &mut screen, &mut h));
        h
    }

    /// Every row of `e`, history and screen, with its wrap flag, as text.
    fn rows(e: &Emulator) -> Vec<(String, bool)> {
        let mut screen = TermSnapshot::default();
        let mut h = TermSnapshot::default();
        assert!(e.try_capture(usize::MAX, false, &mut screen, &mut h));
        let of = |s: &TermSnapshot, row: u16| {
            let cells = s.row(row);
            let text: String = cells
                .iter()
                .filter(|c| !c.flags.contains(CellFlags::WIDE_SPACER))
                .map(glyph)
                .collect();
            let wrapped = cells
                .last()
                .is_some_and(|c| c.flags.contains(CellFlags::WRAPPED));
            (text.trim_end().to_owned(), wrapped)
        };
        let mut out: Vec<_> = (0..h.rows).map(|row| of(&h, row)).collect();
        out.extend((0..screen.rows).map(|row| of(&screen, row)));
        out
    }

    /// `input` on a source terminal, repainted with its history into a fresh
    /// one of the same size, as a peer's is.
    fn round_trip_with_history(input: &[u8], max_len: usize) -> (Emulator, Emulator) {
        let a = emulator(40, 10);
        a.feed(input);
        let b = emulator(40, 10);
        b.feed(&repaint_with_history(
            &snap(&a),
            Some(&history(&a)),
            max_len,
        ));
        (a, b)
    }

    /// A repainted screen reflows on a resize as the host's does: its soft
    /// wraps are wrapped again (a wide glyph at the margin too), and blanks
    /// after the text stay empty cells, not written ones.
    #[test]
    fn a_repainted_screen_reflows_like_the_source_on_a_resize() {
        let cases: [&[u8]; 5] = [
            "abcdefghi世界xyz\r\n".as_bytes(),
            "abcdefgh世界xyz\r\n".as_bytes(),
            b"\x1b[41mredredredredred\x1b[0m\r\nok",
            b"01234567890123456789abc\r\n",
            b"x\r\n\x1b[44m          \x1b[0mtail\r\n",
        ];
        for input in cases {
            for (cols, rows) in [(10, 6), (16, 6), (7, 6), (10, 3)] {
                let a = emulator(10, 6);
                a.feed(input);
                let b = emulator(10, 6);
                b.feed(&repaint(&snap(&a)));
                assert_same_screen(&snap(&a), &snap(&b));
                let size = GridSize { cols, rows };
                assert_eq!(a.try_resize(size), b.try_resize(size));
                assert_eq!(rows_of(&a), rows_of(&b), "{input:?} at {cols}x{rows}");
            }
        }
    }

    /// A wide glyph that is both the first glyph of a continued row and the
    /// last of a wrapped one, on a terminal two columns wide.
    #[test]
    fn a_two_column_screen_of_wide_glyphs_keeps_every_glyph() {
        let a = emulator(2, 3);
        a.feed("世界語".as_bytes());
        let b = emulator(2, 3);
        b.feed(&repaint(&snap(&a)));
        assert_same_screen(&snap(&a), &snap(&b));
        for (cols, rows) in [(6, 3), (4, 3), (2, 3)] {
            let size = GridSize { cols, rows };
            assert_eq!(a.try_resize(size), b.try_resize(size));
            assert_eq!(rows_of(&a), rows_of(&b), "at {cols}x{rows}");
        }
    }

    fn rows_of(e: &Emulator) -> Vec<Vec<Cell>> {
        let s = snap(e);
        (0..s.rows).map(|row| s.row(row).to_vec()).collect()
    }

    #[test]
    fn history_lands_in_the_scrollback_and_the_screen_is_unchanged() {
        let mut input = Vec::new();
        for i in 0..100 {
            input.extend(format!("\x1b[3{}mline {i}\x1b[0m\r\n", i % 7).into_bytes());
        }
        input.extend(b"prompt> ");
        let (a, b) = round_trip_with_history(&input, usize::MAX);
        assert_eq!(b.text_all(), a.text_all());
        assert_same_screen(&snap(&a), &snap(&b));
        let (ha, hb) = (history(&a), history(&b));
        assert_eq!((ha.rows, ha.cols), (hb.rows, hb.cols));
        assert_eq!(ha.cells, hb.cells, "colours in history are kept");
    }

    #[test]
    fn a_soft_wrapped_line_in_history_stays_one_line() {
        let mut input = b"x".repeat(95);
        input.extend(b"\r\n");
        for i in 0..20 {
            input.extend(format!("after {i}\r\n").into_bytes());
        }
        let (a, b) = round_trip_with_history(&input, usize::MAX);
        assert_eq!(b.text_all(), a.text_all());
        assert!(b.text_all().0.contains(&"x".repeat(95)));
    }

    #[test]
    fn only_the_newest_history_that_fits_is_sent() {
        let mut input = Vec::new();
        for i in 0..500 {
            input.extend(format!("line {i}\r\n").into_bytes());
        }
        let a = emulator(40, 10);
        a.feed(&input);
        let screen = repaint(&snap(&a));
        let limit = screen.len() + 1000;
        let bytes = repaint_with_history(&snap(&a), Some(&history(&a)), limit);
        assert!(bytes.len() <= limit, "{} > {limit}", bytes.len());
        let b = emulator(40, 10);
        b.feed(&bytes);
        let (text, _) = b.text_all();
        assert!(text.contains("line 480\n"), "{text}");
        assert!(!text.contains("line 1\n"), "the oldest lines are left out");
        assert_same_screen(&snap(&a), &snap(&b));
        // No room at all: the screen alone, as before.
        assert_eq!(
            repaint_with_history(&snap(&a), Some(&history(&a)), screen.len()),
            screen
        );
    }

    #[test]
    fn tabs_wraps_blank_continuations_and_wide_glyphs_at_the_margin_are_kept() {
        let mut input = Vec::new();
        input.extend(b"a\tb\tc\r\n");
        // A wrapped line whose continuation row is all blanks.
        input.extend(b"y".repeat(40));
        input.extend(b" ".repeat(40));
        input.extend(b"z\r\n");
        // A wide glyph that does not fit the last column wraps.
        input.extend(b"w".repeat(39));
        input.extend("界 after\r\n".as_bytes());
        input.extend(b"\r\n".repeat(12));
        let (a, b) = round_trip_with_history(&input, usize::MAX);
        assert_eq!(rows(&b), rows(&a));
        // A tab comes back as the blank it showed as.
        assert_eq!(b.text_all().0, a.text_all().0.replace('\t', " "));
    }

    #[test]
    fn a_screen_too_big_with_colours_is_drawn_without_them() {
        let a = emulator(40, 10);
        for i in 0..400_u32 {
            a.feed(format!("\x1b[38;2;{i};1;2;48;2;3;{i};4mx").as_bytes());
        }
        let coloured = repaint(&snap(&a));
        let limit = coloured.len() - 1;
        let bytes = repaint_with_history(&snap(&a), Some(&history(&a)), limit);
        assert!(bytes.len() <= limit);
        let b = emulator(40, 10);
        b.feed(&bytes);
        let (sa, sb) = (snap(&a), snap(&b));
        for row in 0..sa.rows {
            let chars = |s: &TermSnapshot| s.row(row).iter().map(|c| c.ch).collect::<String>();
            assert_eq!(chars(&sb), chars(&sa), "row {row}");
        }
        assert_eq!(sb.cursor, sa.cursor);
    }

    #[test]
    fn a_wrapped_continuation_keeps_its_default_background_after_the_text() {
        // On the source the wrap is at the top, so the row it brings in is
        // blank; the replay wraps it at the bottom margin, where a new row
        // takes the current background.
        let mut input = b"line\r\n".repeat(20);
        input.extend(b"\x1b[2;1H");
        input.extend(b"y".repeat(40));
        input.extend(b"\x1b[41mR\x1b[0m\x1b[10;1H");
        input.extend(b"\r\n".repeat(30));
        let (a, b) = round_trip_with_history(&input, usize::MAX);
        let (ha, hb) = (history(&a), history(&b));
        assert_eq!(rows(&b), rows(&a));
        let bgs = |h: &TermSnapshot| h.cells.iter().map(|c| c.bg).collect::<Vec<_>>();
        assert_eq!(bgs(&hb), bgs(&ha));
    }

    const LIMIT: usize = crate::wire::MAX_FRAME - 64;

    #[test]
    fn a_large_screen_fits_a_frame_blank_or_full() {
        let blank = emulator(600, 500);
        let bytes = repaint_with_history(&snap(&blank), Some(&history(&blank)), LIMIT);
        assert!(bytes.len() <= LIMIT, "{}", bytes.len());

        let full = emulator(600, 500);
        for row in 0..500 {
            full.feed(format!("\x1b[{};1H", row + 1).as_bytes());
            full.feed(&b"x".repeat(599));
            full.feed(format!("{}", row % 10).as_bytes());
        }
        let source = snap(&full);
        let bytes = repaint_with_history(&source, Some(&history(&full)), LIMIT);
        assert!(bytes.len() <= LIMIT, "{}", bytes.len());
        let b = emulator(600, 500);
        b.feed(&bytes);
        let chars = |s: &TermSnapshot, row| s.row(row).iter().map(|c| c.ch).collect::<String>();
        assert_eq!(chars(&snap(&b), 499), chars(&source, 499));
    }

    #[test]
    fn concealed_text_stays_hidden_on_a_screen_drawn_plain() {
        let a = emulator(40, 10);
        a.feed(b"user: me \x1b[8msecret\x1b[0m\r\n");
        for i in 0..300_u32 {
            a.feed(format!("\x1b[38;2;{i};1;2;48;2;3;{i};4mx").as_bytes());
        }
        let coloured = repaint(&snap(&a));
        let bytes = repaint_with_history(&snap(&a), None, coloured.len() - 1);
        let b = emulator(40, 10);
        b.feed(&bytes);
        let (text, _) = b.text_all();
        assert!(text.contains("user: me"), "{text}");
        assert!(!text.contains("secret"), "{text}");
    }

    #[test]
    fn a_concealed_wide_glyph_keeps_its_two_columns_when_drawn_plain() {
        let a = emulator(40, 10);
        a.feed("\x1b[8m界\x1b[0mX\r\n".as_bytes());
        for i in 0..300_u32 {
            a.feed(format!("\x1b[38;2;{i};1;2;48;2;3;{i};4mx").as_bytes());
        }
        let coloured = repaint(&snap(&a));
        let b = emulator(40, 10);
        b.feed(&repaint_with_history(&snap(&a), None, coloured.len() - 1));
        let shown = snap(&b);
        assert_eq!(shown.row(0)[2].ch, 'X');
        assert_ne!(shown.row(0)[0].ch, '界');
    }

    #[test]
    fn a_capture_waits_for_a_held_synchronized_update() {
        let a = emulator(40, 10);
        a.feed(b"\x1b[?2026hhello");
        let (mut screen, mut h) = (TermSnapshot::default(), TermSnapshot::default());
        assert!(!a.try_capture(usize::MAX, false, &mut screen, &mut h));
        // Asked to, a capture ends the update itself.
        let b = emulator(40, 10);
        b.feed(b"\x1b[?2026hthere");
        assert!(b.try_capture(usize::MAX, true, &mut screen, &mut h));
        assert!(
            screen
                .row(0)
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .starts_with("there")
        );
        a.feed(b"\x1b[?2026l");
        assert!(a.try_capture(usize::MAX, false, &mut screen, &mut h));
        assert!(
            screen
                .row(0)
                .iter()
                .map(|c| c.ch)
                .collect::<String>()
                .starts_with("hello")
        );
    }

    #[test]
    fn history_of_another_width_is_left_out() {
        let a = emulator(40, 10);
        a.feed(&b"line\r\n".repeat(30));
        let mut other = history(&a);
        other.cols = 30;
        assert_eq!(
            repaint_with_history(&snap(&a), Some(&other), usize::MAX),
            repaint(&snap(&a))
        );
    }

    fn round_trip(input: &[u8]) -> (TermSnapshot, TermSnapshot) {
        let a = emulator(40, 10);
        a.feed(input);
        let sa = snap(&a);
        let b = emulator(40, 10);
        // Junk on the receiving screen must be wiped by the repaint.
        b.feed(b"\x1b[41mstale stale stale\x1b[0m\x1b[?1h");
        b.feed(&repaint(&sa));
        (sa, snap(&b))
    }

    #[test]
    fn colours_attributes_and_cursor_are_reproduced() {
        let (a, b) = round_trip(
            b"plain \x1b[1;31mbold red\x1b[0m \x1b[3;4;32mital\x1b[0m\r\n\x1b[44;37mblue bg\x1b[0m\r\n\x1b[38;2;10;20;30mtruecolor\x1b[0m\x1b[3;7H",
        );
        assert_same_screen(&a, &b);
    }

    #[test]
    fn wide_characters_and_a_full_width_line_survive() {
        let long = "x".repeat(40);
        let (a, b) = round_trip(format!("\u{4e2d}\u{6587}ok\r\n{long}\r\nafter").as_bytes());
        assert_same_screen(&a, &b);
    }

    #[test]
    fn the_modes_the_emulator_keeps_are_reproduced() {
        let (a, b) = round_trip(b"\x1b[?1049h\x1b[?1h\x1b[?2004h\x1b]2;my title\x07hello\x1b[?25l");
        assert!(a.modes.alt_screen && a.modes.app_cursor && a.modes.bracketed_paste);
        assert_same_screen(&a, &b);
        assert_eq!(b.title.as_deref(), Some("my title"));
        assert!(b.cursor.is_none());
    }

    #[test]
    fn modes_off_on_the_source_are_off_after_the_repaint() {
        // The receiver had application cursor keys on; the source did not.
        let (a, b) = round_trip(b"just text");
        assert!(!a.modes.app_cursor);
        assert_same_screen(&a, &b);
    }

    #[test]
    fn mouse_and_focus_reporting_are_reproduced() {
        let (a, b) = round_trip(b"\x1b[?1000h\x1b[?1002h\x1b[?1006h\x1b[?1004hx");
        // The tracking levels replace each other: the last one asked for stays.
        assert!(a.modes.mouse_drag && !a.modes.mouse_click && a.modes.mouse_sgr);
        assert!(a.modes.focus_reporting && !a.modes.mouse_motion);
        assert_same_screen(&a, &b);
        // And they are off after a repaint of a screen that had them off.
        let (a, b) = round_trip(b"plain");
        assert!(!a.modes.mouse_click);
        assert_same_screen(&a, &b);
    }

    #[test]
    fn a_cursor_shape_is_reproduced() {
        let (a, b) = round_trip(b"x\x1b[6 q");
        assert_eq!(a.cursor.unwrap().shape, connecto_core::CursorShape::Beam);
        assert_same_screen(&a, &b);
    }
}
