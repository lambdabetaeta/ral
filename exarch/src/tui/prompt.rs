use super::commands;
use crate::bus::Inbox;
use prompt_editor::{EditMode, PromptEditor};
use ratatui::style::{Color, Modifier, Style};
/// The prompt editing state: editor, history, and draft management.
///
/// Extracted from [`super::App`] to keep the TUI struct flat and
/// the prompt's concerns in one place.
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

    pub fn submit(&mut self) -> Option<String> {
        let prompt = self.editor.text();
        if prompt.is_empty() {
            return None;
        }
        self.history.push(prompt.clone());
        self.hist_pos = None;
        self.editor.clear();
        Some(prompt)
    }

    /// Pull every pending prompt back into the editor for revision, joined
    /// with a blank line so each queued message stays distinct.  A non-empty
    /// live draft wins over queue editing: Up keeps its ordinary history
    /// behaviour rather than discarding text the user has started.
    pub(super) fn edit_queued_prompt(&mut self, inbox: &mut Inbox) -> bool {
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

    pub fn paste(&mut self, s: &str) {
        self.cx_pending = false;
        self.editor.insert_str(s);
    }

    /// Adopt `text` returned by the external editor as the live prompt draft,
    /// leaving any in-progress history browse so a later Up/Down does not
    /// overwrite the edit.
    pub(super) fn adopt_draft(&mut self, text: &str) {
        self.hist_pos = None;
        self.set_prompt(text);
    }

    /// Take the pending `C-x C-e` request, if any: the UI loop calls this after
    /// each edit key to learn whether it must suspend for the external editor.
    pub(super) fn take_editor_request(&mut self) -> bool {
        std::mem::take(&mut self.editor_request)
    }

    /// Note that Ctrl-X was just pressed: the next `C-e` opens the editor.
    pub(super) fn set_cx_pending(&mut self) {
        self.cx_pending = true;
    }

    /// Dismiss the Ctrl-X prefix without action — on any mouse event.
    pub(super) fn clear_cx_pending(&mut self) {
        self.cx_pending = false;
    }

    /// Take and reset the `C-x` prefix: returns true if `C-x` was pending
    /// and clears it, so the chord either fires or is consumed.
    pub(super) fn take_cx_pending(&mut self) -> bool {
        std::mem::take(&mut self.cx_pending)
    }

    /// Request the external editor via `C-x C-e` — drained by the UI loop.
    pub(super) fn request_editor(&mut self) {
        self.editor_request = true;
    }

    /// The prompt'\''s current contents, lines newline-joined.
    pub fn prompt_text(&self) -> String {
        self.editor.text()
    }

    /// Recolor the prompt text in place: a line that names a known slash
    /// command (so the UI loop will dispatch it) glows cyan and bold; anything
    /// else stays plain white. Driven once per frame from [`super::App::draw`], so it
    /// tracks every edit — typing, paste, history recall.
    pub(super) fn style_prompt(&mut self, focused_steerable: bool) {
        let text = self.editor.text();
        let style = if focused_steerable && commands::is_slash_command(&text) {
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD)
        } else {
            Style::default().fg(Color::White)
        };
        self.editor.set_base_style(style);
    }

    /// Replace the prompt'\''s contents, leaving the cursor at the end.
    pub(super) fn set_prompt(&mut self, s: &str) {
        self.editor.clear();
        self.editor.insert_str(s);
    }

    /// Recall the previous prompt (Up from the first row).  The live
    /// draft is stashed on entry; navigation clamps at the oldest
    /// entry.  No-op when no prompts have been submitted yet.
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

    /// Recall the next prompt (Down from the last row), or restore the
    /// stashed draft once browsing walks past the newest entry.  No-op
    /// when not browsing history.
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

    /// Route a plain text-input key into the prompt.  Dispatches to
    /// [`PromptEditor::handle_key`], which folds in vi-mode handling
    /// and shell-line-edit chords (Ctrl-U) internally.
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
