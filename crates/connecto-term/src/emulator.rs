//! The shared screen state: written by the PTY reader, read by the UI.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{self, CursorShape as VteCursorShape};
use connecto_core::{
    Cell, CellFlags, Cursor, CursorShape, GridSize, InputModes, ResizeOutcome, SnapshotOutcome,
    TermSnapshot,
};

use crate::color;
use crate::policy::{PolicyListener, PolicyState};

/// Sends a reply to the child. Called with the emulator locked, on the reader
/// thread, so it must never block: a blocked reply would stop the reader while
/// the child waits for it to drain, deadlocking the session.
pub type Responder = Arc<dyn Fn(Vec<u8>) + Send + Sync>;

/// Callback that asks the UI for a new frame. Must not block.
type Wake = Arc<dyn Fn() + Send + Sync>;

const SCROLLBACK_LINES: usize = 10_000;

/// Smallest grid the emulator accepts.
const MIN_COLS: u16 = 2;
const MIN_ROWS: u16 = 1;

struct Dims {
    cols: usize,
    rows: usize,
}

impl Dimensions for Dims {
    fn total_lines(&self) -> usize {
        self.rows
    }

    fn screen_lines(&self) -> usize {
        self.rows
    }

    fn columns(&self) -> usize {
        self.cols
    }
}

impl From<GridSize> for Dims {
    fn from(size: GridSize) -> Self {
        Self {
            cols: usize::from(size.cols.max(MIN_COLS)),
            rows: usize::from(size.rows.max(MIN_ROWS)),
        }
    }
}

struct Screen {
    term: Term<PolicyListener>,
    parser: ansi::Processor,
}

impl Screen {
    /// Release a synchronized update whose end marker never arrived.
    ///
    /// The parser holds output between the begin and end markers, and relies on
    /// its caller to flush when the timeout passes. The PTY reader is blocked
    /// in `read` at exactly that moment, so the flush happens here instead.
    /// Returns the time left when an update is still being held back.
    fn flush_expired_sync(&mut self) -> Option<Duration> {
        let deadline = self.parser.sync_timeout().sync_timeout()?;
        let remaining = deadline.saturating_duration_since(Instant::now());
        if remaining.is_zero() {
            self.parser.stop_sync(&mut self.term);
            None
        } else {
            Some(remaining)
        }
    }
}

/// A snapshot the reader made for a UI that could not get the lock.
#[derive(Default)]
struct Published {
    snapshot: TermSnapshot,
    fresh: bool,
}

/// A terminal screen shared between one reader thread and the UI.
///
/// Under a flood the reader holds the screen lock most of the time, so the
/// UI's `try_lock` usually fails, and the pane would freeze even though the
/// frame rate stays high. The UI never waits. Instead it raises
/// `snapshot_wanted`, and the reader, which already has the lock, copies the
/// screen into `published` at the end of its current slice.
pub struct Emulator {
    screen: Mutex<Screen>,
    /// A synchronized update is being held back.
    sync_held: AtomicBool,
    state: Arc<PolicyState>,
    generation: AtomicU64,
    /// Set while a change has been announced to the UI and not yet copied, so
    /// that a flood wakes the UI once per frame rather than once per read.
    dirty: AtomicBool,
    snapshot_wanted: AtomicBool,
    published: Mutex<Published>,
    wake: Wake,
}

impl Emulator {
    /// Create an emulator. `respond` carries replies back to the child.
    #[must_use]
    pub fn new(size: GridSize, respond: Responder, wake: Wake) -> Self {
        let state = Arc::new(PolicyState::default());
        let config = Config {
            scrolling_history: SCROLLBACK_LINES,
            // TUIs detect this and fall back to legacy key sequences.
            kitty_keyboard: false,
            osc52: Osc52::Disabled,
            ..Config::default()
        };
        let listener = PolicyListener {
            responder: respond,
            state: Arc::clone(&state),
        };
        Self {
            sync_held: AtomicBool::new(false),
            screen: Mutex::new(Screen {
                term: Term::new(config, &Dims::from(size), listener),
                parser: ansi::Processor::new(),
            }),
            state,
            generation: AtomicU64::new(1),
            dirty: AtomicBool::new(false),
            snapshot_wanted: AtomicBool::new(false),
            published: Mutex::new(Published::default()),
            wake,
        }
    }

