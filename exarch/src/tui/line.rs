//! Line builders and their internal helpers.  Every function returns
//! `Vec<Line<'static>>` ready for the scrollback buffer.  The colour and
//! layout constants they draw from live in [`super::palette`].
//!
//! These builders are the rendering arm of the typed [`crate::bus::Event`]
//! dispatch — producers send semantic events through the channel and
//! the consumer ([`super::App::handle`]) calls into here to turn them
//! into `Line`s.

use super::highlight::highlight_ral;
use super::palette::{
    CODE_BG, CYAN, LIME, LIME_HOT, ORANGE, PROMPT_INK, RAIL_W, READ_W, RED, RED_HOT, SLATE,
};
use super::rail::is_rail_prefix;
use crate::agent::event::ProviderErrorRecord;
use crate::bus::card::{
    Card, Field as CardField, FieldVal, Hunk, Mark, Measure, Role, Row, Seg, Span as CardSpan,
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

/// True when every span in `l` is empty or whitespace-only — i.e. the
/// line carries no glyphs and reads as a vertical separator rather
/// than a row of content.  Shared with `md` (trailing-blank collapse)
/// and `viewport` (chrome-boundary dedup) so the predicate has one
/// definition across the TUI.
pub(super) fn is_blank(l: &Line<'_>) -> bool {
    l.spans.iter().all(|s| s.content.trim().is_empty())
}

/// `1` when `l`'s first span is a marginal rail glyph (the 2-col shape the
/// rail prepends), else `0` — the leading-span count the copy contract and
/// the line wrappers skip so the rail chrome never lands in extracted text.
pub(super) fn rail_skip(l: &Line<'_>) -> usize {
    usize::from(l.spans.first().is_some_and(|s| is_rail_prefix(&s.content)))
}

/// One scrollback line as the plain text a reader would copy: span
/// contents joined, with a leading rail glyph dropped.
pub(super) fn plain(line: &Line<'_>) -> String {
    let skip = rail_skip(line);
    line.spans[skip..]
        .iter()
        .map(|s| s.content.as_ref())
        .collect()
}

/// Width in cells of the header size-bar — the second ordered variable
/// (size) after the rail's value (lightness).  A bar of [`SIZE_BAR_W`]
/// cells, filled `█` / empty `░`, encodes `log2(magnitude)` so a
/// 500-line event fills it and a 2-line event barely shows.
const SIZE_BAR_W: usize = 8;

/// Bucket `magnitude` onto a `log2` scale, clamped to `0..=cap`: `0` reads
/// as step `0`, and the step climbs by one each time `magnitude` doubles,
/// pinning at `cap` once it would run past it.  Shared by [`size_cells`]
/// and [`spark_glyph`], and tracks the rail's own value-step
/// ([`super::rail::value_step`]), which buckets `log2` of the same count.
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

/// Map `magnitude` to a filled-cell count on a `log2` scale, clamped to
/// `0..=SIZE_BAR_W`: `0` reads empty, a 2-line event lights a cell or
/// two, a ~500-line event fills the bar.
fn size_cells(magnitude: u32) -> usize {
    log2_step(magnitude, SIZE_BAR_W)
}

/// The header size-bar span: [`SIZE_BAR_W`] cells, `█` for the filled
/// run and `░` for the remainder, styled [`SLATE`] so it reads as
/// decorative ink beside the path / summary rather than content.  A zero
/// magnitude renders an all-empty bar.
pub(super) fn size_bar(magnitude: u32) -> Span<'static> {
    Span::styled(size_bar_text(magnitude), Style::default().fg(SLATE))
}

pub(super) fn size_bar_text(magnitude: u32) -> String {
    let filled = size_cells(magnitude);
    "█"
        .repeat(filled)
        .chars()
        .chain("░".repeat(SIZE_BAR_W - filled).chars())
        .collect()
}

/// The eight partial-height block glyphs of a vertical sparkline, lowest
/// to highest.  One per call in a coalesced ral block: the glyph's fill
/// height encodes that call's result magnitude, so a run of calls reads as
/// a bar chart of how much each one moved.
const SPARK_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// The sparkline glyph for one call's `magnitude`, on the same `log2` scale
/// as [`size_cells`] but bucketed across the eight bar heights: `None` and a
/// zero-line result read as the shortest bar (a call still ran), a ~500-line
/// result as the tallest.  The eight steps give the sparkline finer
/// resolution than the rail's four [`super::rail::value_step`] buckets while
/// tracking the same scale.
pub(super) fn spark_glyph(magnitude: Option<u32>) -> char {
    SPARK_GLYPHS[log2_step(magnitude.unwrap_or(0), SPARK_GLYPHS.len() - 1)]
}

/// Width in cells of the header grain run — the patch's diff density
/// (Bertin's grain), reading *what kind* of change beside the size-bar's
/// *how much*.
const GRAIN_W: usize = 4;

/// The header grain span: a run of [`GRAIN_W`] braille cells whose density
/// encodes the ratio `a / (a + b)` on the ramp `⣿⣶⣤⣀` — `⣿` (full) is all
/// `a`, `⣀` (sparse) is all `b`. The ratio is bucketed into quartiles so
/// "mostly `a` / balanced / mostly `b`" reads pre-attentively: `≥0.75 →
/// ⣿`, `≥0.50 → ⣶`, `≥0.25 → ⣤`, else `⣀`. Styled [`SLATE`] to match the
/// size-bar — it is decorative ink, not a data colour that would collide
/// with a data hue (the `+`/`-` line colours, say). `a + b == 0` has no
/// balance to show and renders blank. Two call sites read this ramp: the
/// patch header's addition ratio `add / (add + del)`, and the thinking
/// header's deliberation ratio `think / (think + say)` — how dearly an
/// answer was bought.
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

/// One braille cell for a `0.0..=1.0` ratio on the `⣿⣶⣤⣀` density ramp,
/// bucketed into quartiles so the run reads pre-attentively: `≥0.75 → ⣿`,
/// `≥0.50 → ⣶`, `≥0.25 → ⣤`, else `⣀`.
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

/// The collapsed header for a thinking block: a blank separator then the
/// deliberation grain (think-vs-say ratio) beside a [`size_bar`] of the
/// reasoning's own magnitude — "how dearly bought" and "how much thinking".
/// The reasoning prose itself stays folded until the block is dialed.
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

