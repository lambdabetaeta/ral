use super::commands;
use super::palette::SLATE;
use crate::bus::Inbox;
use prompt_editor::completion::Menu;
use prompt_editor::{EditMode, PromptEditor};
use ratatui::crossterm::event::KeyCode;
use ratatui::style::{Color, Modifier, Style};
/// The prompt line: editor, history, and the draft stashed while browsing it.
pub(super) struct PromptState {
    editor: PromptEditor,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
    cx_pending: bool,
    editor_request: bool,
    /// The live slash-command popup, re-derived from the line by every path
    /// that moves the cursor or touches the buffer.
    menu: Option<Menu>,
}

impl PromptState {
    pub(super) fn new(vi: bool) -> Self {
        let mut editor =
            PromptEditor::new(if vi { EditMode::Vi } else { EditMode::Emacs }).wrap(true);
        editor.set_base_style(Style::default().fg(Color::White));
        Self {
            editor,
            history: Vec::new(),
            hist_pos: None,
            draft: String::new(),
            cx_pending: false,
            editor_request: false,
            menu: None,
        }
    }

    pub(super) fn submit(&mut self) -> Option<String> {
        let prompt = self.editor.text();
        if prompt.is_empty() {
            return None;
        }
        self.history.push(prompt.clone());
        self.hist_pos = None;
        self.editor.clear();
        self.refresh_menu();
        Some(prompt)
    }

    /// Pull every queued prompt back for revision, blank-line separated.  Declines
    /// while a draft or a history browse is live, so Up never discards typed text.
    pub(super) fn edit_queued_prompt(&mut self, inbox: &Inbox) -> bool {
        if self.hist_pos.is_some() || !self.editor.is_empty() {
            return false;
        }
        let Some(prompts) = inbox.pop_back_user_all() else {
            return false;
        };
        let joined = prompts.join("\n\n");
        self.set_prompt(&joined);
        true
    }

    pub(super) fn paste(&mut self, s: &str) {
        self.cx_pending = false;
        self.editor.insert_str(s);
        self.refresh_menu();
    }

    /// Adopt the external editor's `text` as the live draft, ending any history
    /// browse so a later Up/Down cannot overwrite the edit.
    pub(super) fn adopt_draft(&mut self, text: &str) {
        self.hist_pos = None;
        self.set_prompt(text);
    }

    /// Take the pending `C-x C-e` request — the UI loop drains it after every edit
    /// key and suspends into `terminal::compose_in_editor`.
    pub(super) fn take_editor_request(&mut self) -> bool {
        std::mem::take(&mut self.editor_request)
    }

    /// Arm the `C-x` prefix: the next `C-e` opens the editor.
    pub(super) fn set_cx_pending(&mut self) {
        self.cx_pending = true;
    }

    pub(super) fn clear_cx_pending(&mut self) {
        self.cx_pending = false;
    }

    pub(super) fn take_cx_pending(&mut self) -> bool {
        std::mem::take(&mut self.cx_pending)
    }

    pub(super) fn request_editor(&mut self) {
        self.editor_request = true;
    }

    pub(super) fn prompt_text(&self) -> String {
        self.editor.text()
    }

