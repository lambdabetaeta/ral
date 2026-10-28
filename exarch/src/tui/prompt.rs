pub(super) mod editor;
mod history;
mod search;

use super::commands;
use super::palette::SLATE;
use crate::bus::Mailbox;
use editor::completion::Menu;
use editor::{EditMode, PromptEditor};
use ratatui::crossterm::event::{KeyCode, KeyEvent, KeyEventKind, KeyModifiers, MouseEvent};
use ratatui::style::{Color, Modifier, Style};

/// The prompt line: editor, history, and the draft stashed while browsing it.
pub(super) struct PromptState {
    editor: PromptEditor,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
    cx_pending: bool,
    persistence: Option<history::Persistence>,
    notice: Option<String>,
    search: Option<search::Search>,
    large_paste: bool,
    /// The live slash-command popup, re-derived from the line by every path
    /// that moves the cursor or touches the buffer.
    menu: Option<Menu>,
}

impl PromptState {
    pub(super) fn new(vi: bool) -> Self {
        let mut editor = PromptEditor::new(if vi { EditMode::Vi } else { EditMode::Emacs });
        editor.set_base_style(Style::default().fg(Color::White));
        #[cfg(not(test))]
        let (persistence, entries, notice) = match history::Persistence::open() {
            Ok(store) => match store.load() {
                Ok(entries) => (Some(store), entries, None),
                // Never overwrite an unreadable history file.
                Err(e) => (None, Vec::new(), Some(e)),
            },
            Err(e) => (None, Vec::new(), Some(e)),
        };
        #[cfg(test)]
        let (persistence, entries, notice) = (None, Vec::new(), None);
        Self {
            editor,
            history: entries,
            persistence,
            notice,
            search: None,
            large_paste: false,
            hist_pos: None,
            draft: String::new(),
            cx_pending: false,
            menu: None,
        }
    }

    pub(super) fn submit(&mut self) -> Option<String> {
        let prompt = self.editor.text();
        if self.is_searching() || prompt.is_empty() {
            return None;
        }
        if self.history.last() != Some(&prompt) {
            self.history.push(prompt.clone());
            history::bound(&mut self.history);
        }
        if let Some(store) = &self.persistence {
            match store.record(&prompt) {
                Ok(entries) => self.history = entries,
                Err(e) => self.notice = Some(e),
            }
        }
        self.hist_pos = None;
        self.draft.clear();
        self.large_paste = false;
        self.editor.adopt("");
        self.refresh_menu();
        Some(prompt)
    }

    /// Pull every queued prompt back for revision, blank-line separated.  Declines
    /// while a draft or a history browse is live, so Up never discards typed text.
    pub(super) fn edit_queued_prompt(&mut self, mailbox: &Mailbox) -> bool {
        if self.is_searching()
            || self.editor.is_searching()
            || !self.editor.is_insert_mode()
            || self.hist_pos.is_some()
            || !self.editor.text().is_empty()
        {
            return false;
        }
        let Some(prompts) = mailbox.pop_back_user_all() else {
            return false;
        };
        let joined = prompts.join("\n\n");
        self.set_prompt(&joined);
        true
    }

    pub(super) fn paste(&mut self, s: &str) {
        self.cx_pending = false;
        let text = search::normalize(s);
        if let Some(search) = &mut self.search {
            search.query.push_str(&text);
            search.restart(&self.history);
            return;
        }
        self.large_paste |= text.len() >= 4096 || text.lines().count() >= 20;
        self.editor.insert_str(&text);
        self.refresh_menu();
    }

    /// Adopt the external editor's `text` as the live draft, ending any history
    /// browse so a later Up/Down cannot overwrite the edit.
    pub(super) fn adopt_draft(&mut self, text: &str) {
        self.hist_pos = None;
        self.set_prompt(text);
    }

    /// Arm the `C-x` prefix: the next `C-e` opens the editor.
    pub(super) fn set_cx_pending(&mut self) {
        self.cx_pending = true;
    }

    pub(super) fn clear_cx_pending(&mut self) {
        self.cx_pending = false;
    }

    pub(super) fn cx_pending(&self) -> bool {
        self.cx_pending
    }

    pub(super) fn take_cx_pending(&mut self) -> bool {
        std::mem::take(&mut self.cx_pending)
    }

    pub(super) fn prompt_text(&self) -> String {
        self.editor.text()
    }

