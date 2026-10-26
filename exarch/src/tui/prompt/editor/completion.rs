//! The slash-command popup; the registry supplies ranked candidates.

use ratatui::layout::Rect;
use ratatui::style::{Modifier, Style};
use ratatui::text::{Line, Span};
use ratatui::widgets::{Block, BorderType, Borders, Clear, Paragraph};

use super::PromptEditor;
use ral_core::text::char_to_byte;
use unicode_width::UnicodeWidthStr;

/// The most candidate rows a [`Menu`] shows at once, scrolling within them.  A
/// host that reserves room for a menu before opening one reserves this many
/// rows plus two, the borders.
const MENU_MAX_ROWS: u16 = 6;

/// The popup is never narrower than this, so a one-character candidate still
/// reads as a list rather than a sliver — unless the area itself is narrower.
const MIN_WIDTH: u16 = 10;

/// Columns between the name and its detail — the same three `/help` uses, so
/// the popup and the listing read as one table.
const DETAIL_GAP: u16 = 3;

/// Below this the detail column is dropped rather than shown: a few characters
/// and an ellipsis are noise, not a gloss.
const MIN_DETAIL_WIDTH: u16 = 12;

/// The display may carry an argument hint; only `replacement` enters the draft.
#[derive(Clone, Debug, PartialEq, Eq)]
pub(in crate::tui) struct Candidate {
    pub(in crate::tui) display: String,
    pub(in crate::tui) detail: Option<String>,
    pub(in crate::tui) replacement: String,
}

/// An open completion menu: ranked candidates and the buffer span they replace.
pub(in crate::tui) struct Menu {
    /// Non-empty: [`Menu::open`] refuses an empty list, so a selection always
    /// names a candidate.
    candidates: Vec<Candidate>,
    /// Widest `display` and widest `detail`, in columns — derived from
    /// `candidates`, which never changes after [`Menu::open`], so measured once
    /// there rather than rescanned every frame.
    name_w: u16,
    detail_w: u16,
    selected: usize,
    item_style: Style,
    border_style: Style,
    detail_style: Style,
}

impl Menu {
    /// Open a menu over `candidates`, selecting the first.  An empty list has
    /// nothing to offer and yields no menu.
    pub(in crate::tui) fn open(candidates: Vec<Candidate>) -> Option<Self> {
        if candidates.is_empty() {
            return None;
        }
        let name_w = candidates
            .iter()
            .map(|c| u16::try_from(c.display.width()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);
        let detail_w = candidates
            .iter()
            .filter_map(|c| c.detail.as_deref())
            .map(|d| u16::try_from(d.width()).unwrap_or(u16::MAX))
            .max()
            .unwrap_or(0);
        Some(Self {
            candidates,
            name_w,
            detail_w,
            selected: 0,
            item_style: Style::default(),
            border_style: Style::default(),
            detail_style: Style::default().add_modifier(Modifier::DIM),
        })
    }

    /// Paint the candidate rows and the border in the host's own palette.  The
    /// selection is reversed on top of `item`, so it reads against any colour.
    #[must_use]
    pub(in crate::tui) fn style(mut self, item: Style, border: Style) -> Self {
        self.item_style = item;
        self.border_style = border;
        self
    }

    /// Paint the detail column, where candidates carry one.  Prefer a colour
    /// to the default `DIM`: the selection reverses this style too, and a
    /// dimmed reverse is unreliable across terminals.
    #[must_use]
    pub(in crate::tui) fn detail_style(mut self, detail: Style) -> Self {
        self.detail_style = detail;
        self
    }

    /// The popup's wanted height, borders included: the rows it would show,
    /// capped at [`MENU_MAX_ROWS`], plus two.  A render clamps this to the
    /// height it is actually given.
    pub(in crate::tui) fn height(&self) -> u16 {
        let rows = u16::try_from(self.candidates.len())
            .unwrap_or(u16::MAX)
            .min(MENU_MAX_ROWS);
        rows + 2
    }

    /// Advance the selection to the next candidate, wrapping.
    pub(in crate::tui) fn select_next(&mut self) {
        self.selected = (self.selected + 1) % self.candidates.len();
    }

    /// Retreat the selection to the previous candidate, wrapping.
    pub(in crate::tui) fn select_prev(&mut self) {
        let n = self.candidates.len();
        self.selected = (self.selected + n - 1) % n;
    }

    /// Replace the slash command up to the cursor on the first row.
    /// An unchanged draft leaves the key available to the host.
    pub(in crate::tui::prompt) fn accept(&self, prompt: &mut PromptEditor) -> bool {
        let row = prompt.row();
        if row != 0 {
            return false;
        }
        let Some(line) = prompt.line(row) else {
            return false;
        };
        let end_col = prompt.col() + usize::from(prompt.state.mode == edtui::EditorMode::Normal);
        let end = char_to_byte(&line, end_col);
        let replacement = &self.candidates[self.selected].replacement;
        prompt.replace_row_bytes(row, 0, end, replacement)
    }

    /// Draw the menu at the top left of `area`.
    /// The selected row is reversed; the list scrolls within
    /// [`MENU_MAX_ROWS`] so a long candidate set stays navigable.
    pub(in crate::tui) fn render(&self, frame: &mut ratatui::Frame, area: Rect) {
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
            x: area.x,
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
                match detail_col {
                    Some(w) => Line::from(vec![
                        Span::styled(padded(&c.display, usize::from(self.name_w)), item),
                        Span::styled(" ".repeat(usize::from(DETAIL_GAP)), item),
                        Span::styled(
                            padded(c.detail.as_deref().unwrap_or(""), usize::from(w)),
                            detail,
                        ),
                    ]),
                    None => Line::from(Span::styled(
                        clip(&c.display, usize::from(rect.width.saturating_sub(2))),
                        item,
                    )),
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
    if s.width() <= max {
        return s.to_owned();
    }
    let ellipsis = "…";
    if max < ellipsis.width() {
        return String::new();
    }
    let budget = max - ellipsis.width();
    let mut used = 0;
    let mut out = String::new();
    for g in Span::raw(s).styled_graphemes(Style::default()) {
        let width = g.symbol.width();
        if used + width > budget {
            break;
        }
        out.push_str(g.symbol);
        used += width;
    }
    out.push_str(ellipsis);
    out
}

fn padded(s: &str, width: usize) -> String {
    let mut out = clip(s, width);
    out.push_str(&" ".repeat(width.saturating_sub(out.width())));
    out
}
