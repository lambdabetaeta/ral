//! Line builders: each returns `Vec<Line<'static>>` ready for the scrollback,
//! drawing colour and width from [`super::palette`].  [`super::App::handle`]
//! calls in here to turn a typed [`crate::bus::card::Card`] into rows.
//!
//! No builder here draws a rail glyph: `Block::render_with` in `block.rs` seats
//! it on the first content row, so a selection through a block copies clean.

use super::highlight::highlight_ral;
use super::palette::{
    CODE_BG, CYAN, LIME, LIME_HOT, ORANGE, PROMPT_INK, RAIL_W, READ_CONTENT_W, READ_W, RED,
    RED_HOT, SLATE, content_w,
};
use super::row::Row;
use crate::agent::event::ProviderErrorRecord;
use crate::bus::card::{
    Card, Field as CardField, FieldVal, Hunk, Mark, Measure, Role, Row as DiffRow, Seg,
    Span as CardSpan,
};
use crate::provider;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::Value;
use std::borrow::Cow;
use std::time::Duration;
use unicode_width::{UnicodeWidthChar, UnicodeWidthStr};

/// True when `l` carries no glyphs, so it reads as a vertical gap — the one
/// "this row is a separator" test `md`, `viewport`, `block` and `group` share.
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

/// Width in cells of the header size-bar — the size channel, ordered beside
/// the rail's lightness.
const SIZE_BAR_W: usize = 8;

/// Bucket `magnitude` onto a `log2` scale clamped to `0..=cap`: step `0` at
/// zero, one step per doubling.  The same scale [`super::rail::value_step`]
/// tracks, at coarser resolution.
fn log2_step(magnitude: u32, cap: usize) -> usize {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "log2 of a small positive count, then min-clamped"
    )]
    let step = ((magnitude as f32 + 1.0).log2().round() as usize).min(cap);
    step
}

/// Filled cells for `magnitude`, log2-scaled: a 1-line event lights one cell;
/// the bar fills around 180 lines.
fn size_cells(magnitude: u32) -> usize {
    log2_step(magnitude, SIZE_BAR_W)
}

/// The header size-bar: [`SIZE_BAR_W`] cells, `█` filled and `░` empty, in
/// [`SLATE`] so it reads as chrome beside the path or summary, not as content.
pub(super) fn size_bar(magnitude: u32) -> Span<'static> {
    Span::styled(size_bar_text(magnitude), Style::default().fg(SLATE))
}

/// The bar's glyphs alone, for a caller that paints them in its own hue — the
/// metadata matrix's per-agent bar.
pub(super) fn size_bar_text(magnitude: u32) -> String {
    let filled = size_cells(magnitude);
    "█"
        .repeat(filled)
        .chars()
        .chain("░".repeat(SIZE_BAR_W - filled).chars())
        .collect()
}

/// Vertical-sparkline glyphs, lowest to highest — one per call in a coalesced
/// run, so the run reads as a bar chart of what each call moved.
const SPARK_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// One call's sparkline glyph, on [`size_cells`]'s scale at eight steps.  A
/// `None` or empty result still lights the shortest bar, because a call ran.
pub(super) fn spark_glyph(magnitude: Option<u32>) -> char {
    SPARK_GLYPHS[log2_step(magnitude.unwrap_or(0), SPARK_GLYPHS.len() - 1)]
}

/// Width in cells of the header grain run — the grain channel, reading *what
/// kind* of change beside the size-bar's *how much*.
const GRAIN_W: usize = 4;

/// Grain: [`GRAIN_W`] braille cells whose density reads `a / (a + b)` on the
/// ramp `⣿⣶⣤⣀`, in [`SLATE`] so it never collides with a data hue like the
/// `+`/`-` line colours.  Read by the patch header (added over changed) and the
/// thinking header (thought over said — how dearly the answer was bought).
pub(super) fn grain_run(a: u32, b: u32) -> Span<'static> {
    let total = a + b;
    #[allow(
        clippy::cast_precision_loss,
        reason = "changed-line counts far below f32 precision limit"
    )]
    let cell = if total == 0 {
        ' '
    } else {
        grain_cell(a as f32 / total as f32)
    };
    Span::styled(cell.to_string().repeat(GRAIN_W), Style::default().fg(SLATE))
}

fn grain_cell(ratio: f32) -> char {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "ratio in [0,1] times a small constant GRAIN_W"
    )]
    let bucket = (ratio * GRAIN_W as f32) as usize;
    match bucket {
        3.. => '⣿',
        2 => '⣶',
        1 => '⣤',
        _ => '⣀',
    }
}

/// A thinking block's collapsed header: the deliberation grain beside a
/// [`size_bar`] of the reasoning's own bulk.  Both the committed block and the
/// still-streaming `Viewport::thinking_seat` render through here, so the
/// provisional header cannot drift from the one the block commits to.
pub(super) fn thinking_header(
    think_chars: u32,
    think_lines: u32,
    say_chars: u32,
) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(vec![
            grain_run(think_chars, say_chars),
            Span::raw(" "),
            size_bar(think_lines),
        ]),
    ]
}

// ── Public line builders ─────────────────────────────────────────────────────

/// Step separator: one blank line.  The number itself reaches only
/// `events.jsonl` / `user.log`; on screen the boundary is whitespace alone.
pub(super) fn step() -> Vec<Line<'static>> {
    vec![Line::default()]
}

