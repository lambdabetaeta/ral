// Kill/yank and grapheme-boundary policies adapted from OpenAI Codex textarea.
// Codex is licensed under Apache-2.0; this adaptation preserves that attribution.

use super::{EditMode, PromptEditor};
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use edtui::{EditorState, Index2};
use std::ops::Range;
use unicode_segmentation::UnicodeSegmentation;

fn floor(text: &str, pos: usize) -> usize {
    text.grapheme_indices(true)
        .map(|(i, _)| i)
        .chain([text.len()])
        .take_while(|&i| i <= pos)
        .last()
        .unwrap_or(0)
}

fn previous(text: &str, pos: usize) -> usize {
    text.grapheme_indices(true)
        .map(|(i, _)| i)
        .take_while(|&i| i < pos)
        .last()
        .unwrap_or(0)
}

fn next(text: &str, pos: usize) -> usize {
    text.grapheme_indices(true)
        .map(|(i, _)| i)
        .find(|&i| i > pos)
        .unwrap_or(text.len())
}

fn word(text: &str, pos: usize, backward: bool) -> usize {
    fn walk<'a>(pos: usize, parts: impl Iterator<Item = (usize, &'a str)>) -> usize {
        let mut end = pos;
        let mut saw_word = false;
        for (boundary, part) in parts {
            let space = part.chars().all(char::is_whitespace);
            if saw_word && space {
                break;
            }
            saw_word |= !space;
            end = boundary;
        }
        end
    }

    let parts = text.grapheme_indices(true);
    if backward {
        walk(pos, parts.rev().skip_while(|&(i, _)| i >= pos))
    } else {
        walk(
            pos,
            parts
                .skip_while(|&(i, _)| i < pos)
                .map(|(i, part)| (i + part.len(), part)),
        )
    }
}

pub(super) fn replace_range(state: &mut EditorState, range: Range<usize>, replacement: &str) {
    let cursor = range.start + replacement.len();
    state.replace_range(range, replacement);
    set_offset(state, cursor);
}

fn set_offset(state: &mut EditorState, mut pos: usize) {
    let mut cursor = Index2::new(0, 0);
    for (row, line) in state.lines.iter_row().enumerate() {
        let len = line.iter().map(|ch| ch.len_utf8()).sum::<usize>();
        cursor = Index2::new(row, line.len());
        if pos <= len {
            let mut text: String = line.iter().collect();
            // CRLF is one grapheme across the row separator.
            if row + 1 < state.lines.len() {
                text.push('\n');
            }
            cursor.col = text[..floor(&text, pos)].chars().count();
            break;
        }
        pos -= len + 1;
    }
    state.move_to(cursor);
}

impl PromptEditor {
    fn offset(&self, index: Index2) -> usize {
        let mut offset = 0;
        for (row, line) in self.state.lines.iter_row().enumerate() {
            if row == index.row {
                return offset
                    + line
                        .iter()
                        .take(index.col)
                        .map(|ch| ch.len_utf8())
                        .sum::<usize>();
            }
            offset += line.iter().map(|ch| ch.len_utf8()).sum::<usize>() + 1;
        }
        offset.saturating_sub(1)
    }

    pub(super) fn cursor_offset(&self) -> usize {
        floor(&self.text(), self.offset(self.state.cursor))
    }

    pub(super) fn snap_cursor(&mut self) {
        let index = self.state.cursor;
        if let Some(line) = self.state.lines.iter_row().nth(index.row) {
            let mut text: String = line.iter().collect();
            if index.row + 1 < self.state.lines.len() {
                text.push('\n');
            }
            let pos = line.iter().take(index.col).map(|ch| ch.len_utf8()).sum();
            self.state.cursor.col = text[..floor(&text, pos)].chars().count();
        } else {
            let pos = self.offset(index);
            set_offset(&mut self.state, pos);
        }
    }

    pub(super) fn selection_range(&self) -> Option<Range<usize>> {
        Some(self.state.selection.as_ref()?.byte_range(&self.state.lines))
    }

    pub(super) fn replace_range(&mut self, range: Range<usize>, replacement: &str) {
        self.reset_navigation();
        if matches!(self.edit_mode, EditMode::Vi) && self.is_insert_mode() {
            self.state
                .insert_transaction(|state| replace_range(state, range, replacement));
        } else {
            self.state
                .transaction(|state| replace_range(state, range, replacement));
        }
    }

    pub(super) fn replace_range_atomic(&mut self, range: Range<usize>, text: &str) {
        self.reset_navigation();
        self.state
            .transaction(|state| replace_range(state, range, text));
    }

    pub(super) fn replace_selection_or_cursor(&mut self, text: &str) {
        let cursor = self.cursor_offset();
        let range = self.selection_range().unwrap_or(cursor..cursor);
        self.replace_range(range, text);
    }

