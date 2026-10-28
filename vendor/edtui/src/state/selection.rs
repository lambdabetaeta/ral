// LOCAL PATCH: project characterwise selections onto whole graphemes.
use std::{cmp::Ordering, ops::Range};
use unicode_segmentation::UnicodeSegmentation;

/// Projects an inclusive scalar interval onto whole graphemes (byte offsets).
#[must_use]
pub fn grapheme_range(text: &str, start: usize, end: usize) -> Range<usize> {
    let mut scalar = 0;
    let mut first = text.len();
    let mut last = text.len();
    for (byte, cluster) in text.grapheme_indices(true) {
        let next = scalar + cluster.chars().count();
        if scalar <= end && next > start {
            first = first.min(byte);
            last = byte + cluster.len();
        }
        scalar = next;
    }
    first..last
}

use jagged::index::RowIndex;

use crate::{Index2, Lines};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Selection {
    pub start: Index2,
    pub end: Index2,
    pub line_mode: bool,
    pub anchor: Option<Index2>,
}

impl Selection {
    #[must_use]
    pub fn new(start: Index2, end: Index2) -> Self {
        Self {
            start,
            end,
            line_mode: false,
            anchor: None,
        }
    }

    pub fn line_mode(mut self) -> Self {
        self.line_mode = true;
        self.anchor = Some(self.start);
        self
    }

    #[must_use]
    pub fn contains(&self, pos: &Index2) -> bool {
        if self.line_mode {
            return self.contains_row(pos.row);
        }

        let (start, end) = if self.start < self.end {
            (&self.start, &self.end)
        } else {
            (&self.end, &self.start)
        };
        let (st_row, st_col) = (start.row, start.col);
        let (en_row, en_col) = (end.row, end.col);

        match (pos.row, pos.col) {
            (line, _) if line > st_row && line < en_row => true,
            (line, column) if line > st_row && line == en_row => column <= en_col,
            (line, column) if line == st_row && line < en_row => column >= st_col,
            (line, column) if line == st_row && line == en_row => {
                column <= en_col && column >= st_col
            }
            _ => false,
        }
    }

    #[must_use]
    pub fn contains_row(&self, row_index: usize) -> bool {
        let (start_row, end_row) = if self.start.row < self.end.row {
            (self.start.row, self.end.row)
        } else {
            (self.end.row, self.start.row)
        };
        if row_index >= start_row && row_index <= end_row {
            return true;
        }
        false
    }

    #[must_use]
    pub fn start(&self) -> Index2 {
        if self.is_reversed() {
            return self.end;
        }
        self.start
    }

    #[must_use]
    pub fn end(&self) -> Index2 {
        if self.is_reversed() {
            return self.start;
        }
        self.end
    }

    #[must_use]
    fn is_reversed(&self) -> bool {
        self.start.row > self.end.row
            || self.start.row == self.end.row && self.start.col > self.end.col
    }

    fn reverse(&mut self) {
        (self.start, self.end) = (self.end, self.start);
    }

    /// Canonical whole-grapheme interval in the newline-joined text.
    #[must_use]
    pub fn byte_range(&self, lines: &Lines) -> Range<usize> {
        let text = lines.to_string();
        let offset = |index: Index2| {
            lines.iter_row().take(index.row).map(|row| row.len() + 1).sum::<usize>()
                + index.col.min(lines.len_col(index.row).unwrap_or(0))
        };
        if self.line_mode {
            let start = offset(Index2::new(self.start().row, 0));
            let end = offset(Index2::new(self.end().row + 1, 0));
            let byte = |pos| text.char_indices().nth(pos).map_or(text.len(), |(i, _)| i);
            return byte(start)..byte(end);
        }
        grapheme_range(&text, offset(self.start()), offset(self.end()))
    }

    pub(crate) fn grapheme_columns(&self, line: &[char], row: usize) -> Option<Range<usize>> {
        if !self.contains_row(row) {
            return None;
        }
        if self.line_mode {
            return Some(0..line.len());
        }
        let (start, end) = self.get_selected_columns_in_row(row, line.len())?;
        let text: String = line.iter().collect();
        let bytes = grapheme_range(&text, start, end);
        Some(text[..bytes.start].chars().count()..text[..bytes.end].chars().count())
    }

    /// Copies a selection from `Lines`.
    #[must_use]
    pub fn copy_from(&self, lines: &Lines) -> Lines {
        if self.line_mode {
            let mut st = self.start();
            let mut en = self.end();
            st.col = 0;
            en.col = lines.last_col_index(en.row);

            let mut lines = lines.copy_range(st..=en);
            lines.insert(RowIndex::new(0), vec![]);

            return lines;
        }

        let text = lines.to_string();
        Lines::from(&text[self.byte_range(lines)])
    }

