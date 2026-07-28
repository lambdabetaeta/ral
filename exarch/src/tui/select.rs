//! Drag-selection geometry, in text-area cell columns: `render::paint_selection`
//! reverse-videos a live drag through [`highlight_range`], and
//! `Viewport::selection_text` copies the released text through [`plain_slice`].
//! Both subtract the leading [`RAIL_W`] gutter, so no selection reaches into the
//! rail glyph, and both stop at a short line's end — which is how the callers
//! spell `u16::MAX` as "to end of row".
use super::line::{plain, rail_skip};
use super::palette::RAIL_W;
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Screen cell columns to ordered content columns; a column inside the rail clamps to 0.
#[allow(
    clippy::cast_possible_truncation,
    reason = "small compile-time constant fits u16"
)]
fn content_range(start_cell: u16, end_cell: u16) -> (u16, u16) {
    let lo = start_cell.saturating_sub(RAIL_W as u16);
    let hi = end_cell.saturating_sub(RAIL_W as u16);
    (lo.min(hi), lo.max(hi))
}

/// Plain text of `line` between two cell columns, in either order.
pub(super) fn plain_slice(line: &Line<'_>, start_cell: u16, end_cell: u16) -> String {
    let text = plain(line);
    if text.is_empty() {
        return String::new();
    }
    let (lo, hi) = content_range(start_cell, end_cell);
    let mut cell: u16 = 0;
    let mut byte_lo = text.len();
    let mut byte_hi = text.len();
    for (i, ch) in text.char_indices() {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "char display width is 0..=2"
        )]
        let w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
        if cell >= lo && byte_lo == text.len() {
            byte_lo = i;
        }
        if cell >= hi {
            byte_hi = i;
            break;
        }
        cell += w;
    }
    text[byte_lo..byte_hi].to_string()
}

/// Reverse-video a cell-column range, splitting any span that straddles an edge.
pub(super) fn highlight_range(line: &mut Line<'static>, start_cell: u16, end_cell: u16) {
    let skip = rail_skip(line);
    let (lo, hi) = content_range(start_cell, end_cell);
    if lo >= hi || skip >= line.spans.len() {
        return;
    }
    let mut new_spans: Vec<Span<'static>> = Vec::new();
    let mut cell: u16 = 0;
    for s in line.spans.iter().take(skip) {
        new_spans.push(s.clone());
    }
    for s in line.spans.iter().skip(skip) {
        let content = s.content.as_ref();
        #[allow(
            clippy::cast_possible_truncation,
            reason = "cell width of a span on a rendered line fits u16"
        )]
        let w = UnicodeWidthStr::width(content) as u16;
        let span_end = cell + w;
        if span_end <= lo || cell >= hi {
            new_spans.push(s.clone());
        } else if cell >= lo && span_end <= hi {
            new_spans.push(Span::styled(
                content.to_owned(),
                s.style.add_modifier(Modifier::REVERSED),
            ));
        } else {
            let mut char_cell = cell;
            let mut buf = String::new();
            let mut in_sel = cell >= lo;
            for ch in content.chars() {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "char display width is 0..=2"
                )]
                let ch_w = UnicodeWidthChar::width(ch).unwrap_or(0) as u16;
                let ch_end = char_cell + ch_w;
                let ch_in_sel = char_cell < hi && ch_end > lo;
                if ch_in_sel != in_sel {
                    if !buf.is_empty() {
                        let style = if in_sel {
                            s.style.add_modifier(Modifier::REVERSED)
                        } else {
                            s.style
                        };
                        new_spans.push(Span::styled(std::mem::take(&mut buf), style));
                    }
                    in_sel = ch_in_sel;
                }
                buf.push(ch);
                char_cell = ch_end;
            }
            if !buf.is_empty() {
                let style = if in_sel {
                    s.style.add_modifier(Modifier::REVERSED)
                } else {
                    s.style
                };
                new_spans.push(Span::styled(buf, style));
            }
        }
        cell = span_end;
    }
    line.spans = new_spans;
}
