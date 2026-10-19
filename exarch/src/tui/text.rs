//! Span-level text primitives: width, wrapping, truncation, washing.

use ratatui::{
    style::{Color, Style},
    text::{Line, Span},
};
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// True when `l` carries no glyphs, so it reads as a vertical gap — the one
/// "this row is a separator" test `md`, `scrollback`, `block` and `group` share.
pub(super) fn is_blank(l: &Line<'_>) -> bool {
    l.spans.iter().all(|s| s.content.trim().is_empty())
}

/// A line's spans joined.  Copy goes through [`super::row::Row::plain`] — a
/// transcript row's margin is a `Row`'s own field, never a span here — so
/// nothing in the app needs this, and it reads bodies for the tests alone.
#[cfg(test)]
pub(super) fn text(line: &Line<'_>) -> String {
    line.spans.iter().map(|s| s.content.as_ref()).collect()
}

/// Repaint `row` on background `bg`, keeping every foreground and modifier —
/// the one place a background stratum is laid down.  `fill_to` pads it
/// edge-to-edge as a panel; `None` hugs the spans, for a swatch.
pub(super) fn wash(row: Line<'static>, bg: Color, fill_to: Option<usize>) -> Line<'static> {
    let used: usize = row
        .spans
        .iter()
        .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
        .sum();
    let mut spans: Vec<Span<'static>> = row
        .spans
        .into_iter()
        .map(|s| Span::styled(s.content, s.style.bg(bg)))
        .collect();
    if let Some(w) = fill_to
        && w > used
    {
        spans.push(Span::styled(" ".repeat(w - used), Style::default().bg(bg)));
    }
    Line::from(spans)
}

/// Total display width of a span run, unicode-aware.
pub(super) fn span_run_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(ratatui::prelude::Span::width).sum()
}

/// Truncate a styled span run to `max_w` columns, appending an `…` when
/// anything is dropped.
pub(super) fn truncate_spans(spans: &[Span<'static>], max_w: usize) -> Vec<Span<'static>> {
    if span_run_width(spans) <= max_w {
        return spans.to_vec();
    }
    let budget = max_w.saturating_sub(1); // reserve the last column for `…`
    let mut out: Vec<Span<'static>> = Vec::new();
    let mut used = 0;
    for s in spans {
        if used >= budget {
            break;
        }
        let mut kept = String::new();
        for ch in s.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if used + cw > budget {
                break;
            }
            kept.push(ch);
            used += cw;
        }
        if !kept.is_empty() {
            out.push(Span::styled(kept, s.style));
        }
    }
    out.push(Span::raw("…"));
    out
}

/// The run of `spans` grown evenly around `spans[at]` until it fills `width`,
/// each cut side marked by a `…` in `mark`, so the span at `at` stays in view.
/// `at` must index `spans`; it panics otherwise.
pub(super) fn window_around(
    spans: &[Span<'static>],
    at: usize,
    width: usize,
    mark: Style,
) -> Vec<Span<'static>> {
    if span_run_width(spans) <= width {
        return spans.to_vec();
    }
    let budget = width.saturating_sub(4); // `… ` and ` …`
    let (mut lo, mut hi, mut used) = (at, at + 1, spans[at].width());
    loop {
        let left = lo > 0 && used + spans[lo - 1].width() <= budget;
        if left {
            lo -= 1;
            used += spans[lo].width();
        }
        let right = hi < spans.len() && used + spans[hi].width() <= budget;
        if right {
            used += spans[hi].width();
            hi += 1;
        }
        if !left && !right {
            break;
        }
    }
    (lo > 0)
        .then(|| Span::styled("… ", mark))
        .into_iter()
        .chain(spans[lo..hi].iter().cloned())
        .chain((hi < spans.len()).then(|| Span::styled(" …", mark)))
        .collect()
}

/// Fold `(text, style)` fragments into `Line`s, splitting on `\n` — shared by
/// [`super::line::render_text`] and `into_lines` in `highlight.rs`, whose one difference is
/// `keep_trailing_blank`: set, a stream ending mid-line still closes with the
/// line in progress; cleared, it mirrors [`str::lines`] and drops it.
pub(super) fn fold_styled_lines(
    fragments: impl Iterator<Item = (String, Style)>,
    keep_trailing_blank: bool,
) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    for (text, style) in fragments {
        let mut parts = text.split('\n');
        if let Some(first) = parts.next()
            && !first.is_empty()
        {
            cur.push(Span::styled(first.to_string(), style));
        }
        for part in parts {
            lines.push(Line::from(std::mem::take(&mut cur)));
            if !part.is_empty() {
                cur.push(Span::styled(part.to_string(), style));
            }
        }
    }
    if keep_trailing_blank || !cur.is_empty() || lines.is_empty() {
        lines.push(Line::from(cur));
    }
    lines
}