/// Scrollback echo of the human's turn, tinted [`PROMPT_INK`] — a quiet island
/// in the machine's chromatic stream, since the agents own the matrix hues.  No
/// background band: background is the machine's ([`CODE_BG`]) and reverse video
/// is the selection's.  `block.rs` lays a [`prompt_fence`] above the first row.
pub(super) fn user_prompt(s: &str) -> Vec<Line<'static>> {
    let ink = Style::default().fg(PROMPT_INK);
    let mut ls: Vec<Line<'static>> = vec![Line::default()];
    ls.extend(
        s.lines()
            .map(|l| Line::from(Span::styled(l.to_string(), ink))),
    );
    ls
}

/// The human turn's opening seam: a full-width rule in [`PROMPT_INK`].  A
/// boundary drawn as a line rather than a region, so background stays free to
/// mean "machine".  The one row whose margin is not blank chrome but part of
/// the mark itself: a rule that stopped short of the edge would read as a rail
/// glyph beside a shorter rule.
pub(super) fn prompt_fence(width: u16) -> Row {
    let ink = Style::default().fg(PROMPT_INK);
    Row::new(
        Span::styled("─".repeat(RAIL_W), ink),
        Line::from(Span::styled("─".repeat(content_w(width).into()), ink)),
    )
}

/// Tool-call header rows: the slate `label`, continuations wrapped under its
/// own column, and `size` as a trailing [`size_bar`] — the collapsed header
/// *is* the call's summary, so the bar is its readout.  `None` for the expanded
/// and static headers, which have a body to speak for them.
fn tool_call_header(label: &str, size: Option<u32>, width: u16) -> Vec<Line<'static>> {
    // Reserve the bar's gutter so the label wraps before it: pinning the bar to
    // a fixed right column is what makes magnitudes comparable down the page,
    // so it must never spill onto a wrapped row.
    let bar_w = if size.is_some() {
        UnicodeWidthStr::width("  ") + SIZE_BAR_W
    } else {
        0
    };
    let body_w = (width as usize).saturating_sub(bar_w).max(8);
    let mut out = Vec::new();
    push_wrapped(&mut out, label, body_w, |chunk, first| {
        if first {
            let mut spans = vec![Span::styled(chunk, Style::default().fg(SLATE))];
            if let Some(magnitude) = size {
                spans.push(Span::raw("  "));
                spans.push(size_bar(magnitude));
            }
            Line::from(spans)
        } else {
            Line::from(Span::styled(chunk, Style::default().fg(SLATE)))
        }
    });
    out
}
/// A tool call at L1: the header alone.  Dialing the row open swaps in
/// [`tool_call_body`].
pub(super) fn tool_call_collapsed(
    label: &str,
    size: Option<u32>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header(label, size, width));
    ls
}

/// The revealed tool call (L2/L3): the header, a blank, then `cmd`'s
/// highlighted source — the first `cap` lines, or all of them when `None`.
pub(super) fn tool_call_body(
    label: &str,
    cmd: &str,
    cap: Option<usize>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header(label, None, width));
    ls.push(Line::default());
    let take = cap.unwrap_or(usize::MAX);
    for line in highlight_ral(cmd).into_iter().take(take) {
        push_code_row(&mut ls, &line, width);
    }
    ls
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

/// Append one highlighted source row, inset two columns and washed into the
/// [`CODE_BG`] panel padded to `width`.  Continuations hang under the inset
/// plus the line's own leading whitespace, so a long expression folds where its
/// content began.
fn push_code_row(ls: &mut Vec<Line<'static>>, line: &Line<'static>, width: u16) {
    for row in wrap_line(line, width as usize) {
        ls.push(wash(row, CODE_BG, Some(width as usize)));
    }
}

/// A summary-less tool call rendered standalone — the per-block `user.log` tee
/// of a `BlockKind::PlainTool`, which on screen coalesces into a run instead.
/// `cmd`'s first line is the label, any remainder follows indented.
pub(super) fn tool_call_static(cmd: &str) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header(
        cmd.lines().next().unwrap_or(""),
        None,
        READ_W,
    ));
    for l in cmd.lines().skip(1) {
        ls.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(l.to_string(), Style::default().fg(SLATE)),
        ]));
    }
    ls
}

/// The verb column of an act row, pinned to the longest verb (`unschedule`)
/// plus a space so verbs align across blocks.  [`render_field_rows`] cannot
/// supply it: it sizes from the rows it is handed, and an act block is one row.
pub(super) const ACT_VERB_W: usize = 11;
/// The subject column of an act row, truncated into rather than allowed to
/// shift the payload column.  Wide enough that a name never has to be cut: a
/// subject is an identity, and a cut identity names nothing.
pub(super) const ACT_SUBJECT_W: usize = 20;

