//! The completion menu for the structural surface.
//!
//! A ranked candidate popup dropping down under the token being completed.
//! Tab opens it (when more than one candidate matches); the compose loop
//! drives Tab/↓ and ⇧Tab/↑ (cycle), Enter (accept), and Esc / any editing
//! key (dismiss) through the methods here.

use prompt_editor::PromptEditor;
use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::repl::completion::Candidate;
use ral_core::text::char_to_byte;

use super::{MENU_MAX_ROWS, NAME_HUE, SLATE};

/// An open completion menu: the ranked candidates from
/// [`completion::complete`](crate::repl::completion::complete) and the buffer
/// span they replace.
pub(super) struct Menu {
    candidates: Vec<Candidate>,
    selected: usize,
    /// Byte offset into the trigger row where the chosen replacement starts.
    replace_from: usize,
    /// The editor row the menu was opened on; accept aborts if the cursor has
    /// since left it.
    row: usize,
    /// Screen column the popup drops down under: the prompt prefix width plus
    /// the token's start column, so the list aligns under what is being typed.
    anchor_col: u16,
}

impl Menu {
    /// Open a menu over `candidates` (more than one, by construction),
    /// selecting the first.
    pub(super) fn open(
        candidates: Vec<Candidate>,
        replace_from: usize,
        row: usize,
        anchor_col: u16,
    ) -> Self {
        Self {
            candidates,
            selected: 0,
            replace_from,
            row,
            anchor_col,
        }
    }

    /// Advance the selection to the next candidate, wrapping.
    pub(super) fn select_next(&mut self) {
        let n = self.candidates.len();
        self.selected = (self.selected + 1) % n;
    }

    /// Retreat the selection to the previous candidate, wrapping.
    pub(super) fn select_prev(&mut self) {
        let n = self.candidates.len();
        self.selected = (self.selected + n - 1) % n;
    }

    /// Apply the selected candidate: replace the token from `replace_from` to
    /// the current cursor (which has not moved while the menu owned the keys)
    /// with the chosen replacement.  Aborts if the cursor has left the trigger
    /// row.
    pub(super) fn accept(&self, prompt: &mut PromptEditor) {
        let row = prompt.row();
        if row != self.row {
            return;
        }
        let Some(line) = prompt.line(row) else {
            return;
        };
        let end = char_to_byte(&line, prompt.col());
        let replacement = self.candidates[self.selected].replacement.clone();
        prompt.replace_row_bytes(row, self.replace_from, end, &replacement);
    }

    /// Draw the menu as a bordered popup dropping down over the top of the
    /// projection band, its left edge anchored under the token being completed
    /// (clamped to stay within the band).  The selected row is reversed; the
    /// list scrolls within [`MENU_MAX_ROWS`] so a long candidate set stays
    /// navigable.
    pub(super) fn render(&self, frame: &mut ratatui::Frame, area: Rect) {
        if area.height < 3 || self.candidates.is_empty() {
            return;
        }
        // Width fits the widest candidate plus borders; height fits the visible
        // rows plus borders.  Both clamp to the band.
        let widest = self
            .candidates
            .iter()
            .map(|c| {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
                )]
                let w = c.display.chars().count() as u16;
                w
            })
            .max()
            .unwrap_or(0);
        let pop_w = (widest + 2).clamp(10, area.width);
        #[allow(
            clippy::cast_possible_truncation,
            reason = "terminal coordinates are u16 (ratatui/crossterm cap columns and rows at u16)"
        )]
        let visible = (self.candidates.len() as u16)
            .min(MENU_MAX_ROWS)
            .min(area.height - 2);
        let rect = Rect {
            x: area.x + self.anchor_col.min(area.width.saturating_sub(pop_w)),
            y: area.y,
            width: pop_w,
            height: visible + 2,
        };

        // Scroll the window so the selected row stays visible.
        let window = visible as usize;
        let start = self.selected.saturating_sub(window.saturating_sub(1));
        let lines: Vec<Line> = self
            .candidates
            .iter()
            .enumerate()
            .skip(start)
            .take(window)
            .map(|(i, c)| {
                let mut style = Style::default().fg(NAME_HUE);
                if i == self.selected {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                Line::from(Span::styled(c.display.clone(), style))
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(Style::default().fg(SLATE));
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new(lines).block(block), rect);
    }
}
