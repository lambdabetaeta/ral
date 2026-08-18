pub mod completion;

use crossterm::event::KeyEvent;
use edtui::{EditorEventHandler, EditorMode, EditorState, EditorTheme, EditorView, Index2, Lines};
use edtui_jagged::index::RowIndex;
use ratatui::Frame;
use ratatui::layout::{Position, Rect};
use ratatui::style::Style;

#[derive(Clone, Copy)]
pub enum EditMode {
    Emacs,
    Vi,
}

pub enum KeyOutcome {
    Edited,
    Ignored,
}

#[derive(Clone)]
pub struct PromptHighlight {
    pub row: usize,
    pub col_start: usize,
    pub col_end: usize,
    pub style: Style,
}

pub struct PromptEditor {
    state: EditorState,
    handler: EditorEventHandler,
    highlights: Vec<PromptHighlight>,
    base_style: Option<Style>,
    wrap: bool,
    last_area: Rect,
}

impl PromptEditor {
    pub fn new(mode: EditMode) -> Self {
        let mut state = EditorState::new(Lines::from(""));
        let handler = match mode {
            EditMode::Emacs => EditorEventHandler::emacs_mode(),
            EditMode::Vi => EditorEventHandler::vim_mode(),
        };
        state.mode = EditorMode::Insert;
        Self {
            state,
            handler,
            highlights: Vec::new(),
            base_style: None,
            wrap: false,
            last_area: Rect::default(),
        }
    }

    /// Soft-wrap long logical lines onto further screen rows. Off by default:
    /// an inline editor whose caller maps each logical row to one screen row
    /// (the structural REPL) must not wrap, or its overlays drift. The exarch
    /// prompt box opts in, since it sizes its height to the wrapped row count.
    #[must_use]
    pub fn wrap(mut self, on: bool) -> Self {
        self.wrap = on;
        self
    }

    pub fn text(&self) -> String {
        lines_to_string(&self.state.lines)
    }

    pub fn lines(&self) -> Vec<String> {
        self.state
            .lines
            .iter_row()
            .map(|r| r.iter().collect())
            .collect()
    }

    pub fn line(&self, row: usize) -> Option<String> {
        self.state
            .lines
            .get(RowIndex::new(row))
            .map(|r| r.iter().collect())
    }

    pub fn is_empty(&self) -> bool {
        self.state.lines.iter_row().all(std::vec::Vec::is_empty)
    }

    pub fn row(&self) -> usize {
        self.state.cursor.row
    }

    pub fn col(&self) -> usize {
        self.state.cursor.col
    }

    pub fn row_count(&self) -> usize {
        self.state.lines.len()
    }

    pub fn cursor_char_offset(&self) -> usize {
        let row = self.state.cursor.row;
        let col = self.state.cursor.col;
        let prior: usize = (0..row)
            .map(|r| self.state.lines.len_col(r).unwrap_or(0) + 1)
            .sum();
        prior + col
    }

    pub fn cursor_byte_offset(&self) -> usize {
        let row = self.state.cursor.row;
        let col = self.state.cursor.col;
        let prior: usize = (0..row)
            .map(|r| row_to_string(&self.state.lines, r).map_or(0, |s| s.len() + 1))
            .sum();
        let byte_col = row_to_string(&self.state.lines, row).map_or(0, |s| char_to_byte(&s, col));
        prior + byte_col
    }

    pub fn at_buffer_end(&self) -> bool {
        let row = self.state.cursor.row;
        let col = self.state.cursor.col;
        let len = self.state.lines.len();
        row + 1 == len && col == self.state.lines.len_col(row).unwrap_or(0)
    }

    pub fn clear(&mut self) {
        self.state.lines = Lines::from("");
        self.state.cursor = Index2::new(0, 0);
        self.state.mode = EditorMode::Insert;
    }

    pub fn set_text(&mut self, text: &str) {
        self.clear();
        self.handler
            .on_paste_event(text.to_string(), &mut self.state);
        // Park cursor at end: on_paste_event may leave it one short
        let last_row = self.state.lines.len().saturating_sub(1);
        let last_col = self.state.lines.len_col(last_row).unwrap_or(0);
        self.state.cursor = Index2::new(last_row, last_col);
    }

