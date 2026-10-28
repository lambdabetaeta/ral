//! The editors state
//! LOCAL PATCH: native transactional history and repeatable custom insert edits.
pub mod highlight;
pub mod mode;
mod search;
pub mod selection;
mod undo;
mod view;

use self::highlight::Highlight;
use self::search::SearchState;
use self::view::ViewState;
use self::{mode::EditorMode, selection::Selection, undo::History};
use crate::actions::{Action, Execute, InsertEdit};
use crate::clipboard::{Clipboard, ClipboardTrait};
use crate::helper::max_col;
use crate::{Index2, Lines};
use ratatui_core::layout::Position;

/// Represents the state of an editor.
#[derive(Clone)]
pub struct EditorState {
    /// The text in the editor.
    pub lines: Lines,

    /// The current cursor position in the editor.
    pub cursor: Index2,

    /// The mode of the editor (insert, visual or normal mode).
    pub mode: EditorMode,

    /// Represents the selection in the editor, if any.
    pub selection: Option<Selection>,

    /// Custom highlight ranges with their styles.
    pub highlights: Vec<Highlight>,

    /// Internal view state of the editor.
    pub(crate) view: ViewState,

    /// State holding the search results in search mode.
    pub(crate) search: SearchState,

    /// Combined undo and redo history.
    pub(crate) history: History,

    /// Clipboard for yank and paste operations.
    pub(crate) clip: Clipboard,

    /// The last buffer-changing command thath can be replayed by dot-repeat.
    pub(crate) last_change: Option<Action>,

    /// Text typed during the last change's insert (e.g. `ciw`).
    pub(crate) last_insert: Option<Vec<Action>>,

    /// Text typed in the current insert session.
    pub(crate) insert_recording: Option<Vec<Action>>,

    /// Flag indicating a system editor was requested.
    #[cfg(feature = "system-editor")]
    pub(crate) system_edit_requested: bool,
}

impl Default for EditorState {
    /// Creates a default `EditorState` with no text.
    fn default() -> Self {
        EditorState::new(Lines::default())
    }
}

impl EditorState {
    /// Creates a new editor state.
    ///
    /// # Example
    ///
    /// ```
    /// use edtui::{EditorState, Lines};
    ///
    /// let state = EditorState::new(Lines::from("First line\nSecond Line"));
    /// ```
    #[must_use]
    pub fn new(lines: Lines) -> EditorState {
        EditorState {
            lines,
            cursor: Index2::new(0, 0),
            mode: EditorMode::Normal,
            selection: None,
            highlights: Vec::new(),
            view: ViewState::default(),
            search: SearchState::default(),
            history: History::default(),
            clip: Clipboard::default(),
            last_change: None,
            last_insert: None,
            insert_recording: None,
            #[cfg(feature = "system-editor")]
            system_edit_requested: false,
        }
    }

    /// Execute an action on the editor state
    /// # Example
    ///
    /// ```
    /// use edtui::{EditorState, Lines};
    /// use edtui::actions::DeleteLine;
    ///
    /// let mut state = EditorState::new(Lines::from("Hello wold!"));
    /// state.execute(DeleteLine(1))
    /// ```
    pub fn execute(&mut self, mut action: impl Execute) {
        action.execute(self);
    }

    /// Executes an action, recording it for the dot-repeat command.
    pub(crate) fn execute_recorded(&mut self, mut action: Action) {
        let mode_before = self.mode;
        if mode_before == EditorMode::Insert {
            let before = match &action {
                Action::Paste(_) => Some(false),
                Action::PasteBefore(_) => Some(true),
                _ => None,
            };
            if let Some(before) = before {
                action = crate::actions::PasteText { text: self.clip.get_text(), before }.into();
            }
        }
        if !matches!(action, Action::Undo(_) | Action::Redo(_)) {
            use crate::actions::Chainable;
            action = action.chain(crate::actions::SnapInsertCursor).into();
        }
        action.execute(self);

        // Inside an insert session, keep capturing until it ends.
        if mode_before == EditorMode::Insert {
            if self.mode != EditorMode::Insert {
                self.last_insert = self.insert_recording.take();
            } else if let Some(buffer) = &mut self.insert_recording {
                buffer.push(action);
            }
            return;
        }

        // Remember repeatable changes started from normal mode.
        if mode_before == EditorMode::Normal && action.is_repeatable() {
            self.last_change = Some(action);
            self.last_insert = None;
            // If the change opened insert mode, start capturing typed text.
            self.insert_recording = (self.mode == EditorMode::Insert).then(Vec::new);
        }
    }