/// A harness act: `verb`, `subject`, `payload` in three columns, the first two
/// pinned.  An act changes the world outside the turn, so it carries no
/// magnitude and wears no size-bar.  A `failed` act tiers the short refusal hot
/// on the row that names the attempt; the long form is the model's raise.
/// `full` wraps the payload under its column; reduced truncates into it.
pub(super) fn act_row(
    verb: &str,
    subject: Option<&str>,
    payload: &str,
    failed: bool,
    width: u16,
    full: bool,
) -> Vec<Line<'static>> {
    let payload_w = (width as usize)
        .saturating_sub(ACT_VERB_W + ACT_SUBJECT_W)
        .max(8);
    let ink = if failed {
        Style::default().fg(RED_HOT).add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(SLATE)
    };
    let mut head = vec![Span::styled(
        format!("{verb:<ACT_VERB_W$}"),
        Style::default().fg(SLATE).add_modifier(Modifier::BOLD),
    )];
    // Pad the subject cell only when a payload follows, so a landed `cancel`
    // copies as `cancel     hunter` with no trailing run of column padding.
    if let Some(subject) = subject.filter(|s| !s.is_empty()) {
        let mut cell = truncate_spans(&[bold(subject.to_string(), LIME)], ACT_SUBJECT_W - 1);
        if !payload.is_empty() {
            cell.push(Span::raw(" ".repeat(ACT_SUBJECT_W - span_run_width(&cell))));
        }
        head.extend(cell);
    } else if !payload.is_empty() {
        head.push(Span::raw(" ".repeat(ACT_SUBJECT_W)));
    }
    let mut out = vec![Line::default()];
    if payload.is_empty() {
        out.push(Line::from(head));
    } else if full {
        push_wrapped(&mut out, payload, payload_w, |chunk, first| {
            if first {
                let mut spans = head.clone();
                spans.push(Span::styled(chunk, ink));
                Line::from(spans)
            } else {
                Line::from(vec![
                    Span::raw(" ".repeat(ACT_VERB_W + ACT_SUBJECT_W)),
                    Span::styled(chunk, ink),
                ])
            }
        });
    } else {
        head.extend(truncate_spans(
            &[Span::styled(payload.to_string(), ink)],
            payload_w,
        ));
        out.push(Line::from(head));
    }
    out
}

/// An async subagent's landed result, in one line: the bold `name` ([`ORANGE`]
/// when `error` is set), the elapsed seconds, a [`size_bar`] of `size`, and an
/// error or empty-output suffix.
pub(super) fn subagent_header(
    name: &str,
    size: u32,
    error: Option<&str>,
    elapsed: Duration,
) -> Vec<Line<'static>> {
    let secs = elapsed.as_secs();
    let name_color = if error.is_some() { ORANGE } else { LIME };
    let mut spans = vec![
        bold(name.to_string(), name_color),
        Span::styled(
            format!(" {secs}s "),
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ),
        size_bar(size),
    ];
    let suffix = match error {
        None if size == 0 => Some("[done, no output]".to_string()),
        None => None,
        Some(reason) if reason.eq_ignore_ascii_case("cancelled") => Some("[cancelled]".to_string()),
        Some(reason) => Some(format!("[failed: {reason}]")),
    };
    if let Some(suffix) = suffix {
        let suffix_color = if error.is_some() { ORANGE } else { SLATE };
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            suffix,
            Style::default()
                .fg(suffix_color)
                .add_modifier(Modifier::DIM),
        ));
    }
    vec![Line::default(), Line::from(spans)]
}

pub(super) fn error(msg: &str) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(vec![
            Span::styled(
                "error ",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Span::raw(msg.to_string()),
        ]),
    ]
}

/// A [`Mark::Diff`]'s body: L1 the header alone, anything above it the header
/// and every hunk.  There is no middle rung to draw, the mark itself being cut
/// to `DIFF_ROWS` where it was composed.  No leading blank —
/// [`render_card`] owns the one blank that opens the card.  The densest object
/// on screen: size in the header bar, grain in the addition ratio, value in the
/// rail's lightness, shape in its `▎` glyph.
fn diff_body(path: &str, hunks: &[Hunk], level: u8) -> Vec<Line<'static>> {
    match level {
        1 => vec![patch_header(path, hunks)],
        _ => diff_capped(path, hunks, None),
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
/// [`grain_run`].  Shared by every level, so L1/L2/L3 headers never drift.
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

/// The header, then `cap` hunks (all when `None`), elision-separated and
/// numbered against one gutter sized for the whole block, so every row's text
/// starts in the same column.
fn diff_capped(path: &str, hunks: &[Hunk], cap: Option<usize>) -> Vec<Line<'static>> {
    let mut ls: Vec<Line<'static>> = vec![patch_header(path, hunks)];
    let shown = cap.unwrap_or(hunks.len()).min(hunks.len());
    let gutter = hunks
        .iter()
        .map(hunk_max_lineno)
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(3);
    for (i, h) in hunks[..shown].iter().enumerate() {
        if i > 0 {
            ls.push(elision_row(gutter));
        }
        push_hunk(&mut ls, h, gutter);
    }
    ls
}