    pub fn insert_str(&mut self, text: &str) {
        self.handler
            .on_paste_event(text.to_string(), &mut self.state);
    }

    pub fn place_char_offset(&mut self, offset: usize) {
        let mut remaining = offset;
        let n_rows = self.state.lines.len();
        for row in 0..n_rows {
            let line_chars = self.state.lines.len_col(row).unwrap_or(0);
            if remaining <= line_chars {
                self.state.cursor = Index2::new(row, remaining);
                return;
            }
            remaining = remaining.saturating_sub(line_chars + 1);
        }
        let last_row = n_rows.saturating_sub(1);
        let last_col = self.state.lines.len_col(last_row).unwrap_or(0);
        self.state.cursor = Index2::new(last_row, last_col);
    }

    pub fn replace_row_bytes(
        &mut self,
        row: usize,
        start: usize,
        end: usize,
        replacement: &str,
    ) -> bool {
        let old_text = lines_to_string(&self.state.lines);
        let mut lines_vec: Vec<String> = old_text.split('\n').map(String::from).collect();

        let abs = {
            let Some(r) = lines_vec.get(row) else {
                return false;
            };
            if start > end || !r.is_char_boundary(start) || !r.is_char_boundary(end) {
                return false;
            }
            let new_col = r[..start].chars().count() + replacement.chars().count();
            let new_row = format!("{}{replacement}{}", &r[..start], &r[end..]);
            let prior: usize = lines_vec
                .iter()
                .take(row)
                .map(|l| l.chars().count() + 1)
                .sum();
            lines_vec[row] = new_row;
            prior + new_col
        };

        self.set_text(&lines_vec.join("\n"));
        self.place_char_offset(abs);
        true
    }

    pub fn handle_key(&mut self, key: crossterm::event::KeyEvent) -> KeyOutcome {
        if shell_line_edit_edtui(&mut self.state, &key) {
            return KeyOutcome::Edited;
        }
        let Some(edtui_key) = key_to_edtui(key) else {
            return KeyOutcome::Ignored;
        };
        self.handler.on_key_event(edtui_key, &mut self.state);
        KeyOutcome::Edited
    }

    pub fn handle_paste(&mut self, text: String) {
        self.handler.on_paste_event(text, &mut self.state);
    }

    pub fn set_base_style(&mut self, style: Style) {
        self.base_style = Some(style);
    }

    pub fn set_highlights(&mut self, spans: &[PromptHighlight]) {
        self.highlights = spans.to_vec();
    }

