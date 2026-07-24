//! Drag-selection helpers, shared by the two moments a selection is read:
//! [`highlight_range`] reverse-videos it live as the mouse drags
//! ([`super::render::paint_selection`]); [`plain_slice`] extracts the plain
//! text once released, for the clipboard copy ([`super::viewport::Viewport::
//! selection_text`]).  Both work in cell columns relative to the text area,
//! with the leading [`RAIL_W`] rail gutter excluded from either end, so a
//! selection never reaches into the glyph.
use super::line::{plain, rail_skip};
use super::palette::RAIL_W;
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Normalize a screen cell-column range to content columns: subtract the
/// [`RAIL_W`] rail gutter from each end (columns landing inside the rail
/// clamp to the start of content) and order them low..high.
#[allow(
    clippy::cast_possible_truncation,
    reason = "small compile-time constant fits u16"
)]
fn content_range(start_cell: u16, end_cell: u16) -> (u16, u16) {
    let lo = start_cell.saturating_sub(RAIL_W as u16);
    let hi = end_cell.saturating_sub(RAIL_W as u16);
    (lo.min(hi), lo.max(hi))
}

/// Extract plain text between two cell-column positions within a line.
/// `start_cell` and `end_cell` are absolute cell columns within the text
/// area (0 = left edge).  The rail glyph occupies the first [`RAIL_W`]
/// columns; columns landing inside the rail clamp to the start of content.
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

/// Apply [`Modifier::REVERSED`] to a cell-column range within a [`Line`],
/// splitting any span that straddles the boundary so the highlight stays
/// granular.  The rail glyph (first [`RAIL_W`] columns) is excluded.
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
