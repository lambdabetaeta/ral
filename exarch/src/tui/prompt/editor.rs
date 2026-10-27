pub(in crate::tui) mod completion;

use crossterm::event::KeyEvent;
use edtui::actions::{SwitchMode, cpaste::PasteOverSelection};
use edtui::clipboard::{ClipboardTrait, InternalClipboard};
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

pub(super) struct PromptEditor {
    state: EditorState,
    handler: EditorEventHandler,
    edit_mode: EditMode,
    base_style: Style,
    last_area: Rect,
}

impl PromptEditor {
    pub(super) fn new(edit_mode: EditMode) -> Self {
        let mut editor = Self {
            state: EditorState::default(),
            handler: EditorEventHandler::default(),
            edit_mode,
            base_style: Style::default(),
            last_area: Rect::default(),
        };
        editor.adopt("");
        editor
    }

    pub(super) fn text(&self) -> String {
        self.lines().join("\n")
    }

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

    pub(super) fn is_empty(&self) -> bool {
        self.state.lines.iter_row().all(std::vec::Vec::is_empty)
    }

    pub(super) fn row(&self) -> usize {
        self.state.cursor.row
    }

    pub(super) fn row_count(&self) -> usize {
        self.state.lines.len()
    }

    /// A fresh draft: new undo history and handler state, Insert mode, cursor at the end.
    pub(super) fn adopt(&mut self, text: &str) {
        self.state = EditorState::new(Lines::from(text));
        self.state.execute(SwitchMode(EditorMode::Insert));
        let row = self.state.lines.len().saturating_sub(1);
        let col = self.state.lines.len_col(row).unwrap_or(0);
        self.state.cursor = Index2::new(row, col);
        self.handler = match self.edit_mode {
            EditMode::Emacs => EditorEventHandler::emacs_mode(),
            EditMode::Vi => EditorEventHandler::vim_mode(),
        };
    }

    pub(super) fn insert_str(&mut self, text: &str) {
        self.handler
            .on_paste_event(text.to_string(), &mut self.state);
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
        let mode = self.state.mode;
        let has_next = self.state.lines.len() > 1;
        let mut clipboard = InternalClipboard::default();
        // edtui extracts a fully selected row, including its row boundary.
        clipboard.set_text(if has_next {
            format!("{replacement}\n")
        } else {
            replacement.to_owned()
        });
        self.state.set_clipboard(clipboard);
        self.state.mode = EditorMode::Insert;
        let cursor = self.state.cursor;
        self.state.execute(SwitchMode(EditorMode::Visual));
        let selection = self
            .state
            .selection
            .as_mut()
            .expect("Visual selects the cursor");
        selection.start = Index2::new(0, 0);
        selection.end = Index2::new(0, line.chars().count() - 1);
        self.state.cursor = cursor;
        self.state.mode = EditorMode::Insert;
        self.state.execute(PasteOverSelection);
        if !has_next && replacement.is_empty() {
            self.state.lines.insert(RowIndex::new(0), Vec::new());
        }
        self.state.selection = None;
        self.state.mode = mode;
        let len = self.state.lines.len_col(0).unwrap_or(0);
        let max_col = if mode == EditorMode::Normal {
            len.saturating_sub(1)
        } else {
            len
        };
        self.state.cursor = Index2::new(0, replacement.chars().count().min(max_col));
        true
    }

    pub(super) fn handle_key(&mut self, key: crossterm::event::KeyEvent) {
        if shell_line_edit_edtui(&mut self.state, &key) {
            return;
        }
        if let Some(key) = key_to_edtui(key) {
            self.handler.on_key_event(key, &mut self.state);
        }
    }

