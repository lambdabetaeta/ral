use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use ratatui::{Terminal, backend::TestBackend};

fn render(editor: &mut PromptEditor, width: u16) {
    let mut terminal = Terminal::new(TestBackend::new(width, 8)).unwrap();
    terminal
        .draw(|frame| editor.render(frame, frame.area()))
        .unwrap();
}

fn key(editor: &mut PromptEditor, code: KeyCode) {
    editor.handle_key(KeyEvent::new(code, KeyModifiers::NONE));
}

#[test]
fn arrows_and_control_p_n_traverse_wrapped_rows() {
    for mode in [EditMode::Emacs, EditMode::Vi] {
        for control in [false, true] {
            let mut editor = PromptEditor::new(mode);
            editor.adopt("abcdefghij");
            render(&mut editor, 4);
            for (down, col) in [(false, 6), (false, 2), (true, 6), (true, 10)] {
                let code = match (control, down) {
                    (false, false) => KeyCode::Up,
                    (false, true) => KeyCode::Down,
                    (true, false) => KeyCode::Char('p'),
                    (true, true) => KeyCode::Char('n'),
                };
                editor.handle_key(KeyEvent::new(
                    code,
                    if control {
                        KeyModifiers::CONTROL
                    } else {
                        KeyModifiers::NONE
                    },
                ));
                assert_eq!(editor.state.cursor, Index2::new(0, col));
            }
        }
    }
}

#[test]
fn terminal_cell_column_survives_short_rows_and_clusters() {
    for (text, start, down) in [("a界e\u{301}b👩‍💻z", 2, 8), ("a\tbcd", 2, 5)] {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.adopt(text);
        editor.state.cursor.col = start;
        render(&mut editor, 4);
        key(&mut editor, KeyCode::Down);
        assert_eq!(editor.state.cursor.col, down);
        key(&mut editor, KeyCode::Up);
        assert_eq!(editor.state.cursor.col, start);
    }
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("abcd\nx\nabcd");
    editor.state.cursor.col = 3;
    render(&mut editor, 8);
    key(&mut editor, KeyCode::Up);
    assert_eq!(editor.state.cursor, Index2::new(1, 1));
    key(&mut editor, KeyCode::Up);
    assert_eq!(editor.state.cursor, Index2::new(0, 3));
}

#[test]
fn full_final_row_has_a_navigable_insertion_row() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("abcdefgh");
    render(&mut editor, 4);
    key(&mut editor, KeyCode::Up);
    assert_eq!(editor.state.cursor.col, 4);
    key(&mut editor, KeyCode::Down);
    assert_eq!(editor.state.cursor.col, 8);
}

#[test]
fn resize_motion_and_mutation_reset_the_preferred_column() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("abcdefghij");
    editor.state.cursor.col = 3;
    render(&mut editor, 4);
    key(&mut editor, KeyCode::Down);
    key(&mut editor, KeyCode::Down);
    assert_eq!(editor.state.cursor.col, 10);
    render(&mut editor, 5);
    key(&mut editor, KeyCode::Up);
    assert_eq!(editor.state.cursor.col, 5);
    key(&mut editor, KeyCode::Right);
    key(&mut editor, KeyCode::Up);
    assert_eq!(editor.state.cursor.col, 1);
    editor.insert_str("x");
    key(&mut editor, KeyCode::Down);
    assert_eq!(editor.state.cursor.col, 7);
}

#[test]
fn cursor_induced_height_growth_preserves_the_preferred_column() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("abcd");
    editor.state.cursor.col = 3;
    let mut terminal = Terminal::new(TestBackend::new(4, 3)).unwrap();
    terminal
        .draw(|frame| editor.render(frame, Rect::new(0, 0, 4, 1)))
        .unwrap();
    key(&mut editor, KeyCode::Down);
    assert_eq!(editor.state.cursor.col, 4);
    terminal
        .draw(|frame| editor.render(frame, Rect::new(0, 0, 4, 2)))
        .unwrap();
    key(&mut editor, KeyCode::Up);
    assert_eq!(editor.state.cursor.col, 3);
}

#[test]
fn normal_vi_j_k_still_move_logical_lines() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("abcdefghij\nlast");
    key(&mut editor, KeyCode::Esc);
    editor.state.cursor = Index2::new(0, 1);
    render(&mut editor, 4);
    key(&mut editor, KeyCode::Char('j'));
    assert_eq!(editor.state.cursor, Index2::new(1, 1));
    key(&mut editor, KeyCode::Char('k'));
    assert_eq!(editor.state.cursor, Index2::new(0, 1));
}

#[test]
fn midwrapped_history_entry_does_not_recall_history() {
    let mut prompt = crate::tui::prompt::tests::with_history(false, &["older", "abcdefghij"]);
    assert!(prompt.history_prev());
    prompt.editor.state.cursor.col = 6;
    render(&mut prompt.editor, 4);
    assert!(!prompt.history_prev());
    assert!(!prompt.history_next());
    assert_eq!(prompt.editor.text(), "abcdefghij");
    key(&mut prompt.editor, KeyCode::Up);
    assert_eq!(prompt.editor.state.cursor.col, 2);
    assert!(prompt.history.is_browsing());
}
