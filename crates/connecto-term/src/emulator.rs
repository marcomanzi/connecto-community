//! The shared screen state: written by the PTY reader, read by the UI.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, PoisonError, TryLockError};
use std::time::{Duration, Instant};

use alacritty_terminal::Term;
use alacritty_terminal::grid::{Dimensions, Scroll};
use alacritty_terminal::index::{Column, Line, Point, Side};
use alacritty_terminal::selection::{Selection, SelectionRange, SelectionType};
use alacritty_terminal::term::cell::Flags;
use alacritty_terminal::term::{Config, Osc52, TermMode};
use alacritty_terminal::vte::ansi::{self, CursorShape as VteCursorShape};
use connecto_core::{
    Cell, CellFlags, Cursor, CursorShape, GridSize, InputModes, ResizeOutcome, SelectOp,
    SelectedRange, SelectionText, SnapshotOutcome, TermSnapshot,
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

/// The size the emulator takes when asked for `size`: never smaller than it
/// can draw.
#[must_use]
pub fn effective_size(size: GridSize) -> GridSize {
    GridSize {
        cols: size.cols.max(MIN_COLS),
        rows: size.rows.max(MIN_ROWS),
    }
}

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
            let before = SelectedCells::of(&self.term);
            self.parser.stop_sync(&mut self.term);
            drop_if_rewritten(before, &mut self.term);
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
        let before = SelectedCells::of(&screen.term);
        screen.parser.advance(&mut screen.term, bytes);
        drop_if_rewritten(before, &mut screen.term);
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

    /// Runs `f` with the screen's generation, holding the screen meanwhile:
    /// no output is taken in until `f` returns, so what `f` does (an input
    /// write conditional on the screen not having changed) cannot be
    /// overtaken by a redraw. None, without running `f`, while the screen
    /// is busy. Never blocks.
    pub fn hold_generation<T>(&self, f: impl FnOnce(u64) -> T) -> Option<T> {
        let Ok(Some(_screen)) = self.try_screen() else {
            return None;
        };
        Some(f(self.generation.load(Ordering::Acquire)))
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

    /// The visible screen's rows as [`Self::text_tail`] gives them, or None
    /// while the screen is busy. Never blocks: for a caller that would rather
    /// go without than wait on a terminal in the middle of a redraw.
    #[must_use]
    pub fn try_text_screen(&self) -> Option<String> {
        let Ok(Some(guard)) = self.try_screen() else {
            return None;
        };
        let term = &guard.term;
        let above = i32::try_from(term.screen_lines().saturating_sub(1)).unwrap_or(i32::MAX);
        let first = term
            .bottommost_line()
            .0
            .saturating_sub(above)
            .max(term.topmost_line().0);
        Some(rows_text(term, first))
    }

    /// The live screen and the newest history above it, read together, for a
    /// peer that shows this terminal: `screen` as [`Self::poll_snapshot`] fills
    /// it but always the live screen, whatever the view is scrolled to, and
    /// `history` with up to `max_cells` cells of history in whole rows of
    /// `screen.cols`, oldest first (only its cells are set). The alternate
    /// screen has no history. Returns false, changing nothing, if the emulator
    /// was busy or a synchronized update is still being held back, unless
    /// `flush` says to end that update now (as its timeout would) and copy what
    /// it held. Never blocks.
    pub fn try_capture(
        &self,
        max_cells: usize,
        flush: bool,
        screen: &mut TermSnapshot,
        history: &mut TermSnapshot,
    ) -> bool {
        let Ok(Some(mut guard)) = self.try_screen() else {
            return false;
        };
        // Output held back by a synchronized update is already past the caller's
        // sequence number but not on the grid yet: wait until it is.
        if guard.flush_expired_sync().is_some() {
            if !flush {
                return false;
            }
            let screen = &mut *guard;
            let before = SelectedCells::of(&screen.term);
            screen.parser.stop_sync(&mut screen.term);
            drop_if_rewritten(before, &mut screen.term);
            self.generation.fetch_add(1, Ordering::AcqRel);
            if !self.dirty.swap(true, Ordering::AcqRel) {
                (self.wake)();
            }
        }
        let term = &mut guard.term;
        // The view is put back before the lock is let go, so nobody sees it move.
        let offset = term.grid().display_offset();
        if offset > 0 {
            term.scroll_display(Scroll::Bottom);
        }
        let generation = self.generation.load(Ordering::Acquire);
        self.fill_snapshot(term, screen, generation);
        if offset > 0 {
            term.scroll_display(Scroll::Delta(i32::try_from(offset).unwrap_or(i32::MAX)));
        }
        let cols = term.columns();
        let lines = term
            .history_size()
            .min(max_cells / cols.max(1))
            .min(usize::from(u16::MAX));
        let colors = term.colors();
        *history = TermSnapshot {
            cols: u16::try_from(cols).unwrap_or(u16::MAX),
            rows: u16::try_from(lines).unwrap_or(u16::MAX),
            default_bg: color::BACKGROUND,
            ..TermSnapshot::default()
        };
        history.cells.reserve(cols * lines);
        for back in (1..=lines).rev() {
            let row = &term.grid()[Line(-i32::try_from(back).unwrap_or(i32::MAX))];
            for column in 0..cols {
                history
                    .cells
                    .push(convert_cell(&row[Column(column)], colors));
            }
        }
        true
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

    /// Change the selection. Cells are given on the screen as shown and kept
    /// in the terminal's own lines, so the selection stays on its text while
    /// the view scrolls or output pushes it up; text that leaves history or is
    /// erased takes the selection with it. Returns false if the emulator was
    /// busy. Never blocks.
    pub fn try_select(&self, op: SelectOp) -> bool {
        let Ok(Some(mut guard)) = self.try_screen() else {
            return false;
        };
        let term = &mut guard.term;
        let offset = i32::try_from(term.grid().display_offset()).unwrap_or(i32::MAX);
        let last_col = term.columns().saturating_sub(1);
        let point = |row: u16, col: u16| {
            Point::new(
                Line(i32::from(row) - offset),
                Column(usize::from(col).min(last_col)),
            )
        };
        match op {
            SelectOp::Start { row, col } => {
                term.selection = Some(Selection::new(
                    SelectionType::Simple,
                    point(row, col),
                    Side::Left,
                ));
            }
            SelectOp::Extend { row, col } => {
                if let Some(selection) = term.selection.as_mut() {
                    selection.update(point(row, col), Side::Right);
                    // Both end cells are in, whichever way the drag went.
                    selection.include_all();
                }
            }
            SelectOp::All => {
                let bottom = u16::try_from(term.screen_lines().saturating_sub(1)).unwrap_or(0);
                let mut selection = Selection::new(SelectionType::Simple, point(0, 0), Side::Left);
                selection.update(point(bottom, u16::MAX), Side::Right);
                term.selection = Some(selection);
            }
            SelectOp::Clear => term.selection = None,
        }
        drop(guard);
        self.generation.fetch_add(1, Ordering::Release);
        true
    }

    /// The selected text, including rows scrolled out of view. Never blocks.
    #[must_use]
    pub fn selection_text(&self) -> SelectionText {
        match self.try_screen() {
            Ok(Some(guard)) => {
                selected_text(&guard.term).map_or(SelectionText::Nothing, SelectionText::Text)
            }
            Ok(None) => SelectionText::Busy,
            Err(()) => SelectionText::Nothing,
        }
    }

    /// The selected text as the clean copy reads it
    /// ([`connecto_core::copy_text`]): an agent's margins dropped and the
    /// rows it broke itself put back together. Never blocks.
    #[must_use]
    pub fn clean_selection_text(&self) -> SelectionText {
        match self.try_screen() {
            Ok(Some(guard)) => {
                selected_lines(&guard.term).map_or(SelectionText::Nothing, |lines| {
                    SelectionText::Text(connecto_core::copy_text::clean_copy(
                        &lines,
                        guard.term.columns(),
                    ))
                })
            }
            Ok(None) => SelectionText::Busy,
            Err(()) => SelectionText::Nothing,
        }
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

/// The selected text. Trailing blanks go and wide-character spacers are
/// skipped. A row the terminal wrapped, or one filled to its last column, joins
/// the next without a newline, so a long login URL that a full-screen program
/// broke over rows still copies whole.
fn selected_text(term: &Term<PolicyListener>) -> Option<String> {
    let range = selection_range(term)?;
    let columns = term.columns();
    let mut out = String::new();
    for line in range.start.line.0..=range.end.line.0 {
        let row = &term.grid()[Line(line)];
        let (start, end) = row_bounds(&range, line, columns);
        let last = columns.checked_sub(1).map(|c| &row[Column(c)]);
        let soft_wrapped = last.is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
        // Blanks are dropped only at the end of a logical line: on a row the
        // terminal wrapped, a space in the last column is part of the text.
        let length = if soft_wrapped {
            columns
        } else {
            (0..columns)
                .rev()
                .find(|&column| !is_blank(&row[Column(column)]))
                .map_or(0, |column| column + 1)
        };
        for column in start..end.min(length) {
            if let Some((ch, extra)) = cell_text(&row[Column(column)]) {
                out.push(ch);
                out.extend(extra);
            }
        }
        let joined = end == columns && (soft_wrapped || last.is_some_and(|cell| !is_blank(cell)));
        if line != range.end.line.0 && !joined {
            out.push('\n');
        }
    }
    // Blank rows at the end, as below a short screen, are not copied.
    let kept = out.trim_end_matches('\n').len();
    out.truncate(kept);
    Some(out)
}

/// The selection as logical lines (rows the terminal wrapped joined, as in
/// [`selected_text`]), each with the column it starts at and where its last
/// row's text ends, for the clean copy.
fn selected_lines(term: &Term<PolicyListener>) -> Option<Vec<connecto_core::copy_text::CopyLine>> {
    let range = selection_range(term)?;
    let columns = term.columns();
    let mut lines = Vec::new();
    let mut current: Option<connecto_core::copy_text::CopyLine> = None;
    for line in range.start.line.0..=range.end.line.0 {
        let row = &term.grid()[Line(line)];
        let (start, end) = row_bounds(&range, line, columns);
        let last = columns.checked_sub(1).map(|c| &row[Column(c)]);
        let soft_wrapped = last.is_some_and(|cell| cell.flags.contains(Flags::WRAPLINE));
        let content_end = (0..columns)
            .rev()
            .find(|&column| !is_blank(&row[Column(column)]))
            .map_or(0, |column| column + 1);
        let length = if soft_wrapped { columns } else { content_end };
        let entry = current.get_or_insert_with(|| connecto_core::copy_text::CopyLine {
            text: String::new(),
            start,
            lead: (0..start)
                .filter_map(|column| cell_text(&row[Column(column)]).map(|(ch, _)| ch))
                .collect(),
            end: 0,
        });
        for column in start..end.min(length) {
            if let Some((ch, extra)) = cell_text(&row[Column(column)]) {
                entry.text.push(ch);
                entry.text.extend(extra);
            }
        }
        entry.end = content_end;
        // Only rows the terminal wrapped join here; every row a program broke
        // itself is left for the clean copy to judge.
        let joined = end == columns && soft_wrapped;
        if line == range.end.line.0 || !joined {
            lines.extend(current.take());
        }
    }
    // Blank rows at the end, as below a short screen, are not copied. A
    // selection of blanks only is still a selection: its copy is empty.
    while lines.last().is_some_and(|line| line.text.trim().is_empty()) {
        lines.pop();
    }
    Some(lines)
}

/// What a cell contributes to copied text: its character and any combining
/// characters on it. A wide character's spacer contributes nothing.
fn cell_text(cell: &alacritty_terminal::term::cell::Cell) -> Option<(char, &[char])> {
    if cell
        .flags
        .intersects(Flags::WIDE_CHAR_SPACER | Flags::LEADING_WIDE_CHAR_SPACER)
    {
        return None;
    }
    Some((cell.c, cell.zerowidth().unwrap_or(&[])))
}

fn is_blank(cell: &alacritty_terminal::term::cell::Cell) -> bool {
    cell_text(cell).is_some_and(|(ch, extra)| ch == ' ' && extra.is_empty())
}

/// The selection's first and last cell, both included, so that the highlight,
/// the copy and the rewrite check all use the same cells. A wide character is
/// taken whole at either end: a first cell on its right half moves left, and
/// a last cell on its left half moves right.
fn selection_range(term: &Term<PolicyListener>) -> Option<SelectionRange> {
    let mut range = term.selection.as_ref()?.to_range(term)?;
    let grid = term.grid();
    if range.start.column.0 > 0
        && grid[range.start.line][range.start.column]
            .flags
            .contains(Flags::WIDE_CHAR_SPACER)
    {
        range.start.column -= 1;
    }
    if range.end.column.0 + 1 < term.columns()
        && grid[range.end.line][range.end.column]
            .flags
            .contains(Flags::WIDE_CHAR)
    {
        range.end.column += 1;
    }
    Some(range)
}

/// The selected columns of `line`, end exclusive.
fn row_bounds(range: &SelectionRange, line: i32, columns: usize) -> (usize, usize) {
    let start = if line == range.start.line.0 {
        range.start.column.0
    } else {
        0
    };
    let end = if line == range.end.line.0 {
        range.end.column.0 + 1
    } else {
        columns
    };
    (start.min(columns), end.min(columns))
}

/// What the selected cells on the screen held before output was applied. Only
/// screen rows can be written; history rows only move.
struct SelectedCells {
    /// The selection's last line, which tells how far output scrolled it.
    end_line: i32,
    rows: Vec<(i32, usize, Vec<CellText>)>,
}

/// A cell as [`cell_text`] reads it, kept.
type CellText = Option<(char, Vec<char>)>;

fn owned_text(cell: &alacritty_terminal::term::cell::Cell) -> CellText {
    cell_text(cell).map(|(ch, extra)| (ch, extra.to_vec()))
}

impl SelectedCells {
    fn of(term: &Term<PolicyListener>) -> Option<Self> {
        let range = selection_range(term)?;
        if range.end.line.0 < 0 {
            return None;
        }
        let columns = term.columns();
        let rows = (range.start.line.0.max(0)..=range.end.line.0)
            .map(|line| {
                let (start, end) = row_bounds(&range, line, columns);
                let row = &term.grid()[Line(line)];
                let chars = (start..end)
                    .map(|column| owned_text(&row[Column(column)]))
                    .collect();
                (line, start, chars)
            })
            .collect();
        Some(Self {
            end_line: range.end.line.0,
            rows,
        })
    }
}

/// Clear the selection if output wrote other text into any of its cells, so a
/// copy never returns text the user did not select. Output that only appends,
/// or scrolls the selection up, leaves it.
fn drop_if_rewritten(before: Option<SelectedCells>, term: &mut Term<PolicyListener>) {
    let Some(before) = before else { return };
    let Some(range) = selection_range(term) else {
        return;
    };
    let shift = before.end_line - range.end.line.0;
    let (top, bottom) = (term.topmost_line().0, term.bottommost_line().0);
    let columns = term.columns();
    let rewritten = before.rows.iter().any(|(line, start, chars)| {
        let line = line - shift;
        if line < top || line > bottom {
            return true;
        }
        let row = &term.grid()[Line(line)];
        chars
            .iter()
            .enumerate()
            .any(|(i, text)| start + i >= columns || owned_text(&row[Column(start + i)]) != *text)
    });
    if rewritten {
        term.selection = None;
    }
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

    out.selection = selection_range(term).map(|range| {
        let at = |point: Point| {
            (
                point.line.0 + offset as i32,
                u16::try_from(point.column.0).unwrap_or(u16::MAX),
            )
        };
        SelectedRange {
            first: at(range.start),
            last: at(range.end),
        }
    });

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
        (
            Flags::LEADING_WIDE_CHAR_SPACER,
            CellFlags::LEADING_WIDE_SPACER,
        ),
        (Flags::WRAPLINE, CellFlags::WRAPPED),
        (Flags::DIM, CellFlags::DIM),
        (Flags::INVERSE, CellFlags::INVERSE),
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
        assert!(cell.flags.contains(CellFlags::INVERSE));
    }

    #[test]
    fn faint_text_is_flagged_and_normal_text_is_not() {
        let h = harness();
        h.emulator.feed(b"a\x1b[2mb\x1b[22mc");
        let snapshot = h.snapshot();
        let row = snapshot.row(0);
        assert!(!row[0].flags.contains(CellFlags::DIM));
        assert!(row[1].flags.contains(CellFlags::DIM));
        assert!(!row[2].flags.contains(CellFlags::DIM));
        assert!(!row[1].flags.contains(CellFlags::INVERSE));
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

    fn select(h: &Harness, op: SelectOp) {
        assert!(h.emulator.try_select(op));
    }

    fn selected(h: &Harness) -> String {
        match h.emulator.selection_text() {
            SelectionText::Text(text) => text,
            other => panic!("no selection: {other:?}"),
        }
    }

    #[test]
    fn a_selection_stays_on_its_text_while_the_view_scrolls_and_copies_it_all() {
        let h = harness();
        for i in 0..20 {
            h.emulator.feed(format!("line {i}\r\n").as_bytes());
        }
        // Back to the top: "line 0" is the first row shown.
        assert!(h.emulator.try_scroll(100));
        assert_eq!(row_text(&h.snapshot(), 0), "line 0");
        select(&h, SelectOp::Start { row: 0, col: 0 });
        // The view scrolls down six lines while the drag goes on.
        assert!(h.emulator.try_scroll(-6));
        let snapshot = h.snapshot();
        assert_eq!(row_text(&snapshot, 5), "line 11");
        select(&h, SelectOp::Extend { row: 5, col: 6 });
        let snapshot = h.snapshot();
        // The start is still on "line 0", now six rows above the view.
        assert_eq!(
            snapshot.selection,
            Some(SelectedRange {
                first: (-6, 0),
                last: (5, 6)
            })
        );
        let expected: Vec<String> = (0..12).map(|i| format!("line {i}")).collect();
        assert_eq!(selected(&h), expected.join("\n"));
        // New output moves nothing under the selection.
        h.emulator.feed(b"more\r\nand more\r\n");
        assert_eq!(selected(&h), expected.join("\n"));
        // Back at the live screen the selection is above the view.
        assert!(h.emulator.try_scroll(-1000));
        let range = h.snapshot().selection.unwrap();
        assert!(range.last.0 < 0, "{range:?}");
        assert!(range.spans(SIZE.rows, SIZE.cols).is_empty());
    }

    #[test]
    fn a_backward_drag_selects_both_end_cells() {
        let h = harness();
        h.emulator.feed(b"abcde\r\nuvwxyz");
        select(&h, SelectOp::Start { row: 1, col: 1 });
        select(&h, SelectOp::Extend { row: 0, col: 4 });
        assert_eq!(selected(&h), "e\nuv");
        // A press with no movement selects nothing.
        select(&h, SelectOp::Start { row: 0, col: 1 });
        assert_eq!(h.emulator.selection_text(), SelectionText::Nothing);
        select(&h, SelectOp::Extend { row: 0, col: 1 });
        assert_eq!(selected(&h), "b");
    }

    #[test]
    fn a_selection_whose_text_is_gone_is_cleared() {
        let h = harness();
        h.emulator.feed(b"keep me\r\n");
        select(&h, SelectOp::Start { row: 0, col: 0 });
        select(&h, SelectOp::Extend { row: 0, col: 3 });
        assert_eq!(selected(&h), "keep");
        // The screen is erased: the selection goes with its text.
        h.emulator.feed(b"\x1b[2J\x1b[H");
        assert_eq!(h.emulator.selection_text(), SelectionText::Nothing);
        assert_eq!(h.snapshot().selection, None);

        // Text pushed out of history takes its selection with it.
        h.emulator.feed(b"oldest\r\n");
        select(&h, SelectOp::Start { row: 0, col: 0 });
        select(&h, SelectOp::Extend { row: 0, col: 5 });
        assert_eq!(selected(&h), "oldest");
        for i in 0..(SCROLLBACK_LINES + 10) {
            h.emulator.feed(format!("{i}\r\n").as_bytes());
        }
        assert_eq!(h.emulator.selection_text(), SelectionText::Nothing);
    }

    #[test]
    fn a_carriage_return_redraw_over_the_selection_clears_it() {
        let h = harness();
        h.emulator.feed(b"original");
        select(&h, SelectOp::Start { row: 0, col: 0 });
        select(&h, SelectOp::Extend { row: 0, col: 7 });
        assert_eq!(selected(&h), "original");
        h.emulator.feed(b"\rreplaced");
        assert_eq!(h.emulator.selection_text(), SelectionText::Nothing);
        assert_eq!(h.snapshot().selection, None);
    }

    #[test]
    fn erasing_selected_characters_clears_the_selection() {
        for erase in [&b"\r\x1b[3X"[..], b"\r\x1b[2P", b"\x08\x08 "] {
            let h = harness();
            h.emulator.feed(b"keep this");
            select(&h, SelectOp::Start { row: 0, col: 0 });
            select(&h, SelectOp::Extend { row: 0, col: 8 });
            h.emulator.feed(erase);
            assert_eq!(
                h.emulator.selection_text(),
                SelectionText::Nothing,
                "{erase:?}"
            );
        }
    }

    #[test]
    fn the_same_text_redrawn_or_output_below_keeps_the_selection() {
        let h = harness();
        h.emulator.feed(b"title\r\n");
        select(&h, SelectOp::Start { row: 0, col: 0 });
        select(&h, SelectOp::Extend { row: 0, col: 4 });
        // A program redraws the line with the same text.
        h.emulator.feed(b"\x1b[H\x1b[1mtitle\x1b[m\r\n");
        assert_eq!(selected(&h), "title");
        // Streaming output below, enough to scroll the selection into history.
        for i in 0..30 {
            h.emulator.feed(format!("stream {i}\r\n").as_bytes());
        }
        assert_eq!(selected(&h), "title");
    }

    #[test]
    fn the_clean_copy_puts_an_agents_cut_rows_back_together() {
        let h = harness();
        assert_eq!(
            h.emulator.try_resize(GridSize { cols: 24, rows: 6 }),
            ResizeOutcome::Applied
        );
        // An agent's own wrapping: hard breaks, an indented continuation.
        h.emulator
            .feed("\u{23fa} see https://a.io/lo\r\n  ng/path now\r\n".as_bytes());
        select(&h, SelectOp::Start { row: 0, col: 0 });
        select(&h, SelectOp::Extend { row: 1, col: 23 });
        assert_eq!(selected(&h), "\u{23fa} see https://a.io/lo\n  ng/path now");
        match h.emulator.clean_selection_text() {
            SelectionText::Text(text) => assert_eq!(text, "see https://a.io/long/path now"),
            other => panic!("no selection: {other:?}"),
        }
    }

    #[test]
    fn the_clean_copy_joins_a_token_cut_at_the_last_column_without_the_margin() {
        let h = harness();
        assert_eq!(
            h.emulator.try_resize(GridSize { cols: 12, rows: 6 }),
            ResizeOutcome::Applied
        );
        // Exactly 12 columns, then a hard break and an indented rest.
        h.emulator.feed(b"  https://a.\r\n  io/x\r\n");
        select(&h, SelectOp::Start { row: 0, col: 2 });
        select(&h, SelectOp::Extend { row: 1, col: 5 });
        match h.emulator.clean_selection_text() {
            SelectionText::Text(text) => assert_eq!(text, "https://a.io/x"),
            other => panic!("no selection: {other:?}"),
        }
    }

    #[test]
    fn a_selection_of_blanks_copies_as_empty_not_as_nothing() {
        let h = harness();
        h.emulator.feed(b"a    b");
        select(&h, SelectOp::Start { row: 0, col: 1 });
        select(&h, SelectOp::Extend { row: 0, col: 3 });
        assert_eq!(
            h.emulator.clean_selection_text(),
            SelectionText::Text(String::new())
        );
    }

    #[test]
    fn a_space_where_a_row_wrapped_is_kept() {
        let h = harness();
        assert_eq!(
            h.emulator.try_resize(GridSize { cols: 12, rows: 6 }),
            ResizeOutcome::Applied
        );
        h.emulator.feed(b"ab cd ef gh ijk   \r\nnext");
        select(&h, SelectOp::Start { row: 0, col: 0 });
        select(&h, SelectOp::Extend { row: 2, col: 11 });
        assert_eq!(selected(&h), "ab cd ef gh ijk\nnext");
        // Inner spaces of an unwrapped row stay too.
        select(&h, SelectOp::Start { row: 0, col: 1 });
        select(&h, SelectOp::Extend { row: 0, col: 3 });
        assert_eq!(selected(&h), "b c");
    }

    #[test]
    fn a_selection_starting_on_half_a_wide_character_takes_it_whole() {
        let h = harness();
        h.emulator.feed("A\u{754c}B".as_bytes());
        select(&h, SelectOp::Start { row: 0, col: 2 });
        select(&h, SelectOp::Extend { row: 0, col: 3 });
        assert_eq!(selected(&h), "\u{754c}B");
        // The same cells dragged the other way.
        select(&h, SelectOp::Start { row: 0, col: 3 });
        select(&h, SelectOp::Extend { row: 0, col: 2 });
        assert_eq!(selected(&h), "\u{754c}B");
        assert_eq!(
            h.snapshot().selection,
            Some(SelectedRange {
                first: (0, 1),
                last: (0, 3)
            })
        );
    }

    #[test]
    fn a_combining_mark_added_to_a_selected_cell_clears_the_selection() {
        let h = harness();
        h.emulator.feed(b"e");
        select(&h, SelectOp::Start { row: 0, col: 0 });
        select(&h, SelectOp::Extend { row: 0, col: 0 });
        assert_eq!(selected(&h), "e");
        h.emulator.feed("\u{301}".as_bytes());
        assert_eq!(h.emulator.selection_text(), SelectionText::Nothing);
        // A mark already there when it was selected is copied with it.
        h.emulator.feed("\r\na\u{301}".as_bytes());
        select(&h, SelectOp::Start { row: 1, col: 0 });
        select(&h, SelectOp::Extend { row: 1, col: 0 });
        assert_eq!(selected(&h), "a\u{301}");
    }

    #[test]
    fn a_selection_ending_on_half_a_wide_character_takes_it_whole() {
        let h = harness();
        h.emulator.feed("A\u{754c}B".as_bytes());
        let whole = Some(SelectedRange {
            first: (0, 0),
            last: (0, 2),
        });
        select(&h, SelectOp::Start { row: 0, col: 0 });
        select(&h, SelectOp::Extend { row: 0, col: 1 });
        assert_eq!(selected(&h), "A\u{754c}");
        assert_eq!(h.snapshot().selection, whole);
        select(&h, SelectOp::Start { row: 0, col: 1 });
        select(&h, SelectOp::Extend { row: 0, col: 0 });
        assert_eq!(selected(&h), "A\u{754c}");
        assert_eq!(h.snapshot().selection, whole);
    }

    #[test]
    fn select_all_takes_the_screen_as_shown_and_clear_drops_it() {
        let h = harness();
        h.emulator.feed(b"first\r\nsecond");
        select(&h, SelectOp::All);
        assert_eq!(selected(&h), "first\nsecond");
        select(&h, SelectOp::Clear);
        assert_eq!(h.emulator.selection_text(), SelectionText::Nothing);
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
