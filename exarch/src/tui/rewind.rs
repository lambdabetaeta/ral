//! The `/rewind` overlay: the prompts on screen, one of which is where the
//! conversation goes back to.  While it is up the transcript behind it
//! previews the cut — every row from the highlighted prompt on is ghosted
//! (`render::draw`, from the row `Scrollback::render_cut_window` returns) and
//! scrolled into view — so what Enter undoes is seen before it is done.
//! Display and input only; [`input`] drives it and posts the [`Rewrite`].

use super::app::{App, Input, Overlay};
use super::overlay::{self, Anchor};
use super::palette::{CYAN, Col, SLATE};
use super::scrollback::Viewport;
use super::text::truncate_spans;
use crate::bus::{Post, Rewrite};
use crate::record::AgentId;
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
    /// The tab being rewound and where its viewport sat, which the preview moves.
    pub(super) tab: AgentId,
    viewport: Viewport,
}

impl RewindOverlay {
    /// Over `prompts`, the latest highlighted; `None` with nothing to rewind.
    fn new(prompts: Vec<(u64, String)>, tab: AgentId, viewport: Viewport) -> Option<Self> {
        let selected = prompts.len().checked_sub(1)?;
        Some(Self {
            prompts,
            selected,
            tab,
            viewport,
        })
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

/// `/rewind`: choose a prompt on the focused tab's transcript and post the
/// rewind to before it.  The viewport the preview moved is put back either way;
/// a rewind that lands truncates the mirror as it always did.
pub(super) fn rewind(app: &mut App) {
    let focused = app.tabs.focused();
    let Some(sb) = app.tabs.scrollback_mut(focused) else {
        return;
    };
    let Some(overlay) = RewindOverlay::new(sb.prompts(), focused, sb.viewport()) else {
        app.push_error(focused, "nothing to rewind: no prompt is on screen yet");
        return;
    };
    app.open(Overlay::Rewind(overlay));
}

/// Move the highlight; Enter posts the rewind and Esc keeps, both closing.
pub(super) fn input(r: &mut RewindOverlay, input: &Input, app: &mut App) -> bool {
    let anchor = match input {
        Input::Cancel => None,
        Input::Key(code) => match r.key(*code) {
            None => return true,
            anchor => anchor,
        },
        Input::Save | Input::Paste(_) => return true,
    };
    if let Some(sb) = app.tabs.scrollback_mut(r.tab) {
        sb.set_viewport(r.viewport);
    }
    if let Some(anchor) = anchor {
        app.gesture.clear();
        if let Some(agent) = app.tabs.agent(r.tab) {
            agent.mailbox.push(Post::Rewrite(Rewrite::Rewind(anchor)));
        } else {
            let focused = app.tabs.focused();
            app.push_error(focused, "the agent you were rewinding has ended");
        }
    }
    false
}