/// Step separator: one blank line.  The step number itself is recorded
/// in `events.json` / `user.log` for greppability; in the live TUI the
/// boundary is conveyed by vertical whitespace alone.
pub(super) fn step(_n: usize) -> Vec<Line<'static>> {
    vec![Line::default()]
}

/// Scrollback echo of the user's submitted prompt — the human's turn, the
/// one party that is not an agent.  Its body is tinted the human's neutral
/// [`PROMPT_INK`] (cooler and dimmer than the machine's white prose), so the
/// turn reads as a quiet, cool island in the bright, chromatic machine
/// stream: agents own the matrix hues, the human owns the neutral tone.  The
/// flatten adds the full-width rule fence ([`prompt_fence`]) just above the
/// first row as the turn's opening seam, and the `❖` rides the rail there.
/// No background band — background is the machine's ([`CODE_BG`]); reverse
/// video stays reserved for an active selection alone
/// ([`super::App::paint_selection`]).  Flush-left at regular weight, every
/// line tinted alike.
pub(super) fn user_prompt(s: &str) -> Vec<Line<'static>> {
    let ink = Style::default().fg(PROMPT_INK);
    let mut ls: Vec<Line<'static>> = vec![Line::default()];
    ls.extend(
        s.lines()
            .map(|l| Line::from(Span::styled(l.to_string(), ink))),
    );
    ls
}

/// The human turn's fence: a full-width rule in [`PROMPT_INK`], painted by the
/// flatten just above a prompt's text.  A *boundary* drawn as a line (its
/// native implantation), not a region — so background stays free to mean
/// "machine".  Scales to any prompt length, since the rule is its own row.
pub(super) fn prompt_fence(width: u16) -> Line<'static> {
    Line::from(Span::styled(
        "─".repeat(width as usize),
        Style::default().fg(PROMPT_INK),
    ))
}

/// Tool-call header rows: the slate one-line `label`, wrapped under its own
/// first column.  This builder is rail-less, the disclosure triangle
/// (`▸`/`▽`) living in the lifted rail, prepended by
/// [`super::block::Block::render`].
/// `size` is the call's result magnitude (`text.lines().count()`),
/// rendered as a [`size_bar`] trailing the label's first row — the
/// collapsed header *is* the call's summary, so the bar is its readout.
/// `None` (no result yet, or the expanded / static headers) omits it.
fn tool_call_header(label: &str, size: Option<u32>, width: u16) -> Vec<Line<'static>> {
    let prefix_w = RAIL_W;
    // Reserve the size-bar's gutter (`  ` gap + the bar) so the label wraps
    // *before* it. The bar is the row's one quantitative readout (length =
    // magnitude); pinning it to a fixed right column is what makes magnitudes
    // comparable down the page, so it must never spill onto a wrapped row.
    let bar_w = if size.is_some() {
        UnicodeWidthStr::width("  ") + SIZE_BAR_W
    } else {
        0
    };
    let body_w = (width as usize).saturating_sub(prefix_w + bar_w).max(8);
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
            Line::from(vec![
                Span::raw(" ".repeat(prefix_w)),
                Span::styled(chunk, Style::default().fg(SLATE)),
            ])
        }
    });
    out
}
/// Clicking the row swaps this for [`tool_call_body`] (L2/L3).  `size` is
/// the call's result magnitude, rendered as the header size-bar.
pub(super) fn tool_call_collapsed(
    label: &str,
    size: Option<u32>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header(label, size, width));
    ls
}

/// The revealed tool-call views (L2/L3): the header, a blank, then `cmd`'s
/// source rows — all of them when `cap` is `None` (L3), or the first `cap`
/// source lines (L2).  Both the header and source body wrap before the
/// viewport edge: header continuations align under the label, and source
/// continuations align under the line's own opening indentation.
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
        push_code_row(&mut ls, line, width);
    }
    ls
}

/// Wash `row` with the background `bg`, preserving every span's foreground
/// and modifiers — the single place a background stratum is painted: the
/// recessed code panel, queued-prompt plane, and `/legend` swatches.
/// `fill_to` pads the row to that display width so the wash reads edge-to-edge
/// (a panel); `None` lets it hug the spans (a swatch).
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

/// Append one highlighted source row to an expanded tool call.  The visible
/// code block has a fixed two-column inset; the row composes that inset ahead
/// of the line's already-highlighted spans, folds to `width` — continuation
/// rows hang beneath the inset plus the source line's own leading whitespace,
/// so a long expression folds where its content began, not at column zero —
/// and washes each resulting row into the recessed [`CODE_BG`] panel, padded
/// uniform to `width` so the machine region reads as a clean rectangle.
fn push_code_row(ls: &mut Vec<Line<'static>>, line: Line<'static>, width: u16) {
    const CODE_INDENT: &str = "  ";
    let mut spans = vec![Span::raw(CODE_INDENT)];
    spans.extend(line.spans);
    for row in wrap_line(&Line::from(spans), width as usize) {
        ls.push(wash(row, CODE_BG, Some(width as usize)));
    }
}

/// A summary-less tool call rendered standalone — the per-block `user.log` tee
/// of a [`super::block::BlockKind::PlainTool`].
/// `cmd`'s first line is the label, any remainder follows 2-space indented;
/// the block wears the shut triangle `▸`.
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

/// The verb column of an act row, pinned to the longest verb
/// (`unschedule`) plus its separating space, so verbs align down the page
/// across separate blocks.  [`render_field_rows`] cannot supply this: it
/// derives its column from the row set it is handed, and an act block is a
/// single row.
pub(super) const ACT_VERB_W: usize = 11;
/// The subject column of an act row — an agent name or a schedule label,
/// truncated into the cell rather than allowed to shift the payload column,
/// since the alignment *is* the point.  Wide enough that a name never has to
/// be cut: a subject is an identity, and a cut identity names nothing.
pub(super) const ACT_SUBJECT_W: usize = 20;

