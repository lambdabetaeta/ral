use super::*;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use edtui::actions::SwitchMode;
use ratatui::{Terminal, backend::TestBackend};

fn key(editor: &mut PromptEditor, code: KeyCode, modifiers: KeyModifiers) {
    editor.handle_key(KeyEvent::new(code, modifiers));
}

fn ctrl(editor: &mut PromptEditor, ch: char) {
    key(editor, KeyCode::Char(ch), KeyModifiers::CONTROL);
}

fn undo(editor: &mut PromptEditor) {
    ctrl(editor, '_');
}
fn redo(editor: &mut PromptEditor) {
    key(
        editor,
        KeyCode::Char('Z'),
        KeyModifiers::CONTROL | KeyModifiers::SHIFT,
    );
}

fn copy_selection(editor: &mut PromptEditor) -> Option<String> {
    let text = editor.selection_text()?;
    editor.clear_selection();
    Some(text)
}

fn select(editor: &mut PromptEditor, start: Index2, end: Index2) {
    let mode = editor.state.mode;
    editor.state.execute(SwitchMode(EditorMode::Visual));
    let selection = editor.state.selection.as_mut().unwrap();
    selection.start = start;
    selection.end = end;
    editor.state.mode = mode;
}

#[test]
fn append_at_combining_cluster_replays_the_same_boundary_policy() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("e\u{301}");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    for code in [KeyCode::Char('a'), KeyCode::Char('X'), KeyCode::Esc] {
        key(&mut editor, code, KeyModifiers::NONE);
    }
    assert_eq!(editor.text(), "Xe\u{301}");
    let cursor = editor.state.cursor;
    key(&mut editor, KeyCode::Char('u'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "e\u{301}");
    key(&mut editor, KeyCode::Char('.'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "Xe\u{301}");
    assert_eq!(editor.state.cursor, cursor);
}

#[test]
fn mouse_movement_inside_insert_is_part_of_native_repeat() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("e\u{301}z");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    editor.state.cursor.col = 0;
    key(&mut editor, KeyCode::Char('i'), KeyModifiers::NONE);
    let mut terminal = Terminal::new(TestBackend::new(12, 3)).unwrap();
    terminal
        .draw(|frame| editor.render(frame, frame.area()))
        .unwrap();
    assert!(
        editor
            .press(mouse(MouseEventKind::Down(MouseButton::Left), 1, 0))
            .is_some()
    );
    key(&mut editor, KeyCode::Char('X'), KeyModifiers::NONE);
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(editor.text(), "e\u{301}Xz");
    key(&mut editor, KeyCode::Char('u'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "e\u{301}z");
    key(&mut editor, KeyCode::Char('.'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "e\u{301}Xz");
}

#[test]
fn native_combining_only_visual_selection_paints_exactly_the_copied_cluster() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("ae\u{301}z");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    for ch in "0llv".chars() {
        key(&mut editor, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    assert_eq!(editor.state.selection.as_ref().unwrap().start.col, 2);
    let mut terminal = Terminal::new(TestBackend::new(12, 3)).unwrap();
    terminal
        .draw(|frame| editor.render(frame, frame.area()))
        .unwrap();
    let buffer = terminal.backend().buffer();
    let cells: Vec<_> = buffer
        .content
        .iter()
        .filter(|cell| Some(cell.bg) == EditorTheme::default().selection_style.bg)
        .map(ratatui::buffer::Cell::symbol)
        .collect();
    assert_eq!(cells.concat(), "e\u{301}");
    assert_eq!(editor.selection_text().as_deref(), Some("e\u{301}"));
    assert_eq!(
        editor
            .state
            .selection
            .as_ref()
            .unwrap()
            .copy_from(&editor.state.lines)
            .to_string(),
        "e\u{301}"
    );
}

#[test]
fn kill_yank_are_independent_undo_steps_in_both_insert_modes() {
    for mode in [EditMode::Emacs, EditMode::Vi] {
        let mut editor = PromptEditor::new(mode);
        editor.adopt("abc def");
        editor.state.cursor.col = 3;
        ctrl(&mut editor, 'k');
        assert_eq!(editor.text(), "abc");
        ctrl(&mut editor, 'y');
        assert_eq!(editor.text(), "abc def");
        undo(&mut editor);
        assert_eq!(editor.text(), "abc");
        undo(&mut editor);
        assert_eq!(editor.text(), "abc def");
        redo(&mut editor);
        assert_eq!(editor.text(), "abc");
        redo(&mut editor);
        assert_eq!(editor.text(), "abc def");
    }
}

#[test]
fn kills_cross_newlines_and_survive_adoption() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("a\nb");
    editor.state.cursor = Index2::new(0, 1);
    ctrl(&mut editor, 'k');
    assert_eq!(editor.text(), "ab");
    undo(&mut editor);
    assert_eq!(editor.text(), "a\nb");
    editor.state.cursor = Index2::new(1, 0);
    ctrl(&mut editor, 'u');
    assert_eq!(editor.text(), "ab");
    editor.adopt("fresh");
    ctrl(&mut editor, 'y');
    assert_eq!(editor.text(), "fresh\n");
    undo(&mut editor);
    assert_eq!(editor.text(), "fresh");
}

#[test]
fn no_op_kill_preserves_yank_and_redo() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("abc");
    ctrl(&mut editor, 'u');
    undo(&mut editor);
    ctrl(&mut editor, 'k');
    redo(&mut editor);
    assert_eq!(editor.text(), "");
    ctrl(&mut editor, 'y');
    assert_eq!(editor.text(), "abc");
}

#[test]
fn divergent_edits_discard_redo() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("a");
    editor.insert_str("b");
    undo(&mut editor);
    editor.insert_str("c");
    redo(&mut editor);
    assert_eq!(editor.text(), "ac");
}

