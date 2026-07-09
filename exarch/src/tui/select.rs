//! Drag-selection helpers: column-aware text extraction and
//! character-range highlighting.
use super::line::{RAIL_GLYPHS, RAIL_W, plain};
use ratatui::{
    style::Modifier,
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// Extract plain text between two cell-column positions within a line.
/// `start_cell` and `end_cell` are absolute cell columns within the text
/// area (0 = left edge).  The rail glyph occupies the first [`RAIL_W`]
/// columns; columns landing inside the rail clamp to the start of content.
pub(super) fn plain_slice(line: &Line<'_>, start_cell: u16, end_cell: u16) -> String {
    let text = plain(line);
    if text.is_empty() {
        return String::new();
    }
    let lo = start_cell.saturating_sub(RAIL_W as u16);
    let hi = end_cell.saturating_sub(RAIL_W as u16);
    let (lo, hi) = (lo.min(hi), lo.max(hi));
    let mut cell: u16 = 0;
    let mut byte_lo = text.len();
    let mut byte_hi = text.len();
    for (i, ch) in text.char_indices() {
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
    let skip = usize::from(
        line.spans
            .first()
            .is_some_and(|s| RAIL_GLYPHS.contains(&s.content.as_ref())),
    );
    let lo = start_cell.saturating_sub(RAIL_W as u16);
    let hi = end_cell.saturating_sub(RAIL_W as u16);
    let (lo, hi) = (lo.min(hi), lo.max(hi));
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