/// A harness act's row: `verb`, `subject`, `payload`, in three columns whose
/// first two are pinned ([`ACT_VERB_W`], [`ACT_SUBJECT_W`]).  An act changes
/// the world outside the turn, so it carries no magnitude and wears no
/// size-bar — the three fields are the whole of what it has
/// ([[decisions/260720_harness-calls-are-acts]]).  A `failed` act tiers its
/// payload — the short refusal — hot, on the row that names the attempt; the
/// long form is the raise, and the raise is the model's.  `full` (L2/L3)
/// wraps the payload under its own column; reduced (L1), it truncates into
/// it with an `…`.  The `↗` / `◷` shape arrives via the lifted rail
/// ([`super::block::Block::render_with`]), so this builder is rail-less.
pub(super) fn act_row(
    verb: &str,
    subject: Option<&str>,
    payload: &str,
    failed: bool,
    width: u16,
    full: bool,
) -> Vec<Line<'static>> {
    let payload_w = (width as usize)
        .saturating_sub(RAIL_W + ACT_VERB_W + ACT_SUBJECT_W)
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
    // The subject cell is padded only when a payload follows it, so a
    // landed `cancel` copies as `cancel     hunter`, with no trailing run
    // of column padding.
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
                    Span::raw(" ".repeat(RAIL_W + ACT_VERB_W + ACT_SUBJECT_W)),
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

/// The one-line header for an async subagent's landed result: a leading
/// blank (like [`tool_call_collapsed`]), then the bold `name` (LIME, or
/// the error hue when `error` is set), a [`SLATE`]-dim ` {elapsed}s `
/// readout, a [`size_bar`] for the result `size` (lines of `text`), and an
/// error suffix when one applies.  The `↘` shape arrives via the lifted
/// rail ([`super::block::Block::render`], `Subagent` shape), so this
/// builder is rail-less.
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
    // The error / empty-output suffix beside the header.
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

/// Error line: the `╳` shape lives in the lifted rail (Error shape); the
/// content is a bold red `error <msg>`.
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

/// The body of a [`Mark::Diff`], graded by disclosure `level` and carrying
/// *no* leading blank — [`render_card`] owns the one blank that opens the
/// whole card.  L1 is the `▎ <path>` header alone; L2 adds the first hunk;
/// L3 unrolls every hunk.  Each hunk is a unified row list — context rows
/// (no sign), removed lines (red `-`), added lines (lime `+`) interleaved —
/// indented two columns and prefixed with a right-aligned [`SLATE`] line
/// number — removed rows keep their pre-edit numbers, added and context
/// rows take their post-edit ones; several hunks are elision-separated.
/// No rail glyph on the body, so a selection through the block copies as
/// plain text.  This is the densest Bertin object: size (the header
/// `size_bar`), grain (the addition-ratio `grain_run`), value (the rail
/// lightness), shape (`▎`).
fn diff_body(path: &str, hunks: &[Hunk], level: u8) -> Vec<Line<'static>> {
    match level {
        // L1, the floor: header only.
        1 => vec![patch_header(path, hunks)],
        // L2: header + the first hunk's located context and changes.
        2 => diff_capped(path, hunks, Some(1)),
        // L3: the full diff.
        _ => diff_capped(path, hunks, None),
    }
}

/// Count the rows across every hunk that satisfy `pred` — the addition /
/// deletion tallies the header's grain run reads.
#[allow(clippy::cast_possible_truncation, reason = "diff row count")]
fn count_rows(hunks: &[Hunk], pred: impl Fn(&Row) -> bool) -> u32 {
    hunks
        .iter()
        .flat_map(|h| h.rows.iter())
        .filter(|r| pred(r))
        .count() as u32
}

/// The `▎ <path>` diff header row: slate label, white path, the
/// `log2`-scaled [`size_bar`] and the addition-ratio [`grain_run`].
/// Shared by every disclosure level so the L1/L2/L3 headers never drift.
fn patch_header(path: &str, hunks: &[Hunk]) -> Line<'static> {
    Line::from(vec![
        Span::styled("diff", Style::default().fg(SLATE)),
        Span::raw("  "),
        Span::styled(path.to_string(), Style::default().fg(Color::White)),
        Span::raw("  "),
        size_bar(crate::bus::card::hunk_magnitude(hunks)),
        Span::raw("  "),
        grain_run(
            count_rows(hunks, |r| matches!(r, Row::Add(_))),
            count_rows(hunks, |r| matches!(r, Row::Del(_))),
        ),
    ])
}

/// Shared diff body: the header, then `cap` hunks (all when `None`),
/// elision-separated, numbered against one gutter sized for the whole
/// block so every row's text column lines up under the header.  No leading
/// blank — see [`diff_body`].
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
            ls.push(Line::from(vec![
                Span::raw("  "),
                Span::styled(format!("{:>gutter$} ", "⋮"), Style::default().fg(SLATE)),
            ]));
        }
        push_hunk(&mut ls, h, gutter);
    }
    ls
}

/// The largest line number [`patch`] will render for `h`, used to size the
/// gutter: walk the unified rows from `h.start`, advancing an old- and a
/// new-side counter the way [`push_hunk`] does, and take the largest number
/// any row is stamped with.
fn hunk_max_lineno(h: &Hunk) -> u32 {
    let (mut old, mut new) = (h.start, h.start);
    let mut max = h.start;
    for row in &h.rows {
        match row {
            Row::Context(_) => {
                max = max.max(new);
                old += 1;
                new += 1;
            }
            Row::Del(_) => {
                max = max.max(old);
                old += 1;
            }
            Row::Add(_) => {
                max = max.max(new);
                new += 1;
            }
        }
    }
    max
}

/// Render one hunk's unified rows into `ls`, walking an old- and a new-side
/// counter from `h.start`: a context row carries the new-side number (it
/// exists in both files), a deletion keeps its pre-edit (old) number, an
/// insertion takes its post-edit (new) number.  This is the numbering
/// invariant the diff shows — removed rows in red `-`, added rows in lime
/// `+`, context in slate, each row's *changed* words lifted brighter (see
/// [`push_gutter_row`]).
fn push_hunk(ls: &mut Vec<Line<'static>>, h: &Hunk, gutter: usize) {
    let (mut old, mut new) = (h.start, h.start);
    for row in &h.rows {
        match row {
            Row::Context(segs) => {
                push_gutter_row(ls, gutter, new, ' ', segs, SLATE, None);
                old += 1;
                new += 1;
            }
            Row::Del(segs) => {
                push_gutter_row(ls, gutter, old, '-', segs, RED_HOT, Some(RED_HOT));
                old += 1;
            }
            Row::Add(segs) => {
                push_gutter_row(ls, gutter, new, '+', segs, LIME_HOT, Some(LIME_HOT));
                new += 1;
            }
        }
    }
}