#[test]
fn paste_is_literal_at_cursor() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("ab");
    editor.state.cursor.col = 1;
    editor.insert_str("\nx\ny\n");
    assert_eq!(editor.text(), "a\nx\ny\nb");
    undo(&mut editor);
    assert_eq!(editor.text(), "ab");
    redo(&mut editor);
    assert_eq!(editor.text(), "a\nx\ny\nb");
}

#[test]
fn deletion_and_horizontal_movement_preserve_extended_graphemes() {
    for grapheme in ["e\u{301}", "👩‍💻", "🇬🇧", "क्ष", "界"] {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.adopt(&format!("a{grapheme}b"));
        key(&mut editor, KeyCode::Left, KeyModifiers::NONE);
        assert_eq!(editor.state.cursor.col, 1 + grapheme.chars().count());
        key(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
        assert_eq!(editor.text(), "ab");
        undo(&mut editor);
        ctrl(&mut editor, 'b');
        assert_eq!(editor.state.cursor.col, 1);
        key(&mut editor, KeyCode::Delete, KeyModifiers::NONE);
        assert_eq!(editor.text(), "ab");
        undo(&mut editor);
        ctrl(&mut editor, 'f');
        assert_eq!(editor.state.cursor.col, 1 + grapheme.chars().count());
    }
}

#[test]
fn reversed_selection_expands_to_grapheme_boundaries() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("ae\u{301}👩‍💻z");
    select(&mut editor, Index2::new(0, 4), Index2::new(0, 2));
    assert_eq!(copy_selection(&mut editor).as_deref(), Some("e\u{301}👩‍💻"));
    assert!(editor.state.selection.is_none());
    assert_eq!(editor.text(), "ae\u{301}👩‍💻z");
    undo(&mut editor);
    assert_eq!(editor.text(), "ae\u{301}👩‍💻z");
}

#[test]
fn text_paste_and_delete_replace_selection_once() {
    for code in [Some(KeyCode::Char('x')), Some(KeyCode::Backspace), None] {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.adopt("abc\ndef");
        select(&mut editor, Index2::new(0, 1), Index2::new(1, 1));
        match code {
            Some(code) => key(&mut editor, code, KeyModifiers::NONE),
            None => assert!(editor.insert_str("x\ny")),
        }
        assert_eq!(
            editor.text(),
            match code {
                Some(KeyCode::Char(_)) => "axf",
                Some(_) => "af",
                None => "ax\nyf",
            }
        );
        undo(&mut editor);
        assert_eq!(editor.text(), "abc\ndef");
    }
}

