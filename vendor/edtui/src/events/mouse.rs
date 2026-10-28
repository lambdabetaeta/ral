// LOCAL PATCH: mouse cells resolve through the renderer's native grapheme geometry.
use crossterm::event::{MouseEvent as CTMouseEvent, MouseEventKind};
use jagged::Index2;

use crate::{
    actions::{Execute, SwitchMode},
    helper::max_col,
    state::selection::set_selection,
    view::geometry::LineGeometry,
    EditorMode, EditorState,
};

/// The number of lines to scroll per scroll wheel event.
const SCROLL_LINES: usize = 1;

/// Handles a mouse event.
#[derive(Clone, Debug, Default)]
pub struct MouseEventHandler {}

impl MouseEventHandler {
    pub fn on_event<E>(event: E, state: &mut EditorState)
    where
        E: Into<MouseEvent>,
    {
        let event = event.into();
        if event == MouseEvent::None {
            return;
        }

        // Handle scroll events
        match event {
            MouseEvent::ScrollUp(mouse) => {
                if Self::is_position_within_bounds(&mouse, state) {
                    Self::handle_scroll_up(state);
                }
                return;
            }
            MouseEvent::ScrollDown(mouse) => {
                if Self::is_position_within_bounds(&mouse, state) {
                    Self::handle_scroll_down(state);
                }
                return;
            }
            _ => {}
        }

        // Check if the mouse event is within the editor's screen area
        if !Self::is_within_bounds(&event, state) {
            return;
        }

        let target = match event {
            MouseEvent::Down(mouse) | MouseEvent::Up(mouse) | MouseEvent::Drag(mouse) => {
                mouse_position_to_cursor_position(state, &mouse)
            }
            _ => unreachable!("scroll and unhandled events returned above"),
        };

        if let MouseEvent::Down(_) = event {
            state.selection = None;
            if state.mode == EditorMode::Visual {
                SwitchMode(EditorMode::Normal).execute(state);
            }
        }

        if let MouseEvent::Drag(_) = event {
            if state.mode != EditorMode::Visual {
                SwitchMode(EditorMode::Visual).execute(state);
            }
            set_selection(&mut state.selection, state.cursor);
        }

        match event {
            MouseEvent::Down(_) | MouseEvent::Up(_) | MouseEvent::Drag(_) => {
                state.cursor = Index2::new(
                    target.row,
                    target.col.min(max_col(&state.lines, &target, state.mode)),
                );

                if let MouseEvent::Drag(_) = event {
                    set_selection(&mut state.selection, state.cursor);
                }
            }
            MouseEvent::ScrollUp(_) | MouseEvent::ScrollDown(_) | MouseEvent::None => (),
        };
    }

    fn handle_scroll_up(state: &mut EditorState) {
        if state.view.wrap && state.view.wrapped_row_offset > 0 {
            state.view.wrapped_row_offset -= 1;
        } else if state.view.viewport.y > 0 {
            state.view.viewport.y -= SCROLL_LINES;
            if state.view.wrap {
                state.view.wrapped_row_offset = row_geometry(state, state.view.viewport.y)
                    .map_or(0, |geometry| geometry.rows().saturating_sub(1));
            }
        }
        Self::clamp_cursor_to_viewport(state);
    }

    fn handle_scroll_down(state: &mut EditorState) {
        if state.view.wrap {
            let mut remaining = 0usize;
            let overflows = (state.view.viewport.y..state.lines.len()).any(|row| {
                remaining += row_geometry(state, row)
                    .map_or(1, |geometry| displayed_rows(state, row, &geometry));
                remaining.saturating_sub(state.view.wrapped_row_offset)
                    > usize::from(state.view.screen_area.height)
            });
            if !overflows {
                return;
            }
            let row = state.view.viewport.y;
            let rows = row_geometry(state, row)
                .map_or(1, |geometry| displayed_rows(state, row, &geometry));
            if state.view.wrapped_row_offset + 1 < rows {
                state.view.wrapped_row_offset += 1;
            } else if row < state.lines.last_row_index() {
                state.view.viewport.y += 1;
                state.view.wrapped_row_offset = 0;
            }
            Self::clamp_cursor_to_viewport(state);
            return;
        }
        let last_visible_row = state.view.viewport.y + state.view.num_rows.saturating_sub(1);
        if last_visible_row >= state.lines.last_row_index() {
            return;
        }
        let max_viewport_y = state.lines.len().saturating_sub(1);
        state.view.viewport.y = (state.view.viewport.y + SCROLL_LINES).min(max_viewport_y);
        Self::clamp_cursor_to_viewport(state);
    }

    fn clamp_cursor_to_viewport(state: &mut EditorState) {
        if state.view.wrap {
            let before: usize = (state.view.viewport.y..state.cursor.row)
                .filter_map(|row| row_geometry(state, row))
                .map(|geometry| geometry.rows()).sum();
            let cursor_y = before + row_geometry(state, state.cursor.row)
                .map_or(0, |geometry| geometry.position(state.cursor.col).row);
            let offset = state.view.wrapped_row_offset;
            let height = usize::from(state.view.screen_area.height);
            let target_row = if state.cursor.row < state.view.viewport.y || cursor_y < offset {
                Some(0)
            } else if cursor_y >= offset + height {
                Some(height.saturating_sub(1))
            } else {
                None
            };
            if let Some(row) = target_row {
                let mouse = MousePosition {
                    row: usize::from(state.view.screen_area.y) + row,
                    col: usize::from(state.view.screen_area.x),
                };
                let target = mouse_position_to_cursor_position(state, &mouse);
                state.cursor = Index2::new(target.row, target.col.min(max_col(&state.lines, &target, state.mode)));
            }
            return;
        }
        let viewport_y = state.view.viewport.y;
        let viewport_height = state.view.num_rows;

        if viewport_height == 0 {
            return;
        }

        let viewport_bottom = viewport_y + viewport_height.saturating_sub(1);

        if state.cursor.row < viewport_y {
            state.cursor.row = viewport_y;
            state.clamp_column();
        } else if state.cursor.row > viewport_bottom {
            state.cursor.row = viewport_bottom.min(state.lines.last_row_index());
            state.clamp_column();
        }
    }

