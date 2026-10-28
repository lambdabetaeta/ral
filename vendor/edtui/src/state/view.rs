// LOCAL PATCH: viewport offsets use the renderer's native grapheme layout.
use crate::{view::geometry::LineGeometry, view::LineNumbers, Index2, Lines};
use ratatui_core::layout::{Position, Rect};

/// Represents the (x, y) offset of the editor's viewport.
/// It represents the top-left local editor coordinate.
#[derive(Debug, Clone)]
pub(crate) struct ViewState {
    /// The offset of the viewport.
    pub(crate) viewport: Offset,
    /// Visual rows skipped within the first logical row.
    pub(crate) wrapped_row_offset: usize,
    /// The number of rows that are displayed on the viewport
    pub(crate) num_rows: usize,
    /// Sets the area (starting upper-left corner of the terminal window) where
    /// the editor text is rendered to.
    ///
    /// Required to calculate the mouse position in relation to the text within the editor.
    pub(crate) screen_area: Rect,
    /// Whether the lines are wrapped.
    pub(crate) wrap: bool,
    /// The number of spaces used to display a tab.
    pub(crate) tab_width: usize,
    /// Line numbers configuration.
    pub(crate) line_numbers: LineNumbers,
    /// The cursor's screen position, computed during the last render.
    /// This is the absolute position in terminal coordinates where the cursor should be displayed.
    pub(crate) cursor_screen_position: Option<Position>,
    /// Whether the editor is in single-line mode (blocks newline insertion).
    pub(crate) single_line: bool,
}

impl Default for ViewState {
    fn default() -> Self {
        Self {
            viewport: Offset::default(),
            wrapped_row_offset: 0,
            num_rows: 0,
            screen_area: Rect::default(),
            wrap: true,
            tab_width: 2,
            line_numbers: LineNumbers::None,
            cursor_screen_position: None,
            single_line: false,
        }
    }
}

#[derive(Debug, Default, Clone, Copy, Eq, PartialEq, Hash)]
pub(crate) struct Offset {
    /// The x-offset.
    pub(crate) x: usize,
    /// The y-offset.
    pub(crate) y: usize,
}

impl Offset {
    pub(crate) fn new(x: usize, y: usize) -> Self {
        Self { x, y }
    }
}

impl From<Rect> for Offset {
    fn from(value: Rect) -> Self {
        Self {
            x: value.x as usize,
            y: value.y as usize,
        }
    }
}

impl ViewState {
    /// Sets the editors area on the screen.
    ///
    /// Equivalent to the upper left coordinate of the editor in the
    /// buffers coordinate system.
    pub(crate) fn set_screen_area<T: Into<Rect>>(&mut self, area: T) {
        self.screen_area = area.into();
    }

    /// Updates the viewports horizontal offset.
    pub(crate) fn update_viewport_horizontal(
        &mut self,
        width: usize,
        cursor_col: usize,
        line: Option<&[char]>, 
    ) -> usize {
        let Some(line) = line else {
            self.viewport.x = 0;
            return self.viewport.x;
        };

        let text: String = line.iter().collect();
        let geometry = LineGeometry::new(&text, width, self.tab_width, false);
        let cursor_x = geometry.position(cursor_col).col;
        self.viewport.x = self.viewport.x.min(cursor_col);
        let start_x = geometry.position(self.viewport.x).col;
        if cursor_x.saturating_sub(start_x) >= width {
            self.viewport.x = geometry.glyphs.iter()
                .find(|glyph| cursor_x.saturating_sub(glyph.position.col) < width)
                .map_or(cursor_col, |glyph| glyph.scalars.start);
        } else {
            self.viewport.x = geometry.hit_test(0, start_x);
        }
        self.wrapped_row_offset = 0;
        self.viewport.x
    }