    /// Extracts a selection from `Lines`.
    #[must_use]
    pub fn extract_from(&self, lines: &mut Lines) -> Lines {
        if self.line_mode {
            let st = Index2::new(self.start().row, 0);
            let en = Index2::new(self.end().row, lines.last_col_index(self.end().row));

            let mut lines = lines.extract(st..=en);
            lines.insert(RowIndex::new(0), vec![]);

            return lines;
        }

        let mut text = lines.to_string();
        let range = self.byte_range(lines);
        if range.is_empty() {
            return Lines::default();
        }
        let extracted = Lines::from(&text[range.clone()]);
        text.replace_range(range, "");
        *lines = Lines::from(text.as_str());
        extracted
    }

    /// Returns the start and end column of the selection in the given row.
    /// If the selection does not intersect with the row, the function returns None.
    #[must_use]
    pub(crate) fn get_selected_columns_in_row(
        &self,
        row_index: usize,
        row_len: usize,
    ) -> Option<(usize, usize)> {
        let (start, end) = (self.start(), self.end());

        let start_col = match start.row.cmp(&row_index) {
            Ordering::Less => 0,
            Ordering::Greater => return None,
            Ordering::Equal => start.col.min(row_len),
        };

        let end_col = match end.row.cmp(&row_index) {
            Ordering::Less => return None,
            Ordering::Greater => row_len,
            Ordering::Equal => end.col.min(row_len),
        };

        Some((start_col, end_col))
    }
}

/// Set the selections end positions
pub(crate) fn set_selection(selection: &mut Option<Selection>, index: Index2) {
    if let Some(selection) = selection {
        selection.end = index;
    }
}

/// Set the selection end position with line mode aware handling
pub(crate) fn set_selection_with_lines(
    selection: &mut Option<Selection>,
    index: Index2,
    lines: &crate::Lines,
) {
    if let Some(sel) = selection {
        if sel.line_mode {
            let anchor_row = sel.anchor.map(|a| a.row).unwrap_or(sel.start.row);
            let cursor_row = index.row;

            let (top_row, bottom_row) = if anchor_row <= cursor_row {
                (anchor_row, cursor_row)
            } else {
                (cursor_row, anchor_row)
            };

            sel.start = Index2::new(top_row, 0);
            let end_col = lines.len_col(bottom_row).unwrap_or(0).saturating_sub(1);
            sel.end = Index2::new(bottom_row, end_col);
        } else {
            sel.end = index;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    fn test_data() -> Lines {
        Lines::from("Hello\nWorld")
    }

    #[test]
    fn characterwise_extraction_preserves_unselected_newlines() {
        for (start, end, removed, remaining) in [
            ((0, 0), (0, 2), "foo", "\nbar"),
            ((1, 0), (1, 2), "bar", "foo\n"),
            ((0, 1), (1, 1), "oo\nba", "fr"),
        ] {
            for (start, end) in [(start, end), (end, start)] {
                let mut lines = Lines::from("foo\nbar");
                let selection = Selection::new(
                    Index2::new(start.0, start.1),
                    Index2::new(end.0, end.1),
                );
                assert_eq!(selection.extract_from(&mut lines).to_string(), removed);
                assert_eq!(lines.to_string(), remaining);
            }
        }
    }

    #[test]
    fn test_copy_from() {
        let data = test_data();
        let selection = Selection::new(Index2::new(0, 3), Index2::new(1, 1));

        assert_eq!(selection.copy_from(&data), Lines::from("lo\nWo"));
    }

    #[test]
    fn test_copy_from_out_of_bounds() {
        let data = test_data();
        let selection = Selection::new(Index2::new(0, 5), Index2::new(1, 1));

        assert_eq!(selection.copy_from(&data), Lines::from("\nWo"));
    }

    #[test]
    fn test_selection_columns_in_row() {
        // given
        let selection = Selection::new(Index2::new(0, 2), Index2::new(1, 1));

        // when
        let selection_columns = selection.get_selected_columns_in_row(0, 5);

        // then
        assert_eq!(selection_columns, Some((2, 5)));

        // when
        let selection = Selection::new(Index2::new(1, 2), Index2::new(1, 1));
        let selection_columns = selection.get_selected_columns_in_row(0, 5);

        // then
        assert_eq!(selection_columns, None);
    }
}