/// `body` folded into the width left of `head`, with `head` on the first row
/// and a blank of its width under every other — the one hanging fold in the
/// kit, shared by an act's payload, a diff row's text and a labelled field.
/// Style-preserving throughout, so a body may carry any ink its builder gave
/// it, syntax highlighting included.
pub(super) fn hang(
    head: &[Span<'static>],
    body: Vec<Span<'static>>,
    width: usize,
) -> Vec<Line<'static>> {
    let indent = span_run_width(head);
    let body_w = width.saturating_sub(indent).max(8);
    wrap_line(&Line::from(body), body_w)
        .into_iter()
        .enumerate()
        .map(|(i, row)| {
            let mut spans = if i == 0 {
                head.to_vec()
            } else {
                vec![Span::raw(" ".repeat(indent))]
            };
            spans.extend(row.spans);
            Line::from(spans)
        })
        .collect()
}

/// Fold one logical line into visual rows no wider than `width`, word-aware and
/// style-preserving.  A line that already fits comes straight back.
///
/// Continuations re-indent to the line's own leading whitespace, so a wrapped
/// prompt echo or code row folds under its content rather than sliding to
/// column zero.  A word wider than the column is hard-broken char by char.
pub(super) fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width {
        return vec![line.clone()];
    }
    // The hang column: the leading whitespace-only spans.  It goes in the head,
    // not the body, or row 0 would lose it — leading body whitespace is dropped
    // as a row-leading gap.
    let spans = line.spans.as_slice();
    let mut head_len = 0;
    while spans
        .get(head_len)
        .is_some_and(|s| !s.content.is_empty() && s.content.chars().all(|c| c == ' '))
    {
        head_len += 1;
    }
    let head = &spans[..head_len];
    let body = &spans[head_len..];
    let indent: usize = head
        .iter()
        .flat_map(|s| s.content.chars())
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();

    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = head.to_vec();
    let mut col = indent;
    // Whether a word has landed on this row's body: a gap before the first is
    // leading whitespace and is dropped; after it, gaps separate words.
    let mut started = false;
    // Whitespace held between words, as style-runs so a styled gap survives.
    let mut gap: Vec<(String, Style)> = Vec::new();
    let mut gap_w = 0;

    for (word, ww) in words(body) {
        // A whitespace run is held pending, never placed eagerly.
        if word.iter().all(|(s, _)| s.chars().all(char::is_whitespace)) {
            gap = word;
            gap_w = ww;
            continue;
        }
        // Break before a word that overflows, once this row carries one.
        if started && col + gap_w + ww > width {
            rows.push(Line::from(std::mem::take(&mut row)));
            row = seed(indent);
            col = indent;
            started = false;
            gap.clear();
            gap_w = 0;
        }
        if started {
            for (s, style) in std::mem::take(&mut gap) {
                row.push(Span::styled(s, style));
            }
            col += gap_w;
        } else {
            gap.clear();
        }
        gap_w = 0;
        for (s, style) in word {
            for ch in s.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if started && col + cw > width {
                    rows.push(Line::from(std::mem::take(&mut row)));
                    row = seed(indent);
                    col = indent;
                }
                push_char(&mut row, ch, style);
                col += cw;
                started = true;
            }
        }
    }
    if started || rows.is_empty() {
        rows.push(Line::from(row));
    }
    rows
}

/// A continuation row seeded with `indent`, or empty when the line wraps flush.
fn seed(indent: usize) -> Vec<Span<'static>> {
    if indent == 0 {
        Vec::new()
    } else {
        vec![Span::raw(" ".repeat(indent))]
    }
}

/// Append `ch`, extending the trailing span when styles match so a word does
/// not fragment into one span per character.
fn push_char(row: &mut Vec<Span<'static>>, ch: char, style: Style) {
    match row.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push(ch),
        _ => row.push(Span::styled(ch.to_string(), style)),
    }
}

/// Tokenise a span stream into maximal whitespace / non-whitespace runs with
/// their widths.  A run carries its style fragments, so a word crossing a span
/// seam keeps each fragment's [`Style`].  Span-aware twin of `md`'s split.
fn words(spans: &[Span<'static>]) -> Vec<(Vec<(String, Style)>, usize)> {
    let mut out: Vec<(Vec<(String, Style)>, usize)> = Vec::new();
    let mut run: Vec<(String, Style)> = Vec::new();
    let mut run_w = 0;
    // `None` until the first char fixes the run's kind.
    let mut ws: Option<bool> = None;
    let mut flush = |run: &mut Vec<(String, Style)>, run_w: &mut usize, ws: &mut Option<bool>| {
        if !run.is_empty() {
            out.push((std::mem::take(run), std::mem::replace(run_w, 0)));
        }
        *ws = None;
    };
    for span in spans {
        for ch in span.content.chars() {
            let is_ws = ch.is_whitespace();
            if ws.is_some_and(|prev| prev != is_ws) {
                flush(&mut run, &mut run_w, &mut ws);
            }
            ws = Some(is_ws);
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            match run.last_mut() {
                Some((s, st)) if *st == span.style => s.push(ch),
                _ => run.push((ch.to_string(), span.style)),
            }
            run_w += cw;
        }
    }
    flush(&mut run, &mut run_w, &mut ws);
    out
}

#[cfg(test)]
mod tests;
