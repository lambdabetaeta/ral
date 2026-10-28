pub(in crate::tui) mod completion;
mod editing;
mod geometry;
mod navigation;
#[cfg(test)]
mod navigation_tests;
#[cfg(test)]
mod parity_tests;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent};
use edtui::{EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, Index2, Lines};
use edtui_jagged::index::RowIndex;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;

#[derive(Clone, Copy)]
pub(super) enum EditMode {
    Emacs,
    Vi,
}

/// Where a mouse selection started, in the draft as it stood at the press.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(in crate::tui) struct Anchor(Index2);

pub(super) struct PromptEditor {
    state: EditorState,
    handler: EditorEventHandler,
    clipboard: edtui::clipboard::Clipboard,
    edit_mode: EditMode,
    base_style: Style,
    last_area: Rect,
    kill_buffer: String,
    preferred_column: Option<usize>,
}

impl PromptEditor {
    pub(super) fn new(edit_mode: EditMode) -> Self {
        let mut editor = Self {
            state: EditorState::default(),
            handler: EditorEventHandler::default(),
            clipboard: edtui::clipboard::Clipboard::default(),
            edit_mode,
            base_style: Style::default(),
            last_area: Rect::default(),
            kill_buffer: String::new(),
            preferred_column: None,
        };
        editor.adopt("");
        editor
    }

    pub(super) fn text(&self) -> String {
        let mut text = String::new();
        for (row, line) in self.state.lines.iter_row().enumerate() {
            if row > 0 {
                text.push('\n');
            }
            text.extend(line);
        }
        text
    }

    #[cfg(test)]
    fn lines(&self) -> Vec<String> {
        self.state
            .lines
            .iter_row()
            .map(|r| r.iter().collect())
            .collect()
    }

    pub(super) fn line(&self, row: usize) -> Option<String> {
        self.state
            .lines
            .get(RowIndex::new(row))
            .map(|r| r.iter().collect())
    }

    /// Whether `text()` is the empty string.
    pub(super) fn is_empty(&self) -> bool {
        self.row_count() <= 1 && self.state.lines.iter_row().all(std::vec::Vec::is_empty)
    }

    pub(super) fn row(&self) -> usize {
        self.state.cursor.row
    }

    pub(super) fn row_count(&self) -> usize {
        self.state.lines.len()
    }

    pub(super) fn at_start(&self) -> bool {
        self.state.cursor == Index2::new(0, 0)
    }

    pub(super) fn at_end(&self) -> bool {
        let row = self.row_count().saturating_sub(1);
        self.state.cursor == Index2::new(row, self.state.lines.len_col(row).unwrap_or(0))
    }

    /// A fresh draft: new undo history and handler state, Insert mode, cursor at the end.
    pub(super) fn adopt(&mut self, text: &str) {
        self.reset_navigation();
        self.state = EditorState::new(Lines::from(text));
        self.state.set_clipboard(self.clipboard.clone());
        self.state.mode = EditorMode::Insert;
        let row = self.state.lines.len().saturating_sub(1);
        let col = self.state.lines.len_col(row).unwrap_or(0);
        self.state.cursor = Index2::new(row, col);
        self.handler = match self.edit_mode {
            EditMode::Emacs => EditorEventHandler::emacs_mode(),
            EditMode::Vi => EditorEventHandler::vim_mode(),
        };
    }

    /// Insert `\n`-normalized `text`; false when it fed vi `/` search instead
    /// of the draft.
    pub(super) fn insert_str(&mut self, text: &str) -> bool {
        if self.is_searching() {
            for ch in text.chars() {
                self.state.execute(edtui::actions::AppendCharToSearch(ch));
            }
            return false;
        }
        if text.is_empty() {
            return true;
        }
        self.handler.reset_pending();
        let range = self.selection_range().unwrap_or_else(|| {
            let pos = self.cursor_offset();
            pos..pos
        });
        self.replace_range_atomic(range, text);
        true
    }

