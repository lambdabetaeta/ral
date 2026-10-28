//! LOCAL PATCH: shared grapheme layout for rendering, measurement, and hit-testing.

use std::ops::Range;

use crate::Index2;
use unicode_segmentation::UnicodeSegmentation;
use unicode_width::UnicodeWidthStr;

/// One indivisible terminal grapheme, indexed in Unicode scalar coordinates.
#[derive(Debug, Clone)]
pub(crate) struct Glyph {
    pub(crate) bytes: Range<usize>,
    pub(crate) scalars: Range<usize>,
    pub(crate) position: Index2,
    pub(crate) width: usize,
    pub(crate) replacement: bool,
}

/// Layout of a logical line. Data columns count scalars; screen columns count cells.
#[derive(Debug, Clone)]
pub struct LineGeometry {
    pub(crate) glyphs: Vec<Glyph>,
    end: Index2,
    scalar_len: usize,
    rows: usize,
}

impl LineGeometry {
    /// Lay out a line, wrapping only between extended grapheme clusters.
    /// A cluster wider than the viewport is displayed as a replacement character.
    /// A zero-width viewport has no drawable cells, but still occupies one row.
    #[must_use]
    pub fn new(text: &str, width: usize, tab_width: usize, wrap: bool) -> Self {
        let mut glyphs = Vec::new();
        let mut position = Index2::new(0, 0);
        let mut scalar_len = 0;
        for (byte, grapheme) in text.grapheme_indices(true) {
            let start = scalar_len;
            scalar_len += grapheme.chars().count();
            let natural_width = if grapheme == "\t" {
                tab_width
            } else {
                grapheme.width()
            };
            let replacement = width > 0 && natural_width > width;
            let cells = if replacement { 1 } else { natural_width };
            if wrap && width > 0 && position.col > 0 && position.col + cells > width {
                position.row += 1;
                position.col = 0;
            }
            glyphs.push(Glyph {
                bytes: byte..byte + grapheme.len(),
                scalars: start..scalar_len,
                position,
                width: cells,
                replacement,
            });
            position.col += cells;
        }
        let rows = position.row + 1;
        if wrap && width > 0 && position.col >= width {
            position.row += 1;
            position.col = 0;
        }
        Self { glyphs, end: position, scalar_len, rows }
    }

    /// Number of content rows, excluding an insertion cursor after a full final row.
    #[must_use]
    pub fn rows(&self) -> usize {
        self.rows
    }

    /// Number of rows needed to include a scalar-indexed cursor.
    #[must_use]
    pub fn rows_with_cursor(&self, column: usize) -> usize {
        self.rows.max(self.position(column).row + 1)
    }

    /// Clamp a scalar column to the containing cluster's start, or the line end.
    #[must_use]
    pub fn snap_column(&self, column: usize) -> usize {
        self.glyphs.iter().find(|g| column < g.scalars.end)
            .map_or(self.scalar_len, |g| g.scalars.start)
    }

    /// Screen position of a scalar column; cluster interiors snap to their start.
    #[must_use]
    pub fn position(&self, column: usize) -> Index2 {
        self.glyphs.iter().find(|g| column < g.scalars.end)
            .map_or(self.end, |g| g.position)
    }

    /// Scalar column under a cell; both halves of a wide cluster hit its start.
    #[must_use]
    pub fn hit_test(&self, row: usize, column: usize) -> usize {
        for glyph in &self.glyphs {
            if glyph.position.row > row {
                return glyph.scalars.start;
            }
            if glyph.position.row == row && column < glyph.position.col + glyph.width {
                return glyph.scalars.start;
            }
        }
        self.scalar_len
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn clusters_are_indivisible() {
        let layout = LineGeometry::new("ae\u{301}👩\u{200d}💻🇬🇷z", 4, 2, true);
        assert_eq!(layout.position(2), Index2::new(0, 1));
        assert_eq!(layout.position(3), Index2::new(0, 2));
        assert_eq!(layout.position(6), Index2::new(1, 0));
        assert_eq!(layout.hit_test(0, 3), 3);
        assert_eq!(layout.hit_test(1, 1), 6);
        assert_eq!(layout.rows(), 2);
    }

    #[test]
    fn full_line_reserves_cursor_row_only_when_needed() {
        let layout = LineGeometry::new("界界", 4, 2, true);
        assert_eq!(layout.rows(), 1);
        assert_eq!(layout.rows_with_cursor(2), 2);
        assert_eq!(layout.position(2), Index2::new(1, 0));
    }

    #[test]
    fn narrow_and_zero_width_layouts_terminate() {
        let layout = LineGeometry::new("界\t", 1, 2, true);
        assert_eq!(layout.rows(), 2);
        assert_eq!(layout.rows_with_cursor(2), 3);
        assert_eq!(layout.hit_test(1, 0), 1);
        assert_eq!(LineGeometry::new("界\t", 0, 2, true).rows(), 1);
    }
}
