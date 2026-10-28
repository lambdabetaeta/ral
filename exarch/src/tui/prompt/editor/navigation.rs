use super::{PromptEditor, geometry};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use edtui::{Index2, LineGeometry};

impl PromptEditor {
    pub(super) fn reset_navigation(&mut self) {
        self.preferred_column = None;
    }

    fn line_geometry(&self, row: usize) -> LineGeometry {
        LineGeometry::new(
            &self.line(row).unwrap_or_default(),
            usize::from(self.last_area.width),
            2,
            geometry::wraps(self.last_area.width),
        )
    }

    pub(super) fn handle_vertical_key(&mut self, key: KeyEvent) -> bool {
        if !self.is_insert_mode() {
            return false;
        }
        let down = match (key.code, key.modifiers) {
            (KeyCode::Up, KeyModifiers::NONE) | (KeyCode::Char('p'), KeyModifiers::CONTROL) => {
                false
            }
            (KeyCode::Down, KeyModifiers::NONE) | (KeyCode::Char('n'), KeyModifiers::CONTROL) => {
                true
            }
            _ => return false,
        };
        let mut row = self.state.cursor.row;
        let mut layout = self.line_geometry(row);
        let position = layout.position(self.state.cursor.col);
        let column = *self.preferred_column.get_or_insert(position.col);
        let end = self.state.lines.len_col(row).unwrap_or(0);
        let target = if down {
            if position.row + 1 < layout.rows_with_cursor(end) {
                position.row + 1
            } else if row + 1 < self.row_count() {
                row += 1;
                layout = self.line_geometry(row);
                0
            } else {
                return true;
            }
        } else if position.row > 0 {
            position.row - 1
        } else if row > 0 {
            row -= 1;
            layout = self.line_geometry(row);
            layout.rows() - 1
        } else {
            return true;
        };
        let mut col = layout.hit_test(target, column);
        // A short wrapped row must not hit the next row's first cluster.
        if layout.position(col).row > target {
            col = layout.snap_column(col.saturating_sub(1));
        }
        self.state.selection = None;
        self.state
            .insert_transaction(|state| state.move_to(Index2::new(row, col)));
        true
    }
}