    pub fn render(&mut self, frame: &mut Frame, area: Rect) {
        // Always sync highlights onto the edtui state, even when empty:
        // otherwise stale spans from a prior frame persist in the widget.
        let edtui_highlights: Vec<edtui::Highlight> = self
            .highlights
            .iter()
            .map(|h| edtui::Highlight {
                start: Index2::new(h.row, h.col_start),
                end: Index2::new(h.row, h.col_end.saturating_sub(1)),
                style: h.style,
            })
            .collect();
        self.state.set_highlights(edtui_highlights);
        // The frontends supply their own chrome and drive the terminal's
        // native cursor, so the widget renders bare: the caller's text style
        // over the terminal background (no opaque fill), no painted cursor
        // cell, and no mode status line — edtui's defaults would otherwise
        // paint a black box, an inverse cursor block, and an "Insert" row that
        // on a one-line band swallows the text entirely.
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
    pub fn cursor_screen_position(&self) -> Option<Position> {
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
    pub fn height_hint(&self, width: u16, max: u16) -> u16 {
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

fn row_to_string(lines: &Lines, row: usize) -> Option<String> {
    lines.get(RowIndex::new(row)).map(|r| r.iter().collect())
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
fn key_to_edtui(key: crossterm::event::KeyEvent) -> Option<edtui::events::KeyInput> {
    use crossterm::event::KeyCode::{
        Backspace, Char, Delete, Down, End, Enter, Esc, Home, Left, PageDown, PageUp, Right, Tab,
        Up,
    };
    match key.code {
        Char(_) | Enter | Esc | Backspace | Delete | Tab | Left | Right | Up | Down | Home
        | End | PageUp | PageDown => Some(edtui::events::KeyInput::from(key)),
        _ => None,
    }
}

pub(crate) fn char_to_byte(text: &str, cursor: usize) -> usize {
    text.char_indices()
        .nth(cursor)
        .map_or(text.len(), |(i, _)| i)
}

// ── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};

    #[test]
    fn place_cursor_restores_offset_across_newlines() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("ab\ncd");
        editor.place_char_offset(0);
        assert_eq!((editor.row(), editor.col()), (0, 0));
        editor.place_char_offset(2);
        assert_eq!((editor.row(), editor.col()), (0, 2));
        editor.place_char_offset(3);
        assert_eq!((editor.row(), editor.col()), (1, 0));
        editor.place_char_offset(4);
        assert_eq!((editor.row(), editor.col()), (1, 1));
        editor.place_char_offset(99);
        assert_eq!((editor.row(), editor.col()), (1, 2));
    }

    #[test]
    fn set_text_replaces_contents() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("first draft");
        editor.set_text("recalled entry");
        assert_eq!(editor.lines(), ["recalled entry"]);
        assert_eq!(editor.col(), "recalled entry".chars().count());
        assert_eq!(editor.row(), 0);
        editor.set_text("a\nb");
        assert_eq!(editor.lines(), ["a", "b"]);
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
        editor.place_char_offset(3);
        editor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.lines(), ["def"]);
        assert_eq!((editor.row(), editor.col()), (0, 0));
    }

    #[test]
    fn ctrl_u_kills_to_line_start_in_vi_insert() {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.insert_str("abcdef");
        editor.place_char_offset(3);
        editor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.lines(), ["def"]);
        assert_eq!(editor.state.mode, EditorMode::Insert);
    }

    #[test]
    fn ctrl_u_is_left_to_vim_in_normal_mode() {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.insert_str("abcdef");
        editor.place_char_offset(3);
        editor.handle_key(KeyEvent::new(KeyCode::Esc, KeyModifiers::NONE));
        assert_eq!(editor.state.mode, EditorMode::Normal);
        editor.handle_key(KeyEvent::new(KeyCode::Char('u'), KeyModifiers::CONTROL));
        assert_eq!(editor.lines(), ["abcdef"]);
        assert_eq!(editor.state.mode, EditorMode::Normal);
    }

    #[test]
    fn unsupported_keys_are_dropped_not_panicked() {
        // edtui's `From<crossterm::KeyCode>` panics on codes it doesn't model;
        // these must be filtered to `Ignored` and leave the buffer untouched.
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("abc");
        for code in [KeyCode::F(1), KeyCode::Insert, KeyCode::BackTab] {
            assert!(matches!(
                editor.handle_key(KeyEvent::new(code, KeyModifiers::NONE)),
                KeyOutcome::Ignored
            ));
        }
        assert_eq!(editor.lines(), ["abc"]);
    }

    #[test]
    fn joined_cursor_byte_spans_rows() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("ab\ncd");
        editor.place_char_offset(0);
        assert_eq!(editor.cursor_byte_offset(), 0);
        editor.place_char_offset(2);
        assert_eq!(editor.cursor_byte_offset(), 2);
        editor.place_char_offset(3);
        assert_eq!(editor.cursor_byte_offset(), 3);
        editor.place_char_offset(5);
        assert_eq!(editor.cursor_byte_offset(), 5);
    }

    #[test]
    fn at_buffer_end_only_at_the_tail() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.insert_str("ab\ncd");
        editor.place_char_offset(5);
        assert!(editor.at_buffer_end());
        editor.place_char_offset(2);
        assert!(!editor.at_buffer_end());
        editor.place_char_offset(4);
        assert!(!editor.at_buffer_end());
    }
}