    /// Checks if the mouse event occurred within the editor's screen area.
    fn is_within_bounds(event: &MouseEvent, state: &EditorState) -> bool {
        let mouse = match event {
            MouseEvent::Down(pos) | MouseEvent::Up(pos) | MouseEvent::Drag(pos) => pos,
            MouseEvent::ScrollUp(pos) | MouseEvent::ScrollDown(pos) => pos,
            MouseEvent::None => return false,
        };

        Self::is_position_within_bounds(mouse, state)
    }

    fn is_position_within_bounds(mouse: &MousePosition, state: &EditorState) -> bool {
        let area = &state.view.screen_area;
        let x: usize = area.x.into();
        let y: usize = area.y.into();
        let width: usize = area.width.into();
        let height: usize = area.height.into();

        mouse.col >= x && mouse.col < x + width && mouse.row >= y && mouse.row < y + height
    }
}

fn displayed_rows(state: &EditorState, row: usize, geometry: &LineGeometry) -> usize {
    if row == state.cursor.row {
        geometry.rows_with_cursor(state.cursor.col.min(max_col(&state.lines, &state.cursor, state.mode)))
    } else {
        geometry.rows()
    }
}

fn row_geometry(state: &EditorState, row: usize) -> Option<LineGeometry> {
    let line = state.lines.get(crate::RowIndex::new(row))?;
    let text: String = line.iter().collect();
    Some(LineGeometry::new(&text, usize::from(state.view.screen_area.width), state.view.tab_width, state.view.wrap))
}

fn mouse_position_to_cursor_position(state: &EditorState, mouse: &MousePosition) -> Index2 {
    let mut screen_row = mouse.row.saturating_sub(state.view.screen_area.y.into());
    let screen_col = mouse.col.saturating_sub(state.view.screen_area.x.into());
    let width = usize::from(state.view.screen_area.width);
    let cursor_col = max_col(&state.lines, &state.cursor, state.mode).min(state.cursor.col);
    if state.view.wrap {
        screen_row += state.view.wrapped_row_offset;
    }
    for (row, line) in state.lines.iter_row().enumerate().skip(state.view.viewport.y) {
        if !state.view.wrap {
            if screen_row > 0 {
                screen_row -= 1;
                continue;
            }
            let full_text: String = line.iter().collect();
            let full_geometry = LineGeometry::new(&full_text, width, state.view.tab_width, false);
            let offset = full_geometry.snap_column(state.view.viewport.x);
            let text: String = line.iter().skip(offset).collect();
            let geometry = LineGeometry::new(&text, width, state.view.tab_width, false);
            return Index2::new(row, offset + geometry.hit_test(0, screen_col));
        }
        let text: String = line.iter().collect();
        let geometry = LineGeometry::new(&text, width, state.view.tab_width, true);
        let rows = if row == state.cursor.row {
            geometry.rows_with_cursor(cursor_col)
        } else {
            geometry.rows()
        };
        if screen_row < rows {
            return Index2::new(row, geometry.hit_test(screen_row, screen_col));
        }
        screen_row -= rows;
    }
    let row = state.lines.last_row_index();
    Index2::new(row, state.lines.len_col(row).unwrap_or_default())
}

/// Represents a mouse event.
#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub enum MouseEvent {
    /// A mouse press event.
    Down(MousePosition),

    /// A mouse release event.
    Up(MousePosition),

    /// A mouse Drag event.
    Drag(MousePosition),

    /// A scroll up (wheel up) event.
    ScrollUp(MousePosition),

    /// A scroll down (wheel down) event.
    ScrollDown(MousePosition),

    /// A mouse event that is not handled by the editor.
    None,
}

impl From<CTMouseEvent> for MouseEvent {
    fn from(event: CTMouseEvent) -> Self {
        match event.kind {
            MouseEventKind::Down(_) => Self::Down(MousePosition::new(event.row, event.column)),
            MouseEventKind::Up(_) => Self::Up(MousePosition::new(event.row, event.column)),
            MouseEventKind::Drag(_) => Self::Drag(MousePosition::new(event.row, event.column)),
            MouseEventKind::ScrollUp => Self::ScrollUp(MousePosition::new(event.row, event.column)),
            MouseEventKind::ScrollDown => {
                Self::ScrollDown(MousePosition::new(event.row, event.column))
            }
            _ => Self::None,
        }
    }
}

#[derive(Debug, Clone, Copy, Eq, PartialEq, Hash)]
pub struct MousePosition {
    /// The row that the event occurred on.
    pub(crate) row: usize,
    /// The column that the event occurred on.
    pub(crate) col: usize,
}

impl MousePosition {
    /// Creates a new `MousePosition` instance.
    fn new(row: u16, col: u16) -> Self {
        Self {
            row: row.into(),
            col: col.into(),
        }
    }
}