    /// Parse child output. Blocks only on a short UI copy; reader thread only.
    ///
    /// After an emulator panic the bytes are discarded, so the reader keeps
    /// draining and the child cannot wedge on a full PTY buffer.
    pub fn feed(&self, bytes: &[u8]) {
        let Ok(mut guard) = self.screen.lock() else {
            return;
        };
        let screen = &mut *guard;
        screen.parser.advance(&mut screen.term, bytes);
        self.sync_held.store(
            screen.parser.sync_timeout().sync_timeout().is_some(),
            Ordering::Release,
        );
        let generation = self.generation.fetch_add(1, Ordering::AcqRel) + 1;
        if self.snapshot_wanted.swap(false, Ordering::AcqRel) {
            self.publish(&screen.term, generation);
        }
        drop(guard);
        if !self.dirty.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    /// Release a synchronized update whose time is up, without a frame. A held
    /// update also holds any query in it, so a child waiting for that reply
    /// would otherwise wait for as long as the window is hidden. Never blocks,
    /// and takes no lock unless an update is held.
    pub fn flush_expired(&self) {
        if !self.sync_held.load(Ordering::Acquire) {
            return;
        }
        let Ok(Some(mut guard)) = self.try_screen() else {
            return;
        };
        if guard.flush_expired_sync().is_some() {
            return;
        }
        self.sync_held.store(false, Ordering::Release);
        drop(guard);
        self.generation.fetch_add(1, Ordering::AcqRel);
        if !self.dirty.swap(true, Ordering::AcqRel) {
            (self.wake)();
        }
    }

    /// Copy the screen for the UI. The caller holds the screen lock.
    fn publish(&self, term: &Term<PolicyListener>, generation: u64) {
        let mut published = self
            .published
            .lock()
            .unwrap_or_else(PoisonError::into_inner);
        self.fill_snapshot(term, &mut published.snapshot, generation);
        published.fresh = true;
        drop(published);
        // The UI takes this frame on its next poll; `feed` then wakes it.
        self.dirty.store(false, Ordering::Release);
    }

    fn fill_snapshot(&self, term: &Term<PolicyListener>, out: &mut TermSnapshot, generation: u64) {
        copy_screen(term, out);
        out.title.clone_from(
            &self
                .state
                .title
                .lock()
                .unwrap_or_else(PoisonError::into_inner),
        );
        out.generation = generation;
    }

    /// Take a snapshot the reader published, without waiting for it.
    fn take_published(&self, out: &mut TermSnapshot) -> bool {
        let Ok(mut published) = self.published.try_lock() else {
            return false;
        };
        if !published.fresh {
            return false;
        }
        published.fresh = false;
        // Swapping hands the old buffers back to the reader for reuse.
        std::mem::swap(out, &mut published.snapshot);
        out.repoll_after = None;
        true
    }

    /// Take the lock without waiting. `Err` means the emulator has failed.
    fn try_screen(&self) -> Result<Option<MutexGuard<'_, Screen>>, ()> {
        match self.screen.try_lock() {
            Ok(guard) => Ok(Some(guard)),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Poisoned(_)) => Err(()),
        }
    }

    /// Copy the screen into `out` if it changed. Never blocks.
    pub fn poll_snapshot(&self, out: &mut TermSnapshot) -> SnapshotOutcome {
        let idle =
            out.repoll_after.is_none() && self.generation.load(Ordering::Acquire) == out.generation;
        if idle {
            return SnapshotOutcome::Unchanged;
        }
        let mut guard = match self.try_screen() {
            Ok(Some(guard)) => guard,
            Ok(None) => {
                // Ask first, so that the next slice publishes for the next
                // frame even when this one takes an older snapshot.
                self.snapshot_wanted.store(true, Ordering::Release);
                return if self.take_published(out) {
                    SnapshotOutcome::Updated
                } else {
                    SnapshotOutcome::Contended
                };
            }
            Err(()) => return SnapshotOutcome::Failed,
        };
        let repoll_after = guard.flush_expired_sync();
        // The lock is held, so the generation cannot move during the copy.
        let generation = self.generation.load(Ordering::Acquire);
        self.dirty.store(false, Ordering::Release);
        self.fill_snapshot(&guard.term, out, generation);
        out.repoll_after = repoll_after;
        SnapshotOutcome::Updated
    }

    /// Resize the grid. Never blocks.
    pub fn try_resize(&self, size: GridSize) -> ResizeOutcome {
        match self.try_screen() {
            Ok(Some(mut guard)) => {
                guard.term.resize(Dims::from(size));
                drop(guard);
                self.generation.fetch_add(1, Ordering::Release);
                ResizeOutcome::Applied
            }
            Ok(None) => ResizeOutcome::Retry,
            Err(()) => ResizeOutcome::Failed,
        }
    }

    /// All the text the terminal holds: the scrollback and the visible grid,
    /// one line per row, with wrapped rows joined and wide-character spacers
    /// skipped. The flag says history has reached its limit, so older lines may
    /// have scrolled out. Takes the screen lock once and waits for it, so call
    /// it from a handler thread and never from the UI (section 5.13).
    #[must_use]
    pub fn text_all(&self) -> (String, bool) {
        let guard = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let term = &guard.term;
        let text = rows_text(term, term.topmost_line().0);
        (text, term.history_size() >= SCROLLBACK_LINES)
    }

    /// The last `rows` rows of [`Self::text_all`]: the visible grid and the
    /// newest history above it. For a caller that only looks at the bottom, so
    /// the whole scrollback is not built under the lock each time. Waits for
    /// the screen lock like `text_all`.
    #[must_use]
    pub fn text_tail(&self, rows: usize) -> String {
        let guard = self.screen.lock().unwrap_or_else(PoisonError::into_inner);
        let term = &guard.term;
        let above = i32::try_from(rows.saturating_sub(1)).unwrap_or(i32::MAX);
        let first = term
            .bottommost_line()
            .0
            .saturating_sub(above)
            .max(term.topmost_line().0);
        rows_text(term, first)
    }

    /// Scroll through history. Returns false if the emulator was busy.
    pub fn try_scroll(&self, lines: i32) -> bool {
        let Ok(Some(mut guard)) = self.try_screen() else {
            return false;
        };
        guard.term.scroll_display(Scroll::Delta(lines));
        drop(guard);
        self.generation.fetch_add(1, Ordering::Release);
        true
    }

    /// How many clipboard requests the policy has dropped.
    #[must_use]
    pub fn dropped_clipboard_events(&self) -> u64 {
        self.state.dropped_clipboard.load(Ordering::Relaxed)
    }
}

