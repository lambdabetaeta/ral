pub(in crate::tui) mod completion;

use crossterm::event::KeyEvent;
use edtui::actions::{PasteBefore, SwitchMode, cpaste::PasteOverSelection};
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
    base_style: Option<Style>,
    wrap: bool,
    last_area: Rect,
}

impl PromptEditor {
    pub(super) fn new(mode: EditMode) -> Self {
        let mut state = EditorState::new(Lines::from(""));
        let handler = match mode {
            EditMode::Emacs => EditorEventHandler::emacs_mode(),
            EditMode::Vi => EditorEventHandler::vim_mode(),
        };
        state.execute(SwitchMode(EditorMode::Insert));
        Self {
            state,
            handler,
            edit_mode: mode,
            base_style: None,
            wrap: false,
            last_area: Rect::default(),
        }
    }

    #[must_use]
    pub(super) fn wrap(mut self, on: bool) -> Self {
        self.wrap = on;
        self
    }

    pub(super) fn text(&self) -> String {
        lines_to_string(&self.state.lines)
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

    fn col(&self) -> usize {
        self.state.cursor.col
    }

    pub(super) fn row_count(&self) -> usize {
        self.state.lines.len()
    }

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

    fn replace_row_bytes(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
        replacement: &str,
    ) -> bool {
        let Some(line) = self
            .line(row)
            .or_else(|| (row == 0 && self.state.lines.is_empty()).then(String::new))
        else {
            return false;
        };
        let Some(old) = line.get(start..end) else {
            return false;
        };
        // Command candidates replace one row, never a line separator.
        if old == replacement || replacement.contains(['\n', '\r']) {
            return false;
        }
        let start_col = line[..start].chars().count();
        let end_col = line[..end].chars().count();
        let new_col = start_col + replacement.chars().count();
        let mode = self.state.mode;
        let whole_row = start == 0 && end == line.len() && start != end;
        let has_next = row + 1 < self.state.lines.len();
        let mut clipboard = InternalClipboard::default();
        // edtui extracts a fully selected row, including its row boundary.
        let pasted = if whole_row && has_next {
            format!("{replacement}\n")
        } else {
            replacement.to_owned()
        };
        clipboard.set_text(pasted);
        self.state.set_clipboard(clipboard);
        self.state.mode = EditorMode::Insert;
        if start == end {
            self.state.cursor = Index2::new(row, start_col);
            self.state.execute(PasteBefore);
        } else {
            let cursor = self.state.cursor;
            self.state.execute(SwitchMode(EditorMode::Visual));
            let selection = self
                .state
                .selection
                .as_mut()
                .expect("Visual selects the cursor");
            selection.start = Index2::new(row, start_col);
            selection.end = Index2::new(row, end_col - 1);
            self.state.cursor = cursor;
            self.state.mode = EditorMode::Insert;
            self.state.execute(PasteOverSelection);
            if whole_row && !has_next && replacement.is_empty() {
                self.state.lines.insert(RowIndex::new(row), Vec::new());
            }
        }
        self.state.selection = None;
        self.state.mode = mode;
        let len = self.state.lines.len_col(row).unwrap_or(0);
        let max_col = if mode == EditorMode::Normal {
            len.saturating_sub(1)
        } else {
            len
        };
        self.state.cursor = Index2::new(row, new_col.min(max_col));
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
        self.base_style = Some(style);
    }

    pub(super) fn render(&mut self, frame: &mut Frame, area: Rect) {
        // The host draws chrome and the terminal cursor; edtui stays transparent.
        let theme = EditorTheme::default()
            .base(self.base_style.unwrap_or_default())
            .cursor_style(Style::default())
            .hide_status_line();
        let view = EditorView::new(&mut self.state)
            .theme(theme)
            .wrap(self.wrap);
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

fn lines_to_string(lines: &Lines) -> String {
    let mut s = String::new();
    for (i, row) in lines.iter_row().enumerate() {
        if i > 0 {
            s.push('\n');
        }
        s.extend(row.iter());
    }
    s
}

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
            assert!(editor.replace_row_bytes(0, 0, 4, "/thinking"));
            assert_eq!(editor.text(), "/thinking\n界 e\u{301} 👩‍💻\n");
            assert_eq!(editor.state.mode, mode);
            assert_eq!(editor.col(), if normal { 8 } else { 9 });
            assert!(editor.state.selection.is_none());
            editor.state.undo();
            assert_eq!(editor.text(), "/thi\n界 e\u{301} 👩‍💻\n");
            editor.state.redo();
            assert_eq!(editor.text(), "/thinking\n界 e\u{301} 👩‍💻\n");
        }
    }

