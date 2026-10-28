// LOCAL PATCH: renderer/mouse regressions for shared grapheme geometry.
use crossterm::event::{KeyModifiers, MouseButton, MouseEvent, MouseEventKind};
use ratatui_core::{buffer::Buffer, layout::{Position, Rect}, widgets::Widget};

use crate::{EditorMode, EditorState, EditorTheme, EditorView, Index2, Lines};
use crate::events::mouse::MouseEventHandler;

fn render(state: &mut EditorState, width: u16, height: u16, wrap: bool) -> Buffer {
    let area = Rect::new(0, 0, width, height);
    let mut buffer = Buffer::empty(area);
    EditorView::new(state).wrap(wrap)
        .theme(EditorTheme::default().hide_status_line())
        .render(area, &mut buffer);
    buffer
}

fn mouse(state: &mut EditorState, kind: MouseEventKind, column: u16, row: u16) {
    MouseEventHandler::on_event(MouseEvent { kind, column, row, modifiers: KeyModifiers::NONE }, state);
}

#[test]
fn mouse_hits_whole_graphemes_and_the_insertion_endpoint() {
    let mut state = EditorState::new(Lines::from("e\u{301}👩\u{200d}💻🇬🇷"));
    state.mode = EditorMode::Insert;
    state.cursor = Index2::new(0, 7);
    let buffer = render(&mut state, 4, 3, true);
    assert_eq!(buffer[(1, 0)].symbol(), "👩\u{200d}💻");
    assert_eq!(buffer[(0, 1)].symbol(), "🇬🇷");
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 2, 0);
    assert_eq!(state.cursor, Index2::new(0, 2));
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 1);
    assert_eq!(state.cursor, Index2::new(0, 5));
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 3, 1);
    assert_eq!(state.cursor, Index2::new(0, 7));
}

#[test]
fn mode_change_does_not_move_the_clicked_text() {
    let mut state = EditorState::new(Lines::from("abcd\nxy\nzz"));
    state.mode = EditorMode::Visual;
    state.cursor = Index2::new(0, 4);
    let buffer = render(&mut state, 4, 4, true);
    assert_eq!(state.cursor_screen_position(), Some(Position::new(0, 1)));
    assert_eq!(buffer[(0, 2)].symbol(), "x");
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 0, 2);
    assert_eq!(state.mode, EditorMode::Normal);
    assert_eq!(state.cursor, Index2::new(1, 0));
}

#[test]
fn horizontal_crop_never_splits_another_lines_cluster() {
    let mut state = EditorState::new(Lines::from("abcdefgh\n👩\u{200d}💻Z"));
    state.mode = EditorMode::Insert;
    state.cursor = Index2::new(0, 4);
    let buffer = render(&mut state, 4, 2, false);
    assert_eq!(state.view.viewport.x, 1);
    assert_eq!(buffer[(0, 1)].symbol(), "👩\u{200d}💻");
    mouse(&mut state, MouseEventKind::Down(MouseButton::Left), 1, 1);
    assert_eq!(state.cursor, Index2::new(1, 0));
}

#[test]
fn wrapped_wheel_scrolls_inside_a_logical_line() {
    let mut state = EditorState::new(Lines::from("abcdefghijklmnopqrst"));
    state.mode = EditorMode::Insert;
    render(&mut state, 4, 2, true);
    mouse(&mut state, MouseEventKind::ScrollDown, 0, 0);
    assert_eq!(state.view.wrapped_row_offset, 1);
    assert_eq!(state.cursor, Index2::new(0, 4));
    let buffer = render(&mut state, 4, 2, true);
    assert_eq!(buffer[(0, 0)].symbol(), "e");
    mouse(&mut state, MouseEventKind::ScrollUp, 0, 0);
    let buffer = render(&mut state, 4, 2, true);
    assert_eq!(buffer[(0, 0)].symbol(), "a");
}

#[test]
fn oversized_clusters_have_an_explicit_narrow_viewport_representation() {
    let mut state = EditorState::new(Lines::from("界"));
    assert_eq!(render(&mut state, 1, 1, true)[(0, 0)].symbol(), "�");
    assert_eq!(render(&mut state, 1, 1, false)[(0, 0)].symbol(), "�");
}

#[test]
fn empty_and_zero_sized_views_have_safe_cursor_positions() {
    let mut state = EditorState::default();
    render(&mut state, 4, 1, true);
    assert_eq!(state.cursor_screen_position(), Some(Position::new(0, 0)));
    render(&mut state, 0, 0, true);
    assert_eq!(state.cursor_screen_position(), None);
}
