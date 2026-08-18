//! The completion popup: a ranked candidate list dropping down under the token
//! being completed.
//!
//! [`Menu`] owns a selection and the buffer span the chosen [`Candidate`]
//! replaces, and nothing else — it neither gathers candidates nor ranks them
//! (that is `ral_core::text::rank`'s job, wherever the host calls it).  The
//! host owns the keys, driving [`Menu::select_next`] / [`Menu::select_prev`]
//! (cycle), [`Menu::accept`] (splice) and dismissal (drop the menu) from its own
//! event loop, and calls [`Menu::render`] with the area the popup may occupy.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use crate::{PromptEditor, char_to_byte};

/// The most candidate rows a [`Menu`] shows at once, scrolling within them.  A
/// host that reserves room for a menu before opening one reserves this many
/// rows plus two, the borders.
pub const MENU_MAX_ROWS: u16 = 6;

/// The popup is never narrower than this, so a one-character candidate still
/// reads as a list rather than a sliver — unless the area itself is narrower.
const MIN_WIDTH: u16 = 10;

/// Columns between the name and its detail — the same three `/help` uses, so
/// the popup and the listing read as one table.
const DETAIL_GAP: u16 = 3;

/// Below this the detail column is dropped rather than shown: a few characters
/// and an ellipsis are noise, not a gloss.
const MIN_DETAIL_WIDTH: u16 = 12;

/// One row of a [`Menu`]: the text it shows (`display`), an optional one-line
/// gloss (`detail`) shown in a dimmer second column, and the text it splices
/// into the buffer when chosen (`replacement`).
///
/// `display` and `replacement` differ wherever a host shows a name but splices
/// something else — a path candidate whose replacement is source-quoted, say.
/// `detail` is `None` where a name is its own whole story, as a filename or a
/// binding is; the column then does not exist.  A host with its own richer
/// notion of a candidate converts into this shape at the call to
/// [`Menu::open`]; the widget wants nothing more than these three fields.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Candidate {
    pub display: String,
    pub detail: Option<String>,
    pub replacement: String,
}

/// An open completion menu: ranked candidates and the buffer span they replace.
pub struct Menu {
    /// Non-empty: [`Menu::open`] refuses an empty list, so a selection always
    /// names a candidate.
    candidates: Vec<Candidate>,
    /// Widest `display` and widest `detail`, in columns — derived from
    /// `candidates`, which never changes after [`Menu::open`], so measured once
    /// there rather than rescanned every frame.
    name_w: u16,
    detail_w: u16,
    selected: usize,
    /// Byte offset into the trigger row where the chosen replacement starts.
    replace_from: usize,
    /// The editor row the menu was opened on; accept aborts if the cursor has
    /// since left it.
    row: usize,
    /// Screen column the popup drops down under: the prompt prefix width plus
    /// the token's start column, so the list aligns under what is being typed.
    anchor_col: u16,
    item_style: Style,
    border_style: Style,
    detail_style: Style,
}