    #[test]
    fn replacements_insert_at_boundaries_and_delete_without_phantom_cells() {
        for (text, row, start, end, replacement, expected, col) in [
            ("", 0, 0, 0, "界", "界", 1),
            ("ab", 0, 0, 0, "界", "界ab", 1),
            ("ab", 0, 1, 1, "界", "a界b", 2),
            ("ab", 0, 2, 2, "界", "ab界", 3),
            ("ab\n", 1, 0, 0, "界", "ab\n界", 1),
            ("ab\ncd", 0, 0, 2, "", "\ncd", 0),
            ("ab\ncd", 1, 0, 2, "", "ab\n", 0),
            ("ab", 0, 0, 2, "", "", 0),
            ("a界b", 0, 1, 4, "é", "aéb", 2),
        ] {
            for normal in [false, true] {
                let mut editor = PromptEditor::new(EditMode::Vi);
                editor.adopt(text);
                if normal {
                    editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
                }
                let mode = editor.state.mode;
                assert!(editor.replace_row_bytes(row, start, end, replacement));
                assert_eq!(editor.text(), expected);
                assert_eq!(editor.row(), row);
                let len = editor.state.lines.len_col(row).unwrap_or(0);
                assert_eq!(
                    editor.col(),
                    col.min(if normal { len.saturating_sub(1) } else { len })
                );
                assert_eq!(editor.state.mode, mode);
                editor.state.undo();
                assert_eq!(editor.text(), text);
                editor.state.redo();
                assert_eq!(editor.text(), expected);
            }
        }
    }

    #[test]
    fn invalid_and_identity_replacements_leave_cursor_mode_and_undo_alone() {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.adopt("a界");
        assert!(editor.replace_row_bytes(0, 0, 1, "b"));
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        let cursor = editor.state.cursor;
        let mode = editor.state.mode;
        for (row, start, end, replacement) in [
            (1, 0, 1, "x"),
            (0, 2, 3, "x"),
            (0, 3, 2, "x"),
            (0, 0, 99, "x"),
            (0, 0, 1, "b"),
            (0, 1, 1, ""),
        ] {
            assert!(!editor.replace_row_bytes(row, start, end, replacement));
            assert_eq!(editor.text(), "b界");
            assert_eq!(editor.state.cursor, cursor);
            assert_eq!(editor.state.mode, mode);
        }
        editor.state.undo();
        assert_eq!(editor.text(), "a界");
        assert!(!editor.replace_row_bytes(0, 0, 1, "a"));
        editor.state.redo();
        assert_eq!(editor.text(), "b界");
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
        assert_eq!((editor.row(), editor.col()), (2, 0));
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
    fn apply_candidate_splices_path_token() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("cd src/re");
        assert!(editor.replace_row_bytes(0, 7, 9, "repl/"));
        assert_eq!(editor.lines(), ["cd src/repl/"]);
        assert_eq!(editor.col(), "cd src/repl/".chars().count());
    }

    #[test]
    fn apply_candidate_targets_the_right_row() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("ls |\nca");
        assert!(editor.replace_row_bytes(1, 0, 2, "cat"));
        assert_eq!(editor.lines(), ["ls |", "cat"]);
        assert_eq!((editor.row(), editor.col()), (1, 3));
    }

    #[test]
    fn apply_candidate_ignores_a_stale_span() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("hi");
        assert!(!editor.replace_row_bytes(0, 1, 99, "xyz"));
        assert_eq!(editor.lines(), ["hi"]);
    }

    #[test]
    fn ctrl_u_kills_to_line_start() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("abcdef");
        editor.state.cursor = Index2::new(0, 3);
        editor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.lines(), ["def"]);
        assert_eq!((editor.row(), editor.col()), (0, 0));
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