/// The "there is more below" row: a bare `⋮` right-aligned in `gutter`, drawn
/// by [`diff_capped`] both between hunks and at its cap, so a break in the
/// middle of a diff and a diff cut short read alike.
fn elision_row(gutter: usize) -> Line<'static> {
    Line::from(Span::styled(
        format!("{:>gutter$} ", "⋮"),
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

/// Render `h`'s unified rows, walking an old- and a new-side counter from
/// `h.start`: a deletion keeps its pre-edit number, an insertion and a context
/// row take their post-edit one.
fn push_hunk(ls: &mut Vec<Line<'static>>, h: &Hunk, gutter: usize) {
    let (mut old, mut new) = (h.start, h.start);
    for row in &h.rows {
        match row {
            DiffRow::Context(segs) => {
                push_gutter_row(ls, gutter, new, ' ', segs, SLATE, None);
                old += 1;
                new += 1;
            }
            DiffRow::Del(segs) => {
                push_gutter_row(ls, gutter, old, '-', segs, RED_HOT, Some(RED_HOT));
                old += 1;
            }
            DiffRow::Add(segs) => {
                push_gutter_row(ls, gutter, new, '+', segs, LIME_HOT, Some(LIME_HOT));
                new += 1;
            }
        }
    }
}

/// Append one diff row, its body wrapped to [`READ_W`] with the number and sign
/// on the first row only.  An empty body still emits a bare marker row, so the
/// diff stays faithful to its input.  `hot` is the inline-emphasis colour for a
/// del/add (`None` on context): the segments `similar` flagged as actually
/// changed go bold in `hot`, the rest dim in `base`.
fn push_gutter_row(
    ls: &mut Vec<Line<'static>>,
    gutter: usize,
    lineno: u32,
    sign: char,
    segs: &[Seg],
    base: Color,
    hot: Option<Color>,
) {
    // The readable content width less the line-number gutter and its space and
    // "<sign> ", floored so a pathological width still wraps.
    let body_w = usize::from(READ_CONTENT_W)
        .saturating_sub(gutter + 1 + 2)
        .max(8);
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
    for (i, wrapped) in wrap_line(&Line::from(body), body_w).into_iter().enumerate() {
        let (num, marker) = if i == 0 {
            (format!("{lineno:>gutter$}"), format!("{sign} "))
        } else {
            (" ".repeat(gutter), "  ".to_string())
        };
        let mut spans = vec![
            Span::styled(format!("{num} "), Style::default().fg(SLATE)),
            Span::styled(marker, Style::default().fg(base)),
        ];
        spans.extend(wrapped.spans);
        ls.push(Line::from(spans));
    }
}

// ── Card rendering ───────────────────────────────────────────────────────────

/// The one binding of a nominal [`Role`] to the retinal variable that carries
/// it: hue, plus a weight shift where emphasis is part of the role.  Content
/// hue lives here alone, so the kit names a role but never a colour.
fn role_style(role: Role) -> Style {
    match role {
        Role::Path => Style::default().fg(CYAN),
        Role::Code => Style::default().fg(Color::White),
        Role::Ok => Style::default().fg(LIME).add_modifier(Modifier::BOLD),
        Role::Warn => Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        Role::Bad => Style::default().fg(RED).add_modifier(Modifier::BOLD),
        Role::Muted => Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        Role::Strong => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    }
}

/// Roled spans bind through [`role_style`]; roleless ones take plain white ink.
fn span_style(role: Option<Role>) -> Style {
    role.map_or_else(|| Style::default().fg(Color::White), role_style)
}

/// The one dispatch every [`Mark`] variant routes through, shared by
/// [`render_effect_lines`] and [`render_framed`], so a new variant cannot be
/// wired into one interpreter and forgotten in the other.
fn render_mark(mark: &Mark, level: u8) -> Vec<Line<'static>> {
    match mark {
        Mark::Text { spans } => render_text(spans),
        Mark::Measure(m) => vec![render_measure(m)],
        Mark::Fields { rows } => render_fields(rows),
        Mark::Diff { path, hunks } => diff_body(path, hunks, level),
        Mark::Raw { bytes } => render_raw(bytes),
    }
}

/// Render an Observation-origin card's marks unframed, for folding as plain
/// rows under its call's intent in a coalesced group: the fold applies its
/// own wrapping and indent, so a card frame here would nest inside another.
/// The one spot that does not go through [`render_card`].
pub(super) fn render_effect_lines(card: &Card, level: u8) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    for mark in card.marks() {
        ls.extend(render_mark(mark, level));
    }
    ls
}

/// Left indent of a card in the transcript, in columns — pushed right of the
/// flow so it reads as a composed object, set apart.  The opening card is the
/// one card placed elsewhere, and names its own inset.
pub(super) const CARD_INDENT: usize = 4;

/// Every card as a framed object at `indent_w` — the one path a `Card` renders
/// through, save [`render_effect_lines`]'s inline fold.  The frame wears
/// neutral ink, since identity lives in the matrix.
pub(super) fn render_card(
    card: &Card,
    indent_w: usize,
    width: u16,
    level: u8,
) -> Vec<Line<'static>> {
    render_framed(
        card,
        indent_w,
        Style::default().fg(SLATE),
        width.min(READ_CONTENT_W),
        false,
        level,
    )
}

/// A card whose frame fills its placement's width.
pub(super) fn render_filled_card(
    card: &Card,
    indent_w: usize,
    width: u16,
    level: u8,
) -> Vec<Line<'static>> {
    render_framed(
        card,
        indent_w,
        Style::default().fg(SLATE),
        width.min(READ_CONTENT_W),
        true,
        level,
    )
}

const REGISTER_CARD_MARGIN: usize = 2;

/// A pinned register card, framed in its producing agent's `hue` and filling
/// the inset width.  The hue is the pin's one departure from a plain card:
/// identity the transcript reads off the matrix, a side column must carry.
/// A pin has no `Reveal` dial of its own, so it always renders at full depth.
pub(super) fn render_pin(card: &Card, width: u16, hue: Color) -> Vec<Line<'static>> {
    #[allow(
        clippy::cast_possible_truncation,
        reason = "small compile-time constant fits u16"
    )]
    let draw_w = width.saturating_sub(REGISTER_CARD_MARGIN as u16);
    render_framed(
        card,
        REGISTER_CARD_MARGIN,
        Style::default().fg(hue),
        draw_w,
        true,
        3,
    )
}