    pub(super) fn set_base_style(&mut self, style: Style) {
        self.base_style = style;
    }

    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect) {
        // The host draws chrome and the terminal cursor; edtui stays transparent.
        let theme = EditorTheme::default()
            .base(self.base_style)
            .cursor_style(Style::default())
            .hide_status_line();
        let view = EditorView::new(&mut self.state).theme(theme).wrap(true);
        frame.render_widget(view, area);
        self.last_area = area;
    }

    /// The cursor's position *within* the last render area — column and row
    /// offsets from the area's top-left. edtui reports an absolute screen
    /// position relative to where it was drawn; the caller owns that origin and
    /// adds it back, so the facade hands back the offset alone (otherwise the
    /// origin is counted twice and the cursor lands off the edit point).
    pub(super) fn cursor_screen_position(&self) -> Option<Position> {
        self.state.cursor_screen_position().map(|p| {
            Position::new(
                p.x.saturating_sub(self.last_area.x),
                p.y.saturating_sub(self.last_area.y),
            )
        })
    }

    /// The box height the exarch prompt needs: the wrapped line count plus the
    /// two border rows the caller draws, clamped to `[3, max]`.
    ///
    /// It counts wrapped rows directly from the text — it must not consult
    /// `cursor_screen_position`, whose `y` is an absolute screen row set by the
    /// last render. Feeding that back into the height makes the box grow toward
    /// the bottom of the screen, which moves it up, which shrinks `y`: a
    /// per-frame oscillation that reads as flashing.
    pub(super) fn height_hint(&self, width: u16, max: u16) -> u16 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "box row count; terminal height is u16"
        )]
        let visual = self
            .lines()
            .iter()
            .map(|line| {
                if line.is_empty() {
                    1
                } else {
                    textwrap::wrap(
                        line,
                        textwrap::Options::new(width as usize).break_words(true),
                    )
                    .len()
                    .max(1)
                }
            })
            .sum::<usize>() as u16;
        let with_border = visual.saturating_add(2);
        with_border.min(max).max(3)
    }
}

// ── Internal helpers ───────────────────────────────────────────────────────

fn shell_line_edit_edtui(state: &mut EditorState, key: &KeyEvent) -> bool {
    use crossterm::event::{KeyCode, KeyModifiers};

    if state.mode != EditorMode::Insert {
        return false;
    }
    if !key.modifiers.contains(KeyModifiers::CONTROL) || key.modifiers.contains(KeyModifiers::ALT) {
        return false;
    }
    match key.code {
        KeyCode::Char('u') => {
            let row = state.cursor.row;
            let col = state.cursor.col;

            let kept: Vec<char> = state
                .lines
                .get(RowIndex::new(row))
                .map(|r| r.iter().skip(col).copied().collect())
                .unwrap_or_default();

            if row < state.lines.len() {
                state.lines.remove(RowIndex::new(row));
            }
            state.lines.insert(RowIndex::new(row), kept);
            state.cursor.col = 0;
            true
        }
        _ => false,
    }
}

/// edtui's `From<crossterm::event::KeyCode>` is partial: it `unimplemented!()`s
/// on any code outside the set below — function keys, `Insert`, `BackTab`, media
/// keys, the kitty protocol's modifier-release events, and so on. Mirror its
/// supported variants here and drop the rest, so the conversion is total and the
/// panicking `From` is only ever reached for codes it accepts. An unmatched key
/// is one the editor has no binding for; ignoring it is the correct behaviour.
fn key_to_edtui(mut key: crossterm::event::KeyEvent) -> Option<edtui::events::KeyInput> {
    use crossterm::event::KeyCode::{
        Backspace, Char, Delete, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Tab,
        Up,
    };
    match key.code {
        Char(_) | Esc | Backspace | Delete | Tab | Left | Right | Up | Down | Home | End
        | PageUp | PageDown => Some(edtui::events::KeyInput::from(key)),
        // A key reaches here at all only once the caller has already decided
        // it means "insert a line break, don't submit" (Shift/Alt+Enter; see
        // `tui_loop::key_action`); edtui's insert-mode register binds only a
        // bare `Enter` to that action, so the modifier is spent information by
        // the time it lands here and is dropped rather than left to mismatch.
        Enter => {
            key.modifiers = crossterm::event::KeyModifiers::NONE;
            Some(edtui::events::KeyInput::from(key))
        }
        _ => None,
    }
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

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
            editor.state.undo();
            assert_eq!(editor.text(), "/thi\n界 e\u{301} 👩‍💻\n");
            editor.state.redo();
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
                editor.state.undo();
                assert_eq!(editor.text(), text);
                editor.state.redo();
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
        editor.state.undo();
        assert_eq!(editor.text(), "a界");
        assert!(!editor.replace_first_row("a界"));
        editor.state.redo();
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