    /// Glow cyan and bold when the line names a slash command and the focused
    /// tab is steerable — a watch-only tab takes no line, command or prompt.
    /// Restyled every frame by `render::draw`.
    pub(super) fn style_prompt(&mut self, steerable: bool) {
        let text = self.editor.text();
        let style = if steerable && commands::is_slash_command(&text) {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        self.editor.set_base_style(style);
    }

    /// Install a fresh draft with its own undo history, cursor at the end.
    pub(super) fn set_prompt(&mut self, s: &str) {
        self.search = None;
        self.large_paste = false;
        self.editor.adopt(s);
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
        self.menu = Menu::open(candidates).map(|m| {
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
    pub(super) fn menu_key(&mut self, key: KeyEvent) -> bool {
        let plain = key.modifiers == KeyModifiers::NONE
            || (key.code == KeyCode::BackTab && key.modifiers == KeyModifiers::SHIFT);
        if key.kind == KeyEventKind::Release || !plain {
            return false;
        }
        if key.code == KeyCode::Enter && key.kind != KeyEventKind::Press {
            return self.menu.is_some();
        }
        let Some(menu) = self.menu.as_mut() else {
            return false;
        };
        match key.code {
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

    pub(super) fn take_notice(&mut self) -> Option<String> {
        self.notice.take()
    }

    pub(super) fn is_searching(&self) -> bool {
        self.search.is_some()
    }

    pub(super) fn editor_is_searching(&self) -> bool {
        self.editor.is_searching()
    }

    pub(super) fn search_status(&self) -> Option<String> {
        self.search
            .as_ref()
            .map(|search| search.status(&self.history))
    }

    /// A presentation-only summary; submission always uses the actual editor text.
    pub(super) fn paste_preview(&self) -> Option<String> {
        if !self.large_paste || self.editor.is_empty() {
            return None;
        }
        let text = self.editor.text();
        Some(format!(
            "Large paste draft: {} bytes, {} lines",
            text.len(),
            text.lines().count()
        ))
    }

    pub(super) fn handle_search_key(&mut self, key: KeyEvent) -> bool {
        let reverse = key.code == KeyCode::Char('r') && key.modifiers == KeyModifiers::CONTROL;
        if key.kind == KeyEventKind::Release {
            return self.is_searching();
        }
        if !self.is_searching() {
            if !reverse || !self.editor.is_insert_mode() || self.editor.is_searching() {
                return false;
            }
            self.cx_pending = false;
            self.menu = None;
            let mut search = search::Search::default();
            search.restart(&self.history);
            self.search = Some(search);
            return true;
        }
        if key.code == KeyCode::Esc
            || (key.code == KeyCode::Char('c') && key.modifiers == KeyModifiers::CONTROL)
        {
            self.search = None;
            self.refresh_menu();
            return true;
        }
        if key.code == KeyCode::Enter && key.modifiers == KeyModifiers::NONE {
            if key.kind != KeyEventKind::Press {
                return true;
            }
            let matched = self.search.take().and_then(|s| s.matched);
            if let Some(i) = matched {
                let text = self.history[i].clone();
                self.adopt_draft(&text);
            } else {
                self.refresh_menu();
            }
            return true;
        }
        let search = self.search.as_mut().expect("search owns input");
        if reverse || (key.code == KeyCode::Up && key.modifiers == KeyModifiers::NONE) {
            search.older(&self.history);
        } else if (key.code == KeyCode::Char('s') && key.modifiers == KeyModifiers::CONTROL)
            || (key.code == KeyCode::Down && key.modifiers == KeyModifiers::NONE)
        {
            search.newer(&self.history);
        } else if key.code == KeyCode::Backspace && key.modifiers == KeyModifiers::NONE {
            search.backspace();
            search.restart(&self.history);
        } else if let KeyCode::Char(c) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            && !c.is_control()
        {
            search.query.push(c);
            search.restart(&self.history);
        }
        true
    }

    pub(super) fn mouse(&mut self, event: MouseEvent) -> bool {
        if self.is_searching() {
            return true;
        }
        let handled = self.editor.handle_mouse(event);
        if handled {
            self.refresh_menu();
        }
        handled
    }

    pub(super) fn selection_text(&self) -> Option<String> {
        self.editor.selection_text()
    }

    pub(super) fn clear_selection(&mut self) {
        self.editor.clear_selection();
    }

    pub(super) fn contains_position(&self, column: u16, row: u16) -> bool {
        self.editor.contains_position(column, row)
    }

    pub(super) fn cancel_mouse(&mut self) {
        self.editor.cancel_mouse();
    }

    fn history_unchanged(&self) -> bool {
        let text = self.editor.text();
        match self.hist_pos {
            Some(i) => self.history[i] == text,
            None => text.is_empty(),
        }
    }

    pub(super) fn history_can_prev(&self) -> bool {
        !self.is_searching()
            && !self.editor.is_searching()
            && self.editor.is_insert_mode()
            && self.editor.at_start()
            && self.history_unchanged()
            && !self.history.is_empty()
            && self.hist_pos != Some(0)
    }

    pub(super) fn history_can_next(&self) -> bool {
        !self.is_searching()
            && !self.editor.is_searching()
            && self.editor.is_insert_mode()
            && self.editor.at_end()
            && self.hist_pos.is_some()
            && self.history_unchanged()
    }

    /// Recall the previous prompt; the live draft is stashed on entry.
    pub(super) fn history_prev(&mut self) {
        if !self.history_can_prev() {
            return;
        }
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
        if !self.history_can_next() {
            return;
        }
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
    pub(super) fn edit_input(&mut self, k: KeyEvent) {
        if self.handle_search_key(k) || k.kind == KeyEventKind::Release {
            return;
        }
        self.editor.handle_key(k);
        if k.code == KeyCode::Esc {
            self.menu = None;
        } else {
            self.refresh_menu();
        }
    }
    pub(super) fn height_hint(&self, text_width: u16, area_height: u16) -> u16 {
        self.editor.height_hint(text_width, area_height)
    }

    pub(super) fn row(&self) -> usize {
        self.editor.row()
    }

    pub(super) fn render(&mut self, f: &mut ratatui::Frame<'_>, inner: ratatui::layout::Rect) {
        self.editor.render(f, inner);
    }

    pub(super) fn cursor_screen_position(&self) -> Option<(u16, u16)> {
        self.editor.cursor_screen_position().map(|p| (p.x, p.y))
    }
}

#[cfg(test)]
mod tests;