/// The framed-card renderer behind both [`render_card`] and [`render_pin`]: a
/// box `indent_w` columns in, drawn in `border`, content wrapped to a budget
/// from `width` — which the caller has already capped. `level` reaches only
/// a `diff` mark; every other mark ignores it.
fn render_framed(
    card: &Card,
    indent_w: usize,
    border: Style,
    width: u16,
    fill: bool,
    level: u8,
) -> Vec<Line<'static>> {
    let indent = " ".repeat(indent_w);
    // Less the indent and the four frame columns (`│ ` … ` │`).
    let max_inner = (width as usize).saturating_sub(indent_w + 4).max(8);

    // A single-line leading text mark lifts into the top rule; anything else
    // leaves no title.  Truncated to the budget, or the rule would outgrow the
    // body rows on a narrow terminal.
    let marks = card.marks();
    let (title, body_marks): (Option<Vec<Span<'static>>>, &[Mark]) = match marks.first() {
        Some(Mark::Text { spans }) => {
            let head = render_text(spans);
            if head.len() == 1 {
                (
                    Some(truncate_spans(&head[0].spans, max_inner.saturating_sub(1))),
                    &marks[1..],
                )
            } else {
                (None, marks)
            }
        }
        _ => (None, marks),
    };

    let mut body: Vec<Line<'static>> = Vec::new();
    for mark in body_marks {
        body.extend(render_mark(mark, level));
    }
    let wrapped: Vec<Line<'static>> = body.iter().flat_map(|l| wrap_line(l, max_inner)).collect();

    // The widest row, but at least one column past the title so `╭─ title ─╮`
    // always closes.
    let title_w = title.as_deref().map_or(0, span_run_width);
    let title_min = if title.is_some() { title_w + 1 } else { 0 };
    let natural_inner = wrapped
        .iter()
        .map(|l| span_run_width(&l.spans))
        .max()
        .unwrap_or(0)
        .max(title_min)
        .clamp(1, max_inner);
    let inner_w = if fill { max_inner } else { natural_inner };
    let interior = inner_w + 2; // one padding column each side

    let mut out: Vec<Line<'static>> = vec![Line::default()];

    let mut top = vec![Span::raw(indent.clone())];
    if let Some(spans) = &title {
        top.push(Span::styled("╭─ ", border));
        top.extend(spans.iter().cloned());
        let fill = interior.saturating_sub(3 + title_w); // "─ " + title + " "
        top.push(Span::styled(format!(" {}", "─".repeat(fill)), border));
    } else {
        top.push(Span::styled("╭", border));
        top.push(Span::styled("─".repeat(interior), border));
    }
    top.push(Span::styled("╮", border));
    out.push(Line::from(top));

    for row in &wrapped {
        let pad = inner_w.saturating_sub(span_run_width(&row.spans));
        let mut spans = vec![Span::raw(indent.clone()), Span::styled("│ ", border)];
        spans.extend(row.spans.iter().cloned());
        spans.push(Span::raw(" ".repeat(pad + 1)));
        spans.push(Span::styled("│", border));
        out.push(Line::from(spans));
    }

    out.push(Line::from(vec![
        Span::raw(indent),
        Span::styled("╰", border),
        Span::styled("─".repeat(interior), border),
        Span::styled("╯", border),
    ]));
    out
}

/// The register column: each pin framed in `hue` and stacked.  Every framed
/// card leads with a blank, so the stack carries its own gutter; slot keys are
/// identity, never shown — a pinned card carries its own label.
pub(super) fn render_register(
    pins: &[(String, Card)],
    width: u16,
    hue: Color,
) -> Vec<Line<'static>> {
    pins.iter()
        .flat_map(|(_key, card)| render_pin(card, width, hue))
        .collect()
}

/// Total display width of a span run, unicode-aware.
fn span_run_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(ratatui::prelude::Span::width).sum()
}

/// Truncate a styled span run to `max_w` columns, appending an `…` when
/// anything is dropped.
fn truncate_spans(spans: &[Span<'static>], max_w: usize) -> Vec<Span<'static>> {
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

/// A `text` mark's roled spans as `Line`s, broken on embedded newlines.
/// Width-folding happens later, in [`wrap_line`].
pub(super) fn render_text(spans: &[CardSpan]) -> Vec<Line<'static>> {
    fold_styled_lines(
        spans
            .iter()
            .map(|cs| (cs.text.clone(), span_style(cs.role))),
        true,
    )
}

/// Fold `(text, style)` fragments into `Line`s, splitting on `\n` — shared by
/// [`render_text`] and `into_lines` in `highlight.rs`, whose one difference is
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

/// A `measure` mark as one line: slate label, then the readout and its bar.
fn render_measure(m: &Measure) -> Line<'static> {
    let mut spans = vec![
        Span::styled(m.label.clone(), Style::default().fg(SLATE)),
        Span::raw("  "),
    ];
    spans.extend(measure_value_spans(m));
    Line::from(spans)
}

/// A [`Measure`]'s value without its label, since a fields row supplies its
/// own: bounded (`max` present) reads `value/max` with a proportional
/// [`progress_bar`], unbounded reads `value[unit]` with a `log2` [`size_bar`].
fn measure_value_spans(m: &Measure) -> Vec<Span<'static>> {
    let white = Style::default().fg(Color::White);
    if let Some(max) = m.max {
        let mut spans = vec![
            Span::styled(format!("{}/{}", m.value, max), white),
            Span::raw("  "),
        ];
        spans.extend(progress_bar(m.value, max));
        spans
    } else {
        let readout = match &m.unit {
            Some(u) => format!("{}{u}", m.value),
            None => m.value.to_string(),
        };
        vec![
            Span::styled(readout, white),
            Span::raw("  "),
            size_bar(m.value),
        ]
    }
}

