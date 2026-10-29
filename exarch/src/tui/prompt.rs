pub(super) mod editor;
mod history;
mod search;

pub(super) use history::History;

use super::commands;
use super::palette::SLATE;
use crossterm::event::{KeyCode, KeyEvent, KeyModifiers, MouseEvent, MouseEventKind};
use editor::completion::Menu;
use editor::{Anchor, EditMode, PromptEditor};
use ratatui::style::{Color, Modifier, Style};

const LARGE_PASTE_BYTES: usize = 4096;
const LARGE_PASTE_LINES: usize = 20;

fn is_large_paste(text: &str) -> bool {
    text.len() >= LARGE_PASTE_BYTES || text.lines().count() >= LARGE_PASTE_LINES
}

/// Pasted line breaks, CRLF and CR alike, as the editor's LF.
fn normalize_newlines(text: &str) -> String {
    text.replace("\r\n", "\n").replace('\r', "\n")
}

/// A left press the prompt claimed, held by the gesture's router until
/// release: the selection anchor, or none while a search, which leaves the
/// draft untouched, makes the press inert.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(super) struct Grip(Option<Anchor>);

/// The prompt line: editor and history.
pub(super) struct PromptState {
    editor: PromptEditor,
    history: History,
    notice: Option<String>,
    search: Option<search::Search>,
    large_paste: bool,
    /// The live slash-command popup, re-derived from the line by every path
    /// that moves the cursor or touches the buffer.
    menu: Option<Menu>,
}

impl PromptState {
    pub(super) fn new(vi: bool, history: History) -> Self {
        let mut editor = PromptEditor::new(if vi { EditMode::Vi } else { EditMode::Emacs });
        editor.set_base_style(Style::default().fg(Color::White));
        Self {
            editor,
            history,
            notice: None,
            search: None,
            large_paste: false,
            menu: None,
        }
    }

    pub(super) fn submit(&mut self) -> Option<String> {
        let prompt = self.editor.text();
        if prompt.is_empty() {
            return None;
        }
        if let Err(e) = self.history.record(&prompt) {
            self.notice = Some(e);
        }
        self.large_paste = false;
        self.editor.adopt("");
        self.refresh_menu();
        Some(prompt)
    }

    /// Pull every queued prompt back for revision, blank-line separated.  Declines
    /// while a draft or a history browse is live, so Up never discards typed text.
    pub(super) fn edit_queued_prompt(&mut self, recall: impl FnOnce() -> Vec<String>) -> bool {
        if self.is_searching()
            || self.editor.is_searching()
            || !self.editor.is_insert_mode()
            || self.history.is_browsing()
            || !self.editor.is_empty()
        {
            return false;
        }
        let prompts = recall();
        if prompts.is_empty() {
            return false;
        }
        let joined = prompts.join("\n\n");
        self.set_prompt(&joined);
        true
    }

    pub(super) fn restore_stopped_prompts(&mut self, prompts: Vec<String>) {
        let mut prompts = prompts.into_iter();
        let Some(mut text) = prompts.next() else {
            return;
        };
        for prompt in prompts {
            text.push_str("\n\n");
            text.push_str(&prompt);
        }
        let draft = self.editor.text();
        if !draft.is_empty() {
            text.push_str("\n\n");
            text.push_str(&draft);
        }
        self.adopt_draft(&text);
    }

    pub(super) fn paste(&mut self, s: &str) {
        let text = normalize_newlines(s);
        if let Some(search) = &mut self.search {
            search.query.push_str(&text);
            search.restart(self.history.entries());
            return;
        }
        let entered_draft = self.editor.insert_str(&text);
        self.large_paste |= entered_draft && is_large_paste(&text);
        self.refresh_menu();
    }