/// The rows from `first` to the bottom as text, one line per row, with wrapped
/// rows joined and wide-character spacers skipped.
fn rows_text(term: &Term<PolicyListener>, first: i32) -> String {
    let columns = term.columns();
    let mut out = String::new();
    for line in first..=term.bottommost_line().0 {
        let row = &term.grid()[Line(line)];
        let mut text = String::with_capacity(columns);
        for column in 0..columns {
            let cell = &row[Column(column)];
            // Both spacers: the one after a wide character, and the one
            // left in the last column when a wide character had to wrap.
            if cell
                .flags
                .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
            {
                continue;
            }
            text.push(cell.c);
            if let Some(extra) = cell.zerowidth() {
                text.extend(extra);
            }
        }
        let wrapped = columns > 0 && row[Column(columns - 1)].flags.contains(Flags::WRAPLINE);
        if wrapped {
            out.push_str(&text);
        } else {
            out.push_str(text.trim_end());
            out.push('\n');
        }
    }
    let trimmed = out.trim_end_matches('\n').len();
    out.truncate(trimmed);
    if !out.is_empty() {
        out.push('\n');
    }
    out
}

fn copy_screen(term: &Term<PolicyListener>, out: &mut TermSnapshot) {
    let content = term.renderable_content();
    let cols = term.columns();
    let rows = term.screen_lines();
    let offset = content.display_offset;
    let colors = content.colors;

    out.cols = u16::try_from(cols).unwrap_or(u16::MAX);
    out.rows = u16::try_from(rows).unwrap_or(u16::MAX);
    out.display_offset = offset;
    out.default_bg = color::BACKGROUND;
    out.modes = InputModes {
        app_cursor: content.mode.contains(TermMode::APP_CURSOR),
        bracketed_paste: content.mode.contains(TermMode::BRACKETED_PASTE),
        alt_screen: content.mode.contains(TermMode::ALT_SCREEN),
        mouse_click: content.mode.contains(TermMode::MOUSE_REPORT_CLICK),
        mouse_drag: content.mode.contains(TermMode::MOUSE_DRAG),
        mouse_motion: content.mode.contains(TermMode::MOUSE_MOTION),
        mouse_sgr: content.mode.contains(TermMode::SGR_MOUSE),
        mouse_utf8: content.mode.contains(TermMode::UTF8_MOUSE),
        focus_reporting: content.mode.contains(TermMode::FOCUS_IN_OUT),
    };
    out.cells.clear();
    out.cells.resize(cols * rows, Cell::default());

    for indexed in content.display_iter {
        let Ok(row) = usize::try_from(indexed.point.line.0 + offset as i32) else {
            continue;
        };
        let index = row * cols + indexed.point.column.0;
        if let Some(slot) = out.cells.get_mut(index) {
            *slot = convert_cell(indexed.cell, colors);
        }
    }

    out.cursor = convert_cursor(content.cursor.shape).and_then(|shape| {
        let row = content.cursor.point.line.0 + offset as i32;
        Some(Cursor {
            col: u16::try_from(content.cursor.point.column.0).ok()?,
            // A cursor outside the viewport, after scrolling back, is not drawn.
            row: u16::try_from(row)
                .ok()
                .filter(|row| usize::from(*row) < rows)?,
            shape,
        })
    });
}