/// A proportional `██████░░░░` fill of `done/total`.  `total == 0` reads as no
/// progress rather than a divide by zero.
fn progress_bar(done: u32, total: u32) -> Vec<Span<'static>> {
    const W: u32 = 10;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "value already clamped to W via min"
    )]
    let filled = if total == 0 {
        0
    } else {
        ((u64::from(done) * u64::from(W)) / u64::from(total)).min(u64::from(W)) as u32
    };
    vec![
        Span::styled("█".repeat(filled as usize), Style::default().fg(LIME)),
        Span::styled(
            "░".repeat((W - filled) as usize),
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ),
    ]
}

/// A `fields` mark — selective alignment: every value lands in one shared
/// column.  A single-span text value wraps under it; a multi-span value or a
/// [`Measure`] renders inline on one row.
fn render_fields(rows: &[CardField]) -> Vec<Line<'static>> {
    let field_rows: Vec<FieldRow> = rows
        .iter()
        .map(|f| FieldRow {
            label: f.label.clone(),
            value: match &f.value {
                FieldVal::Inline(spans) => match spans.as_slice() {
                    [one] => FieldValue::Wrapped {
                        text: one.text.clone(),
                        style: span_style(one.role),
                    },
                    many => FieldValue::Inline(
                        many.iter()
                            .map(|s| Span::styled(s.text.clone(), span_style(s.role)))
                            .collect(),
                    ),
                },
                FieldVal::Measure(m) => FieldValue::Inline(measure_value_spans(m)),
            },
        })
        .collect();
    render_field_rows(&field_rows, READ_CONTENT_W.into())
}

/// A `raw` mark — bytes appended verbatim as lossy UTF-8, unstyled: it is an
/// image, not an encoding, so no role applies.
fn render_raw(bytes: &[u8]) -> Vec<Line<'static>> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| Line::from(Span::raw(l.to_string())))
        .collect()
}

/// Slate text for system notes — model switches, stream stalls, compaction.
pub(super) fn note(s: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        s.to_string(),
        Style::default().fg(SLATE),
    ))]
}

/// Bracketed notice around the provider's own un-normalised reason string.
pub(super) fn stop_reason(raw: &str) -> Vec<Line<'static>> {
    note(&format!("[stop: {raw}]"))
}

/// The permanent usage status bar, styling the pieces
/// [`provider::Usage::parts`] yields.  `Usage`'s own `Display` is the
/// long-form log spelling instead.
pub(super) fn usage_text(usage: &provider::Usage) -> Vec<Span<'static>> {
    let p = usage.parts();
    let s = |b: &str| Span::styled(b.to_string(), Style::default().fg(SLATE));
    let n = |b: String, c: Color| Span::styled(b, Style::default().fg(c));
    let db =
        |b: String, c: Color| Span::styled(b, Style::default().fg(c).add_modifier(Modifier::BOLD));
    let mut sp = vec![
        s("["),
        db(p.input, LIME),
        s(" in/"),
        db(p.output, LIME),
        s(" out"),
    ];
    if let Some((wr, rd)) = p.cache {
        sp.extend([s("/"), n(wr, LIME), s(" wr/"), n(rd, LIME), s(" rd")]);
    }
    sp.extend([s("] · "), db(p.cost, LIME)]);
    sp
}

// ── Internal helpers ─────────────────────────────────────────────────────────

pub(super) fn bold(c: String, col: Color) -> Span<'static> {
    Span::styled(c, Style::default().fg(col).add_modifier(Modifier::BOLD))
}

/// Wrap `text` to `body_w` and push `row(chunk, first)` per chunk.
/// [`textwrap::wrap`] always yields at least one chunk, empty input included,
/// so a blank value still renders its marker via `row("", true)`.
pub(super) fn push_wrapped(
    out: &mut Vec<Line<'static>>,
    text: &str,
    body_w: usize,
    mut row: impl FnMut(String, bool) -> Line<'static>,
) {
    let mut first = true;
    for chunk in textwrap::wrap(text, body_w) {
        out.push(row(chunk.into_owned(), first));
        first = false;
    }
}

// ── Aligned-field rendering (the `fields` mark + provider errors) ────────────

/// Text to wrap under the shared label column, or pre-styled spans to render
/// inline on the value row.
enum FieldValue {
    Wrapped { text: String, style: Style },
    Inline(Vec<Span<'static>>),
}

/// One `(label, value)` row ahead of layout — what the `fields` mark and
/// [`provider_error`] both feed into [`render_field_rows`].
struct FieldRow {
    label: String,
    value: FieldValue,
}

fn text_field(label: impl Into<String>, value: impl Into<String>) -> FieldRow {
    FieldRow {
        label: label.into(),
        value: FieldValue::Wrapped {
            text: value.into(),
            style: Style::default(),
        },
    }
}

/// The slate-bold label lead, padded into the shared `label_w` column.
fn field_label(label: &str, label_w: usize) -> Span<'static> {
    Span::styled(
        format!("{label:<label_w$}"),
        Style::default().fg(SLATE).add_modifier(Modifier::BOLD),
    )
}