/// Append one diff row — a two-column indent, a right-aligned line number in
/// [`SLATE`], the `<sign> ` marker in the row's `base` hue, then the row's
/// segmented body — wrapping the body to [`READ_W`] so long source lines fold
/// onto continuation rows instead of clipping.  The number and sign sit on the
/// first wrapped row only; continuations blank both and align under the body
/// column.  An empty body still emits a bare marker row so the diff stays
/// faithful to the input.
///
/// `hot` is the inline-emphasis colour for a del/add (`None` for context): an
/// emphasised segment — the bit `similar` flagged as actually changed — is
/// painted bold in `hot`, the unchanged remainder dimmed in `base`, so the eye
/// lands on the edit within the line.
fn push_gutter_row(
    ls: &mut Vec<Line<'static>>,
    gutter: usize,
    lineno: u32,
    sign: char,
    segs: &[Seg],
    base: Color,
    hot: Option<Color>,
) {
    // Body width: readable width minus the 2-col indent, the gutter, its
    // trailing space, and the 2-col "<sign> " marker, floored so
    // pathological widths wrap.
    let body_w = (READ_W as usize).saturating_sub(2 + gutter + 1 + 2).max(8);
    // Each segment painted by its emphasis: a changed run bold in `hot`, the
    // rest dimmed in `base`; a context row (no `hot`) reads flat in `base`.
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
    // Word-wrap the styled body, then prepend the gutter to each wrapped row:
    // the real number + sign on the first, blanks on continuations.
    for (i, wrapped) in wrap_line(&Line::from(body), body_w).into_iter().enumerate() {
        let (num, marker) = if i == 0 {
            (format!("{lineno:>gutter$}"), format!("{sign} "))
        } else {
            (" ".repeat(gutter), "  ".to_string())
        };
        let mut spans = vec![
            Span::raw("  "),
            Span::styled(format!("{num} "), Style::default().fg(SLATE)),
            Span::styled(marker, Style::default().fg(base)),
        ];
        spans.extend(wrapped.spans);
        ls.push(Line::from(spans));
    }
}

// ── Card rendering ───────────────────────────────────────────────────────────

/// The one binding table: each nominal [`Role`] to the Bertin retinal
/// variable that carries identity — hue, plus a weight/texture shift for
/// `Strong`/`Code`.  This is the single place hue lives for kit *content*,
/// so the kit can name a role but never a colour, and magnitude can never
/// land on hue.  Themeable here, once.
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

/// The style of a span by its (optional) role: a roled span binds through
/// [`role_style`]; a roleless one — and the degradation target of an
/// unknown role — renders as plain content ink (white).
fn span_style(role: Option<Role>) -> Style {
    role.map_or_else(|| Style::default().fg(Color::White), role_style)
}

/// Render one mark at disclosure `level` — the one dispatch every `Mark`
/// variant routes through, shared by [`render_card`] (every mark, `diff`
/// included) and [`render_framed`] (every mark but `diff`, which it filters
/// before dispatch) so a new variant cannot be wired into one interpreter
/// and forgotten in the other.
fn render_mark(mark: &Mark, level: u8) -> Vec<Line<'static>> {
    match mark {
        Mark::Text { spans } => render_text(spans),
        Mark::Measure(m) => vec![render_measure(m)],
        Mark::Fields { rows } => render_fields(rows),
        Mark::Diff { path, hunks } => diff_body(path, hunks, level),
        Mark::Listing { bytes, more } => render_listing(bytes, *more),
        Mark::Raw { bytes } => render_raw(bytes),
    }
}

/// Render a [`Card`] — the one generic interpreter the `surface` builtin
/// feeds.  Opens with the single leading blank every block wears, then
/// renders each mark top-to-bottom; the `diff` mark alone honours the
/// disclosure `level` (the other marks are chrome-level and always render
/// full).  The data-encoding rail span is prepended later by
/// [`super::block::Block`] to the first content row.
pub(super) fn render_card(card: &Card, level: u8) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    for mark in card.marks() {
        ls.extend(render_mark(mark, level));
    }
    ls
}

/// Left indent of a framed surfaced card, in columns — pushed right of the
/// transcript so the card reads as a composed object, set apart from the flow.
const CARD_INDENT: usize = 4;

/// Render a surfaced general card (no `diff` mark) as a framed, indented
/// object: a neutral box, its heading set into the top rule, the remaining
/// marks padded inside.  A surfaced card is the model's deliberate "look at
/// this" — rare enough to earn the chrome, and so distinct from the calm human
/// band and the rail-glyph trace of incremental work.  `width` bounds the box;
/// the frame wears the neutral rail ink, since identity lives in the matrix.
pub(super) fn render_card_framed(card: &Card, width: u16) -> Vec<Line<'static>> {
    render_framed(
        card,
        CARD_INDENT,
        Style::default().fg(SLATE),
        width.min(READ_W),
        false,
    )
}

/// Register-card side margin, in columns.  The register owns the whole area
/// right of the transcript; the card itself breathes inside it.
const REGISTER_CARD_MARGIN: usize = 2;

/// Render a pinned register card: framed in its producing agent's `hue`, inset
/// inside the register column and filling that inset width.
/// The hue is the register's only departure from a surfaced card — identity
/// that the transcript reads from the matrix, a side column must carry itself.
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
    )
}

