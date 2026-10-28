//! LOCAL PATCH: bounded native history and custom edit transactions.
use std::{collections::VecDeque, mem::size_of};

use crate::{EditorMode, EditorState, Index2, Lines};

#[derive(Debug, Clone)]
pub(crate) struct History {
    undo: VecDeque<UndoState>,
    redo: VecDeque<UndoState>,
    max_steps: usize,
    max_bytes: usize,
    depth: usize,
    inserting: bool,
    pending_insert: Option<Index2>,
}

impl Default for History {
    fn default() -> Self {
        Self {
            undo: VecDeque::new(),
            redo: VecDeque::new(),
            max_steps: 100,
            max_bytes: 8 * 1024 * 1024,
            depth: 0,
            inserting: false,
            pending_insert: None,
        }
    }
}

impl History {
    fn trim(&mut self) {
        let mut bytes = self.undo.iter().chain(&self.redo).map(UndoState::bytes).sum::<usize>();
        while bytes > self.max_bytes || self.undo.len() + self.redo.len() > self.max_steps {
            let Some(old) = self.undo.pop_front().or_else(|| self.redo.pop_front()) else {
                break;
            };
            bytes -= old.bytes();
        }
    }
}

#[derive(Debug, Clone)]
struct UndoState {
    lines: Vec<Vec<char>>,
    cursor: Index2,
}

impl UndoState {
    fn new(state: &EditorState) -> Self {
        Self { lines: state.lines.clone().into_vecs(), cursor: state.cursor }
    }

    fn bytes(&self) -> usize {
        size_of::<Self>()
            + self.lines.capacity() * size_of::<Vec<char>>()
            + self.lines.iter().map(|row| row.capacity() * size_of::<char>()).sum::<usize>()
    }

    fn restore(self, state: &mut EditorState) {
        // Traversal during insertion abandons that unfinished repeat command.
        if state.insert_recording.take().is_some() {
            state.last_change = None;
            state.last_insert = None;
        }
        state.lines = Lines::new(self.lines);
        state.cursor = self.cursor;
        state.selection = None;
        state.clamp_column();
    }
}

impl EditorState {
    pub(crate) fn invalidate_redo(&mut self) {
        if self.history.depth == 0 {
            self.history.redo.clear();
        }
    }

    /// Captures the current text and cursor before an external edit.
    /// Prefer `transaction` when an edit might be a no-op.
    pub fn capture(&mut self) {
        if self.history.depth != 0 {
            return;
        }
        self.history.redo.clear();
        self.history.undo.push_back(UndoState::new(self));
        self.history.trim();
    }

    /// Bounds the combined undo and redo snapshots by steps and retained bytes.
    /// Bytes include character storage, row containers and snapshot metadata.
    pub fn set_history_limits(&mut self, steps: usize, bytes: usize) {
        self.history.max_steps = steps;
        self.history.max_bytes = bytes;
        self.history.trim();
    }

    /// Clears undo and redo without changing editor configuration or dot-repeat.
    pub fn clear_history(&mut self) {
        self.history.undo.clear();
        self.history.redo.clear();
        self.history.inserting = false;
        self.history.pending_insert = None;
    }

    /// Performs one atomic edit, suppressing nested captures and ignoring no-ops.
    /// Atomic edits end the current insert undo group.
    pub fn transaction<R>(&mut self, edit: impl FnOnce(&mut Self) -> R) -> R {
        self.edit_transaction(false, edit)
    }

    /// Joins the current insert undo group; does not infer edits for dot-repeat.
    /// Use `replace_range` and `move_to` for repeatable custom insert edits.
    pub fn insert_transaction<R>(&mut self, edit: impl FnOnce(&mut Self) -> R) -> R {
        self.edit_transaction(true, edit)
    }

    fn edit_transaction<R>(&mut self, insert: bool, edit: impl FnOnce(&mut Self) -> R) -> R {
        if self.history.depth != 0 {
            return edit(self);
        }
        let mut before = UndoState::new(self);
        let mode = self.mode;
        let grouped = insert && self.history.inserting;
        self.history.depth += 1;
        let result = edit(self);
        self.history.depth -= 1;
        let changed = !self.lines.iter_row().eq(before.lines.iter());
        let entered_insert = mode != EditorMode::Insert && self.mode == EditorMode::Insert;
        if changed {
            self.history.redo.clear();
            if !grouped {
                self.history.undo.push_back(before);
            } else if let Some(cursor) = self.history.pending_insert.take() {
                before.cursor = cursor;
                self.history.undo.push_back(before);
            }
        } else if entered_insert && !grouped {
            self.history.pending_insert = Some(before.cursor);
        }
        self.history.inserting = self.mode == EditorMode::Insert
            && (entered_insert || (insert && (self.history.inserting || changed)));
        if !self.history.inserting {
            self.history.pending_insert = None;
        }
        self.history.trim();
        result
    }

