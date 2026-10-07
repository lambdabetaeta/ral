//! The `/rewind` overlay: the prompts on screen, one of which is where the
//! conversation goes back to.  While it is up the transcript behind it
//! previews the cut — every row from the highlighted prompt on is ghosted
//! (`render::paint_cut`) and scrolled into view — so what ⏎ undoes is seen
//! before it is done.  Display and input only; [`rewind`] drives it and posts
//! the [`Rewrite`].

use super::app::Overlay;
use super::line::truncate_spans;
use super::palette::{CYAN, Col, OVERLAY_BG, SLATE};
use super::picker::{PAD_Y, overlay_frame};
use super::tui_loop::{OverlayTick, Tui, overlay_tick};
use crate::bus::{Mailbox, Post, Rewrite};
use ratatui::Frame;
use ratatui::crossterm::event::KeyCode;
use ratatui::layout::{Constraint, Layout, Rect};
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::Paragraph;

const OVERLAY_W: u16 = 64;
const VISIBLE_ROWS: usize = 8;
/// Under the list: a blank, then the reminder of what a rewind spares.
const NOTE_ROWS: u16 = 2;
/// Kept rows shown above the cut, so the seam is read in context.
const LEAD_ROWS: usize = 2;
/// The frame's edge the overlay keeps clear for its shadow.
const SHADOW_W: u16 = 2;
const SHADOW_H: u16 = 1;

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

    /// The turn the highlighted prompt opened — the anchor ⏎ would post.
    pub(super) fn anchor(&self) -> u64 {
        self.prompts[self.selected].0
    }

    /// Move the highlight, or resolve on ⏎.
    fn key(&mut self, code: KeyCode) -> Option<u64> {
        match code {
            KeyCode::Up => self.selected = self.selected.saturating_sub(1),
            KeyCode::Down => self.selected = (self.selected + 1).min(self.prompts.len() - 1),
            KeyCode::Home => self.selected = 0,
            KeyCode::End => self.selected = self.prompts.len() - 1,
            KeyCode::Enter => return Some(self.anchor()),
            _ => {}
        }
        None
    }

    pub(super) fn render(&self, f: &mut Frame, frame: Rect) {
        let plane = Style::default().bg(OVERLAY_BG);
        let rows = self.prompts.len().min(VISIBLE_ROWS);
        let list_h = u16::try_from(rows).unwrap_or(u16::MAX);
        // The bezel's two rows around the padded body.
        let h = list_h + NOTE_ROWS + 2 * PAD_Y + 2;
        let area = corner(OVERLAY_W, h, frame);
        let inner = overlay_frame(f, area, " REWIND ", " ↑↓ choose · ⏎ rewind · esc keep ");
        let [list, _, note] = Layout::vertical([
            Constraint::Length(list_h),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(inner);
        f.render_widget(
            Paragraph::new(self.list_lines(rows, list.width)).style(plane),
            list,
        );
        f.render_widget(
            Paragraph::new(Line::from(Span::styled(
                format!(
                    "turn {} and all after it cease to be; the shell is not rewound",
                    self.anchor()
                ),
                Style::default().fg(SLATE).add_modifier(Modifier::DIM),
            )))
            .style(plane),
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
        let start = self.selected.saturating_sub(window - 1);
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

/// A `w × h` rect in `area`'s bottom-right corner, its shadow's margin kept,
/// clamped to fit.  Over the prompt box, inert under a modal, so the
/// transcript — the thing being previewed — stays uncovered above it.
fn corner(w: u16, h: u16, area: Rect) -> Rect {
    let w = w.min(area.width.saturating_sub(SHADOW_W));
    let h = h.min(area.height.saturating_sub(SHADOW_H));
    Rect {
        x: area.right().saturating_sub(w + SHADOW_W),
        y: area.bottom().saturating_sub(h + SHADOW_H),
        width: w,
        height: h,
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

/// Poll keys until the overlay resolves; `None` on cancel.  Each frame first
/// scrolls the trunk so the highlighted cut sits [`LEAD_ROWS`] below the top.
fn drive(tui: &mut Tui) -> Option<u64> {
    loop {
        reveal_cut(tui);
        match overlay_tick(tui) {
            OverlayTick::Idle => {}
            OverlayTick::Key(code) => {
                if let Some(anchor) = tui.app.rewind_mut()?.key(code) {
                    return Some(anchor);
                }
            }
            OverlayTick::Cancel | OverlayTick::TerminalLost => return None,
        }
    }
}

fn reveal_cut(tui: &mut Tui) {
    let root = tui.app.tabs.root();
    let Some(anchor) = tui.app.rewind_mut().map(|r| r.anchor()) else {
        return;
    };
    if let Some(sb) = tui.app.tabs.scrollback_mut(root)
        && let Some(row) = sb.cut_row(anchor)
    {
        sb.scroll_to(row.saturating_sub(LEAD_ROWS));
    }
}