/// Core framed-card renderer shared by the transcript's surfaced cards and the
/// register's pins: a bordered box `indent_w` columns in, drawn in `border`
/// ink, content wrapped to a budget derived from `width` (the caller caps it —
/// the transcript at [`READ_W`], the register at its column width).
fn render_framed(
    card: &Card,
    indent_w: usize,
    border: Style,
    width: u16,
    fill: bool,
) -> Vec<Line<'static>> {
    let indent = " ".repeat(indent_w);
    // Inner content budget: the content column less the indent and the four
    // frame columns (`│ ` … ` │`).
    let max_inner = (width as usize).saturating_sub(indent_w + 4).max(8);

    // Lift a single-line leading heading into the top rule; everything else
    // renders inside.  A multi-line or non-text first mark leaves no title.
    // A title wider than the inner budget is truncated so the top rule can
    // never grow past the body rows on a narrow terminal.
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

    // Body marks → logical lines → wrapped to the inner budget.  A diff mark
    // is filtered out — diff-bearing cards take the diff path — so the
    // shared dispatch's level argument is never read here.
    let mut body: Vec<Line<'static>> = Vec::new();
    for mark in body_marks
        .iter()
        .filter(|m| !matches!(m, Mark::Diff { .. }))
    {
        body.extend(render_mark(mark, 0));
    }
    let wrapped: Vec<Line<'static>> = body.iter().flat_map(|l| wrap_line(l, max_inner)).collect();

    // Inner width: the widest row, and at least one column past the title so
    // the top rule's `╭─ title ─╮` always closes.  Capped at the budget.
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

    // Top rule, with the heading set into it.
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

    // Content rows, each padded out to the inner width inside the borders.
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

/// Assemble the register column: each pin's card framed in `hue`, stacked
/// top-down.  Each framed card leads with a blank row, so the stack carries its
/// own inter-card gutter; the slot keys are identity, not shown — a pinned card
/// carries its own label.
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

/// Truncate a styled span run to at most `max_w` display columns, appending
/// an `…` in the last column when content is dropped.  Keeps a lifted card
/// heading from overrunning the frame's top rule on a narrow terminal.
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

/// Render a `text` mark — a run of optionally-roled spans into one or more
/// `Line`s, breaking on embedded newlines so a multi-line span stays
/// faithful.  Width-folding happens later in [`wrap_line`], which
/// preserves each span's style.
fn render_text(spans: &[CardSpan]) -> Vec<Line<'static>> {
    fold_styled_lines(
        spans
            .iter()
            .map(|cs| (cs.text.clone(), span_style(cs.role))),
        true,
    )
}

/// Fold a stream of `(text, style)` fragments into `Line`s, splitting on
/// embedded `\n` — the one fold both [`render_text`] and
/// [`super::highlight::into_lines`] perform over their own fragment source (a
/// `text` mark's roled spans; a lexer's styled token stream).
/// `keep_trailing_blank` is their one semantic difference: set, a stream
/// that ends mid-line (or is empty) still closes with the line in progress
/// — [`render_text`]'s contract, so a `text` mark's trailing blank line
/// survives; cleared, it mirrors [`str::lines`] and drops that trailing
/// empty line — [`super::highlight::into_lines`]'s contract.
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

/// Render a `measure` mark as one line: the slate label, then the
/// quantitative readout + bar ([`measure_value_spans`]).
fn render_measure(m: &Measure) -> Line<'static> {
    let mut spans = vec![
        Span::styled(m.label.clone(), Style::default().fg(SLATE)),
        Span::raw("  "),
    ];
    spans.extend(measure_value_spans(m));
    Line::from(spans)
}

/// The quantitative value of a [`Measure`] — the readout then the bar,
/// without the measure's own label (a fields row supplies its own label
/// column).  A bounded measure (`max` present) reads as `value/max` with a
/// proportional fill bar; an unbounded one reads as `value[unit]` with a
/// `log2` [`size_bar`].
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

/// A proportional fill bar `██████░░░░` of `done/total` — 10 cells, lime
/// for the filled run and dim slate for the empty.  `total == 0` reads as
/// no progress (all empty) rather than a divide-by-zero.  The bounded
/// branch of [`measure_value_spans`].
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

/// Render a `fields` mark — Bertin's selective alignment: every value
/// lands in one shared label column.  Each row's value is either inline
/// roled text or a nested [`Measure`].  A single-span text value wraps
/// under the value column; a multi-span value or a measure renders inline.
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
    render_field_rows(&field_rows, READ_W as usize)
}

/// Render a `raw` mark — un-encoded ink appended verbatim, decoded lossily
/// as UTF-8 and split into rows.  Honest about being outside Bertin's
/// variables: it is an image, not an encoding, so it wears no role styling.
fn render_raw(bytes: &[u8]) -> Vec<Line<'static>> {
    String::from_utf8_lossy(bytes)
        .lines()
        .map(|l| Line::from(Span::raw(l.to_string())))
        .collect()
}

/// Render a [`Mark::Listing`] — the head of a freshly-written file as a numbered
/// source listing: each line ral-highlighted, fronted by a right-aligned
/// [`SLATE`] line-number gutter counting from 1, long lines folded to [`READ_W`]
/// with continuations hanging under the body column (the gutter blanked on
/// them).  When `more`, a trailing `⋮` gutter row marks content elided past the
/// preview cap — the same elision glyph [`diff_capped`] sets between hunks, so a
/// write and a diff share one vocabulary for "there is more below".  This is the
/// write card's body, kin to [`push_hunk`] minus the two-sided sign column: a
/// write is not a diff but a listing of what now stands in the file.
fn render_listing(bytes: &[u8], more: bool) -> Vec<Line<'static>> {
    let text = String::from_utf8_lossy(bytes);
    let rows = highlight_ral(&text);
    let gutter = rows.len().to_string().len().max(3);
    // Body width: readable width less the 2-col indent, the gutter, and its
    // trailing space; floored so pathological widths still wrap.
    let body_w = (READ_W as usize).saturating_sub(2 + gutter + 1).max(8);
    let mut ls: Vec<Line<'static>> = Vec::new();
    for (i, line) in rows.into_iter().enumerate() {
        for (j, wrapped) in wrap_line(&line, body_w).into_iter().enumerate() {
            let num = if j == 0 {
                format!("{:>gutter$}", i + 1)
            } else {
                " ".repeat(gutter)
            };
            let mut spans = vec![
                Span::raw("  "),
                Span::styled(format!("{num} "), Style::default().fg(SLATE)),
            ];
            spans.extend(wrapped.spans);
            ls.push(Line::from(spans));
        }
    }
    if more {
        ls.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{:>gutter$} ", "⋮"), Style::default().fg(SLATE)),
        ]));
    }
    ls
}

/// Slate text for system notes — operational metadata (model switches, stream stalls, compaction).
pub(super) fn note(s: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        s.to_string(),
        Style::default().fg(SLATE),
    ))]
}