fn convert_cursor(shape: VteCursorShape) -> Option<CursorShape> {
    match shape {
        VteCursorShape::Hidden => None,
        VteCursorShape::Beam => Some(CursorShape::Beam),
        VteCursorShape::Underline => Some(CursorShape::Underline),
        VteCursorShape::Block | VteCursorShape::HollowBlock => Some(CursorShape::Block),
    }
}

fn convert_cell(
    cell: &alacritty_terminal::term::cell::Cell,
    colors: &alacritty_terminal::term::color::Colors,
) -> Cell {
    let flags = cell.flags;
    let fg_source = if flags.contains(Flags::BOLD) {
        color::brighten(cell.fg)
    } else {
        cell.fg
    };
    let mut fg = color::resolve(fg_source, colors);
    let mut bg = color::resolve(cell.bg, colors);
    if flags.contains(Flags::DIM) {
        fg = color::dimmed(fg);
    }
    if flags.contains(Flags::INVERSE) {
        std::mem::swap(&mut fg, &mut bg);
    }
    if flags.contains(Flags::HIDDEN) {
        fg = bg;
    }

    let mut out_flags = CellFlags::default();
    for (source, target) in [
        (Flags::BOLD, CellFlags::BOLD),
        (Flags::ITALIC, CellFlags::ITALIC),
        (Flags::STRIKEOUT, CellFlags::STRIKEOUT),
        (Flags::WIDE_CHAR, CellFlags::WIDE),
        (Flags::WIDE_CHAR_SPACER, CellFlags::WIDE_SPACER),
        (Flags::WRAPLINE, CellFlags::WRAPPED),
    ] {
        if flags.contains(source) {
            out_flags.insert(target);
        }
    }
    if flags.intersects(Flags::ALL_UNDERLINES) {
        out_flags.insert(CellFlags::UNDERLINE);
    }

    Cell {
        ch: cell.c,
        fg,
        bg,
        flags: out_flags,
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used, clippy::panic)]
mod tests {
    use std::sync::Mutex as StdMutex;

    use alacritty_terminal::event::EventListener;

    use super::*;

    const SIZE: GridSize = GridSize { cols: 40, rows: 6 };

    struct Harness {
        emulator: Emulator,
        replies: Arc<StdMutex<Vec<u8>>>,
    }