    /// Replaces a byte range in the newline-joined text and clears selection.
    /// The cursor lands immediately after the replacement. During insert recording,
    /// dot-repeat retains the exact range relative to the pre-edit cursor.
    /// Wrap in `transaction` or `insert_transaction` for undo grouping.
    ///
    /// # Panics
    /// Panics if the range is reversed, out of bounds, or not on UTF-8 boundaries.
    pub fn replace_range(&mut self, range: std::ops::Range<usize>, replacement: &str) {
        let text = self.lines.to_string();
        assert!(range.start <= range.end, "replacement range must be ordered");
        let removed = text.get(range.clone())
            .expect("replacement range must be within the text and on UTF-8 boundaries");
        let start = text[..range.start].chars().count();
        let mut edit = InsertEdit {
            offset: start as isize - InsertEdit::position(&self.lines, self.cursor) as isize,
            removed: removed.chars().count(),
            inserted: replacement.to_owned(),
            cursor: replacement.chars().count() as isize,
        };
        edit.execute(self);
        if let Some(buffer) = &mut self.insert_recording {
            buffer.push(edit.into());
        }
    }

    /// Moves to an exact scalar-indexed cursor, recording its relative movement
    /// during an insert session. The caller must supply a valid text position.
    pub fn move_to(&mut self, cursor: Index2) {
        if self.cursor == cursor {
            return;
        }
        let edit = InsertEdit {
            offset: 0,
            removed: 0,
            inserted: String::new(),
            cursor: InsertEdit::position(&self.lines, cursor) as isize
                - InsertEdit::position(&self.lines, self.cursor) as isize,
        };
        self.cursor = cursor;
        if let Some(buffer) = &mut self.insert_recording {
            buffer.push(edit.into());
        }
    }

    /// Set a custom clipboard.
    pub fn set_clipboard(&mut self, clipboard: impl ClipboardTrait + 'static) {
        self.clip = Clipboard::new(clipboard);
    }

    /// Returns the current search pattern.
    #[must_use]
    pub fn search_pattern(&self) -> String {
        self.search.pattern.clone()
    }

    /// Clamps the column of the cursor if the cursor is out of bounds.
    /// In normal or visual mode, clamps on `col = len() - 1`, in insert
    /// mode on `col = len()`.
    pub(crate) fn clamp_column(&mut self) {
        let max_col = max_col(&self.lines, &self.cursor, self.mode);
        self.cursor.col = self.cursor.col.min(max_col);
    }

    /// Returns the cursor's screen position, computed during the last render.
    ///
    /// This is the absolute position in terminal coordinates where the cursor
    /// should be displayed. It accounts for viewport scrolling, line wrapping,
    /// tab width, and the editor's position on screen.
    ///
    /// Returns `None` if the editor has not been rendered yet.
    #[must_use]
    pub fn cursor_screen_position(&self) -> Option<Position> {
        self.view.cursor_screen_position
    }

    /// Enables or disables single-line mode.
    ///
    /// When enabled, newline insertion is blocked. This is useful for search boxes,
    /// single-line input fields, and similar use cases.
    ///
    /// # Example
    ///
    /// ```
    /// use edtui::{EditorState, Lines};
    ///
    /// let mut state = EditorState::new(Lines::from("Search query"));
    /// state.set_single_line(true);
    /// ```
    pub fn set_single_line(&mut self, single_line: bool) {
        self.view.single_line = single_line;
    }

    /// Returns whether single-line mode is enabled.
    ///
    /// In single-line mode, newline insertion is blocked.
    #[must_use]
    pub fn is_single_line(&self) -> bool {
        self.view.single_line
    }

    /// Add a custom highlight range.
    pub fn add_highlight(&mut self, highlight: Highlight) {
        self.highlights.push(highlight);
    }

    /// Clear all custom highlights.
    pub fn clear_highlights(&mut self) {
        self.highlights.clear();
    }

    /// Set all highlights, replacing any existing ones.
    pub fn set_highlights(&mut self, highlights: Vec<Highlight>) {
        self.highlights = highlights;
    }

