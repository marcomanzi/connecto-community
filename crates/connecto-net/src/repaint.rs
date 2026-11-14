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
//! What it does not carry, because the emulator does not expose it: scrollback,
//! the scroll region, tab stops, the origin and autowrap modes, character sets.
//! A full-screen program repaints these on its next frame.

use std::fmt::Write;

use connecto_core::{CellFlags, CursorShape, Rgb, TermSnapshot};

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

/// The sequences that redraw `snapshot` on a blank terminal of the same size.
#[must_use]
pub fn repaint(snapshot: &TermSnapshot) -> Vec<u8> {
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
    for row in 0..snapshot.rows {
        let _ = write!(out, "\x1b[{};1H", u32::from(row) + 1);
        let mut current: Option<(Rgb, Rgb, CellFlags)> = None;
        for cell in snapshot.row(row) {
            if cell.flags.contains(CellFlags::WIDE_SPACER) {
                continue;
            }
            let attrs = (cell.fg, cell.bg, cell.flags);
            if current != Some(attrs) {
                sgr(&mut out, cell.fg, cell.bg, cell.flags, snapshot.default_bg);
                current = Some(attrs);
            }
            out.push(cell.ch);
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
    out.into_bytes()
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
