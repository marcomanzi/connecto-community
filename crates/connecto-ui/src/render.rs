//! Painting a terminal snapshot.
//!
//! Text is painted in runs of equally styled ASCII, each placed at its exact
//! column. Everything else is painted one glyph at a time, so a fallback font
//! with a different advance width cannot shift the rest of the row.

use connecto_core::{Cell, CellFlags, Cursor, CursorShape, GridSize, Rgb, TermSnapshot};
use egui::text::{LayoutJob, TextFormat};
use egui::{Color32, FontFamily, FontId, Painter, Pos2, Rect, Stroke, StrokeKind, Vec2};

use crate::fonts;

const FONT_SIZE: f32 = 13.0;

/// Size of one character cell, in points.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) struct CellMetrics {
    pub(crate) width: f32,
    pub(crate) height: f32,
}

impl CellMetrics {
    pub(crate) fn measure(ctx: &egui::Context) -> Self {
        let font = regular_font();
        ctx.fonts_mut(|fonts| Self {
            width: fonts.glyph_width(&font, 'M'),
            height: fonts.row_height(&font),
        })
    }

    /// How many whole cells fit in `area`.
    pub(crate) fn grid_size(self, area: Vec2) -> GridSize {
        let count = |length: f32, cell: f32| {
            if cell > 0.0 {
                (length / cell).floor().clamp(1.0, f32::from(u16::MAX)) as u16
            } else {
                1
            }
        };
        GridSize {
            cols: count(area.x, self.width).max(2),
            rows: count(area.y, self.height),
        }
    }
}

fn regular_font() -> FontId {
    FontId::new(FONT_SIZE, FontFamily::Monospace)
}

fn color(rgb: Rgb) -> Color32 {
    Color32::from_rgb(rgb.r, rgb.g, rgb.b)
}

/// A horizontal span of cells sharing one background colour.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct BackgroundRun {
    pub(crate) start: usize,
    pub(crate) len: usize,
    pub(crate) bg: Rgb,
}

/// A horizontal span of text sharing one style.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct TextRun {
    pub(crate) start: usize,
    pub(crate) text: String,
    pub(crate) fg: Rgb,
    pub(crate) flags: CellFlags,
}

fn style_flags(flags: CellFlags) -> [bool; 4] {
    [
        flags.contains(CellFlags::BOLD),
        flags.contains(CellFlags::ITALIC),
        flags.contains(CellFlags::UNDERLINE),
        flags.contains(CellFlags::STRIKEOUT),
    ]
}

fn has_line_decoration(flags: CellFlags) -> bool {
    flags.contains(CellFlags::UNDERLINE) || flags.contains(CellFlags::STRIKEOUT)
}

/// Split one row into the background spans that differ from `default_bg`.
pub(crate) fn background_runs(row: &[Cell], default_bg: Rgb) -> Vec<BackgroundRun> {
    let mut runs: Vec<BackgroundRun> = Vec::new();
    for (col, cell) in row.iter().enumerate() {
        if cell.bg == default_bg {
            continue;
        }
        match runs.last_mut() {
            Some(run) if run.bg == cell.bg && run.start + run.len == col => run.len += 1,
            _ => runs.push(BackgroundRun {
                start: col,
                len: 1,
                bg: cell.bg,
            }),
        }
    }
    runs
}

/// Split one row into text spans. Blank cells without decoration are skipped.
pub(crate) fn text_runs(row: &[Cell]) -> Vec<TextRun> {
    let mut runs: Vec<TextRun> = Vec::new();
    let mut open = false;
    for (col, cell) in row.iter().enumerate() {
        if cell.flags.contains(CellFlags::WIDE_SPACER) {
            open = false;
            continue;
        }
        if cell.ch == ' ' && !has_line_decoration(cell.flags) {
            open = false;
            continue;
        }
        if !cell.ch.is_ascii() {
            open = false;
            runs.push(TextRun {
                start: col,
                text: cell.ch.to_string(),
                fg: cell.fg,
                flags: cell.flags,
            });
            continue;
        }
        match runs.last_mut() {
            Some(run)
                if open
                    && run.fg == cell.fg
                    && style_flags(run.flags) == style_flags(cell.flags) =>
            {
                run.text.push(cell.ch);
            }
            _ => {
                runs.push(TextRun {
                    start: col,
                    text: cell.ch.to_string(),
                    fg: cell.fg,
                    flags: cell.flags,
                });
                open = true;
            }
        }
    }
    runs
}

