//! A drag selection over the visible screen.

use connecto_core::{CellFlags, TermSnapshot};
use egui::Vec2;

/// A cell position as `(row, col)`.
pub(crate) type CellPos = (u16, u16);

/// A selection in viewport coordinates. It is a user gesture and nothing else
/// creates one.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Selection {
    pub(crate) anchor: CellPos,
    pub(crate) head: CellPos,
}

impl Selection {
    pub(crate) const fn at(pos: CellPos) -> Self {
        Self {
            anchor: pos,
            head: pos,
        }
    }

    /// Every cell of the screen, or nothing for an empty one.
    pub(crate) fn all(snapshot: &TermSnapshot) -> Option<Self> {
        (snapshot.rows > 0 && snapshot.cols > 0).then(|| Self {
            anchor: (0, 0),
            head: (snapshot.rows - 1, snapshot.cols - 1),
        })
    }

    /// The cell under a point, clamped to the screen.
    pub(crate) fn cell_at(
        offset: Vec2,
        width: f32,
        height: f32,
        snapshot: &TermSnapshot,
    ) -> CellPos {
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

    fn ordered(self) -> (CellPos, CellPos) {
        if self.anchor <= self.head {
            (self.anchor, self.head)
        } else {
            (self.head, self.anchor)
        }
    }

    /// Selected columns per row, as `(row, start, end)` with `end` exclusive.
    pub(crate) fn spans(self, snapshot: &TermSnapshot) -> Vec<(u16, u16, u16)> {
        let (first, last) = self.ordered();
        (first.0..=last.0.min(snapshot.rows.saturating_sub(1)))
            .map(|row| {
                let start = if row == first.0 { first.1 } else { 0 };
                let end = if row == last.0 {
                    last.1.saturating_add(1)
                } else {
                    snapshot.cols
                };
                (row, start, end.min(snapshot.cols))
            })
            .collect()
    }

    /// The selected text. Trailing blanks go. A row filled to its last column
    /// is taken as wrapped and joins the next without a newline, so a long
    /// login URL copies whole. A line that exactly fills the row joins too.
    pub(crate) fn text(self, snapshot: &TermSnapshot) -> String {
        let mut out = String::new();
        let spans = self.spans(snapshot);
        let last = spans.len().saturating_sub(1);
        for (index, (row, start, end)) in spans.into_iter().enumerate() {
            let cells = snapshot.row(row);
            let selected = cells
                .get(usize::from(start)..usize::from(end))
                .unwrap_or(&[]);
            let line: String = selected
                .iter()
                .filter(|cell| !cell.flags.contains(CellFlags::WIDE_SPACER))
                .map(|cell| cell.ch)
                .collect();
            out.push_str(line.trim_end());
            let wrapped = end == snapshot.cols && cells.last().is_some_and(|c| c.ch != ' ');
            if index != last && !wrapped {
                out.push('\n');
            }
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use connecto_core::Cell;

    use super::*;

    fn screen(lines: [&str; 2]) -> TermSnapshot {
        let cols = 6;
        let cells = lines
            .iter()
            .flat_map(|line| {
                let padded = format!("{line:<6}");
                padded
                    .chars()
                    .map(|ch| Cell {
                        ch,
                        ..Cell::default()
                    })
                    .collect::<Vec<_>>()
            })
            .collect();
        TermSnapshot {
            cols,
            rows: 2,
            cells,
            ..TermSnapshot::default()
        }
    }

    #[test]
    fn one_row_is_copied_from_anchor_to_head_inclusive() {
        let selection = Selection {
            anchor: (0, 1),
            head: (0, 3),
        };
        assert_eq!(selection.text(&screen(["abcdef", ""])), "bcd");
    }

    #[test]
    fn a_backward_drag_selects_the_same_text() {
        let selection = Selection {
            anchor: (1, 1),
            head: (0, 4),
        };
        assert_eq!(selection.text(&screen(["abcde ", "uvwxyz"])), "e\nuv");
    }

    #[test]
    fn a_wrapped_line_copies_without_a_break() {
        let selection = Selection {
            anchor: (0, 0),
            head: (1, 5),
        };
        assert_eq!(selection.text(&screen(["https:", "//a.b"])), "https://a.b");
    }

    #[test]
    fn trailing_blanks_are_not_copied() {
        let selection = Selection {
            anchor: (0, 0),
            head: (1, 5),
        };
        assert_eq!(selection.text(&screen(["ab", "cd"])), "ab\ncd");
    }

    #[test]
    fn a_stale_selection_past_a_smaller_screen_is_harmless() {
        let selection = Selection {
            anchor: (0, 0),
            head: (9, 9),
        };
        assert_eq!(selection.text(&screen(["ab", "cd"])), "ab\ncd");
    }

    #[test]
    fn points_map_to_cells_and_clamp_to_the_screen() {
        let snapshot = screen(["", ""]);
        let cell = |x, y| Selection::cell_at(Vec2::new(x, y), 10.0, 20.0, &snapshot);
        assert_eq!(cell(25.0, 21.0), (1, 2));
        assert_eq!(cell(-5.0, -5.0), (0, 0));
        assert_eq!(cell(999.0, 999.0), (1, 5));
    }
}