/// Aligned `(label, value)` rows in one shared column — the primitive
/// [`render_fields`], [`provider_error`] and [`legend_rows`] all feed.  The
/// column is measured once from the longest label, so every value starts alike.
fn render_field_rows(rows: &[FieldRow], width: usize) -> Vec<Line<'static>> {
    let Some(label_w) = rows.iter().map(|r| r.label.chars().count()).max() else {
        return Vec::new();
    };
    let label_w = label_w + 2; // "<label>  "
    let mut ls: Vec<Line<'static>> = Vec::new();
    for r in rows {
        match &r.value {
            FieldValue::Wrapped { text, style } => {
                push_field(&mut ls, &r.label, text, *style, label_w, width);
            }
            FieldValue::Inline(spans) => {
                let mut line = vec![field_label(&r.label, label_w)];
                line.extend(spans.iter().cloned());
                ls.push(Line::from(line));
            }
        }
    }
    ls
}

/// Align pre-styled `(label, sample)` rows — the `/legend` panel.  Its samples
/// are the literal output of the rail / bar / grain builders, not data a
/// [`Role`] could name, so they arrive styled: the one place the TUI shows
/// appearance, because appearance *is* the subject.
pub(super) fn legend_rows(rows: Vec<(&str, Vec<Span<'static>>)>) -> Vec<Line<'static>> {
    let rows: Vec<FieldRow> = rows
        .into_iter()
        .map(|(label, spans)| FieldRow {
            label: label.to_string(),
            value: FieldValue::Inline(spans),
        })
        .collect();
    render_field_rows(&rows, READ_CONTENT_W.into())
}

// ── Provider-error rendering ────────────────────────────────────────────────

/// Body keys carrying the retry-after wait as a second count, in precedence
/// order — read by [`wait_from_body`], then suppressed from the body dump
/// (along with the absolute `resets_at` twin) once the wait field renders them.
const RETRY_SECS_KEYS: &[&str] = &["resets_in_seconds", "retry_after_seconds", "retry_after"];

/// A [`ProviderErrorRecord`] as a block: the `error: <kind>` headline, then an
/// ordered field list in one shared column.  A parsed `body` supplies the
/// fields ([`body_fields`]); without one the free-text `cause`/`message` is
/// shown honestly rather than dressed as structure.
pub(super) fn provider_error(e: &ProviderErrorRecord) -> Vec<Line<'static>> {
    let mut ls: Vec<Line<'static>> = vec![Line::default()];
    // Cancellation folds its site into the headline and carries no body.
    if let ProviderErrorRecord::Cancelled { where_ } = e {
        ls.push(headline(&format!("cancelled ({where_})")));
        return ls;
    }
    ls.push(headline(error_kind(e)));
    ls.extend(render_field_rows(&error_fields(e), READ_CONTENT_W.into()));
    ls
}

/// A stall as a block: the same weight and the same field list a fatal failure
/// gets, under a headline that says the exchange survived it.  The `continuing`
/// field is the whole distinction — without it the block would read as the end
/// of the run, which is precisely what a stall is not.
pub(super) fn stalled(e: &ProviderErrorRecord) -> Vec<Line<'static>> {
    let mut ls: Vec<Line<'static>> = vec![Line::default(), headline("stream stalled")];
    let mut fields = error_fields(e);
    fields.push(text_field(
        "continuing",
        "the partial reply above is kept; the turn resumes from it",
    ));
    ls.extend(render_field_rows(&fields, READ_CONTENT_W.into()));
    ls
}

/// The ordered field list under either headline.  `Cancelled` never reaches
/// here: [`provider_error`] returns before the call, and a cancel never commits
/// as a stall — `Engine::complete` exempts it by name.
fn error_fields(e: &ProviderErrorRecord) -> Vec<FieldRow> {
    match e {
        ProviderErrorRecord::Cancelled { .. } => unreachable!("cancellation carries no fields"),
        ProviderErrorRecord::RateLimited {
            retry_after_secs,
            cause,
            body,
        } => {
            let mut fs = Vec::new();
            let wait = retry_after_secs.or_else(|| body.as_ref().and_then(wait_from_body));
            if let Some(secs) = wait {
                fs.push(wait_field(secs));
            }
            match body {
                // Suppress the raw keys the wait field already subsumes.
                Some(b) => {
                    let consumed: Vec<&str> = RETRY_SECS_KEYS
                        .iter()
                        .copied()
                        .chain(["resets_at"])
                        .collect();
                    fs.extend(body_fields(b, &consumed));
                }
                None => fs.push(text_field("cause", prettify(cause))),
            }
            fs
        }
        ProviderErrorRecord::Transient {
            cause,
            attempts,
            body,
            status,
        } => {
            let mut fs = vec![text_field("attempts", attempts.to_string())];
            if let Some(s) = status {
                fs.push(text_field("status", s.to_string()));
            }
            match body {
                Some(b) => fs.extend(body_fields(b, &[])),
                None => fs.push(text_field("cause", prettify(cause))),
            }
            fs
        }
        ProviderErrorRecord::Api {
            status,
            model,
            message,
            body,
        } => {
            let mut fs = Vec::new();
            if let Some(s) = status {
                fs.push(text_field("status", s.to_string()));
            }
            fs.push(text_field("model", model.clone()));
            if let Some(u) = provider::extract_url(message) {
                fs.push(text_field("url", u));
            }
            match body {
                Some(b) => fs.extend(body_fields(b, &[])),
                None => fs.push(text_field("message", message.clone())),
            }
            fs
        }
        ProviderErrorRecord::Truncated { reason } => vec![
            text_field("stop_reason", reason.clone()),
            text_field(
                "remedy",
                "raise `--max-tokens N` or split the turn into smaller writes",
            ),
        ],
        ProviderErrorRecord::Other { cause } => vec![text_field("cause", prettify(cause))],
    }
}

