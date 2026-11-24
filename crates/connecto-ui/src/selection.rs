//! A drag selection. The session keeps the selected cells in the terminal's
//! own lines, so the selection stays on its text while the view scrolls and
//! copies text scrolled out of view; the pane only tracks the gesture.

use connecto_core::{SelectOp, TermSnapshot};
use egui::Vec2;

/// A cell position as `(row, col)`.
pub(crate) type CellPos = (u16, u16);

/// A selection the user made. It is a user gesture and nothing else creates
/// one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    /// A start or select-all the session was too busy to take, sent again on
    /// the next frame.
    pub(crate) unsent: Option<SelectOp>,
    /// The drag's last cell, until the session has taken it.
    pub(crate) unsent_extend: Option<CellPos>,
}

impl Selection {
    pub(crate) const fn new(op: SelectOp) -> Self {
        Self {
            unsent: Some(op),
            unsent_extend: None,
        }
    }
}

/// The cell under a point, clamped to the screen.
pub(crate) fn cell_at(offset: Vec2, width: f32, height: f32, snapshot: &TermSnapshot) -> CellPos {
    let index = |value: f32, size: f32, count: u16| {
        if size <= 0.0 || count == 0 {
            return 0;
        }
        ((value / size).floor().max(0.0) as u16).min(count - 1)
    };
    (
        index(offset.y, height, snapshot.rows),
        index(offset.x, width, snapshot.cols),
    )
}

/// Lines to scroll while a drag is held past the top (positive, back in time)
/// or the bottom (negative) of a grid `rows` tall, for a pointer `y` below the
/// grid's top. One line per row of overshoot, at least one.
pub(crate) fn edge_scroll(y: f32, height: f32, rows: u16) -> i32 {
    if height <= 0.0 {
        return 0;
    }
    let bottom = f32::from(rows) * height;
    if y < 0.0 {
        (-y / height).ceil().max(1.0) as i32
    } else if y >= bottom {
        -((y - bottom) / height).floor().max(1.0) as i32
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn points_map_to_cells_and_clamp_to_the_screen() {
        let snapshot = TermSnapshot {
            cols: 6,
            rows: 2,
            ..TermSnapshot::default()
        };
        let cell = |x, y| cell_at(Vec2::new(x, y), 10.0, 20.0, &snapshot);
        assert_eq!(cell(25.0, 21.0), (1, 2));
        assert_eq!(cell(-5.0, -5.0), (0, 0));
        assert_eq!(cell(999.0, 999.0), (1, 5));
    }

    #[test]
    fn a_drag_past_an_edge_scrolls_toward_it() {
        // A grid of 10 rows, 20 points each.
        assert_eq!(edge_scroll(50.0, 20.0, 10), 0);
        assert_eq!(edge_scroll(-1.0, 20.0, 10), 1);
        assert_eq!(edge_scroll(-45.0, 20.0, 10), 3);
        assert_eq!(edge_scroll(200.0, 20.0, 10), -1);
        assert_eq!(edge_scroll(265.0, 20.0, 10), -3);
        assert_eq!(edge_scroll(-5.0, 0.0, 10), 0);
    }
}
