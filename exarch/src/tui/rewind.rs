//! The `/rewind` overlay: the prompts on screen, one of which is where the
//! conversation goes back to.  While it is up the transcript behind it
//! previews the cut — every row from the highlighted prompt on is ghosted
//! (`render::draw`, from the row `Scrollback::render_cut_window` returns) and
//! scrolled into view — so what Enter undoes is seen before it is done.
//! Display and input only; [`rewind`] drives it and posts the [`Rewrite`].

use super::app::Overlay;
use super::line::truncate_spans;
use super::overlay::{self, Anchor};
use super::palette::{CYAN, Col, SLATE};
use super::tui_loop::{OverlayTick, Tui, overlay_tick};
use crate::bus::{Mailbox, Post, Rewrite};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const BODY_W: u16 = 96;
const VISIBLE_ROWS: usize = 8;

pub(super) struct RewindOverlay {
    /// Turn-opening prompts on screen, oldest first; never empty.
    prompts: Vec<(u64, String)>,
    selected: usize,
}

impl RewindOverlay {
    /// Over `prompts`, the latest highlighted; `None` with nothing to rewind.
    fn new(prompts: Vec<(u64, String)>) -> Option<Self> {
        let selected = prompts.len().checked_sub(1)?;
        Some(Self { prompts, selected })
    }

    /// The turn the highlighted prompt opened — the anchor Enter would post.
    pub(super) fn anchor(&self) -> u64 {
        self.prompts[self.selected].0
    }

    /// Move the highlight, or resolve on Enter.
    fn key(&mut self, code: KeyCode) -> Option<u64> {
        match code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.prompts.len() - 1),
            KeyCode::PageUp => self.selected = self.selected.saturating_sub(VISIBLE_ROWS),
            KeyCode::PageDown => {
                self.selected = (self.selected + VISIBLE_ROWS).min(self.prompts.len() - 1);
            }
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.prompts.len() - 1,
            KeyCode::Enter => return Some(self.anchor()),
            _ => {}
        }
        None
    }

    pub(super) fn render(&self, f: &mut Frame, frame: Rect) {
        let rows = self.prompts.len().min(VISIBLE_ROWS);
        let list_h = u16::try_from(rows).unwrap_or(u16::MAX);
        let heights = [list_h, 1, 2];
        let body = overlay::draw(
            f,
            frame,
            Anchor::Corner,
            (BODY_W, usize::from(heights.iter().sum::<u16>())),
            &format!(" REWIND {}/{} ", self.selected + 1, self.prompts.len()),
            " ↑↓ choose · pgup/pgdn page · home/end · enter rewind · esc keep ",
        );
        let [list, _, note] = Layout::vertical(heights.map(Constraint::Length)).areas(body);
        f.render_widget(
            Paragraph::new(self.list_lines(usize::from(list.height), list.width)),
            list,
        );
        f.render_widget(
            Paragraph::new(format!(
                "⚠ This will remove the conversation from turn {} onwards from the context.\nThe state of the shell or the code will not be changed.",
                self.anchor()
            ))
            .style(Style::default().fg(SLATE).add_modifier(Modifier::DIM)),
            note,
        );
    }

    /// `window` rows of `turn N  opening line`, the highlight kept in view,
    /// turn figures ending level.
    fn list_lines(&self, window: usize, width: u16) -> Vec<Line<'static>> {
        let turns: Vec<String> = self
            .prompts
            .iter()
            .map(|(turn, _)| turn.to_string())
            .collect();
        let figures = Col::of(turns.iter().map(String::as_str));
        let start = self
            .selected
            .saturating_sub(window / 2)
            .min(self.prompts.len().saturating_sub(window));
        (start..)
            .zip(&turns[start..])
            .take(window)
            .map(|(i, turn)| {
                let lit = |style: Style| {
                    if i == self.selected {
                        style.add_modifier(Modifier::REVERSED)
                    } else {
                        style
                    }
                };
                let opening = self.prompts[i].1.lines().next().unwrap_or_default();
                let spans = [
                    Span::styled(
                        format!("turn {}  ", figures.right(turn)),
                        lit(Style::default().fg(CYAN)),
                    ),
                    Span::styled(opening.to_owned(), lit(Style::default().fg(SLATE))),
                ];
                Line::from(truncate_spans(&spans, usize::from(width)))
            })
            .collect()
    }
}

/// `/rewind`: choose a prompt on the trunk's transcript and post the rewind
/// to before it.  The viewport the preview moved is put back either way; a
/// rewind that lands truncates the mirror as it always did.
pub(super) fn rewind(tui: &mut Tui, mailbox: &Mailbox) {
    let root = tui.app.tabs.root();
    let Some(sb) = tui.app.tabs.scrollback_mut(root) else {
        return;
    };
    let Some(overlay) = RewindOverlay::new(sb.prompts()) else {
        tui.app
            .push_error(root, "nothing to rewind: no prompt is on screen yet");
        return;
    };
    let viewport = sb.viewport();
    tui.app.overlay = Some(Overlay::Rewind(overlay));
    let anchor = drive(tui);
    tui.app.overlay = None;
    if let Some(sb) = tui.app.tabs.scrollback_mut(root) {
        sb.set_viewport(viewport);
    }
    if let Some(anchor) = anchor {
        mailbox.push(Post::Rewrite(Rewrite::Rewind(anchor)));
    }
}

/// Poll keys until the overlay resolves; `None` on cancel.
fn drive(tui: &mut Tui) -> Option<u64> {
    loop {
        match overlay_tick(tui) {
            OverlayTick::Idle | OverlayTick::Save | OverlayTick::Paste(_) => {}
            OverlayTick::Key(code) => {
                if let Some(anchor) = tui.app.rewind_mut()?.key(code) {
                    return Some(anchor);
                }
            }
            OverlayTick::Cancel | OverlayTick::TerminalLost => return None,
        }
    }
}
