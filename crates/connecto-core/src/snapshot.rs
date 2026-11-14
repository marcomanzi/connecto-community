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
pub struct CellFlags(u8);

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