#[test]
fn empty_paste_does_not_delete_selection() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("abc");
    select(&mut editor, Index2::new(0, 0), Index2::new(0, 1));
    editor.insert_str("");
    assert_eq!(editor.text(), "abc");
    assert!(editor.state.selection.is_some());
}

#[test]
fn word_aliases_and_line_aliases_agree() {
    for modifiers in [KeyModifiers::ALT, KeyModifiers::CONTROL] {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.adopt("ab 界e\u{301}");
        key(&mut editor, KeyCode::Left, modifiers);
        assert_eq!(editor.state.cursor.col, 3);
        key(&mut editor, KeyCode::Right, modifiers);
        assert_eq!(editor.state.cursor.col, 6);
        key(&mut editor, KeyCode::Backspace, modifiers);
        assert_eq!(editor.text(), "ab ");
        ctrl(&mut editor, 'y');
        assert_eq!(editor.text(), "ab 界e\u{301}");
        ctrl(&mut editor, 'a');
        assert_eq!(editor.state.cursor.col, 0);
        ctrl(&mut editor, 'e');
        assert_eq!(editor.state.cursor.col, 6);
    }
}

#[test]
fn height_is_saturating_bounded_and_character_wrapped() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("ab cd");
    assert_eq!(editor.height_hint(4, 20), 4);
    editor.adopt("a\t界");
    assert_eq!(editor.height_hint(4, 20), 4);
    editor.adopt("abcd");
    assert_eq!(editor.height_hint(4, 20), 4);
    editor.adopt(&"\n".repeat(70_000));
    assert_eq!(editor.height_hint(10, u16::MAX), u16::MAX);
    for max in 0..4 {
        assert_eq!(editor.height_hint(0, max), max);
    }
}

#[test]
fn height_includes_rendered_cursor_on_full_fragments() {
    for text in ["abcd", "abcdef", "界界", "a\t界", "ab cd", "e\u{301}xyz"] {
        let mut editor = PromptEditor::new(EditMode::Emacs);
        editor.adopt(text);
        let height = editor.height_hint(4, 20);
        let mut terminal = Terminal::new(TestBackend::new(12, 20)).unwrap();
        terminal
            .draw(|frame| editor.render(frame, Rect::new(2, 3, 4, height - 2)))
            .unwrap();
        let cursor = editor.cursor_screen_position().unwrap();
        assert!(cursor.x < 4, "{text:?}: {cursor:?}");
        assert!(cursor.y < height - 2, "{text:?}: {cursor:?}");
    }
}

#[test]
fn one_cell_render_cannot_loop_on_wide_scalars() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("界\t");
    assert_eq!(editor.height_hint(1, 10), 3);
    let mut terminal = Terminal::new(TestBackend::new(1, 2)).unwrap();
    terminal
        .draw(|frame| editor.render(frame, Rect::new(0, 0, 1, 2)))
        .unwrap();
}

#[test]
fn vi_native_delete_and_undo_share_native_history() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("abc");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('x'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "ab");
    key(&mut editor, KeyCode::Char('u'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "abc");
    ctrl(&mut editor, 'r');
    assert_eq!(editor.text(), "ab");
    assert!(!editor.is_insert_mode());
}

#[test]
fn vi_insert_session_is_one_undo_step() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("abc");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('a'), KeyModifiers::NONE);
    for ch in "def".chars() {
        key(&mut editor, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(editor.text(), "abcdef");
    undo(&mut editor);
    assert_eq!(editor.text(), "abc");
    redo(&mut editor);
    assert_eq!(editor.text(), "abcdef");
}

#[test]
fn vi_insert_corrections_and_newlines_stay_in_the_session() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    for code in [
        KeyCode::Char('a'),
        KeyCode::Char('b'),
        KeyCode::Backspace,
        KeyCode::Char('c'),
        KeyCode::Enter,
        KeyCode::Char('d'),
        KeyCode::Esc,
    ] {
        key(&mut editor, code, KeyModifiers::NONE);
    }
    assert_eq!(editor.text(), "ac\nd");
    undo(&mut editor);
    assert_eq!(editor.text(), "");
    redo(&mut editor);
    assert_eq!(editor.text(), "ac\nd");
}

