//! A patch rendered as a block: a header with its size and grain, hunks
//! numbered against one shared gutter, and elision rows where content is cut.

use super::block::Detail;
use super::line::{grain_run, hang, size_bar};
use super::palette::{Col, LIME_HOT, RED_HOT, SLATE};
use crate::bus::card::{Hunk, Row as DiffRow, Seg};
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Diff rows a `Summary` shows before the elision: enough to read the change,
/// not enough to bury the transcript under it.
pub(super) const DIFF_PEEK_ROWS: usize = 20;

/// A [`crate::bus::card::Mark::Diff`]'s body at `width`, graded by disclosure: `Tally` the header
/// alone, `Summary` its first [`DIFF_PEEK_ROWS`] rows, `Full` every hunk.  No
/// leading blank — the unframed card renderer owns the one blank that opens the
/// block.  The densest object on screen: size in the header bar, grain in the
/// addition ratio, value in the rail's lightness, shape in its `▎` glyph.
pub(super) fn diff_body(
    path: &str,
    hunks: &[Hunk],
    width: usize,
    at: Detail,
) -> Vec<Line<'static>> {
    match at {
        Detail::Tally => vec![patch_header(path, hunks)],
        Detail::Summary => diff_capped(path, hunks, width, Some(DIFF_PEEK_ROWS)),
        Detail::Full => diff_capped(path, hunks, width, None),
    }
}

/// Rows across every hunk satisfying `pred` — the tallies the header's grain
/// run reads.
#[allow(clippy::cast_possible_truncation, reason = "diff row count")]
fn count_rows(hunks: &[Hunk], pred: impl Fn(&DiffRow) -> bool) -> u32 {
    hunks
        .iter()
        .flat_map(|h| h.rows.iter())
        .filter(|r| pred(r))
        .count() as u32
}

/// The `diff  <path>` header row, with its [`size_bar`] and addition-ratio
/// [`grain_run`].  Shared by every rung, so the headers never drift.
fn patch_header(path: &str, hunks: &[Hunk]) -> Line<'static> {
    Line::from(vec![
        Span::styled("diff", Style::default().fg(SLATE)),
        Span::raw("  "),
        Span::styled(path.to_string(), Style::default().fg(Color::White)),
        Span::raw("  "),
        size_bar(crate::bus::card::hunk_magnitude(hunks)),
        Span::raw("  "),
        grain_run(
            count_rows(hunks, |r| matches!(r, DiffRow::Add(_))),
            count_rows(hunks, |r| matches!(r, DiffRow::Del(_))),
        ),
    ])
}

/// A diff block's columns: the line-number gutter, measured once for the whole
/// block so every row's text starts in the same one, and the block's own width.
/// What is left for a row's text is [`hang`]'s to work out, from the head it is
/// handed.
#[derive(Clone, Copy)]
struct DiffCols {
    gutter: Col,
    width: usize,
}

/// The header, then the first `cap` diff rows (all when `None`), the hunks
/// elision-separated and numbered against one gutter sized for the whole
/// block, so every row's text starts in the same column.  A diff cut short
/// ends in the same elision a break between hunks wears.
fn diff_capped(path: &str, hunks: &[Hunk], width: usize, cap: Option<usize>) -> Vec<Line<'static>> {
    let mut ls: Vec<Line<'static>> = vec![patch_header(path, hunks)];
    let total: usize = hunks.iter().map(|h| h.rows.len()).sum();
    let mut left = cap.unwrap_or(total).min(total);
    let cut = left < total;
    let widest = hunks.iter().map(hunk_max_lineno).max().unwrap_or(0);
    let cols = DiffCols {
        // Three columns even for a two-digit file, so a short patch's gutter is
        // the one a long patch wears.
        gutter: Col::wide(3).seeing(&widest.to_string()),
        width,
    };
    for (i, h) in hunks.iter().enumerate() {
        if left == 0 {
            break;
        }
        if i > 0 {
            ls.push(elision_row(cols.gutter));
        }
        left -= push_hunk(&mut ls, h, cols, left);
    }
    if cut {
        ls.push(elision_row(cols.gutter));
    }
    ls
}

/// The "there is more below" row: a bare `⋮` right-aligned in `gutter`, drawn
/// by [`diff_capped`] both between hunks and at its cap, so a break in the
/// middle of a diff and a diff cut short read alike.
fn elision_row(gutter: Col) -> Line<'static> {
    Line::from(Span::styled(
        format!("{} ", gutter.right("⋮")),
        Style::default().fg(SLATE),
    ))
}

/// The largest number [`push_hunk`] will stamp on `h`, which sizes the gutter.
/// Walks the same two counters, so the two must move together.
fn hunk_max_lineno(h: &Hunk) -> u32 {
    let (mut old, mut new) = (h.start, h.start);
    let mut max = h.start;
    for row in &h.rows {
        match row {
            DiffRow::Context(_) => {
                max = max.max(new);
                old += 1;
                new += 1;
            }
            DiffRow::Del(_) => {
                max = max.max(old);
                old += 1;
            }
            DiffRow::Add(_) => {
                max = max.max(new);
                new += 1;
            }
        }
    }
    max
}

/// Render up to `cap` of `h`'s unified rows, walking an old- and a new-side
/// counter from `h.start`: a deletion keeps its pre-edit number, an insertion
/// and a context row take their post-edit one.  Returns how many rows it drew.
fn push_hunk(ls: &mut Vec<Line<'static>>, h: &Hunk, cols: DiffCols, cap: usize) -> usize {
    let (mut old, mut new) = (h.start, h.start);
    for row in h.rows.iter().take(cap) {
        match row {
            DiffRow::Context(segs) => {
                push_gutter_row(ls, cols, new, ' ', segs, SLATE, None);
                old += 1;
                new += 1;
            }
            DiffRow::Del(segs) => {
                push_gutter_row(ls, cols, old, '-', segs, RED_HOT, Some(RED_HOT));
                old += 1;
            }
            DiffRow::Add(segs) => {
                push_gutter_row(ls, cols, new, '+', segs, LIME_HOT, Some(LIME_HOT));
                new += 1;
            }
        }
    }
    h.rows.len().min(cap)
}

/// Append one diff row, its body wrapped into `cols` with the number and sign
/// on the first row only.  An empty body still emits a bare marker row, so the
/// diff stays faithful to its input.  `hot` is the inline-emphasis colour for a
/// del/add (`None` on context): the segments `similar` flagged as actually
/// changed go bold in `hot`, the rest dim in `base`.
fn push_gutter_row(
    ls: &mut Vec<Line<'static>>,
    cols: DiffCols,
    lineno: u32,
    sign: char,
    segs: &[Seg],
    base: Color,
    hot: Option<Color>,
) {
    let DiffCols { gutter, width } = cols;
    let body: Vec<Span<'static>> = segs
        .iter()
        .filter(|s| !s.text.is_empty())
        .map(|s| {
            let style = match (hot, s.emph) {
                (Some(h), true) => Style::default().fg(h).add_modifier(Modifier::BOLD),
                (Some(_), false) => Style::default().fg(base).add_modifier(Modifier::DIM),
                (None, _) => Style::default().fg(base),
            };
            Span::styled(s.text.clone(), style)
        })
        .collect();
    let head = vec![
        Span::styled(
            format!("{} ", gutter.right(&lineno.to_string())),
            Style::default().fg(SLATE),
        ),
        Span::styled(format!("{sign} "), Style::default().fg(base)),
    ];
    ls.extend(hang(&head, body, width));
}