    pub(super) fn handle_insert_key(&mut self, key: KeyEvent) -> bool {
        if !self.is_insert_mode() {
            return false;
        }
        use KeyCode::{Backspace, Char, Delete, End, Enter, Home, Left, Right, Tab};
        let ctrl = key.modifiers == KeyModifiers::CONTROL;
        let alt = key.modifiers == KeyModifiers::ALT;
        let plain = (key.modifiers - KeyModifiers::SHIFT).is_empty();
        let super_key = key.modifiers == KeyModifiers::SUPER;
        let ctrl_shift = key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::SHIFT);
        let ctrl_alt = key.modifiers == (KeyModifiers::CONTROL | KeyModifiers::ALT);
        let text = self.text();
        let pos = floor(&text, self.offset(self.state.cursor));
        let bol = text[..pos].rfind('\n').map_or(0, |i| i + 1);
        let eol = text[pos..].find('\n').map_or(text.len(), |i| pos + i);
        let line_start = (if pos == bol {
            previous(&text, pos)
        } else {
            bol
        })..pos;
        let movement = match key.code {
            Left if plain || ctrl || alt => Some(if ctrl || alt {
                word(&text, pos, true)
            } else {
                previous(&text, pos)
            }),
            Right if plain || ctrl || alt => Some(if ctrl || alt {
                word(&text, pos, false)
            } else {
                next(&text, pos)
            }),
            Char('b') if ctrl => Some(previous(&text, pos)),
            Char('f') if ctrl => Some(next(&text, pos)),
            Char('b') if alt => Some(word(&text, pos, true)),
            Char('f') if alt => Some(word(&text, pos, false)),
            Left if super_key => Some(bol),
            Right if super_key => Some(eol),
            Home if ctrl => Some(0),
            End if ctrl => Some(text.len()),
            Home if plain => Some(bol),
            End if plain => Some(eol),
            Char('a') if ctrl => Some(bol),
            Char('e') if ctrl => Some(eol),
            Char('<') if alt => Some(0),
            Char('>') if alt => Some(text.len()),
            _ => None,
        };
        if let Some(target) = movement {
            self.state.selection = None;
            self.state
                .insert_transaction(|state| set_offset(state, target));
            return true;
        }
        let (range, kill) = match key.code {
            Backspace if super_key => (line_start, true),
            Char('u') if ctrl => (line_start, true),
            Char('k') if ctrl => (pos..(if pos == eol { next(&text, pos) } else { eol }), true),
            Char('w') if ctrl => (word(&text, pos, true)..pos, true),
            Char('h') if ctrl_alt => (word(&text, pos, true)..pos, true),
            Backspace if alt || ctrl || ctrl_shift => (word(&text, pos, true)..pos, true),
            Delete if alt || ctrl || ctrl_shift => (pos..word(&text, pos, false), true),
            Char('d') if alt => (pos..word(&text, pos, false), true),
            Backspace if plain => (previous(&text, pos)..pos, false),
            Char('h') if ctrl => (previous(&text, pos)..pos, false),
            Delete if plain => (pos..next(&text, pos), false),
            Char('d') if ctrl => (pos..next(&text, pos), false),
            Char('y') if ctrl => {
                let yank = self.kill_buffer.clone();
                if !yank.is_empty() {
                    let range = self.selection_range().unwrap_or(pos..pos);
                    self.replace_range_atomic(range, &yank);
                }
                return true;
            }
            Char('j' | 'm') if ctrl => {
                self.replace_selection_or_cursor("\n");
                return true;
            }
            Enter => {
                self.replace_selection_or_cursor("\n");
                return true;
            }
            Char(ch) if plain && self.state.selection.is_some() => {
                self.replace_selection_or_cursor(&ch.to_string());
                return true;
            }
            Tab if plain && self.state.selection.is_some() => {
                self.replace_selection_or_cursor("\t");
                return true;
            }
            _ => return false,
        };
        let range = self.selection_range().unwrap_or(range);
        if !range.is_empty() {
            if kill {
                text[range.clone()].clone_into(&mut self.kill_buffer);
                self.replace_range_atomic(range, "");
            } else {
                self.replace_range(range, "");
            }
        }
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tui::prompt::editor::EditMode;

    #[test]
    fn word_traversal_matches_boundary_steps() {
        for text in ["", "  one two  ", "a\n\tb", "e\u{301} 👩‍💻 🇬🇧\n終"] {
            for pos in text
                .grapheme_indices(true)
                .map(|(i, _)| i)
                .chain([text.len()])
            {
                for backward in [false, true] {
                    let mut expected = pos;
                    let mut saw_word = false;
                    loop {
                        let boundary = if backward {
                            previous(text, expected)
                        } else {
                            next(text, expected)
                        };
                        if boundary == expected {
                            break;
                        }
                        let space = text[boundary.min(expected)..boundary.max(expected)]
                            .chars()
                            .all(char::is_whitespace);
                        if saw_word && space {
                            break;
                        }
                        saw_word |= !space;
                        expected = boundary;
                    }
                    assert_eq!(word(text, pos, backward), expected);
                }
            }
        }
    }

    #[test]
    fn cursor_offsets_match_whole_text_boundaries() {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        for draft in ["", "\n\n", "a\r\nb", "e\u{301}\n👩‍💻 🇬🇧\n終\n"] {
            editor.adopt(draft);
            let text = editor.text();
            for pos in 0..=text.len() + 1 {
                let boundary = floor(&text, pos.min(text.len()));
                let prefix = &text[..boundary];
                let expected = Index2::new(
                    prefix.bytes().filter(|&b| b == b'\n').count(),
                    prefix.rsplit('\n').next().unwrap_or("").chars().count(),
                );
                set_offset(&mut editor.state, pos);
                assert_eq!(editor.state.cursor, expected);
                assert_eq!(editor.offset(expected), boundary);
            }
            for row in 0..=editor.row_count() {
                let len = editor.state.lines.len_col(row).unwrap_or(0);
                for col in 0..=len + 1 {
                    let index = Index2::new(row, col);
                    let pos = editor.offset(index);
                    set_offset(&mut editor.state, pos);
                    let expected = editor.state.cursor;
                    editor.state.cursor = index;
                    editor.snap_cursor();
                    assert_eq!(editor.state.cursor, expected);
                }
            }
        }
    }
}
