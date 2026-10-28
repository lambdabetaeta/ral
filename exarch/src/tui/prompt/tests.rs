use super::*;
use crate::bus::Inbox;
use ratatui::crossterm::event::{KeyEvent, KeyModifiers};

fn key(prompt: &mut PromptState, code: KeyCode, modifiers: KeyModifiers) {
    prompt.edit_input(KeyEvent::new(code, modifiers));
}

fn undo(prompt: &mut PromptState) {
    key(prompt, KeyCode::Esc, KeyModifiers::NONE);
    key(prompt, KeyCode::Char('u'), KeyModifiers::NONE);
}

#[test]
fn slash_completion_is_one_edit_with_redo() {
    let mut prompt = PromptState::new(true);
    prompt.set_prompt("/th");
    key(&mut prompt, KeyCode::Char('i'), KeyModifiers::NONE);
    assert!(prompt.menu_key(KeyCode::Enter));
    assert_eq!(prompt.prompt_text(), "/thinking");
    assert!(prompt.menu().is_none());
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "/thi");
    key(&mut prompt, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(prompt.prompt_text(), "/thinking");
}

#[test]
fn accept_replaces_the_whole_token_wherever_the_cursor_is() {
    let mut prompt = PromptState::new(false);
    prompt.set_prompt("/thi");
    key(&mut prompt, KeyCode::Left, KeyModifiers::NONE);
    key(&mut prompt, KeyCode::Left, KeyModifiers::NONE);
    assert!(prompt.menu_key(KeyCode::Enter));
    assert_eq!(prompt.prompt_text(), "/thinking");
}

#[test]
fn normal_mode_menu_accept_replaces_the_token_and_stays_normal() {
    let mut prompt = PromptState::new(true);
    prompt.set_prompt("/thi");
    key(&mut prompt, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut prompt, KeyCode::Right, KeyModifiers::NONE);
    assert!(prompt.menu_key(KeyCode::Enter));
    assert_eq!(prompt.prompt_text(), "/thinking");
    key(&mut prompt, KeyCode::Char('u'), KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "/thi");
    key(&mut prompt, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert_eq!(prompt.prompt_text(), "/thinking");
}

#[test]
fn aliases_accept_their_own_spelling_without_the_gloss() {
    let mut prompt = PromptState::new(true);
    prompt.set_prompt("/exi");
    assert!(prompt.menu_key(KeyCode::Enter));
    assert_eq!(prompt.prompt_text(), "/exit");
    prompt.set_prompt("/expor");
    assert!(prompt.menu_key(KeyCode::Enter));
    assert_eq!(prompt.prompt_text(), "/export");
    assert!(prompt.submit().is_some());
}

#[test]
fn an_identity_accept_declines_the_submit_key() {
    let mut prompt = PromptState::new(true);
    prompt.set_prompt("/thinking");
    assert!(!prompt.menu_key(KeyCode::Enter));
    assert!(prompt.menu().is_none());
    assert_eq!(prompt.submit().as_deref(), Some("/thinking"));
}

#[test]
fn submit_and_fresh_draft_cannot_undo_into_a_submitted_prompt() {
    let mut prompt = PromptState::new(true);
    prompt.paste("submitted");
    assert_eq!(prompt.submit().as_deref(), Some("submitted"));
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "");
    prompt.set_prompt("fresh");
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "fresh");
    assert_eq!(prompt.history, ["submitted"]);
}

#[test]
fn history_restore_starts_local_undo_for_the_stashed_draft() {
    let mut prompt = PromptState::new(true);
    prompt.paste("old");
    prompt.submit();
    prompt.paste("draft");
    prompt.history_prev();
    assert_eq!(prompt.prompt_text(), "old");
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "old");
    prompt.history_next();
    assert_eq!(prompt.prompt_text(), "draft");
    assert_eq!(prompt.hist_pos, None);
    key(&mut prompt, KeyCode::Char('!'), KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "draft!");
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "draft");
    key(&mut prompt, KeyCode::Char('u'), KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "draft");
}

#[test]
fn external_editor_adoption_ends_browsing_and_discards_old_undo() {
    let mut prompt = PromptState::new(true);
    prompt.paste("history");
    prompt.submit();
    prompt.paste("draft");
    prompt.history_prev();
    prompt.adopt_draft("external\n界\n");
    assert_eq!(prompt.hist_pos, None);
    assert_eq!(prompt.row(), 2);
    prompt.history_next();
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "external\n界\n");
    assert_eq!(prompt.history, ["history"]);
    prompt.history_prev();
    prompt.history_next();
    assert_eq!(prompt.prompt_text(), "external\n界\n");
}

#[test]
fn queued_recall_is_fresh_and_keeps_history_bookkeeping() {
    let mut prompt = PromptState::new(true);
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    prompt.paste("submitted");
    prompt.submit();
    inbox.push_user("one".into());
    inbox.push_user("two".into());
    assert!(prompt.edit_queued_prompt(|| mailbox.recall()));
    assert_eq!(mailbox.queued_human_messages(), Vec::<String>::new());
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "one\ntwo");
    assert_eq!(prompt.hist_pos, None);
    assert_eq!(prompt.history, ["submitted"]);
    inbox.push_user("waiting".into());
    assert!(!prompt.edit_queued_prompt(|| panic!("a draft must guard recall")));
    assert_eq!(mailbox.queued_human_messages(), ["waiting"]);
    prompt.history_prev();
    assert!(!prompt.edit_queued_prompt(|| panic!("history must guard recall")));
    prompt.history_next();
    assert_eq!(prompt.prompt_text(), "one\ntwo");
}

#[test]
fn narrow_ascii_command_menu_clips_the_name_before_the_border() {
    use ratatui::{Terminal, backend::TestBackend, layout::Rect};
    let mut prompt = PromptState::new(false);
    prompt.set_prompt("/expor");
    let mut terminal = Terminal::new(TestBackend::new(10, 3)).unwrap();
    terminal
        .draw(|f| prompt.menu().unwrap().render(f, Rect::new(0, 0, 10, 3)))
        .unwrap();
    let row: String = (0..10)
        .map(|x| terminal.backend().buffer()[(x, 1)].symbol())
        .collect();
    assert_eq!(row, "│/export…│");
    assert!(prompt.menu_key(KeyCode::Enter));
    assert_eq!(prompt.prompt_text(), "/export");
}