    fn harness() -> Harness {
        let replies = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&replies);
        let respond: Responder = Arc::new(move |bytes| sink.lock().unwrap().extend(bytes));
        Harness {
            emulator: Emulator::new(SIZE, respond, Arc::new(|| {})),
            replies,
        }
    }

    impl Harness {
        fn replies(&self) -> Vec<u8> {
            self.replies.lock().unwrap().clone()
        }

        fn snapshot(&self) -> TermSnapshot {
            let mut out = TermSnapshot::default();
            assert_eq!(
                self.emulator.poll_snapshot(&mut out),
                SnapshotOutcome::Updated
            );
            out
        }
    }

    fn row_text(snapshot: &TermSnapshot, row: u16) -> String {
        let text: String = snapshot.row(row).iter().map(|cell| cell.ch).collect();
        text.trim_end().to_owned()
    }

    #[test]
    fn text_reaches_the_grid() {
        let h = harness();
        h.emulator.feed(b"hello\r\nworld");
        let snapshot = h.snapshot();
        assert_eq!(row_text(&snapshot, 0), "hello");
        assert_eq!(row_text(&snapshot, 1), "world");
        assert_eq!(snapshot.cols, SIZE.cols);
        assert_eq!(snapshot.rows, SIZE.rows);
    }

    #[test]
    fn a_utf8_character_split_across_reads_is_reassembled() {
        let h = harness();
        let bytes = "é".as_bytes();
        h.emulator.feed(&bytes[..1]);
        h.emulator.feed(&bytes[1..]);
        assert_eq!(row_text(&h.snapshot(), 0), "é");
    }

    #[test]
    fn invalid_utf8_does_not_stop_the_terminal() {
        let h = harness();
        h.emulator.feed(b"a\xff\xfeb");
        let text = row_text(&h.snapshot(), 0);
        assert!(text.starts_with('a') && text.ends_with('b'), "{text:?}");
    }

    #[test]
    fn an_escape_sequence_split_across_reads_is_reassembled() {
        let h = harness();
        h.emulator.feed(b"\x1b[3");
        h.emulator.feed(b"1mX");
        let snapshot = h.snapshot();
        assert_eq!(snapshot.row(0)[0].ch, 'X');
        assert_ne!(snapshot.row(0)[0].fg, color::FOREGROUND);
    }

    #[test]
    fn osc52_never_reaches_the_clipboard_or_gets_a_reply() {
        let h = harness();
        h.emulator.feed(b"\x1b]52;c;aGVsbG8=\x07"); // store
        h.emulator.feed(b"\x1b]52;c;?\x07"); // load
        assert!(h.replies().is_empty());
        // Disabled at the source, so the listener's second layer never fires.
        assert_eq!(h.emulator.dropped_clipboard_events(), 0);
    }

    #[test]
    fn the_listener_drops_clipboard_events_even_if_one_is_raised() {
        use alacritty_terminal::term::ClipboardType;
        let state = Arc::new(PolicyState::default());
        let replies = Arc::new(StdMutex::new(Vec::new()));
        let sink = Arc::clone(&replies);
        let listener = PolicyListener {
            responder: Arc::new(move |bytes| sink.lock().unwrap().extend(bytes)),
            state: Arc::clone(&state),
        };
        listener.send_event(alacritty_terminal::event::Event::ClipboardStore(
            ClipboardType::Clipboard,
            "secret".into(),
        ));
        listener.send_event(alacritty_terminal::event::Event::ClipboardLoad(
            ClipboardType::Clipboard,
            Arc::new(|text| text.to_owned()),
        ));
        assert_eq!(state.dropped_clipboard.load(Ordering::Relaxed), 2);
        assert!(replies.lock().unwrap().is_empty());
    }

    #[test]
    fn titles_are_sanitized_and_can_be_reset() {
        let h = harness();
        h.emulator.feed(b"\x1b]2;bad\xe2\x80\xaename\x07");
        assert_eq!(h.snapshot().title.as_deref(), Some("badname"));
        h.emulator.feed(b"\x1b]2;\x07");
        assert_eq!(h.snapshot().title, None);
    }

    #[test]
    fn device_status_and_attribute_queries_are_answered() {
        let h = harness();
        h.emulator.feed(b"\x1b[5n");
        assert_eq!(h.replies(), b"\x1b[0n");
        h.replies.lock().unwrap().clear();
        h.emulator.feed(b"\x1b[c");
        assert!(h.replies().starts_with(b"\x1b[?"));
    }

    #[test]
    fn cursor_position_report_is_answered() {
        let h = harness();
        h.emulator.feed(b"ab\x1b[6n");
        assert_eq!(h.replies(), b"\x1b[1;3R");
    }

    #[test]
    fn background_colour_query_is_answered() {
        let h = harness();
        h.emulator.feed(b"\x1b]11;?\x07");
        let reply = String::from_utf8(h.replies()).unwrap();
        assert!(reply.starts_with("\x1b]11;rgb:14"), "{reply:?}");
    }

    #[test]
    fn kitty_keyboard_query_gets_no_answer_so_apps_fall_back() {
        let h = harness();
        h.emulator.feed(b"\x1b[?u");
        assert!(h.replies().is_empty());
        h.emulator.feed(b"\x1b[>1u");
        assert!(h.replies().is_empty());
    }

    #[test]
    fn application_cursor_and_bracketed_paste_reach_the_snapshot() {
        let h = harness();
        h.emulator.feed(b"\x1b[?1h\x1b[?2004h");
        let modes = h.snapshot().modes;
        assert!(modes.app_cursor && modes.bracketed_paste);
        h.emulator.feed(b"\x1b[?1l\x1b[?2004l");
        let modes = h.snapshot().modes;
        assert!(!modes.app_cursor && !modes.bracketed_paste);
    }

    #[test]
    fn alternate_screen_is_reported_and_restores_the_main_screen() {
        let h = harness();
        h.emulator.feed(b"main");
        h.emulator.feed(b"\x1b[?1049h\x1b[2J\x1b[Halt");
        let snapshot = h.snapshot();
        assert!(snapshot.modes.alt_screen);
        assert_eq!(row_text(&snapshot, 0), "alt");
        h.emulator.feed(b"\x1b[?1049l");
        assert_eq!(row_text(&h.snapshot(), 0), "main");
    }

    #[test]
    fn resize_changes_the_snapshot_dimensions() {
        let h = harness();
        let size = GridSize { cols: 20, rows: 3 };
        assert_eq!(h.emulator.try_resize(size), ResizeOutcome::Applied);
        let snapshot = h.snapshot();
        assert_eq!((snapshot.cols, snapshot.rows), (20, 3));
        assert_eq!(snapshot.cells.len(), 60);
    }

    #[test]
    fn a_degenerate_size_is_clamped_rather_than_crashing() {
        let h = harness();
        let size = GridSize { cols: 0, rows: 0 };
        assert_eq!(h.emulator.try_resize(size), ResizeOutcome::Applied);
        h.emulator.feed(b"still alive");
        let snapshot = h.snapshot();
        assert_eq!((snapshot.cols, snapshot.rows), (MIN_COLS, MIN_ROWS));
    }

    #[test]
    fn only_a_soft_wrapped_row_is_flagged_wrapped() {
        let h = harness();
        assert_eq!(
            h.emulator.try_resize(GridSize { cols: 12, rows: 4 }),
            ResizeOutcome::Applied
        );
        h.emulator.feed(b"https://a.io\r\nnext\r\n0123456789abcdef");
        let snapshot = h.snapshot();
        let last = |row: u16| snapshot.row(row)[11].flags.contains(CellFlags::WRAPPED);
        assert!(!last(0), "a line that fills the row and then ends");
        assert!(last(2), "text that ran past the last column");
        assert!(!last(3));
    }

    #[test]
    fn wide_characters_are_flagged_with_their_spacer() {
        let h = harness();
        h.emulator.feed("界x".as_bytes());
        let snapshot = h.snapshot();
        let row = snapshot.row(0);
        assert_eq!(row[0].ch, '界');
        assert!(row[0].flags.contains(CellFlags::WIDE));
        assert!(row[1].flags.contains(CellFlags::WIDE_SPACER));
        assert_eq!(row[2].ch, 'x');
    }

    #[test]
    fn inverse_swaps_colours() {
        let h = harness();
        h.emulator.feed(b"\x1b[7mX");
        let snapshot = h.snapshot();
        let cell = snapshot.row(0)[0];
        assert_eq!(cell.fg, color::BACKGROUND);
        assert_eq!(cell.bg, color::FOREGROUND);
    }

    #[test]
    fn truecolor_is_preserved() {
        let h = harness();
        h.emulator.feed(b"\x1b[38;2;1;2;3mX");
        assert_eq!(h.snapshot().row(0)[0].fg, connecto_core::Rgb::new(1, 2, 3));
    }

    #[test]
    fn hidden_cursor_is_absent_from_the_snapshot() {
        let h = harness();
        assert!(h.snapshot().cursor.is_some());
        h.emulator.feed(b"\x1b[?25l");
        assert_eq!(h.snapshot().cursor, None);
    }

    #[test]
    fn an_unchanged_screen_is_not_copied_again() {
        let h = harness();
        let mut out = TermSnapshot::default();
        assert_eq!(h.emulator.poll_snapshot(&mut out), SnapshotOutcome::Updated);
        assert_eq!(
            h.emulator.poll_snapshot(&mut out),
            SnapshotOutcome::Unchanged
        );
        h.emulator.feed(b"x");
        assert_eq!(h.emulator.poll_snapshot(&mut out), SnapshotOutcome::Updated);
    }

    #[test]
    fn a_held_lock_reports_contention_instead_of_blocking() {
        let h = harness();
        let mut out = TermSnapshot::default();
        let guard = h.emulator.screen.lock().unwrap();
        assert_eq!(
            h.emulator.poll_snapshot(&mut out),
            SnapshotOutcome::Contended
        );
        let size = GridSize { cols: 9, rows: 9 };
        assert_eq!(h.emulator.try_resize(size), ResizeOutcome::Retry);
        assert!(!h.emulator.try_scroll(1));
        drop(guard);
        assert_eq!(h.emulator.poll_snapshot(&mut out), SnapshotOutcome::Updated);
    }

    #[test]
    fn a_contended_poll_makes_the_reader_publish_a_frame() {
        let h = harness();
        h.emulator.feed(b"\x1b]2;live\x07first");
        let mut out = TermSnapshot::default();

        // The UI loses the race and asks for a frame.
        let guard = h.emulator.screen.lock().unwrap();
        assert_eq!(
            h.emulator.poll_snapshot(&mut out),
            SnapshotOutcome::Contended
        );
        drop(guard);

        // The reader's next slice publishes it.
        h.emulator.feed(b" second");

        // The lock is busy again, yet the UI still gets the new content.
        let guard = h.emulator.screen.lock().unwrap();
        assert_eq!(h.emulator.poll_snapshot(&mut out), SnapshotOutcome::Updated);
        assert_eq!(row_text(&out, 0), "first second");
        assert_eq!(out.title.as_deref(), Some("live"));
        // A published frame is delivered once.
        assert_eq!(
            h.emulator.poll_snapshot(&mut out),
            SnapshotOutcome::Unchanged
        );
        drop(guard);
    }

    #[test]
    fn the_reader_publishes_only_when_asked() {
        let h = harness();
        h.emulator.feed(b"quiet");
        assert!(!h.emulator.published.lock().unwrap().fresh);
    }

    #[test]
    fn a_frame_is_published_after_its_wake_so_the_ui_is_not_left_idle() {
        let wakes = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&wakes);
        let emulator = Emulator::new(
            SIZE,
            Arc::new(|_| {}),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        emulator.feed(b"a"); // first change wakes
        emulator.feed(b"b"); // still announced, no second wake
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        emulator.snapshot_wanted.store(true, Ordering::Release);
        emulator.feed(b"c"); // a published frame must wake the UI again
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    /// Models the UI: it polls only when woken, or after the short timer it
    /// arms on contention. Any lost wake leaves it asleep while output flows.
    #[test]
    fn a_ui_that_polls_only_when_woken_never_stalls_during_a_flood() {
        use crossbeam_channel::{RecvTimeoutError, bounded};
        use std::sync::atomic::AtomicBool;

        let (wake_tx, wake_rx) = bounded::<()>(1);
        let emulator = Arc::new(Emulator::new(
            SIZE,
            Arc::new(|_| {}),
            Arc::new(move || {
                let _ = wake_tx.try_send(());
            }),
        ));

        let running = Arc::new(AtomicBool::new(true));
        let reader = {
            let emulator = Arc::clone(&emulator);
            let running = Arc::clone(&running);
            std::thread::spawn(move || {
                let slice = b"flood flood flood\r\n".repeat(200);
                while running.load(Ordering::Acquire) {
                    emulator.feed(&slice);
                }
            })
        };

        let started = Instant::now();
        let mut out = TermSnapshot::default();
        let mut updates = 0_u32;
        let mut timer = None;
        while started.elapsed() < Duration::from_secs(3) {
            let wait = timer.unwrap_or(Duration::from_millis(1000));
            match wake_rx.recv_timeout(wait) {
                Ok(()) => {}
                Err(RecvTimeoutError::Timeout) if timer.is_some() => {}
                Err(err) => panic!("the UI was never woken again: {err:?}"),
            }
            timer = None;
            match emulator.poll_snapshot(&mut out) {
                SnapshotOutcome::Updated => updates += 1,
                SnapshotOutcome::Contended => timer = Some(Duration::from_millis(16)),
                SnapshotOutcome::Unchanged => {}
                SnapshotOutcome::Failed => panic!("emulator failed"),
            }
            // A 60 Hz frame budget, so the reader runs between polls.
            std::thread::sleep(Duration::from_millis(16));
        }
        running.store(false, Ordering::Release);
        reader.join().unwrap();
        assert!(updates > 10, "only {updates} updates in 3 s");
    }

    #[test]
    fn a_stalled_synchronized_update_is_released_by_polling() {
        let h = harness();
        h.emulator.feed(b"\x1b[?2026hheld back");
        let mut out = TermSnapshot::default();
        assert_eq!(h.emulator.poll_snapshot(&mut out), SnapshotOutcome::Updated);
        assert_eq!(row_text(&out, 0), "");
        let delay = out.repoll_after.expect("a repoll must be requested");
        assert!(delay <= Duration::from_millis(150));

        std::thread::sleep(delay + Duration::from_millis(5));
        assert_eq!(h.emulator.poll_snapshot(&mut out), SnapshotOutcome::Updated);
        assert_eq!(out.repoll_after, None);
        assert_eq!(row_text(&out, 0), "held back");
    }

    #[test]
    fn a_query_in_a_stalled_update_is_answered_without_a_poll() {
        let h = harness();
        h.emulator.feed(b"\x1b[?2026h\x1b[6n");
        h.emulator.flush_expired();
        assert!(h.replies().is_empty(), "released before its time");

        std::thread::sleep(Duration::from_millis(155));
        h.emulator.flush_expired();
        assert_eq!(h.replies(), b"\x1b[1;1R");
    }

    #[test]
    fn flushing_tells_the_ui_there_is_a_new_screen() {
        let woken = Arc::new(AtomicU64::new(0));
        let count = Arc::clone(&woken);
        let emulator = Emulator::new(
            SIZE,
            Arc::new(|_| {}),
            Arc::new(move || {
                count.fetch_add(1, Ordering::Relaxed);
            }),
        );
        emulator.feed(b"\x1b[?2026hheld back");
        let mut out = TermSnapshot::default();
        emulator.poll_snapshot(&mut out);
        let before = woken.load(Ordering::Relaxed);
        std::thread::sleep(Duration::from_millis(155));
        emulator.flush_expired();
        assert_eq!(woken.load(Ordering::Relaxed), before + 1);
        assert_eq!(emulator.poll_snapshot(&mut out), SnapshotOutcome::Updated);
        assert_eq!(row_text(&out, 0), "held back");
    }

    #[test]
    fn nothing_held_means_no_lock_is_taken() {
        let h = harness();
        h.emulator.feed(b"plain");
        let _busy = h.emulator.screen.lock().unwrap();
        h.emulator.flush_expired();
    }

    #[test]
    fn a_completed_synchronized_update_appears_at_once() {
        let h = harness();
        h.emulator.feed(b"\x1b[?2026hframe\x1b[?2026l");
        let snapshot = h.snapshot();
        assert_eq!(row_text(&snapshot, 0), "frame");
        assert_eq!(snapshot.repoll_after, None);
    }

    #[test]
    fn a_flood_wakes_the_ui_once_until_it_takes_a_frame() {
        let wakes = Arc::new(AtomicU64::new(0));
        let counter = Arc::clone(&wakes);
        let emulator = Emulator::new(
            SIZE,
            Arc::new(|_| {}),
            Arc::new(move || {
                counter.fetch_add(1, Ordering::Relaxed);
            }),
        );
        for _ in 0..100 {
            emulator.feed(b"y\r\n");
        }
        assert_eq!(wakes.load(Ordering::Relaxed), 1);
        let mut out = TermSnapshot::default();
        assert_eq!(emulator.poll_snapshot(&mut out), SnapshotOutcome::Updated);
        emulator.feed(b"y\r\n");
        assert_eq!(wakes.load(Ordering::Relaxed), 2);
    }

    #[test]
    fn text_all_returns_history_and_the_screen_and_joins_wrapped_rows() {
        let h = harness();
        for i in 0..40 {
            h.emulator.feed(format!("line {i}\r\n").as_bytes());
        }
        let (text, full) = h.emulator.text_all();
        assert!(text.starts_with("line 0\n"), "{text}");
        assert!(text.contains("line 39\n"));
        assert!(!full, "history has not reached its limit");
        // A row wider than the grid is one line.
        let wide = "w".repeat(200);
        h.emulator.feed(format!("{wide}\r\n").as_bytes());
        let (text, _) = h.emulator.text_all();
        assert!(
            text.contains(&format!("{wide}\n")),
            "wrapped rows are joined"
        );
        // A wide character leaves no spacer behind.
        h.emulator.feed("\u{4e2d}\u{6587}\r\n".as_bytes());
        let (text, _) = h.emulator.text_all();
        assert!(text.contains("\u{4e2d}\u{6587}\n"), "{text}");
    }

    #[test]
    fn text_tail_is_the_end_of_text_all_without_spacers() {
        let h = harness();
        for i in 0..300 {
            h.emulator.feed(format!("line {i}\r\n").as_bytes());
        }
        h.emulator.feed("\u{4e2d}\u{6587} end".as_bytes());
        let (all, _) = h.emulator.text_all();
        let tail = h.emulator.text_tail(50);
        assert_eq!(tail.lines().count(), 50, "{tail}");
        assert!(all.ends_with(&tail) && all.len() > tail.len());
        assert!(tail.ends_with("\u{4e2d}\u{6587} end\n"), "{tail}");
        // Asking for more than there is gives everything.
        assert_eq!(h.emulator.text_tail(usize::MAX), all);
        assert_eq!(h.emulator.text_tail(0), h.emulator.text_tail(1));
    }

    #[test]
    fn text_all_says_when_history_is_full() {
        let h = harness();
        for i in 0..(SCROLLBACK_LINES + 100) {
            h.emulator.feed(format!("{i}\r\n").as_bytes());
        }
        let (text, full) = h.emulator.text_all();
        assert!(full);
        assert!(!text.starts_with("0\n"), "the oldest lines have gone");
    }

    #[test]
    fn scrolling_back_shows_history_and_hides_the_cursor() {
        let h = harness();
        for i in 0..20 {
            h.emulator.feed(format!("line {i}\r\n").as_bytes());
        }
        assert!(h.emulator.try_scroll(3));
        let snapshot = h.snapshot();
        assert_eq!(snapshot.display_offset, 3);
        assert_eq!(row_text(&snapshot, 0), "line 12");
    }

    #[test]
    fn arbitrary_bytes_never_panic_the_emulator() {
        let h = harness();
        let mut state = 0x2545_f491_4f6c_dd1d_u64;
        let mut bytes = vec![0_u8; 64 * 1024];
        for _ in 0..16 {
            for byte in &mut bytes {
                state ^= state << 13;
                state ^= state >> 7;
                state ^= state << 17;
                *byte = state as u8;
            }
            h.emulator.feed(&bytes);
        }
        let mut out = TermSnapshot::default();
        assert_ne!(h.emulator.poll_snapshot(&mut out), SnapshotOutcome::Failed);
    }
}