/// Bracketed stop-reason notice (e.g. `[stop: content_filter]`).  Uses [`note`] styling.
/// model's normalised raw reason goes inside; render is the same
/// styling as [`note`].
pub(super) fn stop_reason(raw: &str) -> Vec<Line<'static>> {
    note(&format!("[stop: {raw}]"))
}

/// Spans for the permanent usage status bar
/// (`[46.6k in/459 out] · $0.1466`, or `[46.6k in/459 out/1.2k wr/3.4k rd] · $0.1466`
/// with cache).  Styles the pieces [`provider::Usage::parts`] yields.
/// (the plain [`provider::Usage`] `Display` uses a long-form log format).
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

/// Bold coloured span.
pub(super) fn bold(c: String, col: Color) -> Span<'static> {
    Span::styled(c, Style::default().fg(col).add_modifier(Modifier::BOLD))
}

/// Wrap `text` to `body_w` columns and push one [`Line`] per chunk into
/// `out`, building each from `row(chunk, first)` where `first` marks the
/// opening chunk.  [`textwrap::wrap`] always yields at least one chunk —
/// an empty input wraps to a single empty chunk — so a blank value still
/// renders its marker faithfully via `row("", true)`.  This is the one
/// wrap-and-emit discipline every chrome builder in this module shares.
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

/// A field value ahead of layout: text to wrap under the shared label
/// column, or a row of pre-styled spans rendered inline on the value row
/// (a measure's bar, a duration plus its `size_bar`).
enum FieldValue {
    Wrapped { text: String, style: Style },
    Inline(Vec<Span<'static>>),
}

/// One `(label, value)` row ahead of layout — the unit both the `fields`
/// mark and [`provider_error`] feed into [`render_field_rows`].
struct FieldRow {
    label: String,
    value: FieldValue,
}

/// A wrapped plain-text field row — the common case (a label and an
/// unstyled value), used pervasively by [`provider_error`].
fn text_field(label: impl Into<String>, value: impl Into<String>) -> FieldRow {
    FieldRow {
        label: label.into(),
        value: FieldValue::Wrapped {
            text: value.into(),
            style: Style::default(),
        },
    }
}

/// The slate-bold label lead for an aligned field row, left-padded into
/// the shared `label_w` column.  One definition so the `fields` mark and
/// `provider_error` size and colour their label column identically.
fn field_label(label: &str, label_w: usize) -> Span<'static> {
    Span::styled(
        format!("{label:<label_w$}"),
        Style::default().fg(SLATE).add_modifier(Modifier::BOLD),
    )
}

/// Render aligned `(label, value)` rows into one shared label column —
/// Bertin's selective alignment, the matrix primitive both the `fields`
/// mark ([`render_fields`]) and [`provider_error`] feed.  The column width
/// is the longest label plus its two-space gap, measured once so every
/// value starts in the same column.  `Wrapped` values fold under that
/// column; `Inline` values render their pre-styled spans on one row.
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

/// Align pre-styled `(label, sample)` rows into one shared label column —
/// the `/legend` panel's primitive.  The legend's samples are not data the
/// renderer styles (a [`FieldRow`] names a role and lets the kit stay
/// colour-blind); they are the literal styled output of the rail / bar /
/// grain builders, exhibited so the reader can decode the rail.  So this
/// takes already-styled spans straight through the shared alignment
/// ([`render_field_rows`]) rather than a [`Role`], the one place the TUI
/// shows appearance because appearance *is* the subject.
pub(super) fn legend_rows(rows: Vec<(&str, Vec<Span<'static>>)>) -> Vec<Line<'static>> {
    let rows: Vec<FieldRow> = rows
        .into_iter()
        .map(|(label, spans)| FieldRow {
            label: label.to_string(),
            value: FieldValue::Inline(spans),
        })
        .collect();
    render_field_rows(&rows, READ_W as usize)
}

// ── Provider-error rendering ────────────────────────────────────────────────

/// Body keys carrying the retry-after wait as a second count, in precedence
/// order — the readers [`wait_from_body`] consults, and (with the absolute
/// `resets_at` twin) the keys the rendered wait field then suppresses from
/// the body dump.
const RETRY_SECS_KEYS: &[&str] = &["resets_in_seconds", "retry_after_seconds", "retry_after"];

/// Render a [`ProviderErrorRecord`] as a structured multi-line block.
///
/// Header: blank line + bold-red `error: <kind>` (the `╳` shape lives in
/// the lifted rail, Error shape).  Body: an ordered field list rendered
/// into one shared, slate-bold label column with a single aligned value
/// column — JSON syntax stripped, null fields dropped.  When a parsed
/// `body` is present the fields come from it ([`body_fields`]); otherwise
/// the renderer falls back to the free-text `cause`/`message`, honestly
/// rendered rather than dressed as structure.  The rate-limit wait is the
/// one quantitative field, rendered as a human duration plus a [`size_bar`].
pub(super) fn provider_error(e: &ProviderErrorRecord) -> Vec<Line<'static>> {
    let mut ls: Vec<Line<'static>> = vec![Line::default()];
    // Cancellation folds its site into the headline and carries no body —
    // there is nothing to align, so it returns after the header alone.
    if let ProviderErrorRecord::Cancelled { where_ } = e {
        ls.push(headline(&format!("cancelled ({where_})")));
        return ls;
    }
    ls.push(headline(error_kind(e)));

    let fields: Vec<FieldRow> = match e {
        ProviderErrorRecord::Cancelled { .. } => unreachable!("handled above"),
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
                // Suppress the raw retry-after keys the wait field subsumes:
                // the second-count readers plus the absolute `resets_at` twin.
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
        } => {
            let mut fs = vec![text_field("attempts", attempts.to_string())];
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
            url,
            body,
        } => {
            let mut fs = Vec::new();
            if let Some(s) = status {
                fs.push(text_field("status", s.to_string()));
            }
            fs.push(text_field("model", model.clone()));
            if let Some(u) = url {
                fs.push(text_field("url", u.clone()));
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
    };

    ls.extend(render_field_rows(&fields, READ_W as usize));
    ls
}

/// The rate-limit wait as an aligned field: a human duration plus a
/// `size_bar` of the seconds — the one quantitative provider-error field,
/// so it earns the size channel the text fields don't.
fn wait_field(secs: u64) -> FieldRow {
    FieldRow {
        label: "retry-after".into(),
        value: FieldValue::Inline(vec![
            Span::raw(format!("{}  ", crate::agent::resources::hms(secs, " "))),
            size_bar(u32::try_from(secs).unwrap_or(u32::MAX)),
        ]),
    }
}

/// The `error: <kind>` headline row: a bold-red `error: ` lead-in then
/// the bold-red kind.  Shared by every block so the chrome never drifts —
/// the only variation is the kind string the caller folds in.
fn headline(kind: &str) -> Line<'static> {
    Line::from(vec![
        Span::styled(
            "error: ",
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        ),
        bold(kind.into(), RED),
    ])
}

/// Short human label for the error header line.
fn error_kind(e: &ProviderErrorRecord) -> &'static str {
    match e {
        ProviderErrorRecord::Cancelled { .. } => "cancelled",
        ProviderErrorRecord::Transient { .. } => "web stream failed",
        ProviderErrorRecord::RateLimited { .. } => "rate limited",
        ProviderErrorRecord::Api { .. } => "api error",
        ProviderErrorRecord::Truncated { .. } => "truncated",
        ProviderErrorRecord::Other { .. } => "provider error",
    }
}

