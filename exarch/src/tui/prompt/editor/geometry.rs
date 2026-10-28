use super::PromptEditor;
use edtui::LineGeometry;

/// Columns a tab occupies, for layout and rendering alike.
pub(super) const TAB_WIDTH: usize = 2;

pub(super) fn wraps(width: u16) -> bool {
    width >= 2
}

impl PromptEditor {
    /// Wrapped content and its insertion cursor, plus the host's two border rows.
    pub(in crate::tui::prompt) fn height_hint(&self, width: u16, max: u16) -> u16 {
        if max <= 3 || width == 0 {
            return max;
        }
        let limit = usize::from(max - 2);
        let mut visual = 0usize;
        for (row, line) in self.state.lines.iter_row().enumerate() {
            let text: String = line.iter().collect();
            let geometry = LineGeometry::new(&text, usize::from(width), TAB_WIDTH, wraps(width));
            let rows = if row == self.row() {
                geometry.rows_with_cursor(self.state.cursor.col)
            } else {
                geometry.rows()
            };
            visual = visual.saturating_add(rows);
            if visual >= limit {
                return max;
            }
        }
        u16::try_from(visual.saturating_add(2))
            .unwrap_or(u16::MAX)
            .max(3)
            .min(max)
    }
}
