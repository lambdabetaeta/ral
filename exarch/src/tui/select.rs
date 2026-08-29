//! Drag-selection geometry, in text-area cell columns: `render::paint_selection`
//! reverse-videos a live drag through [`highlight_range`], and
//! `Viewport::selection_text` copies the released text through [`plain_slice`].
//!
//! Both take screen cells and both convert once, through [`content_range`]: a
//! [`Row`]'s margin is a field rather than a leading span, so subtracting
//! [`RAIL_W`] is exact on every row — head, continuation and blank alike — and
//! no selection can reach the chrome whatever the row is made of.  Both stop at
//! a short line's end, which is how the callers spell `u16::MAX` as "to end of
//! row".
use super::palette::RAIL_W;
use super::row::Row;
use ratatui::{style::Modifier, text::Span};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Screen cell columns to ordered content columns; a column inside the margin
/// clamps to the content's first.
#[allow(
    clippy::cast_possible_truncation,
    reason = "small compile-time constant fits u16"
)]
fn content_range(start_cell: u16, end_cell: u16) -> (u16, u16) {
    let lo = start_cell.saturating_sub(RAIL_W as u16);
    let hi = end_cell.saturating_sub(RAIL_W as u16);
    (lo.min(hi), lo.max(hi))
}

/// Copyable text of `row` between two screen cell columns, in either order.
/// A zero-width combining mark shares its base character's cell, so it rides
/// along with whatever `in_range` that base landed: it never opens or closes
/// the selection on its own.
pub(super) fn plain_slice(row: &Row, start_cell: u16, end_cell: u16) -> String {
    let text = row.plain();
    if text.is_empty() {
        return String::new();
    }
    let (lo, hi) = content_range(start_cell, end_cell);
    let mut cell: u16 = 0;
    let mut byte_lo = text.len();
    let mut byte_hi = text.len();
    let mut in_range = false;
    for (i, ch) in text.char_indices() {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "char display width is 0..=2"
        )]
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if w == 0 {
            if in_range {
                byte_hi = i + ch.len_utf8();
            }
            continue;
        }
        if cell >= hi {
            break;
        }
        in_range = cell >= lo;
        if in_range {
            if byte_lo == text.len() {
                byte_lo = i;
            }
            byte_hi = i + ch.len_utf8();
        }
        cell += w;
    }
    text[byte_lo..byte_hi].to_string()
}

/// Reverse-video a screen cell-column range of `row`'s content, splitting any
/// span that straddles an edge.  The margin is untouched by construction.
pub(super) fn highlight_range(row: &mut Row, start_cell: u16, end_cell: u16) {
    let (lo, hi) = content_range(start_cell, end_cell);
    if lo >= hi {
        return;
    }
    let content = row.content_mut();
    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut cell: u16 = 0;
    for s in &content.spans {
        let text = s.content.as_ref();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "cell width of a span on a rendered row fits u16"
        )]
        let w = UnicodeWidthStr::width(text) as u16;
        let span_end = cell + w;
        if span_end <= lo || cell >= hi {
            new_spans.push(s.clone());
        } else if cell >= lo && span_end <= hi {
            new_spans.push(Span::styled(
                text.to_owned(),
                s.style.add_modifier(Modifier::REVERSED),
            ));
        } else {
            new_spans.extend(split_span(s, cell, lo, hi));
        }
        cell = span_end;
    }
    content.spans = new_spans;
}

/// Split one span that straddles a selection edge into its in- and out-of-
/// selection runs, `cell` being the span's own opening column.
fn split_span(s: &Span<'static>, cell: u16, lo: u16, hi: u16) -> Vec<Span<'static>> {
    let mut out = Vec::new();
    let mut char_cell = cell;
    let mut buf = String::new();
    let mut in_sel = cell >= lo;
    let flush = |buf: &mut String, in_sel: bool, out: &mut Vec<Span<'static>>| {
        if !buf.is_empty() {
            let style = if in_sel {
                s.style.add_modifier(Modifier::REVERSED)
            } else {
                s.style
            };
            out.push(Span::styled(std::mem::take(buf), style));
        }
    };
    for ch in s.content.chars() {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "char display width is 0..=2"
        )]
        let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        let ch_end = char_cell + ch_w;
        let ch_in_sel = char_cell < hi && ch_end > lo;
        if ch_in_sel != in_sel {
            flush(&mut buf, in_sel, &mut out);
            in_sel = ch_in_sel;
        }
        buf.push(ch);
        char_cell = ch_end;
    }
    flush(&mut buf, in_sel, &mut out);
    out
}