/// Append one labelled text field as one-or-more flush-left Lines, its
/// value left-padded into the shared `label_w` column and styled with
/// `value_style`.
///
/// The first wrapped row carries the slate-bold [`field_label`] then the
/// value; continuation rows blank the label so the value column lines up
/// under itself.  `label_w` is the block-wide column width (the longest
/// label plus its two-space gap), passed in so every field aligns to the
/// same column rather than each measuring its own.  `value` is wrapped to
/// `width` columns via [`textwrap::wrap`] so long URLs and stack-like
/// strings fold instead of clipping.
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

/// Convenience over [`prettify_embedded_json`] that hands back an owned
/// `String`, for the few field-build sites that feed free-text `cause`
/// values into the structured field list.
fn prettify(s: &str) -> String {
    prettify_embedded_json(s).into_owned()
}

/// The retry-after wait carried by a parsed `body`, if any: the first of
/// the recognised second-count keys whose value reads as a number.  Used
/// only when the response header didn't already supply the wait.
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

/// One JSON value as the plain text a field row should show, with JSON
/// syntax stripped: strings unquoted, scalars stringified, and the rare
/// nested array/object compacted as a last resort.  `Null` carries no
/// action, so it renders as nothing and the field is dropped.
fn value_display(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(_) | Value::Number(_) => Some(v.to_string()),
        Value::Array(_) | Value::Object(_) => Some(serde_json::to_string(v).unwrap_or_default()),
    }
}

/// Flatten a parsed error `body` into ordered fields, JSON syntax stripped.
///
/// The error object's `type`/`code` leads (the machine-readable class),
/// then its remaining keys in the `Map`'s deterministic (sorted) order —
/// skipping the framed `type`/`code`/`message`, any key already `consumed`
/// by a dedicated field (e.g. the rate-limit wait), and null values — and
/// `message` trails last because it is the one field that wraps.  A body
/// with no error object yields no fields.
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

/// Reformat the first embedded JSON object/array in `s` with two-space
/// indentation, leaving the surrounding text intact.  Provider errors
/// often splice a raw, single-line JSON body into a free-text `cause`
/// (`… Body: {"error":{…}}`); pretty-printing it turns an unreadable wall
/// into a nested block whose newlines the wrapper honours as hard breaks.
/// Returns the input unchanged (borrowed) when no parseable JSON value is
/// found, so non-JSON fields pay only a scan for the first `{`/`[`.
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

/// Fold one logical line into visual rows no wider than `width`,
/// word-aware and preserving each span's style.  The line builders already
/// lay content out within [`READ_W`], so on a terminal at least that wide
/// this hands the line straight back; it only folds on a narrower one.
///
/// Continuations re-indent to the line's leading indentation — an optional
/// rail glyph ([`is_rail_prefix`], prepended by [`super::block::Block::render_with`])
/// plus any leading whitespace the builders inset content with — so a wrapped
/// prompt echo, code row, or io effect folds under its own indent rather than
/// sliding back to column zero.  A line with no leading indent wraps flush at
/// `0`.  The greedy placement breaks between words, dropping the inter-word
/// gap at the break; a single word wider than the body column is hard-broken
/// char-by-char.
pub(super) fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width {
        return vec![line.clone()];
    }
    // The hang column is the line's leading indentation: an optional rail
    // glyph (the first span, when it is one the rail prepends) followed by any
    // whitespace-only spans the builders inset with.  Carrying the indent into
    // the head — rather than leaving it in the body — is what keeps it on a
    // wrapped row 0: the body's leading whitespace would otherwise be dropped
    // as a row-leading gap.  The head spans ride row 0 verbatim (so the copy
    // contract still strips a leading rail glyph), and continuations re-indent
    // to their summed width.
    let spans = line.spans.as_slice();
    let mut head_len = rail_skip(line);
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
    // Row 0 opens carrying the head spans verbatim (the rail glyph and indent,
    // occupying columns `0..indent`); continuation rows are seeded with the
    // indent.
    let mut row: Vec<Span<'static>> = head.to_vec();
    let mut col = indent;
    // Whether a word has landed on the current row's body.  A pending gap
    // before the first body word of a row is leading whitespace and is
    // dropped; once a word lands the row may break and gaps become
    // inter-word separators.
    let mut started = false;
    // The whitespace pending between the last word and the next: carried as
    // style-runs so a styled gap survives, or dropped at a break / row start.
    let mut gap: Vec<(String, Style)> = Vec::new();
    let mut gap_w = 0;

    for (word, ww) in words(body) {
        // A whitespace run is held as the pending gap, never placed eagerly.
        if word.iter().all(|(s, _)| s.chars().all(char::is_whitespace)) {
            gap = word;
            gap_w = ww;
            continue;
        }
        // Break before a word that overflows once this row carries one; the
        // pending gap is dropped at the break.
        if started && col + gap_w + ww > width {
            rows.push(Line::from(std::mem::take(&mut row)));
            row = seed(indent);
            col = indent;
            started = false;
            gap.clear();
            gap_w = 0;
        }
        // Place the pending gap only between words on a started row; drop it
        // when it would lead the row.
        if started {
            for (s, style) in std::mem::take(&mut gap) {
                row.push(Span::styled(s, style));
            }
            col += gap_w;
        } else {
            gap.clear();
        }
        gap_w = 0;
        // Place the word, hard-breaking it char-by-char when it alone is
        // wider than the body column.
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

/// A fresh continuation row seeded with `indent` spaces, or empty when the
/// line wraps flush — the body column every wrapped row re-indents to.
fn seed(indent: usize) -> Vec<Span<'static>> {
    if indent == 0 {
        Vec::new()
    } else {
        vec![Span::raw(" ".repeat(indent))]
    }
}

