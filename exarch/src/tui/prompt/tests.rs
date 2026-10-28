use super::*;
use crate::bus::{Inbox, Post, Read};
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
    assert!(prompt.menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
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
    assert!(prompt.menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(prompt.prompt_text(), "/thinking");
}

#[test]
fn normal_mode_menu_accept_replaces_the_token_and_stays_normal() {
    let mut prompt = PromptState::new(true);
    prompt.set_prompt("/thi");
    key(&mut prompt, KeyCode::Esc, KeyModifiers::NONE);
    key(&mut prompt, KeyCode::Right, KeyModifiers::NONE);
    assert!(prompt.menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
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
    assert!(prompt.menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(prompt.prompt_text(), "/exit");
    prompt.set_prompt("/expor");
    assert!(prompt.menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(prompt.prompt_text(), "/export");
    assert!(prompt.submit().is_some());
}

#[test]
fn an_identity_accept_declines_the_submit_key() {
    let mut prompt = PromptState::new(true);
    prompt.set_prompt("/thinking");
    assert!(!prompt.menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
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
fn history_restore_starts_local_undo_for_the_empty_draft() {
    let mut prompt = PromptState::new(true);
    prompt.paste("old");
    prompt.submit();
    prompt.history_prev();
    assert_eq!(prompt.prompt_text(), "old");
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "old");
    assert!(!prompt.history_can_next());
    key(&mut prompt, KeyCode::Char('i'), KeyModifiers::NONE);
    key(&mut prompt, KeyCode::End, KeyModifiers::NONE);
    prompt.history_next();
    assert_eq!(prompt.prompt_text(), "");
    assert_eq!(prompt.hist_pos, None);
    key(&mut prompt, KeyCode::Char('!'), KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "!");
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "");
    key(&mut prompt, KeyCode::Char('u'), KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "");
}

#[test]
fn external_editor_adoption_ends_browsing_and_discards_old_undo() {
    let mut prompt = PromptState::new(true);
    prompt.paste("history");
    prompt.submit();
    prompt.history_prev();
    prompt.adopt_draft("external\n界\n");
    assert_eq!(prompt.hist_pos, None);
    assert!(prompt.editor.at_end());
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
    // A non-user entry keeps the steering messages separate.
    inbox.push(Post::Read(Read::Resources));
    inbox.push_user("two".into());
    assert_eq!(
        mailbox.queued_human_messages(),
        ["one", "/resources", "two"]
    );
    assert!(prompt.edit_queued_prompt(&mailbox));
    assert_eq!(mailbox.queued_human_messages(), ["/resources"]);
    assert_eq!(prompt.prompt_text(), "one\n\ntwo");
    assert!(prompt.editor.is_insert_mode());
    assert!(prompt.editor.at_end());
    undo(&mut prompt);
    assert_eq!(prompt.prompt_text(), "one\n\ntwo");
    assert_eq!(prompt.hist_pos, None);
    assert_eq!(prompt.history, ["submitted"]);
    inbox.push_user("waiting".into());
    assert!(!prompt.edit_queued_prompt(&mailbox));
    assert_eq!(mailbox.queued_human_messages(), ["/resources", "waiting"]);
    prompt.history_prev();
    assert!(!prompt.edit_queued_prompt(&mailbox));
    prompt.history_next();
    assert_eq!(prompt.prompt_text(), "one\n\ntwo");
}

#[test]
fn queued_recall_preserves_coalesced_text_and_starts_local_undo() {
    for vi in [false, true] {
        let mut prompt = PromptState::new(vi);
        let inbox = Inbox::new();
        let mailbox = inbox.mailbox();
        prompt.paste("submitted");
        prompt.submit();
        inbox.push_user("one".into());
        inbox.push_user("two\n\n界\n".into());
        let expected = "one\ntwo\n\n界\n";
        assert_eq!(mailbox.queued_human_messages(), [expected]);
        assert!(prompt.edit_queued_prompt(&mailbox));
        assert_eq!(prompt.prompt_text(), expected);
        assert!(prompt.editor.is_insert_mode());
        assert!(prompt.editor.at_end());
        key(&mut prompt, KeyCode::Char('!'), KeyModifiers::NONE);
        assert_eq!(prompt.prompt_text(), format!("{expected}!"));
        key(&mut prompt, KeyCode::Char('_'), KeyModifiers::CONTROL);
        assert_eq!(prompt.prompt_text(), expected);
        key(&mut prompt, KeyCode::Char('_'), KeyModifiers::CONTROL);
        assert_eq!(prompt.prompt_text(), expected);
        assert_eq!(prompt.hist_pos, None);
        assert_eq!(prompt.history, ["submitted"]);
        assert_eq!(prompt.submit().as_deref(), Some(expected));
        assert_eq!(prompt.history, ["submitted", expected]);
    }
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
    assert!(prompt.menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE)));
    assert_eq!(prompt.prompt_text(), "/export");
}

fn search_key(prompt: &mut PromptState, code: KeyCode, modifiers: KeyModifiers) {
    assert!(prompt.handle_search_key(KeyEvent::new(code, modifiers)));
}

#[test]
fn search_cancel_preserves_draft_cursor_and_undo() {
    let mut prompt = PromptState::new(false);
    prompt.history = vec!["old match".into(), "new match".into()];
    prompt.set_prompt("draft");
    key(&mut prompt, KeyCode::Char('!'), KeyModifiers::NONE);
    key(&mut prompt, KeyCode::Left, KeyModifiers::NONE);
    search_key(&mut prompt, KeyCode::Char('r'), KeyModifiers::CONTROL);
    prompt.paste("match");
    assert!(prompt.search_status().unwrap().contains("new match"));
    search_key(&mut prompt, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(prompt.search_status().unwrap().contains("old match"));
    search_key(&mut prompt, KeyCode::Char('c'), KeyModifiers::CONTROL);
    assert_eq!(prompt.prompt_text(), "draft!");
    key(&mut prompt, KeyCode::Char('?'), KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "draft?!");
    key(&mut prompt, KeyCode::Char('_'), KeyModifiers::CONTROL);
    assert_eq!(prompt.prompt_text(), "draft!");
}

#[test]
fn search_acceptance_never_submits_and_paste_is_normalized() {
    let mut prompt = PromptState::new(false);
    prompt.history = vec!["first\nsecond".into()];
    prompt.set_prompt("draft");
    search_key(&mut prompt, KeyCode::Char('r'), KeyModifiers::CONTROL);
    prompt.paste("first\r\nsecond");
    assert_eq!(prompt.search.as_ref().unwrap().query, "first\nsecond");
    assert_eq!(prompt.prompt_text(), "draft");
    assert!(prompt.submit().is_none());
    search_key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE);
    assert!(!prompt.is_searching());
    assert_eq!(prompt.prompt_text(), "first\nsecond");
    assert_eq!(prompt.history.len(), 1);
}

#[test]
fn failed_search_and_escape_leave_original_draft() {
    let mut prompt = PromptState::new(false);
    prompt.history = vec!["old".into()];
    prompt.set_prompt("draft");
    search_key(&mut prompt, KeyCode::Char('r'), KeyModifiers::CONTROL);
    prompt.paste("missing");
    assert!(prompt.search_status().unwrap().contains("no match"));
    search_key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "draft");
    search_key(&mut prompt, KeyCode::Char('r'), KeyModifiers::CONTROL);
    search_key(&mut prompt, KeyCode::Esc, KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "draft");
}

#[test]
fn recall_cannot_overwrite_edits_or_steal_vi_motion() {
    let mut prompt = PromptState::new(false);
    prompt.history = vec!["one".into(), "two".into()];
    prompt.history_prev();
    prompt.paste("!");
    assert!(!prompt.history_can_prev());
    assert!(!prompt.history_can_next());
    prompt.history_prev();
    prompt.history_next();
    assert_eq!(prompt.prompt_text(), "two!");
    prompt.adopt_draft("first\nlast");
    assert!(!prompt.history_can_prev());

    let mut vi = PromptState::new(true);
    vi.history = vec!["one".into()];
    key(&mut vi, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!vi.history_can_prev());
    assert!(!vi.handle_search_key(KeyEvent::new(KeyCode::Char('r'), KeyModifiers::CONTROL)));
}

#[test]
fn paste_preview_is_only_presentation_and_cannot_be_forged() {
    let mut prompt = PromptState::new(false);
    let source = format!("[Pasted text #1]\r\n{}", "界".repeat(2000));
    prompt.paste(&source);
    assert!(prompt.paste_preview().is_some());
    let actual = source.replace("\r\n", "\n");
    assert_eq!(prompt.prompt_text(), actual);
    assert_eq!(prompt.submit(), Some(actual));
    assert!(prompt.paste_preview().is_none());
    prompt.paste("[Pasted text #1]");
    assert!(prompt.paste_preview().is_none());
    assert_eq!(prompt.submit().as_deref(), Some("[Pasted text #1]"));
}

#[test]
fn modified_or_released_menu_enter_does_not_accept() {
    let mut prompt = PromptState::new(false);
    prompt.set_prompt("/thi");
    assert!(!prompt.menu_key(KeyEvent::new(KeyCode::Enter, KeyModifiers::ALT)));
    let mut release = KeyEvent::new(KeyCode::Enter, KeyModifiers::NONE);
    release.kind = KeyEventKind::Release;
    assert!(!prompt.menu_key(release));
    assert_eq!(prompt.prompt_text(), "/thi");
}

#[test]
fn persistence_failure_is_reportable_without_losing_submission() {
    let dir = ral_core::own::OwnDir::for_test("prompt-save-error").unwrap();
    dir.make_dir("prompt-history.json").unwrap();
    let mut prompt = PromptState::new(false);
    prompt.persistence = Some(history::Persistence::at(ral_core::own::OwnDir::clone(&dir)));
    prompt.paste("keep me");
    assert_eq!(prompt.submit().as_deref(), Some("keep me"));
    assert!(prompt.take_notice().unwrap().contains("prompt history"));
    assert!(prompt.take_notice().is_none());
}

#[test]
fn recall_requires_an_empty_draft_or_unchanged_entry_at_absolute_boundary() {
    let mut prompt = PromptState::new(false);
    prompt.history = vec!["older".into(), "first\nlast".into()];
    for draft in ["draft", "\n", "\n\n"] {
        prompt.adopt_draft(draft);
        key(&mut prompt, KeyCode::Home, KeyModifiers::CONTROL);
        assert!(!prompt.history_can_prev());
        prompt.history_prev();
        assert_eq!(prompt.prompt_text(), draft);
    }
    prompt.adopt_draft("");
    prompt.history_prev();
    assert_eq!(prompt.prompt_text(), "first\nlast");
    assert!(!prompt.history_can_prev());
    assert!(prompt.history_can_next());
    key(&mut prompt, KeyCode::Left, KeyModifiers::NONE);
    assert!(!prompt.history_can_next());
    key(&mut prompt, KeyCode::Home, KeyModifiers::CONTROL);
    assert!(prompt.history_can_prev());
    key(&mut prompt, KeyCode::Right, KeyModifiers::NONE);
    assert!(!prompt.history_can_prev());
    key(&mut prompt, KeyCode::Home, KeyModifiers::CONTROL);
    prompt.history_prev();
    assert_eq!(prompt.prompt_text(), "older");
}

#[test]
fn queued_recall_declines_vi_normal_and_editor_search_modes() {
    let mut prompt = PromptState::new(true);
    let inbox = Inbox::new();
    let mailbox = inbox.mailbox();
    inbox.push_user("waiting".into());
    key(&mut prompt, KeyCode::Esc, KeyModifiers::NONE);
    assert!(!prompt.edit_queued_prompt(&mailbox));
    key(&mut prompt, KeyCode::Char('/'), KeyModifiers::NONE);
    assert!(prompt.editor_is_searching());
    assert!(!prompt.edit_queued_prompt(&mailbox));
    assert_eq!(mailbox.queued_human_messages(), ["waiting"]);
}

#[test]
fn empty_search_previews_newest_and_backspace_removes_whole_graphemes() {
    let mut prompt = PromptState::new(false);
    prompt.history = vec!["old".into(), "new".into()];
    search_key(&mut prompt, KeyCode::Char('r'), KeyModifiers::CONTROL);
    assert!(prompt.search_status().unwrap().contains("new"));
    prompt.paste("e\u{301}👨‍👩‍👧‍👦");
    search_key(&mut prompt, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(prompt.search.as_ref().unwrap().query, "e\u{301}");
    search_key(&mut prompt, KeyCode::Backspace, KeyModifiers::NONE);
    assert_eq!(prompt.search.as_ref().unwrap().query, "");
    assert!(prompt.search_status().unwrap().contains("new"));
    search_key(&mut prompt, KeyCode::Enter, KeyModifiers::NONE);
    assert_eq!(prompt.prompt_text(), "new");
}