    /// Replace the first row by `replacement` as one undoable edit that keeps the
    /// mode and the other rows.  False, the draft untouched, when the row is
    /// empty or already reads `replacement`, or `replacement` spans rows.
    pub(super) fn replace_first_row(&mut self, replacement: &str) -> bool {
        let Some(line) = self.line(0).filter(|line| !line.is_empty()) else {
            return false;
        };
        // Command candidates replace one row, never a line separator.
        if line == replacement || replacement.contains(['\n', '\r']) {
            return false;
        }
        self.reset_navigation();
        self.handler.reset_pending();
        self.state.transaction(|state| {
            editing::replace_range(state, 0..line.len(), replacement);
            let len = state.lines.len_col(0).unwrap_or(0);
            let max_col = if state.mode == EditorMode::Normal {
                len.saturating_sub(1)
            } else {
                len
            };
            state.cursor = Index2::new(0, replacement.chars().count().min(max_col));
        });
        true
    }

    pub(super) fn handle_key(&mut self, mut key: KeyEvent) {
        if key.modifiers.is_empty()
            && let KeyCode::Char(ch) = key.code
            && matches!(ch, '\u{1}'..='\u{1a}' | '\u{1f}')
        {
            let ch = if ch == '\u{1f}' {
                '_'
            } else {
                char::from_u32(u32::from(ch) + u32::from('a') - 1).unwrap_or(ch)
            };
            key.code = KeyCode::Char(ch);
            key.modifiers = KeyModifiers::CONTROL;
        }
        if self.handle_vertical_key(key) {
            return;
        }
        self.reset_navigation();
        if self.handle_undo(key) {
            return;
        }
        let custom = self.handle_insert_key(key);
        if !custom && let Some(key) = key_to_edtui(key) {
            self.handler.on_key_event(key, &mut self.state);
        }
    }

    fn handle_undo(&mut self, key: KeyEvent) -> bool {
        // Native vi commands (including pending character arguments) stay native.
        if self.is_searching() || self.handler.is_pending() {
            return false;
        }
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        let undo = ctrl && matches!(key.code, KeyCode::Char('_' | 'z' | '/'));
        let redo = (key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT)
            && matches!(key.code, KeyCode::Char('z' | 'Z')))
            || (key.modifiers == KeyModifiers::ALT && key.code == KeyCode::Char('_'));
        if undo {
            self.state.undo();
        } else if redo {
            self.state.redo();
        } else {
            return false;
        }
        self.snap_cursor();
        true
    }

    pub(super) fn is_insert_mode(&self) -> bool {
        self.state.mode == EditorMode::Insert
    }

    pub(super) fn is_searching(&self) -> bool {
        self.state.mode == EditorMode::Search
    }

    /// A left press inside the box moves the cursor there, which anchors the
    /// selection a drag extends.
    pub(super) fn press(&mut self, event: MouseEvent) -> Option<Anchor> {
        self.point(event).then_some(Anchor(self.state.cursor))
    }

    /// Select from `anchor` to a drag or release inside the box.
    pub(super) fn drag(&mut self, anchor: Anchor, event: MouseEvent) -> bool {
        if !self.point(event) {
            return false;
        }
        if let Some(selection) = self.state.selection.as_mut() {
            selection.start = anchor.0;
            selection.end = self.state.cursor;
        }
        true
    }

    pub(super) fn wheel(&mut self, event: MouseEvent) {
        self.point(event);
    }

    /// Hand edtui a pointer event inside the box, keeping the mode and
    /// recording the motion in the current native insert.
    fn point(&mut self, event: MouseEvent) -> bool {
        if !self.contains_position(event.column, event.row) {
            return false;
        }
        self.reset_navigation();
        self.handler.reset_pending();
        let mode = self.state.mode;
        let origin = self.state.cursor;
        self.handler.on_mouse_event(event, &mut self.state);
        self.state.mode = mode;
        self.snap_cursor();
        let target = self.state.cursor;
        self.state.cursor = origin;
        self.state.move_to(target);
        true
    }

    pub(super) fn selection_text(&self) -> Option<String> {
        let range = self.selection_range()?;
        (!range.is_empty()).then(|| self.text()[range].to_owned())
    }

    pub(super) fn clear_selection(&mut self) {
        self.state.selection = None;
    }

    pub(super) fn contains_position(&self, column: u16, row: u16) -> bool {
        self.last_area.contains(Position::new(column, row))
    }

    pub(super) fn set_base_style(&mut self, style: Style) {
        self.base_style = style;
    }

    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect) {
        if self.last_area.width != area.width {
            self.reset_navigation();
        }
        // The host draws chrome and the terminal cursor; edtui stays transparent.
        let theme = EditorTheme::default()
            .base(self.base_style)
            .cursor_style(Style::default())
            .hide_status_line();
        if area.is_empty() {
            self.last_area = area;
            return;
        }
        let view = EditorView::new(&mut self.state)
            .theme(theme)
            .tab_width(geometry::TAB_WIDTH)
            .wrap(geometry::wraps(area.width));
        frame.render_widget(view, area);
        self.last_area = area;
    }

    /// The cursor's position *within* the last render area — column and row
    /// offsets from the area's top-left. edtui reports an absolute screen
    /// position relative to where it was drawn; the caller owns that origin and
    /// adds it back, so the facade hands back the offset alone (otherwise the
    /// origin is counted twice and the cursor lands off the edit point).
    pub(super) fn cursor_screen_position(&self) -> Option<Position> {
        self.state.cursor_screen_position().and_then(|p| {
            self.last_area
                .contains(p)
                .then(|| Position::new(p.x - self.last_area.x, p.y - self.last_area.y))
        })
    }
}