    /// Glow cyan and bold when the line names a slash command and the trunk tab has
    /// focus — `commands::route_submit` refuses one typed on a sub-agent tab.
    /// Restyled every frame by `render::draw`.
    pub(super) fn style_prompt(&mut self, on_trunk: bool) {
        let text = self.editor.text();
        let style = if on_trunk && commands::is_slash_command(&text) {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        self.editor.set_base_style(style);
    }

    /// Replace the prompt's contents, leaving the cursor at the end.
    pub(super) fn set_prompt(&mut self, s: &str) {
        self.editor.clear();
        self.editor.insert_str(s);
        self.refresh_menu();
    }

    /// The open slash-command popup, for the frame to float above the box.
    pub(super) fn menu(&self) -> Option<&Menu> {
        self.menu.as_ref()
    }

    /// Re-derive the popup from the line as it now stands, so it can never
    /// describe a line that is no longer there.  It is live only while the
    /// cursor is still composing a command token on the first row; the token
    /// starts at that row's first byte, and the area the frame hands
    /// [`Menu::render`] is the box's interior, so neither offset is the
    /// popup's to carry.
    fn refresh_menu(&mut self) {
        let candidates = match self.editor.line(0) {
            Some(line) if self.editor.row() == 0 => commands::command_candidates(&line),
            _ => Vec::new(),
        };
        self.menu = Menu::open(candidates, 0, 0, 0).map(|m| {
            m.style(Style::default().fg(Color::Cyan), Style::default().fg(SLATE))
                .detail_style(Style::default().fg(SLATE))
        });
    }

    /// Offer `code` to an open popup, which owns ↓/Tab, ↑/⇧Tab, Enter and Esc
    /// for as long as it is up, and reports whether it took the key.
    ///
    /// Enter takes the highlighted command into the line and closes rather
    /// than submitting: a line under an open popup is not yet what the user
    /// has chosen.  A second Enter, with the popup gone, sends it.  Unless the
    /// line already spells the choice — the popup then has nothing to add, and
    /// Enter belongs to the submit it looks like: the popup closes and declines
    /// the key rather than eating a keystroke to no visible effect.
    pub(super) fn menu_key(&mut self, code: KeyCode) -> bool {
        let Some(menu) = self.menu.as_mut() else {
            return false;
        };
        match code {
            KeyCode::Down | KeyCode::Tab => menu.select_next(),
            KeyCode::Up | KeyCode::BackTab => menu.select_prev(),
            KeyCode::Enter => {
                let menu = self.menu.take().expect("the popup is open");
                return menu.accept(&mut self.editor);
            }
            KeyCode::Esc => self.menu = None,
            _ => return false,
        }
        true
    }

    /// Recall the previous prompt; the live draft is stashed on entry.
    pub(super) fn history_prev(&mut self) {
        let pos = match self.hist_pos {
            _ if self.history.is_empty() => return,
            None => {
                self.draft = self.editor.text();
                self.history.len() - 1
            }
            Some(0) => return,
            Some(i) => i - 1,
        };
        self.hist_pos = Some(pos);
        let entry = self.history[pos].clone();
        self.set_prompt(&entry);
    }

    /// Recall the next prompt, or the stashed draft once past the newest entry.
    pub(super) fn history_next(&mut self) {
        let Some(i) = self.hist_pos else {
            return;
        };
        if i + 1 < self.history.len() {
            self.hist_pos = Some(i + 1);
            let entry = self.history[i + 1].clone();
            self.set_prompt(&entry);
        } else {
            self.hist_pos = None;
            let draft = std::mem::take(&mut self.draft);
            self.set_prompt(&draft);
        }
    }

    /// Route a text-input key to `PromptEditor::handle_key`, which absorbs vi mode
    /// and the shell line-edit chords itself.
    pub(super) fn edit_input(&mut self, k: ratatui::crossterm::event::KeyEvent) {
        self.editor.handle_key(k);
        self.refresh_menu();
    }
    pub(super) fn height_hint(&self, text_width: u16, area_height: u16) -> u16 {
        self.editor.height_hint(text_width, area_height)
    }

    pub(super) fn row(&self) -> usize {
        self.editor.row()
    }

    pub(super) fn row_count(&self) -> usize {
        self.editor.row_count()
    }

    pub(super) fn render(&mut self, f: &mut ratatui::Frame<'_>, inner: ratatui::layout::Rect) {
        self.editor.render(f, inner);
    }

    pub(super) fn cursor_screen_position(&self) -> Option<(u16, u16)> {
        self.editor.cursor_screen_position().map(|p| (p.x, p.y))
    }
}