    pub fn undo(&mut self) {
        self.history.inserting = false;
        self.history.pending_insert = None;
        if let Some(previous) = self.history.undo.pop_back() {
            self.history.redo.push_back(UndoState::new(self));
            previous.restore(self);
            self.history.trim();
        }
    }

    pub fn redo(&mut self) {
        self.history.inserting = false;
        self.history.pending_insert = None;
        if let Some(next) = self.history.redo.pop_back() {
            self.history.undo.push_back(UndoState::new(self));
            next.restore(self);
            self.history.trim();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn transaction_suppresses_nested_captures_and_ignores_noops() {
        let mut state = EditorState::new(Lines::from("a"));
        state.transaction(|state| {
            state.capture();
            state.transaction(|state| state.replace_range(0..1, "b"));
        });
        state.transaction(|_| ());
        assert_eq!(state.history.undo.len(), 1);
        state.undo();
        assert_eq!(state.lines, Lines::from("a"));
        state.transaction(|_| ());
        state.redo();
        assert_eq!(state.lines, Lines::from("b"));
    }

    #[test]
    fn empty_insert_sessions_do_not_capture_or_invalidate_redo() {
        use crate::{actions::SwitchMode, events::{KeyEventHandler, KeyInput}};
        let mut state = EditorState::new(Lines::from("a"));
        state.transaction(|state| state.replace_range(0..1, "b"));
        state.undo();
        let mut handler = KeyEventHandler::vim_mode();
        for _ in 0..3 {
            handler.on_event(KeyInput::new('i'), &mut state);
            handler.on_event(KeyInput::new(crossterm::event::KeyCode::Esc), &mut state);
        }
        assert!(state.history.undo.is_empty());
        assert_eq!(state.history.redo.len(), 1);
        state.redo();
        assert_eq!(state.lines, Lines::from("b"));

        state.transaction(|state| state.execute(SwitchMode(EditorMode::Insert)));
        state.insert_transaction(|state| state.replace_range(1..1, "c"));
        assert_eq!(state.history.undo.len(), 2);
        state.undo();
        assert_eq!(state.lines, Lines::from("b"));
    }

    #[test]
    fn insert_noop_before_first_edit_does_not_skip_capture() {
        let mut state = EditorState::new(Lines::from("a"));
        state.mode = EditorMode::Insert;
        state.insert_transaction(|_| ());
        state.insert_transaction(|state| state.replace_range(1..1, "b"));
        state.undo();
        assert_eq!(state.lines, Lines::from("a"));
    }

    #[test]
    fn snapshot_bytes_count_allocated_capacity() {
        let mut row = Vec::with_capacity(64);
        row.push('a');
        let mut rows = Vec::with_capacity(8);
        rows.push(row);
        let snapshot = UndoState { lines: rows, cursor: Index2::new(0, 0) };
        assert_eq!(snapshot.bytes(), size_of::<UndoState>()
            + snapshot.lines.capacity() * size_of::<Vec<char>>()
            + snapshot.lines[0].capacity() * size_of::<char>());
    }

    #[test]
    fn divergent_edit_invalidates_redo() {
        let mut state = EditorState::new(Lines::from("a"));
        state.transaction(|state| state.replace_range(0..1, "b"));
        state.undo();
        state.transaction(|state| state.replace_range(0..1, "c"));
        state.redo();
        assert_eq!(state.lines, Lines::from("c"));
    }

    #[test]
    fn budgets_cover_both_stacks_and_oversized_snapshots() {
        let mut state = EditorState::new(Lines::from("a"));
        for text in ["b", "c", "d"] {
            state.transaction(|state| state.replace_range(0..1, text));
        }
        state.undo();
        assert_eq!(state.history.redo.len(), 1);
        state.set_history_limits(2, usize::MAX);
        assert_eq!(state.history.undo.len() + state.history.redo.len(), 2);
        state.set_history_limits(100, UndoState::new(&state).bytes());
        assert_eq!(state.history.undo.len() + state.history.redo.len(), 1);
        state.set_history_limits(100, 0);
        assert!(state.history.undo.is_empty() && state.history.redo.is_empty());
    }
}
