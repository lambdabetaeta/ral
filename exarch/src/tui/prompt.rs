use super::commands;
use crate::bus::Inbox;
use prompt_editor::{EditMode, PromptEditor};
use ratatui::style::{Color, Modifier, Style};
/// The prompt line: editor, history, and the draft stashed while browsing it.
pub(super) struct PromptState {
    editor: PromptEditor,
    history: Vec<String>,
    hist_pos: Option<usize>,
    draft: String,
    cx_pending: bool,
    editor_request: bool,
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
