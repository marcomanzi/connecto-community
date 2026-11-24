//! Render-ready terminal state, copied out of the emulator.

use std::time::Duration;

/// Terminal size in character cells.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct GridSize {
    pub cols: u16,
    pub rows: u16,
}

/// An sRGB colour, already resolved from the palette.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct Rgb {
    pub r: u8,
    pub g: u8,
    pub b: u8,
}

impl Rgb {
    #[must_use]
    pub const fn new(r: u8, g: u8, b: u8) -> Self {
        Self { r, g, b }
    }
}

/// Text attributes of one cell, as a small bit set.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Default)]
pub struct CellFlags(u16);

impl CellFlags {
    pub const BOLD: Self = Self(1);
    pub const ITALIC: Self = Self(1 << 1);
    pub const UNDERLINE: Self = Self(1 << 2);
    pub const STRIKEOUT: Self = Self(1 << 3);
    /// The glyph occupies this cell and the next one.
    pub const WIDE: Self = Self(1 << 4);
    /// The right half of a wide glyph; nothing is drawn here.
    pub const WIDE_SPACER: Self = Self(1 << 5);
    /// Set on the last cell of a row that the terminal soft-wrapped: the text
    /// goes on in the next row (no line break was sent).
    pub const WRAPPED: Self = Self(1 << 6);
    /// Faint (dimmed, SGR 2) text: what a tool draws as a hint rather than as
    /// content, such as a placeholder or a suggested prompt. `fg` is already
    /// dimmed; the flag says why.
    pub const DIM: Self = Self(1 << 7);
    /// Reverse video (SGR 7): `fg` and `bg` are already swapped. A TUI that
    /// hides the terminal cursor draws its own this way.
    pub const INVERSE: Self = Self(1 << 8);
    /// The blank left in a row's last column when a wide glyph did not fit
    /// there and went on to the next row. Nothing is drawn here.
    pub const LEADING_WIDE_SPACER: Self = Self(1 << 9);

    #[must_use]
    pub const fn contains(self, other: Self) -> bool {
        self.0 & other.0 == other.0
    }

    pub fn insert(&mut self, other: Self) {
        self.0 |= other.0;
    }
}

/// One character cell with resolved colours.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cell {
    pub ch: char,
    pub fg: Rgb,
    pub bg: Rgb,
    pub flags: CellFlags,
}

impl Default for Cell {
    fn default() -> Self {
        Self {
            ch: ' ',
            fg: Rgb::default(),
            bg: Rgb::default(),
            flags: CellFlags::default(),
        }
    }
}

/// Cursor drawing style.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CursorShape {
    Block,
    Underline,
    Beam,
}

/// A visible cursor position in viewport coordinates.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    pub col: u16,
    pub row: u16,
    pub shape: CursorShape,
}

/// A copy of the visible screen. The buffers are reused between frames.
#[derive(Debug, Clone, Default)]
pub struct TermSnapshot {
    pub cols: u16,
    pub rows: u16,
    /// Row-major, `cols * rows` entries.
    pub cells: Vec<Cell>,
    pub cursor: Option<Cursor>,
    pub modes: crate::InputModes,
    /// Sanitized title set by the application, if any.
    pub title: Option<String>,
    pub default_bg: Rgb,
    /// Lines scrolled back from the live screen. Zero means live.
    pub display_offset: usize,
    /// Changes whenever the emulator content changes.
    pub generation: u64,
    /// Poll again after this delay even if no output arrives. Set while a
    /// synchronized update is held back.
    pub repoll_after: Option<Duration>,
    /// The selection, as its first and last cell (both included) in rows of
    /// this view. A row above the view is negative and one below is at least
    /// `rows`, since the selection can reach into text scrolled out of view.
    pub selection: Option<SelectedRange>,
}

/// A selection's first and last cell as `(row, col)`, `first <= last`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct SelectedRange {
    pub first: (i32, u16),
    pub last: (i32, u16),
}

impl SelectedRange {
    /// Selected columns per visible row, as `(row, start, end)` with `end`
    /// exclusive.
    #[must_use]
    pub fn spans(self, rows: u16, cols: u16) -> Vec<(u16, u16, u16)> {
        let top = self.first.0.max(0);
        let bottom = self.last.0.min(i32::from(rows) - 1);
        (top..=bottom)
            .filter_map(|row| {
                let start = if row == self.first.0 { self.first.1 } else { 0 };
                let end = if row == self.last.0 {
                    self.last.1.saturating_add(1)
                } else {
                    cols
                };
                let row = u16::try_from(row).ok()?;
                Some((row, start.min(cols), end.min(cols)))
            })
            .filter(|(_, start, end)| start < end)
            .collect()
    }
}

impl TermSnapshot {
    /// The cells of one row, or an empty slice when out of range.
    #[must_use]
    pub fn row(&self, row: u16) -> &[Cell] {
        let cols = usize::from(self.cols);
        let start = usize::from(row) * cols;
        self.cells.get(start..start + cols).unwrap_or(&[])
    }
}