#[test]
fn external_edits_cancel_pending_but_undo_preserves_native_repeat() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("abc");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('x'), KeyModifiers::NONE);
    undo(&mut editor);
    key(&mut editor, KeyCode::Char('.'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "ab");
    key(&mut editor, KeyCode::Char('d'), KeyModifiers::NONE);
    editor.insert_str("!");
    assert!(!editor.handler.is_pending());
    undo(&mut editor);
    assert_eq!(editor.text(), "ab");
}

#[test]
fn pending_vi_arguments_named_u_are_not_undo() {
    for (command, expected, insert) in [
        ("ru", "uau", false),
        ("fu", "aau", false),
        ("dfu", "", false),
        ("cfu", "", true),
    ] {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.adopt("aau!");
        key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
        key(&mut editor, KeyCode::Char('x'), KeyModifiers::NONE);
        editor.state.cursor.col = 0;
        for ch in command.chars() {
            key(&mut editor, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        assert_eq!(editor.text(), expected, "{command}");
        assert_eq!(editor.is_insert_mode(), insert, "{command}");
        if command == "fu" {
            assert_eq!(editor.state.cursor.col, 2);
        }
    }
}

#[test]
fn corrected_change_inner_word_repeats_and_undoes_natively() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("foo bar");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    editor.state.cursor.col = 0;
    for ch in "ciwx👩‍💻".chars() {
        key(&mut editor, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    key(&mut editor, KeyCode::Backspace, KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('é'), KeyModifiers::NONE);
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(editor.text(), "xé bar");
    key(&mut editor, KeyCode::Char('w'), KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('.'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "xé xé");
    undo(&mut editor);
    assert_eq!(editor.text(), "xé bar");
    undo(&mut editor);
    assert_eq!(editor.text(), "foo bar");
    redo(&mut editor);
    redo(&mut editor);
    assert_eq!(editor.text(), "xé xé");
}

#[test]
fn change_inner_word_repeat_records_custom_movement_and_paste() {
    for paste in [false, true] {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.adopt("foo bar");
        key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
        editor.state.cursor.col = 0;
        for ch in "ciw".chars() {
            key(&mut editor, KeyCode::Char(ch), KeyModifiers::NONE);
        }
        if paste {
            editor.insert_str("axb");
        } else {
            for ch in "ab".chars() {
                key(&mut editor, KeyCode::Char(ch), KeyModifiers::NONE);
            }
            key(&mut editor, KeyCode::Left, KeyModifiers::NONE);
            key(&mut editor, KeyCode::Char('x'), KeyModifiers::NONE);
        }
        key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(editor.text(), "axb bar");
        key(&mut editor, KeyCode::Char('w'), KeyModifiers::NONE);
        key(&mut editor, KeyCode::Char('.'), KeyModifiers::NONE);
        assert_eq!(editor.text(), "axb axb");
        undo(&mut editor);
        assert_eq!(editor.text(), "axb bar");
    }
}

#[test]
fn paste_repeat_preserves_the_actual_range_when_following_text_matches() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("foo bar qux");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    editor.state.cursor.col = 0;
    for ch in "ciw".chars() {
        key(&mut editor, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    editor.insert_str(" b");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(editor.text(), " b bar qux");
    key(&mut editor, KeyCode::Char('w'), KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('.'), KeyModifiers::NONE);
    assert_eq!(editor.text(), " b  b qux");
    undo(&mut editor);
    assert_eq!(editor.text(), " b bar qux");
}

#[test]
fn undo_during_insert_abandons_the_unfinished_repeat() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("foo bar");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    editor.state.cursor.col = 0;
    for ch in "ciwx".chars() {
        key(&mut editor, KeyCode::Char(ch), KeyModifiers::NONE);
    }
    undo(&mut editor);
    assert_eq!(editor.text(), "foo bar");
    key(&mut editor, KeyCode::Char('y'), KeyModifiers::NONE);
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('w'), KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('.'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "yfoo bar");
}

#[test]
fn inner_changes_preserve_the_insertion_point_at_end_of_line() {
    for (text, command, expected) in [
        ("foo bar", "ciw", "foo x"),
        ("foo bar.baz", "ciW", "foo x"),
        ("foo (bar", "ciw", "foo (x"),
    ] {
        let mut editor = PromptEditor::new(EditMode::Vi);
        editor.adopt(text);
        key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
        for ch in command.chars().chain(['x']) {
            let modifiers = if ch.is_uppercase() {
                KeyModifiers::SHIFT
            } else {
                KeyModifiers::NONE
            };
            key(&mut editor, KeyCode::Char(ch), modifiers);
        }
        key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
        assert_eq!(editor.text(), expected, "{command} on {text}");
        undo(&mut editor);
        assert_eq!(editor.text(), text);
    }
}

#[test]
fn native_clipboard_survives_native_history_restore() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("abc");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('x'), KeyModifiers::NONE);
    undo(&mut editor);
    key(&mut editor, KeyCode::Char('p'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "abcc");
}

#[test]
fn unicode_span_width_not_scalar_sum_determines_height() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("👩‍💻");
    assert_eq!(editor.height_hint(4, 20), 3);
    editor.adopt("🇬🇧");
    assert_eq!(editor.height_hint(4, 20), 3);
}

#[test]
fn search_owns_keys_and_paste_without_changing_the_draft() {
    let mut editor = PromptEditor::new(EditMode::Vi);
    editor.adopt("abc");
    key(&mut editor, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut editor, KeyCode::Char('/'), KeyModifiers::NONE);
    assert!(editor.is_searching());
    key(&mut editor, KeyCode::Char('b'), KeyModifiers::NONE);
    editor.insert_str("not draft text");
    assert_eq!(editor.text(), "abc");
    assert_eq!(editor.state.search_pattern(), "bnot draft text");
}

#[test]
fn c0_aliases_match_control_key_events() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("abc");
    key(&mut editor, KeyCode::Char('\u{1}'), KeyModifiers::NONE);
    assert_eq!(editor.state.cursor.col, 0);
    key(&mut editor, KeyCode::Char('\u{6}'), KeyModifiers::NONE);
    assert_eq!(editor.state.cursor.col, 1);
    key(&mut editor, KeyCode::Char('\u{b}'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "a");
    key(&mut editor, KeyCode::Char('\u{1f}'), KeyModifiers::NONE);
    assert_eq!(editor.text(), "abc");
}

fn mouse(kind: MouseEventKind, x: u16, y: u16) -> MouseEvent {
    MouseEvent {
        kind,
        column: x,
        row: y,
        modifiers: KeyModifiers::NONE,
    }
}

#[test]
fn mouse_drag_keeps_anchor_and_does_not_change_mode() {
    let mut editor = PromptEditor::new(EditMode::Emacs);
    editor.adopt("abcdef");
    let mut terminal = Terminal::new(TestBackend::new(20, 5)).unwrap();
    terminal
        .draw(|frame| editor.render(frame, Rect::new(2, 1, 10, 3)))
        .unwrap();
    assert!(
        editor
            .press(mouse(MouseEventKind::Down(MouseButton::Left), 0, 0))
            .is_none()
    );
    let anchor = editor
        .press(mouse(MouseEventKind::Down(MouseButton::Left), 3, 1))
        .expect("a press inside the box");
    for event in [
        mouse(MouseEventKind::Drag(MouseButton::Left), 4, 1),
        mouse(MouseEventKind::Drag(MouseButton::Left), 6, 1),
        mouse(MouseEventKind::Up(MouseButton::Left), 6, 1),
    ] {
        assert!(editor.drag(anchor, event));
    }
    assert!(!editor.drag(anchor, mouse(MouseEventKind::Drag(MouseButton::Left), 0, 0)));
    assert!(editor.is_insert_mode());
    assert_eq!(copy_selection(&mut editor).as_deref(), Some("bcde"));
    assert!(copy_selection(&mut editor).is_none());
    assert_eq!(editor.text(), "abcdef");
}