fn text_format(run: &TextRun) -> TextFormat {
    let fg = color(run.fg);
    let family = if run.flags.contains(CellFlags::BOLD) {
        fonts::bold_family()
    } else {
        FontFamily::Monospace
    };
    let line = |on: bool| {
        if on {
            Stroke::new(1.0, fg)
        } else {
            Stroke::NONE
        }
    };
    TextFormat {
        font_id: FontId::new(FONT_SIZE, family),
        color: fg,
        italics: run.flags.contains(CellFlags::ITALIC),
        underline: line(run.flags.contains(CellFlags::UNDERLINE)),
        strikethrough: line(run.flags.contains(CellFlags::STRIKEOUT)),
        ..TextFormat::default()
    }
}

/// Paint `snapshot` with its top-left corner at `origin`.
pub(crate) fn paint(
    painter: &Painter,
    origin: Pos2,
    snapshot: &TermSnapshot,
    metrics: CellMetrics,
    focused: bool,
) {
    let cell_rect = |col: usize, row: u16, len: usize| {
        Rect::from_min_size(
            origin + Vec2::new(col as f32 * metrics.width, f32::from(row) * metrics.height),
            Vec2::new(len as f32 * metrics.width, metrics.height),
        )
    };

    let mut row_cells: Vec<Cell> = Vec::with_capacity(usize::from(snapshot.cols));
    for row in 0..snapshot.rows {
        row_cells.clear();
        row_cells.extend_from_slice(snapshot.row(row));
        if focused {
            invert_block_cursor(&mut row_cells, snapshot.cursor, row);
        }

        for run in background_runs(&row_cells, snapshot.default_bg) {
            painter.rect_filled(cell_rect(run.start, row, run.len), 0.0, color(run.bg));
        }
        for run in text_runs(&row_cells) {
            let format = text_format(&run);
            let start = run.start;
            let fg = color(run.fg);
            let mut job = LayoutJob::simple_singleline(run.text, FontId::default(), fg);
            if let Some(section) = job.sections.first_mut() {
                section.format = format;
            }
            let galley = painter.fonts_mut(|fonts| fonts.layout_job(job));
            painter.galley(cell_rect(start, row, 1).min, galley, fg);
        }
    }

    if let Some(cursor) = snapshot.cursor {
        paint_cursor(
            painter,
            cursor,
            snapshot,
            focused,
            cell_rect(usize::from(cursor.col), cursor.row, 1),
        );
    }
}

/// Swap the colours of the cell under a solid block cursor.
fn invert_block_cursor(row_cells: &mut [Cell], cursor: Option<Cursor>, row: u16) {
    let Some(cursor) = cursor.filter(|c| c.row == row && c.shape == CursorShape::Block) else {
        return;
    };
    if let Some(cell) = row_cells.get_mut(usize::from(cursor.col)) {
        std::mem::swap(&mut cell.fg, &mut cell.bg);
    }
}

