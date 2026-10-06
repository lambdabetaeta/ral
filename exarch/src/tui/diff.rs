//! A patch rendered as a block: a header with its size and grain, hunks
//! numbered against one shared gutter, and elision rows where content is cut.

use super::block::Detail;
use super::line::{card_span, grain_run, hang, size_bar};
use super::palette::{Col, LIME_HOT, RED_HOT, SLATE};
use crate::bus::card::{self, Change, Diff, Hunk, Row as DiffRow, Seg};
use ral_core::types::WriteOutcome;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};

/// Diff rows a `Summary` shows before the elision: enough to read the change,
/// not enough to bury the transcript under it.
pub(super) const DIFF_PEEK_ROWS: usize = 10;

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
    let hunks: Vec<&Hunk> = hunks.iter().collect();
    let counts = (
        count_rows(&hunks, |r| matches!(r, DiffRow::Add(_))),
        count_rows(&hunks, |r| matches!(r, DiffRow::Del(_))),
    );
    let mut ls = vec![header(path, Some(counts), None)];
    match at {
        Detail::Tally => {}
        Detail::Summary => ls.extend(hunk_rows(&hunks, width, DIFF_PEEK_ROWS, 0)),
        Detail::Full => ls.extend(hunk_rows(&hunks, width, usize::MAX, 0)),
    }
    ls
}

/// A run of file changes at `width`: each file once, in the order it was first
/// touched, its changes stacked under one header.  Every header shows at every
/// rung, since a write is never folded away; `Summary` shares
/// [`DIFF_PEEK_ROWS`] across the run, and `Full` shows every row the source
/// kept.
pub(super) fn changes_body(changes: &[Change], width: usize, at: Detail) -> Vec<Line<'static>> {
    let mut room = match at {
        Detail::Tally => 0,
        Detail::Summary => DIFF_PEEK_ROWS,
        Detail::Full => usize::MAX,
    };
    let mut ls = Vec::new();
    for (path, file) in by_path(changes) {
        let diffs: Vec<&Diff> = file.iter().filter_map(|c| c.diff.as_ref()).collect();
        let worst = file.iter().map(|c| c.outcome).max();
        let counts = (!diffs.is_empty()).then(|| {
            diffs
                .iter()
                .fold((0, 0), |(a, r), d| (a + d.added, r + d.removed))
        });
        // How it settled is news when it did not commit, or all there is to say.
        let settled = worst.filter(|w| *w != WriteOutcome::Committed || counts.is_none());
        ls.push(header(path, counts, settled));
        if at > Detail::Tally {
            let hunks: Vec<&Hunk> = diffs.iter().flat_map(|d| &d.hunks).collect();
            let cut = diffs.iter().map(|d| d.cut()).sum();
            ls.extend(hunk_rows(&hunks, width, room, cut));
            room = room.saturating_sub(hunks.iter().map(|h| h.rows.len()).sum());
        }
    }
    ls
}

/// `changes` gathered per path, paths in the order first touched.
fn by_path(changes: &[Change]) -> Vec<(&str, Vec<&Change>)> {
    let mut files: Vec<(&str, Vec<&Change>)> = Vec::new();
    for change in changes {
        match files.iter_mut().find(|(path, _)| *path == change.path) {
            Some((_, file)) => file.push(change),
            None => files.push((&change.path, vec![change])),
        }
    }
    files
}

/// Rows across every hunk satisfying `pred` — the tallies a kit diff's header
/// reads, having no counts of its own.
fn count_rows(hunks: &[&Hunk], pred: impl Fn(&DiffRow) -> bool) -> u32 {
    let n = hunks
        .iter()
        .flat_map(|h| h.rows.iter())
        .filter(|r| pred(r))
        .count();
    u32::try_from(n).unwrap_or(u32::MAX)
}

/// A file's header row: `diff  <path>` with its [`size_bar`] and
/// addition-ratio [`grain_run`] when there are `counts` to show, else
/// `write  <path>`; and how it `settled`, when that is news.  Shared by every
/// rung, so the headers never drift.
fn header(path: &str, counts: Option<(u32, u32)>, settled: Option<WriteOutcome>) -> Line<'static> {
    let verb = if counts.is_some() { "diff" } else { "write" };
    let mut spans = vec![
        Span::styled(verb, Style::default().fg(SLATE)),
        Span::raw("  "),
        Span::styled(path.to_string(), Style::default().fg(Color::White)),
    ];
    if let Some((added, removed)) = counts {
        spans.extend([
            Span::raw("  "),
            size_bar(added + removed),
            Span::raw("  "),
            grain_run(added, removed),
        ]);
    }
    if let Some(outcome) = settled {
        spans.extend([Span::raw("  "), card_span(&card::settled(outcome))]);
    }
    Line::from(spans)
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

/// The first `cap` rows of `hunks`, the hunks elision-separated and numbered
/// against one gutter sized for them all, so every row's text starts in the
/// same column.  Rows cut short by `cap` end in the elision a break between
/// hunks wears; rows the source cut, `cut` changed lines of them, end in one
/// that counts them.
fn hunk_rows(hunks: &[&Hunk], width: usize, cap: usize, cut: u32) -> Vec<Line<'static>> {
    let total: usize = hunks.iter().map(|h| h.rows.len()).sum();
    let mut left = cap.min(total);
    let peeked = left < total;
    let widest = hunks.iter().map(|h| hunk_max_lineno(h)).max().unwrap_or(0);
    let cols = DiffCols {
        // Three columns even for a two-digit file, so a short patch's gutter is
        // the one a long patch wears.
        gutter: Col::wide(3).seeing(&widest.to_string()),
        width,
    };
    let mut ls = Vec::new();
    for (i, h) in hunks.iter().enumerate() {
        if left == 0 {
            break;
        }
        if i > 0 {
            ls.push(elision_row(cols.gutter, ""));
        }
        left -= push_hunk(&mut ls, h, cols, left);
    }
    if peeked {
        ls.push(elision_row(cols.gutter, ""));
    } else if cut > 0 {
        ls.push(elision_row(
            cols.gutter,
            &format!("{cut} more changed lines"),
        ));
    }
    ls
}

/// The "there is more below" row: a `⋮` right-aligned in `gutter`, drawn
/// by [`hunk_rows`] both between hunks and where it stops, so a break in the
/// middle of a diff and a diff cut short read alike; `note` says how much.
fn elision_row(gutter: Col, note: &str) -> Line<'static> {
    Line::from(Span::styled(
        format!("{} {note}", gutter.right("⋮")),
        Style::default().fg(SLATE),
    ))
}

/// The largest number [`push_hunk`] will stamp on `h`, which sizes the gutter.
/// Walks the same two counters, so the two must move together.
fn hunk_max_lineno(h: &Hunk) -> u32 {
    let (mut old, mut new) = (h.old, h.new);
    let mut max = old.max(new);
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
/// counter from `h.old` and `h.new`: a deletion keeps its pre-edit number, an insertion
/// and a context row take their post-edit one.  Returns how many rows it drew.
fn push_hunk(ls: &mut Vec<Line<'static>>, h: &Hunk, cols: DiffCols, cap: usize) -> usize {
    let (mut old, mut new) = (h.old, h.new);
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