    /// Returns the current viewport offset as (x, y).
    ///
    /// The viewport offset represents the top-left corner of the visible area
    /// in editor coordinates. This is useful for implementing custom scroll
    /// behavior.
    ///
    /// - `x` is the horizontal offset (column)
    /// - `y` is the vertical offset (row)
    #[must_use]
    pub fn viewport_offset(&self) -> (usize, usize) {
        (self.view.viewport.x, self.view.viewport.y)
    }

    /// Sets the viewport offset to the specified (x, y) position.
    ///
    /// This allows manual control of the scroll position. The viewport offset
    /// represents the top-left corner of the visible area in editor coordinates.
    ///
    /// - `x` is the horizontal offset (column)
    /// - `y` is the vertical offset (row)
    ///
    /// Note: The viewport may be adjusted during the next render to keep the
    /// cursor visible, depending on the cursor position.
    pub fn set_viewport_offset(&mut self, x: usize, y: usize) {
        self.view.viewport.x = x;
        self.view.viewport.y = y;
    }

    /// Sets the viewport height (number of visible rows).
    ///
    /// This is set automatically during render, so there is usually no need
    /// to call this manually.
    pub fn set_viewport_height(&mut self, height: usize) {
        self.view.update_num_rows(height);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::EditorView;
    use ratatui_core::{buffer::Buffer, layout::Rect, widgets::Widget};

    #[test]
    fn test_cursor_screen_position_after_render() {
        let mut state = EditorState::new(Lines::from("Hello World"));
        assert!(state.cursor_screen_position().is_none());

        let area = Rect::new(0, 0, 20, 5);
        let mut buffer = Buffer::empty(area);
        EditorView::new(&mut state).render(area, &mut buffer);

        let pos = state.cursor_screen_position();
        assert!(pos.is_some());

        let pos = pos.unwrap();
        assert_eq!(pos.x, 0);
        assert_eq!(pos.y, 0);
    }

    #[test]
    fn test_cursor_screen_position_with_offset() {
        let mut state = EditorState::new(Lines::from("Hello World"));
        state.cursor = Index2::new(0, 5);

        let area = Rect::new(10, 5, 20, 5);
        let mut buffer = Buffer::empty(Rect::new(0, 0, 40, 20));
        EditorView::new(&mut state).render(area, &mut buffer);

        let pos = state.cursor_screen_position().unwrap();
        // Cursor column 5 + area.x (10) = 15
        assert_eq!(pos.x, 15);
        // Cursor row 0 + area.y (5) = 5
        assert_eq!(pos.y, 5);
    }

    #[test]
    fn test_cursor_screen_position_multiline() {
        let mut state = EditorState::new(Lines::from("Line 1\nLine 2\nLine 3"));
        state.cursor = Index2::new(2, 3);

        let area = Rect::new(0, 0, 20, 10);
        let mut buffer = Buffer::empty(area);
        EditorView::new(&mut state).render(area, &mut buffer);

        let pos = state.cursor_screen_position().unwrap();
        assert_eq!(pos.x, 3);
        assert_eq!(pos.y, 2);
    }

    #[test]
    fn test_single_line_mode_blocks_line_break() {
        use crate::actions::LineBreak;

        let mut state = EditorState::new(Lines::from("Hello World"));
        state.set_single_line(true);
        state.cursor = Index2::new(0, 5);

        LineBreak(1).execute(&mut state);

        // Line break should be blocked
        assert_eq!(state.lines, Lines::from("Hello World"));
        assert_eq!(state.cursor, Index2::new(0, 5));
    }

    #[test]
    fn test_single_line_mode_blocks_insert_newline_char() {
        use crate::actions::InsertChar;

        let mut state = EditorState::new(Lines::from("Hello"));
        state.set_single_line(true);
        state.cursor = Index2::new(0, 5);

        InsertChar('\n').execute(&mut state);

        // Newline char should be blocked
        assert_eq!(state.lines, Lines::from("Hello"));
    }

    #[test]
    fn test_single_line_mode_paste_replaces_newlines() {
        use crate::actions::Paste;
        use crate::clipboard::InternalClipboard;

        let mut state = EditorState::new(Lines::from("Hello"));
        state.set_clipboard(InternalClipboard::default());
        state.set_single_line(true);
        state.cursor = Index2::new(0, 5);

        // Paste text with newlines
        state.clip.set_text("Line1\nLine2\nLine3".to_string());
        Paste.execute(&mut state);

        // Newlines should be replaced with spaces
        assert_eq!(state.lines, Lines::from("HelloLine1 Line2 Line3"));
    }
}