/// The rate-limit wait as an aligned field: a human duration plus a
/// [`size_bar`] of the seconds.
fn wait_field(secs: u64) -> FieldRow {
    FieldRow {
        label: "retry-after".into(),
        value: FieldValue::Inline(vec![
            Span::raw(format!("{}  ", crate::agent::resources::hms(secs, " "))),
            size_bar(u32::try_from(secs).unwrap_or(u32::MAX)),
        ]),
    }
}

/// The `error: <kind>` headline row, shared by every kind so the chrome never
/// drifts; the kind string is the only variation.
fn headline(kind: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "error: ",
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        ),
        bold(kind.into(), RED),
    ])
}

/// Short human label for the error headline.
fn error_kind(e: &ProviderErrorRecord) -> &'static str {
    match e {
        ProviderErrorRecord::Cancelled { .. } => "cancelled",
        ProviderErrorRecord::Transient { status, .. } => provider::transient_label(*status),
        ProviderErrorRecord::RateLimited { .. } => "rate limited",
        ProviderErrorRecord::Api { .. } => "api error",
        ProviderErrorRecord::Truncated { .. } => "truncated",
        ProviderErrorRecord::Other { .. } => "provider error",
    }
}

/// Append one labelled text field, wrapped to `width` so long URLs fold rather
/// than clip.  Continuations blank the label; `label_w` is the block-wide
/// width, passed in rather than measured here, so every field shares a column.
fn push_field(
    ls: &mut Vec<Line<'static>>,
    label: &str,
    value: &str,
    value_style: Style,
    label_w: usize,
    width: usize,
) {
    let body_w = width.saturating_sub(label_w).max(8);
    push_wrapped(ls, value, body_w, |chunk, first| {
        let lead = if first {
            field_label(label, label_w)
        } else {
            Span::raw(" ".repeat(label_w))
        };
        Line::from(vec![lead, Span::styled(chunk, value_style)])
    });
}

/// Owned-`String` form of [`prettify_embedded_json`].
fn prettify(s: &str) -> String {
    prettify_embedded_json(s).into_owned()
}

/// The retry-after wait carried by a parsed `body`: the first
/// [`RETRY_SECS_KEYS`] entry reading as a number.  Consulted only when the
/// response header did not already supply the wait.
#[allow(
    clippy::cast_possible_truncation,
    clippy::cast_sign_loss,
    reason = "float->int cast saturates: a negative or absurd retry-seconds pins to 0 / u64::MAX, both acceptable for a wait readout"
)]
fn wait_from_body(body: &Value) -> Option<u64> {
    let obj = provider::error_object(body)?;
    RETRY_SECS_KEYS.iter().find_map(|k| {
        obj.get(*k)
            .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
    })
}

/// One JSON value as the text a field row shows, syntax stripped.  `Null`
/// carries no action, so it renders as nothing and the field is dropped.
fn value_display(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(_) | Value::Number(_) => Some(v.to_string()),
        Value::Array(_) | Value::Object(_) => Some(serde_json::to_string(v).unwrap_or_default()),
    }
}

/// Flatten a parsed error `body` into ordered fields: `type`/`code` leads as
/// the machine-readable class, then the rest in the `Map`'s sorted order,
/// skipping nulls and anything already `consumed` by a dedicated field, with
/// `message` last because it is the one that wraps.
fn body_fields(body: &Value, consumed: &[&str]) -> Vec<FieldRow> {
    let Some(obj) = provider::error_object(body) else {
        return vec![];
    };
    let mut fs = Vec::new();
    if let Some(v) = obj
        .get("type")
        .or_else(|| obj.get("code"))
        .and_then(value_display)
    {
        fs.push(text_field("type", v));
    }
    for (k, v) in obj {
        if matches!(k.as_str(), "type" | "code" | "message") || consumed.contains(&k.as_str()) {
            continue;
        }
        if let Some(v) = value_display(v) {
            fs.push(text_field(k.clone(), v));
        }
    }
    if let Some(v) = obj.get("message").and_then(value_display) {
        fs.push(text_field("message", v));
    }
    fs
}

/// Re-indent the first embedded JSON object or array in `s`, leaving the
/// surrounding text intact: providers splice a single-line body into a
/// free-text `cause`, and pretty-printing turns that wall into a nested block
/// whose newlines the wrapper honours as hard breaks.
fn prettify_embedded_json(s: &str) -> Cow<'_, str> {
    let Some(start) = s.find(['{', '[']) else {
        return Cow::Borrowed(s);
    };
    let mut stream =
        serde_json::Deserializer::from_str(&s[start..]).into_iter::<serde_json::Value>();
    let Some(Ok(value)) = stream.next() else {
        return Cow::Borrowed(s);
    };
    let Ok(pretty) = serde_json::to_string_pretty(&value) else {
        return Cow::Borrowed(s);
    };
    let end = start + stream.byte_offset();
    Cow::Owned(format!("{}{}{}", &s[..start], pretty, &s[end..]))
}

/// Fold one logical line into visual rows no wider than `width`, word-aware and
/// style-preserving.  The builders already lay out within [`READ_W`], so a
/// terminal at least that wide gets the line straight back.
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