// ── Internal helpers ───────────────────────────────────────────────────────

/// edtui's `From<crossterm::event::KeyCode>` is partial: it `unimplemented!()`s
/// on any code outside the set below — function keys, `Insert`, `BackTab`, media
/// keys, the kitty protocol's modifier-release events, and so on. Mirror its
/// supported variants here and drop the rest, so the conversion is total and the
/// panicking `From` is only ever reached for codes it accepts. An unmatched key
/// is one the editor has no binding for; ignoring it is the correct behaviour.
fn key_to_edtui(mut key: KeyEvent) -> Option<edtui::events::KeyInput> {
    use KeyCode::{
        Backspace, Char, Delete, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Tab,
        Up,
    };
    match key.code {
        Char(_) | Esc | Backspace | Delete | Tab | Left | Right | Up | Down | Home | End
        | PageUp | PageDown => Some(edtui::events::KeyInput::from(key)),
        // Only non-Insert modes reach here (Insert Enter is handled in
        // `editing`); edtui binds a bare `Enter` only, so drop the modifier.
        Enter => {
            key.modifiers = KeyModifiers::NONE;
            Some(edtui::events::KeyInput::from(key))
        }
        _ => None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn undo(editor: &mut PromptEditor) {
        editor.handle_key(KeyEvent::new(KeyCode::Char('_'), KeyModifiers::CONTROL));
    }

    fn redo(editor: &mut PromptEditor) {
        editor.handle_key(KeyEvent::new(
            KeyCode::Char('Z'),
            KeyModifiers::CONTROL | KeyModifiers::SHIFT,
        ));
    }

    #[test]
    fn completion_preserves_mode_and_other_rows() {
        for normal in [false, true] {
            let mut editor = PromptEditor::new(EditMode::Vi);
            editor.adopt("/thi\n界 e\u{301} 👩‍💻\n");
            editor.state.cursor = Index2::new(0, 4);
            if normal {
                editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
            }
            let mode = editor.state.mode;
            assert!(editor.replace_first_row("/thinking"));
            assert_eq!(editor.text(), "/thinking\n界 e\u{301} 👩‍💻\n");
            assert_eq!(editor.state.mode, mode);
            assert_eq!(editor.state.cursor.col, if normal { 8 } else { 9 });
            assert!(editor.state.selection.is_none());
            undo(&mut editor);
            assert_eq!(editor.text(), "/thi\n界 e\u{301} 👩‍💻\n");
            redo(&mut editor);
            assert_eq!(editor.text(), "/thinking\n界 e\u{301} 👩‍💻\n");
        }
    }

    #[test]
    fn emptying_the_first_row_leaves_no_phantom_cells() {
        for (text, expected) in [("ab\ncd", "\ncd"), ("ab", "")] {
            for normal in [false, true] {
                let mut editor = PromptEditor::new(EditMode::Vi);
                editor.adopt(text);
                if normal {
                    editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                }
                let mode = editor.state.mode;
                assert!(editor.replace_first_row(""));
                assert_eq!(editor.text(), expected);
                assert_eq!((editor.row(), editor.state.cursor.col), (0, 0));
                assert_eq!(editor.state.mode, mode);
                undo(&mut editor);
                assert_eq!(editor.text(), text);
                redo(&mut editor);
                assert_eq!(editor.text(), expected);
            }
        }
    }

    #[test]
    fn refused_replacements_leave_cursor_mode_and_undo_alone() {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.adopt("a界");
        assert!(editor.replace_first_row("b界"));
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let cursor = editor.state.cursor;
        let mode = editor.state.mode;
        for replacement in ["b界", "x\ny", "x\r"] {
            assert!(!editor.replace_first_row(replacement));
            assert_eq!(editor.text(), "b界");
            assert_eq!(editor.state.cursor, cursor);
            assert_eq!(editor.state.mode, mode);
        }
        undo(&mut editor);
        assert_eq!(editor.text(), "a界");
        assert!(!editor.replace_first_row("a界"));
        redo(&mut editor);
        assert_eq!(editor.text(), "b界");
        assert!(!PromptEditor::new(EditMode::Vi).replace_first_row("x"));
    }

    #[test]
    fn adoption_discards_selection_search_repeat_and_pending_keys() {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.adopt("before");
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Char('x'), KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        editor.adopt("fresh\n界\n");
        assert_eq!(editor.text(), "fresh\n界\n");
        assert_eq!((editor.row(), editor.state.cursor.col), (2, 0));
        assert_eq!(editor.state.mode, EditorMode::Insert);
        assert!(editor.state.selection.is_none());
        assert_eq!(editor.state.search_pattern(), "");
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Char('.'), KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL));
        assert_eq!(editor.text(), "fresh\n界\n");
        editor.handle_key(KeyEvent::new(KeyCode::Char('v'), KeyModifiers::NONE));
        assert!(editor.state.selection.is_some());
        editor.adopt("selected draft replaced");
        assert!(editor.state.selection.is_none());
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Char('/'), KeyModifiers::NONE));
        editor.handle_key(KeyEvent::new(KeyCode::Char('d'), KeyModifiers::NONE));
        assert_eq!(editor.state.mode, EditorMode::Search);
        assert_eq!(editor.state.search_pattern(), "d");
        editor.adopt("search draft replaced");
        assert_eq!(editor.state.search_pattern(), "");
        assert_eq!(editor.state.mode, EditorMode::Insert);
    }

    #[test]
    fn ctrl_u_kills_to_line_start() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("abcdef");
        editor.state.cursor = Index2::new(0, 3);
        editor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.lines(), ["def"]);
        assert_eq!((editor.row(), editor.state.cursor.col), (0, 0));
    }

    #[test]
    fn ctrl_u_kills_to_line_start_in_vi_insert() {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.insert_str("abcdef");
        editor.state.cursor = Index2::new(0, 3);
        editor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.lines(), ["def"]);
        assert_eq!(editor.state.mode, EditorMode::Insert);
    }

    #[test]
    fn ctrl_u_is_left_to_vim_in_normal_mode() {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.insert_str("abcdef");
        editor.state.cursor = Index2::new(0, 3);
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(editor.state.mode, EditorMode::Normal);
        editor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.lines(), ["abcdef"]);
        assert_eq!(editor.state.mode, EditorMode::Normal);
    }

    #[test]
    fn unsupported_keys_are_dropped_not_panicked() {
        // edtui's `From<crossterm::KeyCode>` panics on codes it doesn't model;
        // these must leave the buffer untouched.
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("abc");
        for code in [KeyCode::F(1), KeyCode::Insert, KeyCode::BackTab] {
            editor.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
        }
        assert_eq!(editor.lines(), ["abc"]);
    }

    #[test]
    fn shift_and_alt_enter_insert_a_line_break() {
        // edtui's insert-mode register binds only a bare Enter to the
        // line-break action; Shift/Alt+Enter must fold down to it rather than
        // miss the binding and do nothing.
        for modifiers in [KeyModifiers::SHIFT, KeyModifiers::ALT] {
            let mut editor = PromptEditor::new(EditMode::Emacs);
            for c in "ab".chars() {
                editor.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            editor.handle_key(KeyEvent::new(KeyCode::Enter, modifiers));
            for c in "cd".chars() {
                editor.handle_key(KeyEvent::new(KeyCode::Char(c), KeyModifiers::NONE));
            }
            assert_eq!(editor.lines(), ["ab", "cd"], "modifiers: {modifiers:?}");
        }
    }
}