    /// Updates the view ports vertical offset.
    pub(crate) fn update_viewport_vertical(&mut self, height: usize, cursor_row: usize) -> usize {
        let max_cursor_pos = height.saturating_sub(1) + self.viewport.y;

        // scroll up
        if cursor_row < self.viewport.y {
            self.viewport.y = cursor_row;
        }

        // scroll down
        if cursor_row >= max_cursor_pos {
            self.viewport.y += cursor_row.saturating_sub(max_cursor_pos);
        }

        self.viewport.y
    }

    /// Updates the view ports vertical offset.
    pub(crate) fn update_viewport_vertical_wrap(
        &mut self,
        width: usize,
        height: usize,
        cursor: Index2,
        lines: &Lines,
    ) -> usize {
        if cursor.row < self.viewport.y {
            self.viewport.y = cursor.row;
            self.wrapped_row_offset = 0;
        }
        let mut preceding_rows = Vec::new();
        let mut cursor_y = 0usize;
        for (row, line) in lines.iter_row().enumerate().skip(self.viewport.y)
            .take(cursor.row.saturating_sub(self.viewport.y) + 1)
        {
            let text: String = line.iter().collect();
            let geometry = LineGeometry::new(&text, width, self.tab_width, true);
            if row == cursor.row {
                cursor_y += geometry.position(cursor.col).row;
            } else {
                preceding_rows.push(geometry.rows());
                cursor_y += geometry.rows();
            }
        }
        if cursor_y < self.wrapped_row_offset {
            self.wrapped_row_offset = cursor_y;
        } else if cursor_y.saturating_sub(self.wrapped_row_offset) >= height {
            self.wrapped_row_offset = cursor_y.saturating_sub(height.saturating_sub(1));
        }
        for rows in preceding_rows {
            if self.wrapped_row_offset < rows {
                break;
            }
            self.wrapped_row_offset -= rows;
            self.viewport.y += 1;
        }
        self.viewport.y
    }

    /// Updates the number of rows that are currently shown on the viewport.
    /// Refers to the number of editor lines, not visual lines.
    pub(crate) fn update_num_rows(&mut self, num_rows: usize) {
        self.num_rows = num_rows;
    }

}

#[cfg(test)]
mod tests {
    use super::*;

    macro_rules! update_view_vertical_test {
        ($name:ident: {
        view: $view:expr,
        height: $height:expr,
        cursor: $cursor:expr,
        expected: $expected:expr
    }) => {
            #[test]
            fn $name() {
                // given
                let mut view = $view;

                // when
                let offset = view.update_viewport_vertical($height, $cursor);

                // then
                assert_eq!(offset, $expected);
            }
        };
    }

    macro_rules! update_view_horizontal_test {
        ($name:ident: {
        view: $view:expr,
        width: $width:expr,
        cursor: $cursor:expr,
        expected: $expected:expr
    }) => {
            #[test]
            fn $name() {
                // given
                let mut view = $view;
                let line = vec!['a'; 4];

                // when
                let offset = view.update_viewport_horizontal($width, $cursor, Some(&line));

                // then
                assert_eq!(offset, $expected);
            }
        };
    }

    // cursor above viewport → scroll up
    update_view_vertical_test!(
        scroll_up: {
            view: ViewState{
                viewport: Offset::new(0, 1),
                ..Default::default()
            },
            height:  2,
            cursor: 0,
            expected: 0
        }
    );

    // cursor below viewport → scroll down
    update_view_vertical_test!(
        scroll_down: {
            view: ViewState{
                viewport: Offset::new(0, 0),
                ..Default::default()
            },
            height:  2,
            cursor: 2,
            expected: 1
        }
    );

    // cursor left of viewport → scroll left
    update_view_horizontal_test!(
        scroll_left: {
            view: ViewState{
                viewport: Offset::new(1, 0),
                ..Default::default()
            },
            width: 2,
            cursor: 0,
            expected: 0
        }
    );

    // cursor right of viewport → scroll right
    update_view_horizontal_test!(
        scroll_right: {
            view: ViewState{
                viewport: Offset::new(0, 0),
                ..Default::default()
            },
            width: 2,
            cursor: 2,
            expected: 1
        }
    );
}