/// Append `ch` to `row`, extending the trailing span when it shares `ch`'s
/// style so a word does not fragment into one span per character.
fn push_char(row: &mut Vec<Span<'static>>, ch: char, style: Style) {
    match row.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push(ch),
        _ => row.push(Span::styled(ch.to_string(), style)),
    }
}

/// Tokenise a span stream into maximal whitespace / non-whitespace runs,
/// paired with each run's display width.  A run carries its style-fragments
/// so a word that crosses a span seam (a style change mid-word) keeps each
/// fragment's [`Style`].  Mirrors `md`'s word/space split, but span-aware.
fn words(spans: &[Span<'static>]) -> Vec<(Vec<(String, Style)>, usize)> {
    let mut out: Vec<(Vec<(String, Style)>, usize)> = Vec::new();
    let mut run: Vec<(String, Style)> = Vec::new();
    let mut run_w = 0;
    // Whether the run accumulated so far is whitespace — `None` until the
    // first char fixes its kind.
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
mod tests {
    use super::*;

    fn vibes_card() -> Card {
        Card(vec![
            Mark::Text {
                spans: vec![CardSpan {
                    role: Some(Role::Strong),
                    text: "hello cutie".into(),
                }],
            },
            Mark::Measure(Measure {
                label: "vibes".into(),
                value: 5,
                max: Some(42),
                unit: None,
            }),
            Mark::Fields {
                rows: vec![CardField {
                    label: "mood".into(),
                    value: FieldVal::Inline(vec![CardSpan {
                        role: Some(Role::Ok),
                        text: "rainy".into(),
                    }]),
                }],
            },
        ])
    }

    fn text(l: &Line<'static>) -> String {
        l.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    /// A surfaced general card frames as a closed box: a `╭…╮` top rule with
    /// the heading lifted into it, `│`-flanked content, a `╰…╯` bottom, and
    /// every framed line the same display width so the right edge is flush.
    #[test]
    fn framed_card_is_a_closed_box() {
        let lines = render_card_framed(&vibes_card(), 80);
        let rows: Vec<&Line<'static>> = lines.iter().filter(|l| !is_blank(l)).collect();
        assert!(rows.len() >= 3, "top, content, bottom: got {}", rows.len());

        let top = text(rows[0]);
        let bottom = text(rows[rows.len() - 1]);
        assert!(top.contains('╭') && top.contains('╮'), "top rule: {top:?}");
        assert!(
            top.contains("hello cutie"),
            "heading set into the rule: {top:?}"
        );
        assert!(!top.contains("rainy"), "body stays inside, not in the rule");
        assert!(
            bottom.contains('╰') && bottom.contains('╯'),
            "bottom rule: {bottom:?}"
        );

        let w = span_run_width(&rows[0].spans);
        for r in &rows {
            assert_eq!(
                span_run_width(&r.spans),
                w,
                "ragged box edge: {:?}",
                text(r)
            );
        }
        for r in &rows[1..rows.len() - 1] {
            assert!(
                text(r).contains('│'),
                "content row not flanked: {:?}",
                text(r)
            );
        }
    }

    /// The box is bounded by `width`: a narrow terminal still closes flush.
    #[test]
    fn framed_card_fits_width() {
        let lines = render_card_framed(&vibes_card(), 30);
        for l in lines.iter().filter(|l| !is_blank(l)) {
            assert!(
                span_run_width(&l.spans) <= 30,
                "overflows width 30: {}",
                span_run_width(&l.spans)
            );
        }
    }

    fn indent_of(s: &str) -> usize {
        s.len() - s.trim_start().len()
    }

    /// A wrapped, indented, rail-less line (a source or io-effect row) keeps
    /// its indent on row 0 and hangs every continuation under it — the leading
    /// whitespace is the hang column, not a row-leading gap to drop.
    #[test]
    fn wrap_hangs_indented_line_under_its_indent() {
        let line = Line::from(vec![
            Span::raw("    "),
            Span::raw("let paper_candidates = filter re-match candidates over the files"),
        ]);
        let rows = wrap_line(&line, 24);
        assert!(rows.len() > 1, "expected a fold at width 24");
        for row in &rows {
            assert_eq!(
                indent_of(&text(row)),
                4,
                "row lost its indent: {:?}",
                text(row)
            );
        }
    }

    /// A rail-led line still hangs continuations two columns under the glyph,
    /// and the colour-styled glyph rides row 0 as its own span — what the copy
    /// contract ([`plain`]) keys off to strip the chrome.
    #[test]
    fn wrap_hangs_rail_led_line_under_the_glyph() {
        let rail = Span::styled("▸ ", Style::default().fg(ratatui::style::Color::Cyan));
        let line = Line::from(vec![
            rail,
            Span::raw("alpha beta gamma delta epsilon zeta eta theta iota"),
        ]);
        let rows = wrap_line(&line, 20);
        assert!(rows.len() > 1);
        assert!(is_rail_prefix(&rows[0].spans[0].content));
        for row in &rows[1..] {
            let t = text(row);
            assert_eq!(indent_of(&t), 2);
            assert!(!t.starts_with('▸'));
        }
    }

    /// A flush, unindented line wraps back to column zero — no spurious indent.
    #[test]
    fn wrap_keeps_flush_line_flush() {
        let line = Line::from(Span::raw(
            "one two three four five six seven eight nine ten",
        ));
        let rows = wrap_line(&line, 16);
        assert!(rows.len() > 1);
        for row in &rows {
            assert_eq!(indent_of(&text(row)), 0);
        }
    }
}