impl Menu {
    /// Open a menu over `candidates`, selecting the first.  An empty list has
    /// nothing to offer and yields no menu.
    pub fn open(
        candidates: Vec<Candidate>,
        replace_from: usize,
        row: usize,
        anchor_col: u16,
    ) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        let name_w = candidates
            .iter()
            .map(|c| u16::try_from(c.display.chars().count()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);
        let detail_w = candidates
            .iter()
            .filter_map(|c| c.detail.as_deref())
            .map(|d| u16::try_from(d.chars().count()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);
        Some(Self {
            candidates,
            name_w,
            detail_w,
            selected: 0,
            replace_from,
            row,
            anchor_col,
            item_style: Style::default(),
            border_style: Style::default(),
            detail_style: Style::default().add_modifier(Modifier::DIM),
        })
    }

    /// Paint the candidate rows and the border in the host's own palette.  The
    /// selection is reversed on top of `item`, so it reads against any colour.
    #[must_use]
    pub fn style(mut self, item: Style, border: Style) -> Self {
        self.item_style = item;
        self.border_style = border;
        self
    }

    /// Paint the detail column, where candidates carry one.  Prefer a colour
    /// to the default `DIM`: the selection reverses this style too, and a
    /// dimmed reverse is unreliable across terminals.
    #[must_use]
    pub fn detail_style(mut self, detail: Style) -> Self {
        self.detail_style = detail;
        self
    }

    /// The editor row the menu was opened on.
    pub fn row(&self) -> usize {
        self.row
    }

    /// The byte offset in that row where the chosen replacement starts.
    pub fn replace_from(&self) -> usize {
        self.replace_from
    }

    /// The popup's wanted height, borders included: the rows it would show,
    /// capped at [`MENU_MAX_ROWS`], plus two.  A render clamps this to the
    /// height it is actually given.
    pub fn height(&self) -> u16 {
        let rows = u16::try_from(self.candidates.len())
            .unwrap_or(u16::MAX)
            .min(MENU_MAX_ROWS);
        rows + 2
    }

    /// Advance the selection to the next candidate, wrapping.
    pub fn select_next(&mut self) {
        self.selected = (self.selected + 1) % self.candidates.len();
    }

    /// Retreat the selection to the previous candidate, wrapping.
    pub fn select_prev(&mut self) {
        let n = self.candidates.len();
        self.selected = (self.selected + n - 1) % n;
    }

    /// Apply the selected candidate: replace the token from `replace_from` to
    /// the current cursor (which has not moved while the menu owned the keys)
    /// with the chosen replacement.  Reports whether the splice happened — it
    /// does not if the cursor has left the trigger row, or if the recorded span
    /// no longer fits that row.
    pub fn accept(&self, prompt: &mut PromptEditor) -> bool {
        let row = prompt.row();
        if row != self.row {
            return false;
        }
        let Some(line) = prompt.line(row) else {
            return false;
        };
        let end = char_to_byte(&line, prompt.col());
        prompt.replace_row_bytes(
            row,
            self.replace_from,
            end,
            &self.candidates[self.selected].replacement,
        )
    }

    /// Draw the menu as a bordered popup at the top of `area`, dropping down,
    /// its left edge anchored under the token being completed (clamped to stay
    /// within the area).  The selected row is reversed; the list scrolls within
    /// [`MENU_MAX_ROWS`] so a long candidate set stays navigable.
    pub fn render(&self, frame: &mut ratatui::Frame, area: Rect) {
        if area.height < 3 {
            return;
        }
        // Width fits the widest name, plus a detail column if one survives the
        // room left after names, gap, and borders, plus borders.  Height is the
        // visible rows plus borders.  Both clamp to the area — the floor
        // first, so an area narrower than the floor yields the area rather
        // than an empty range.
        //
        // With no details at all `detail_col` is `None` and the popup is exactly
        // as wide as its names — the detail column is the first thing a narrow
        // area gives up.
        let room = area
            .width
            .saturating_sub(self.name_w.saturating_add(DETAIL_GAP).saturating_add(2))
            .min(self.detail_w);
        let detail_col = (room >= MIN_DETAIL_WIDTH).then_some(room);
        let pop_w = self
            .name_w
            .saturating_add(2)
            .saturating_add(detail_col.map_or(0, |w| w.saturating_add(DETAIL_GAP)))
            .clamp(MIN_WIDTH.min(area.width), area.width);
        let pop_h = self.height().min(area.height);
        let rect = Rect {
            x: area.x + self.anchor_col.min(area.width.saturating_sub(pop_w)),
            y: area.y,
            width: pop_w,
            height: pop_h,
        };

        // Scroll the window so the selected row stays visible.
        let window = usize::from(pop_h - 2);
        let start = self.selected.saturating_sub(window.saturating_sub(1));
        let lines: Vec<Line> = self
            .candidates
            .iter()
            .enumerate()
            .skip(start)
            .take(window)
            .map(|(i, c)| {
                let (mut item, mut detail) = (self.item_style, self.detail_style);
                if i == self.selected {
                    item = item.add_modifier(Modifier::REVERSED);
                    detail = detail.add_modifier(Modifier::REVERSED);
                }
                match (detail_col, c.detail.as_deref()) {
                    (Some(w), Some(d)) => {
                        // Both columns are padded to a fixed width, so the
                        // reversed selection is a clean rectangle rather than
                        // a ragged one.
                        let pad = usize::from(self.name_w + DETAIL_GAP);
                        let w = usize::from(w);
                        Line::from(vec![
                            Span::styled(format!("{:pad$}", c.display), item),
                            Span::styled(format!("{:w$}", clip(d, w)), detail),
                        ])
                    }
                    _ => Line::from(Span::styled(c.display.clone(), item)),
                }
            })
            .collect();
        let block = Block::default()
            .borders(Borders::ALL)
            .border_type(BorderType::Rounded)
            .border_style(self.border_style);
        frame.render_widget(Clear, rect);
        frame.render_widget(Paragraph::new(lines).block(block), rect);
    }
}

/// `s` cut to `max` columns, the last spent on an `…` when anything is dropped.
fn clip(s: &str, max: usize) -> String {
    if s.chars().count() <= max {
        return s.to_string();
    }
    s.chars().take(max.saturating_sub(1)).chain(['…']).collect()
}