    /// Adopt the external editor's `text` as the live draft, ending any history
    /// browse so a later Up/Down cannot overwrite the edit.
    pub(super) fn adopt_draft(&mut self, text: &str) {
        self.history.stop_browsing();
        self.set_prompt(text);
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
        if !plain {
            return false;
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

    #[cfg(test)]
    pub(super) fn editor_is_searching(&self) -> bool {
        self.editor.is_searching()
    }

    pub(super) fn search_status(&self) -> Option<String> {
        self.search
            .as_ref()
            .map(|search| search.status(self.history.entries()))
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

    /// Offer `key` to a live search, which owns every key while it lasts —
    /// the editor's native Vi search or Ctrl-R history search — or let Ctrl-R
    /// open the latter.
    pub(super) fn search_key(&mut self, key: KeyEvent) -> bool {
        if self.editor.is_searching() {
            self.edit_input(key);
            return true;
        }
        let ctrl = |c| key.code == KeyCode::Char(c) && key.modifiers == KeyModifiers::CONTROL;
        let plain = |code| key.code == code && key.modifiers == KeyModifiers::NONE;
        let Some(search) = &mut self.search else {
            if !ctrl('r') || !self.editor.is_insert_mode() {
                return false;
            }
            self.menu = None;
            let mut search = search::Search::default();
            search.restart(self.history.entries());
            self.search = Some(search);
            return true;
        };
        let history = self.history.entries();
        if key.code == KeyCode::Esc || ctrl('c') {
            self.search = None;
            self.refresh_menu();
        } else if plain(KeyCode::Enter) {
            match self.search.take().and_then(|s| s.matched) {
                Some(i) => {
                    let text = history[i].clone();
                    self.adopt_draft(&text);
                }
                None => self.refresh_menu(),
            }
        } else if ctrl('r') || plain(KeyCode::Up) {
            search.older(history);
        } else if ctrl('s') || plain(KeyCode::Down) {
            search.newer(history);
        } else if plain(KeyCode::Backspace) {
            search.backspace();
            search.restart(history);
        } else if let KeyCode::Char(c) = key.code
            && !key
                .modifiers
                .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT | KeyModifiers::SUPER)
            && !c.is_control()
        {
            search.query.push(c);
            search.restart(history);
        }
        true
    }

    /// Claim a left press inside the box, placing the cursor unless a search
    /// owns the draft.
    pub(super) fn press(&mut self, event: MouseEvent) -> Option<Grip> {
        if !self.contains_position(event.column, event.row) {
            return None;
        }
        if self.is_searching() {
            return Some(Grip(None));
        }
        let anchor = self.editor.press(event);
        self.refresh_menu();
        Some(Grip(anchor))
    }

    /// Extend a claimed press's selection to `event`, a drag or the release.
    pub(super) fn drag(&mut self, grip: Grip, event: MouseEvent) {
        if let Grip(Some(anchor)) = grip
            && self.editor.drag(anchor, event)
        {
            self.refresh_menu();
        }
    }

    /// Claim a wheel notch inside the box, scrolling unless a search owns the draft.
    pub(super) fn wheel(&mut self, event: MouseEvent) -> bool {
        if !matches!(
            event.kind,
            MouseEventKind::ScrollUp | MouseEventKind::ScrollDown
        ) || !self.contains_position(event.column, event.row)
        {
            return false;
        }
        if !self.is_searching() {
            self.editor.wheel(event);
            self.refresh_menu();
        }
        true
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

    /// Browse is possible only from Insert mode, outside any search, on a
    /// draft still reading what browsing last showed.
    fn browsable(&self) -> bool {
        !self.is_searching()
            && !self.editor.is_searching()
            && self.editor.is_insert_mode()
            && self.history.shows(&self.editor.text())
    }

    /// Recall the previous entry from the very start of the draft; false,
    /// nothing changed, where browsing does not apply.
    pub(super) fn history_prev(&mut self) -> bool {
        if !self.browsable() || !self.editor.at_start() {
            return false;
        }
        let Some(entry) = self.history.older() else {
            return false;
        };
        self.set_prompt(&entry);
        true
    }

    /// Recall the next entry, or the empty draft past the newest, from the
    /// very end of the draft; false, nothing changed, where browsing does not
    /// apply.
    pub(super) fn history_next(&mut self) -> bool {
        if !self.browsable() || !self.editor.at_end() {
            return false;
        }
        let Some(entry) = self.history.newer() else {
            return false;
        };
        self.set_prompt(&entry);
        true
    }

    /// Route a text-input key to `PromptEditor::handle_key`, which absorbs vi mode
    /// and the shell line-edit chords itself.
    pub(super) fn edit_input(&mut self, k: KeyEvent) {
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

    pub(super) fn render(&mut self, f: &mut ratatui::Frame<'_>, inner: ratatui::layout::Rect) {
        self.editor.render(f, inner);
    }

    pub(super) fn cursor_screen_position(&self) -> Option<(u16, u16)> {
        self.editor.cursor_screen_position().map(|p| (p.x, p.y))
    }
}

#[cfg(test)]
mod tests;