fn paint_cursor(
    painter: &Painter,
    cursor: Cursor,
    snapshot: &TermSnapshot,
    focused: bool,
    rect: Rect,
) {
    let fg = snapshot
        .row(cursor.row)
        .get(usize::from(cursor.col))
        .map_or(Color32::WHITE, |cell| color(cell.fg));
    match cursor.shape {
        CursorShape::Block if focused => {}
        CursorShape::Block => {
            painter.rect_stroke(rect, 0.0, Stroke::new(1.0, fg), StrokeKind::Inside);
        }
        CursorShape::Beam => {
            let bar = Rect::from_min_size(rect.min, Vec2::new(1.5, rect.height()));
            painter.rect_filled(bar, 0.0, fg);
        }
        CursorShape::Underline => {
            let min = Pos2::new(rect.min.x, rect.max.y - 1.5);
            let bar = Rect::from_min_size(min, Vec2::new(rect.width(), 1.5));
            painter.rect_filled(bar, 0.0, fg);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const FG: Rgb = Rgb::new(200, 200, 200);
    const OTHER: Rgb = Rgb::new(10, 200, 10);
    const BG: Rgb = Rgb::new(0, 0, 0);

    fn cell(ch: char) -> Cell {
        Cell {
            ch,
            fg: FG,
            bg: BG,
            flags: CellFlags::default(),
        }
    }

    fn row(text: &str) -> Vec<Cell> {
        text.chars().map(cell).collect()
    }

    #[test]
    fn equally_styled_ascii_forms_one_run() {
        let runs = text_runs(&row("hello"));
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].text, "hello");
        assert_eq!(runs[0].start, 0);
    }

    #[test]
    fn spaces_split_runs_and_keep_later_text_on_its_column() {
        let runs = text_runs(&row("ab   cd"));
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].start, 5);
        assert_eq!(runs[1].text, "cd");
    }

    #[test]
    fn a_colour_change_starts_a_new_run() {
        let mut cells = row("ab");
        cells[1].fg = OTHER;
        let runs = text_runs(&cells);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].fg, OTHER);
    }

    #[test]
    fn a_non_ascii_glyph_stands_alone_on_its_column() {
        let runs = text_runs(&row("a─b"));
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[1].text, "─");
        assert_eq!(runs[1].start, 1);
        assert_eq!(runs[2].start, 2);
    }

    #[test]
    fn the_spacer_of_a_wide_glyph_draws_nothing() {
        let mut cells = row("界 x");
        cells[0].flags.insert(CellFlags::WIDE);
        cells[1].flags.insert(CellFlags::WIDE_SPACER);
        let runs = text_runs(&cells);
        assert_eq!(runs.len(), 2);
        assert_eq!(runs[1].start, 2);
    }

    #[test]
    fn an_underlined_space_is_still_drawn() {
        let mut cells = row(" ");
        cells[0].flags.insert(CellFlags::UNDERLINE);
        assert_eq!(text_runs(&cells).len(), 1);
    }

    #[test]
    fn default_background_paints_nothing() {
        assert!(background_runs(&row("abc"), BG).is_empty());
    }

    #[test]
    fn adjacent_backgrounds_merge() {
        let mut cells = row("abcd");
        cells[1].bg = OTHER;
        cells[2].bg = OTHER;
        assert_eq!(
            background_runs(&cells, BG),
            vec![BackgroundRun {
                start: 1,
                len: 2,
                bg: OTHER
            }]
        );
    }

    #[test]
    fn grid_size_rounds_down_and_never_reaches_zero() {
        let metrics = CellMetrics {
            width: 8.0,
            height: 16.0,
        };
        let size = metrics.grid_size(Vec2::new(85.0, 40.0));
        assert_eq!((size.cols, size.rows), (10, 2));
        let tiny = metrics.grid_size(Vec2::new(0.0, 0.0));
        assert_eq!((tiny.cols, tiny.rows), (2, 1));
    }

    #[test]
    fn zero_sized_cells_do_not_divide_by_zero() {
        let metrics = CellMetrics {
            width: 0.0,
            height: 0.0,
        };
        let size = metrics.grid_size(Vec2::new(100.0, 100.0));
        assert_eq!((size.cols, size.rows), (2, 1));
    }

    #[test]
    fn the_block_cursor_inverts_only_its_own_cell() {
        let mut cells = row("ab");
        let cursor = Cursor {
            col: 1,
            row: 3,
            shape: CursorShape::Block,
        };
        invert_block_cursor(&mut cells, Some(cursor), 3);
        assert_eq!(cells[0].fg, FG);
        assert_eq!(cells[1].fg, BG);
        assert_eq!(cells[1].bg, FG);
    }

    #[test]
    fn the_cursor_on_another_row_changes_nothing() {
        let mut cells = row("ab");
        let cursor = Cursor {
            col: 1,
            row: 9,
            shape: CursorShape::Block,
        };
        invert_block_cursor(&mut cells, Some(cursor), 3);
        assert_eq!(cells[1].fg, FG);
    }
}
