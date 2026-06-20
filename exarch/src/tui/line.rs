//! Line builders and their internal helpers.  Every function returns
//! `Vec<Line<'static>>` ready for the scrollback buffer.  Color constants
//! and layout constants live here and are used by sibling modules.
//!
//! These builders are the rendering arm of the typed [`crate::bus::Event`]
//! dispatch — producers send semantic events through the channel and
//! the consumer ([`super::App::handle`]) calls into here to turn them
//! into `Line`s.

use crate::bus::{Hunk, Row};
use crate::card::{Card, Field as CardField, FieldVal, Mark, Measure, Role, Span as CardSpan};
use crate::event::ProviderErrorRecord;
use crate::provider;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
use serde_json::{Map, Value};
use std::borrow::Cow;
use unicode_width::UnicodeWidthStr;

// ── Color palette ────────────────────────────────────────────────────────────

/// Muted vaporwave palette for the per-step chrome — same hue
/// identities as the bright banner set, pulled toward dusty pastels so
/// the repeating chrome reads as accent rather than alarm.  The bright
/// counterparts (`BANNER_*`) are used only by the one-shot startup
/// banner + eagle so the splash carries the neon punch without
/// bleeding into the rest of the session.
pub(super) const PINK: Color = Color::Rgb(220, 140, 175);
pub(super) const CYAN: Color = Color::Rgb(135, 200, 215);
pub(super) const LIME: Color = Color::Rgb(165, 210, 155);
pub(super) const PURPLE: Color = Color::Rgb(175, 145, 210);
pub(super) const ORANGE: Color = Color::Rgb(215, 145, 115);
pub(super) const RED: Color = Color::Rgb(215, 110, 125);
pub(super) const SLATE: Color = Color::Rgb(140, 150, 170);
pub(super) const CODE_BG: Color = Color::Rgb(36, 38, 46);
/// Agent rail palette: one hue per producing agent, indexed by
/// [`super::block::AgentSlot`]. Root keeps [`CYAN`] — the existing rail
/// accent — so a root-only session is visually unchanged in hue. The
/// rail's value-step lightens a slot toward white with magnitude, so hue
/// stays the identity channel and value stays the magnitude channel.
pub(super) const AGENT_HUES: [Color; 6] = [CYAN, PINK, LIME, PURPLE, ORANGE, RED];

/// Saturated banner-only palette — restricted to the startup
/// banner + eagle so the splash reads as neon while the chrome below
/// stays muted.
pub(super) const BANNER_PINK: Color = Color::Rgb(255, 20, 147);
pub(super) const BANNER_CYAN: Color = Color::Rgb(0, 240, 255);
pub(super) const BANNER_LIME: Color = Color::Rgb(57, 255, 20);
pub(super) const BANNER_GOLD: Color = Color::Rgb(255, 191, 0);
pub(super) const BANNER_PURPLE: Color = Color::Rgb(191, 64, 255);
pub(super) const BANNER_ORANGE: Color = Color::Rgb(255, 95, 31);
pub(super) const BANNER_RED: Color = Color::Rgb(255, 50, 80);

// ── Layout constants ─────────────────────────────────────────────────────────

/// Maximum readable width in columns; markdown is wrapped to this.
pub(super) const READ_W: u16 = 100;

/// Rail accent glyph for chrome that owns its own marker (the pending-
/// prompt strip, which is not a [`super::block::Block`] and so does not
/// receive the lifted rail). Block content instead gets its rail from
/// [`super::rail::span`], prepended by [`super::block::Block::render`].
pub(super) const RAIL: &str = "❖ ";

/// Rail width in columns: one shape glyph plus one trailing space. Every
/// block's first content row carries a rail of this width; body rows do
/// not, so a selection through the block copies as plain text.
pub(super) const RAIL_W: usize = 2;

/// True when every span in `l` is empty or whitespace-only — i.e. the
/// line carries no glyphs and reads as a vertical separator rather
/// than a row of content.  Shared with `md` (trailing-blank collapse)
/// and `viewport` (chrome-boundary dedup) so the predicate has one
/// definition across the TUI.
pub(super) fn is_blank(l: &Line<'_>) -> bool {
    l.spans.iter().all(|s| s.content.trim().is_empty())
}

/// The full rail shape vocabulary: one glyph + space per block kind.
/// [`plain`] drops a leading span whose content matches one of these so
/// copied text carries the content, not the chrome glyph; [`super::block::wrap_line`]
/// reuses the set to detect a rail-led row and indent its continuations.
pub(super) const RAIL_GLYPHS: [&str; 7] = ["▎ ", "▸ ", "▾ ", "· ", "━ ", "✗ ", RAIL];

/// One scrollback line as the plain text a reader would copy: span
/// contents joined, with a leading rail glyph dropped.
pub(super) fn plain(line: &Line<'_>) -> String {
    let skip = line
        .spans
        .first()
        .is_some_and(|s| RAIL_GLYPHS.contains(&s.content.as_ref())) as usize;
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

/// Map `magnitude` to a filled-cell count on a `log2` scale, clamped to
/// `0..=SIZE_BAR_W`: `0` reads empty, a 2-line event lights a cell or
/// two, a ~500-line event fills the bar.  Tracks the rail's value-step,
/// which also buckets `log2` of the line count.
fn size_cells(magnitude: u32) -> usize {
    (((magnitude + 1) as f32).log2().round() as usize).min(SIZE_BAR_W)
}

/// The header size-bar span: [`SIZE_BAR_W`] cells, `█` for the filled
/// run and `░` for the remainder, styled [`SLATE`] so it reads as
/// decorative ink beside the path / summary rather than content.  A zero
/// magnitude renders an all-empty bar.
pub(super) fn size_bar(magnitude: u32) -> Span<'static> {
    let filled = size_cells(magnitude);
    let bar: String = "█"
        .repeat(filled)
        .chars()
        .chain("░".repeat(SIZE_BAR_W - filled).chars())
        .collect();
    Span::styled(bar, Style::default().fg(SLATE))
}

/// Width in cells of the header grain run — the patch's diff density
/// (Bertin's grain), reading *what kind* of change beside the size-bar's
/// *how much*.
const GRAIN_W: usize = 4;

/// The header grain span: a run of [`GRAIN_W`] braille cells whose
/// density encodes the addition ratio `add / (add + del)` on the ramp
/// `⣿⣶⣤⣀` — `⣿` (full) is all additions, `⣀` (sparse) is all deletions.
/// The ratio is bucketed into quartiles so "mostly additions / balanced
/// / mostly deletions" reads pre-attentively: `≥0.75 → ⣿`, `≥0.50 → ⣶`,
/// `≥0.25 → ⣤`, else `⣀`.  Styled [`SLATE`] to match the size-bar — it is
/// decorative ink, not a data colour that would collide with the `+`/`-`
/// line colours.  A patch with no changed lines (`add + del == 0`) has no
/// balance to show and renders blank.
pub(super) fn grain_run(add: u32, del: u32) -> Span<'static> {
    let total = add + del;
    let cell = if total == 0 {
        ' '
    } else {
        let ratio = add as f32 / total as f32;
        match (ratio * GRAIN_W as f32) as usize {
            3.. => '⣿',
            2 => '⣶',
            1 => '⣤',
            _ => '⣀',
        }
    };
    Span::styled(cell.to_string().repeat(GRAIN_W), Style::default().fg(SLATE))
}

// ── Public line builders ─────────────────────────────────────────────────────

/// Step separator: one blank line.  The step number itself is recorded
/// in `events.json` / `user.log` for greppability; in the live TUI the
/// boundary is conveyed by vertical whitespace alone.
pub(super) fn step(_n: usize) -> Vec<Line<'static>> {
    vec![Line::default()]
}

/// Scrollback echo of the user's submitted prompt. The typed text renders
/// in reverse video so the user's turn boundary is unmistakable against
/// the surrounding markdown/chrome; the `❖` glyph now arrives via the
/// lifted rail ([`super::block::Block::render`], Generic shape), so the
/// first line carries only the body and continuations indent two columns
/// to align under it.
pub(super) fn user_prompt(s: &str) -> Vec<Line<'static>> {
    let cont = Span::raw("  ");
    let body = Style::default().add_modifier(Modifier::REVERSED);
    let mut ls: Vec<Line<'static>> = vec![Line::default()];
    ls.extend(s.lines().enumerate().map(|(i, l)| {
        let body_span = Span::styled(l.to_string(), body);
        if i == 0 {
            Line::from(vec![body_span])
        } else {
            Line::from(vec![cont.clone(), body_span])
        }
    }));
    ls
}

/// The pending-prompt strip shown above the input while a turn runs:
/// each message the user submitted mid-turn, oldest first.  Pending
/// prompts use the same cyan rail and reverse-video body as committed
/// user prompts, so the strip reads as "this is your text waiting to be
/// sent" rather than separate status chrome.  Wrapped to `width`
/// columns; continuations indent under the text.  Capped at `max_rows`
/// total — a longer queue closes with a `⋯ (N more)` line so it can
/// never crowd the transcript off-screen.
pub(super) fn queued_prompt(
    messages: &[String],
    width: u16,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let body_w = (width as usize)
        .saturating_sub(UnicodeWidthStr::width(RAIL))
        .max(8);
    let rail = Style::default().fg(CYAN);
    let body = Style::default().add_modifier(Modifier::REVERSED);
    let more = Style::default().fg(SLATE).add_modifier(Modifier::ITALIC);
    let mut out: Vec<Line<'static>> = Vec::new();
    for msg in messages {
        // The rail keys off the message-level first chunk, not each
        // wrapped line's, so a multi-line message marks only its very
        // first row and indents every continuation under it.
        let first = std::cell::Cell::new(true);
        let lead = || {
            if first.replace(false) {
                Span::styled(RAIL, rail)
            } else {
                Span::raw("  ")
            }
        };
        for raw in msg.lines() {
            push_wrapped(&mut out, raw, body_w, |chunk, _first| {
                Line::from(vec![lead(), Span::styled(chunk, body)])
            });
        }
    }
    if out.len() > max_rows {
        let hidden = out.len() - (max_rows - 1);
        out.truncate(max_rows - 1);
        out.push(Line::from(Span::styled(format!("⋯ ({hidden} more)"), more)));
    }
    out
}

/// Tool-call header rows: the slate tool name then the white one-line
/// `label`. The disclosure triangle (`▸`/`▾`) lives in the lifted rail,
/// prepended by [`super::block::Block::render`], not here — so this
/// builder is rail-less. Long labels wrap under the label's own first
/// column (rail width + tool prefix), so the rail + tool prefix stays
/// visually fixed while the comment reads as a paragraph.
/// `size` is the call's result magnitude (`text.lines().count()`),
/// rendered as a [`size_bar`] trailing the label's first row — the
/// collapsed header *is* the call's summary, so the bar is its readout.
/// `None` (no result yet, or the expanded / static headers) omits it.
fn tool_call_header(label: &str, tool: &str, size: Option<u32>, width: u16) -> Vec<Line<'static>> {
    let prefix_w = RAIL_W + UnicodeWidthStr::width(tool) + UnicodeWidthStr::width("  ");
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
            let mut spans = vec![
                Span::styled(tool.to_string(), Style::default().fg(SLATE)),
                Span::raw("  "),
                Span::styled(chunk, Style::default().fg(Color::White)),
            ];
            if let Some(magnitude) = size {
                spans.push(Span::raw("  "));
                spans.push(size_bar(magnitude));
            }
            Line::from(spans)
        } else {
            Line::from(vec![
                Span::raw(" ".repeat(prefix_w)),
                Span::styled(chunk, Style::default().fg(Color::White)),
            ])
        }
    });
    out
}
/// Clicking the row swaps this for [`tool_call_expanded`].  `size` is the
/// call's result magnitude, rendered as the header size-bar.
pub(super) fn tool_call_collapsed(
    label: &str,
    tool: &str,
    size: Option<u32>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header(label, tool, size, width));
    ls
}

/// Expanded tool call (L3): the `▾` header followed by the full ral `cmd`.
/// Both the header comment and source body wrap before the viewport edge:
/// header continuations align under the comment, and source continuations
/// align under the line's own opening indentation.
pub(super) fn tool_call_expanded(
    label: &str,
    tool: &str,
    cmd: &str,
    width: u16,
) -> Vec<Line<'static>> {
    tool_call_body(label, tool, cmd, None, width)
}

/// Tool call with context (L2): the header followed by the first `n`
/// source lines of `cmd`.  The same layout as [`tool_call_expanded`],
/// only the script is capped to `n` rows so the call reveals its head
/// without unrolling a long block.
pub(super) fn tool_call_context(
    label: &str,
    tool: &str,
    cmd: &str,
    n: usize,
    width: u16,
) -> Vec<Line<'static>> {
    tool_call_body(label, tool, cmd, Some(n), width)
}

/// Shared body for the revealed tool-call views: the header, a blank, then
/// `cmd`'s source rows — all of them when `cap` is `None` (L3), or the
/// first `cap` source lines (L2).
fn tool_call_body(
    label: &str,
    tool: &str,
    cmd: &str,
    cap: Option<usize>,
    width: u16,
) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header(label, tool, None, width));
    ls.push(Line::default());
    let take = cap.unwrap_or(usize::MAX);
    for l in cmd.lines().take(take) {
        push_code_row(&mut ls, l, width);
    }
    ls
}

/// Append one source row to an expanded tool call.  The visible code block
/// has a fixed two-column inset; wrapped continuation rows then repeat the
/// source line's own leading whitespace so a long expression folds beneath
/// the place where its content began, not back at column zero.
fn push_code_row(ls: &mut Vec<Line<'static>>, line: &str, width: u16) {
    const CODE_INDENT: &str = "  ";
    let body_start = line
        .char_indices()
        .find_map(|(i, c)| (!c.is_whitespace()).then_some(i))
        .unwrap_or(line.len());
    let source_indent = &line[..body_start];
    let body = &line[body_start..];
    let prefix = format!("{CODE_INDENT}{source_indent}");
    let prefix_w = UnicodeWidthStr::width(prefix.as_str());
    let body_w = (width as usize).saturating_sub(prefix_w).max(8);
    let code_style = Style::default().fg(Color::White).bg(CODE_BG);
    let indent_style = Style::default().bg(CODE_BG);
    push_wrapped(ls, body, body_w, |chunk, _first| {
        Line::from(vec![
            Span::styled(prefix.clone(), indent_style),
            Span::styled(chunk, code_style),
        ])
    });
}

/// A tool call with no separate summary — the `fff` query, an
/// invalid-input header.  There is nothing to reveal, so it carries the
/// static `❖` rail rather than a disclosure triangle: `cmd`'s first line
/// is the label, any remainder follows 2-space indented.
pub(super) fn tool_call_static(cmd: &str, tool: &str) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header(
        cmd.lines().next().unwrap_or(""),
        tool,
        None,
        READ_W,
    ));
    for l in cmd.lines().skip(1) {
        ls.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(l.to_string(), Style::default().fg(Color::White)),
        ]));
    }
    ls
}

/// Error line: the `✗` shape lives in the lifted rail (Error shape); the
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
        // L1: header only.  (L0 is the rail glyph alone, handled by the block.)
        0 | 1 => vec![patch_header(path, hunks)],
        // L2: header + the first hunk's located context and changes.
        2 => diff_capped(path, hunks, Some(1)),
        // L3: the full diff.
        _ => diff_capped(path, hunks, None),
    }
}

/// Count the rows across every hunk that satisfy `pred` — the addition /
/// deletion tallies the header's grain run reads.
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
        size_bar(crate::card::hunk_magnitude(hunks)),
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
                Span::styled(format!("{:>gutter$} ", "⋯"), Style::default().fg(SLATE)),
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
/// `+`, context in slate.
fn push_hunk(ls: &mut Vec<Line<'static>>, h: &Hunk, gutter: usize) {
    let (mut old, mut new) = (h.start, h.start);
    for row in &h.rows {
        match row {
            Row::Context(line) => {
                push_gutter_row(ls, gutter, new, ' ', line, SLATE);
                old += 1;
                new += 1;
            }
            Row::Del(line) => {
                push_gutter_row(ls, gutter, old, '-', line, RED);
                old += 1;
            }
            Row::Add(line) => {
                push_gutter_row(ls, gutter, new, '+', line, LIME);
                new += 1;
            }
        }
    }
}

/// Append one diff row — a two-column indent, a right-aligned line number
/// in [`SLATE`], then a `<sign> text` body in `color` — wrapping the text
/// to [`READ_W`] so long source lines fold onto continuation rows instead
/// of clipping.  The number and sign sit on the first wrapped chunk only;
/// continuation chunks blank both and align under the text column.  An
/// empty `line` still emits a bare marker row so the diff stays faithful
/// to the input.
fn push_gutter_row(
    ls: &mut Vec<Line<'static>>,
    gutter: usize,
    lineno: u32,
    sign: char,
    line: &str,
    color: Color,
) {
    // Body width: readable width minus the 2-col indent, the gutter, its
    // trailing space, and the 2-col "<sign> " marker, floored so
    // pathological widths wrap.
    let body_w = (READ_W as usize).saturating_sub(2 + gutter + 1 + 2).max(8);
    push_wrapped(ls, line, body_w, |chunk, first| {
        let (num, marker) = if first {
            (format!("{lineno:>gutter$}"), format!("{sign} "))
        } else {
            (" ".repeat(gutter), "  ".to_string())
        };
        Line::from(vec![
            Span::raw("  "),
            Span::styled(format!("{num} "), Style::default().fg(SLATE)),
            Span::styled(format!("{marker}{chunk}"), Style::default().fg(color)),
        ])
    });
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
        Role::Code => Style::default().fg(Color::White).bg(CODE_BG),
        Role::Ok => Style::default().fg(LIME).add_modifier(Modifier::BOLD),
        Role::Warn => Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        Role::Bad => Style::default().fg(RED).add_modifier(Modifier::BOLD),
        Role::Muted => Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        Role::Strong => Style::default().fg(Color::White).add_modifier(Modifier::BOLD),
    }
}

/// The style of a span by its (optional) role: a roled span binds through
/// [`role_style`]; a roleless one — and the degradation target of an
/// unknown role — renders as plain content ink (white).
fn span_style(role: Option<Role>) -> Style {
    role.map(role_style)
        .unwrap_or_else(|| Style::default().fg(Color::White))
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
        match mark {
            Mark::Text { spans } => ls.extend(render_text(spans)),
            Mark::Measure(m) => ls.push(render_measure(m)),
            Mark::Fields { rows } => ls.extend(render_fields(rows)),
            Mark::Diff { path, hunks } => ls.extend(diff_body(path, hunks, level)),
            Mark::Raw { bytes } => ls.extend(render_raw(bytes)),
        }
    }
    ls
}

/// Render a `text` mark — a run of optionally-roled spans into one or more
/// `Line`s, breaking on embedded newlines so a multi-line span stays
/// faithful.  Width-folding happens later in `block::wrap_line`, which
/// preserves each span's style.
fn render_text(spans: &[CardSpan]) -> Vec<Line<'static>> {
    let mut lines: Vec<Line<'static>> = Vec::new();
    let mut cur: Vec<Span<'static>> = Vec::new();
    for cs in spans {
        let style = span_style(cs.role);
        let mut parts = cs.text.split('\n');
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
    lines.push(Line::from(cur));
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
/// proportional fill bar (subsuming the old progress meter); an unbounded
/// one reads as `value[unit]` with a `log2` [`size_bar`].
fn measure_value_spans(m: &Measure) -> Vec<Span<'static>> {
    let white = Style::default().fg(Color::White);
    match m.max {
        Some(max) => {
            let mut spans = vec![
                Span::styled(format!("{}/{}", m.value, max), white),
                Span::raw("  "),
            ];
            spans.extend(progress_bar(m.value, max));
            spans
        }
        None => {
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
}

/// A proportional fill bar `██████░░░░` of `done/total` — 10 cells, lime
/// for the filled run and dim slate for the empty.  `total == 0` reads as
/// no progress (all empty) rather than a divide-by-zero.  The bounded
/// branch of [`measure_value_spans`]; subsumes the old `meter`.
fn progress_bar(done: u32, total: u32) -> Vec<Span<'static>> {
    const W: u32 = 10;
    let filled = if total == 0 {
        0
    } else {
        ((done as u64 * W as u64) / total as u64).min(W as u64) as u32
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

/// Dim slate text — used for informational messages.
pub(super) fn dim(s: &str) -> Vec<Line<'static>> {
    vec![Line::from(Span::styled(
        s.to_string(),
        Style::default().fg(SLATE).add_modifier(Modifier::DIM),
    ))]
}

/// Bracketed stop-reason notice (e.g. `[stop: content_filter]`).  The
/// model's normalised raw reason goes inside; render is the same
/// styling as [`dim`].
pub(super) fn stop_reason(raw: &str) -> Vec<Line<'static>> {
    dim(&format!("[stop: {raw}]"))
}

/// Spans for the permanent usage status bar
/// (`total 46.6k in / 459 out · $0.1466`).  Styles the pieces
/// [`provider::Usage::parts`] yields — the one renderer the plain
/// [`provider::Usage`] `Display` shares — so the chrome and the logs never
/// disagree on what a turn cost (X9).
pub(super) fn usage_text(usage: &provider::Usage) -> Vec<Span<'static>> {
    let p = usage.parts();
    let s = |b: &str| Span::styled(b.to_string(), Style::default().fg(SLATE));
    let n = |b: String, c: Color| Span::styled(b, Style::default().fg(c));
    let db =
        |b: String, c: Color| Span::styled(b, Style::default().fg(c).add_modifier(Modifier::BOLD));
    let mut sp = vec![
        s("total "),
        db(p.input, LIME),
        s(" in / "),
        db(p.output, LIME),
        s(" out"),
    ];
    if let Some((wr, rd)) = p.cache {
        sp.extend([s(" ["), n(wr, LIME), s(" wr/"), n(rd, LIME), s(" rd]")]);
    }
    sp.extend([s(" · "), db(p.cost, LIME)]);
    sp
}

// ── Internal helpers ─────────────────────────────────────────────────────────

/// Bold coloured span.
pub(super) fn bold(c: String, col: Color) -> Span<'static> {
    Span::styled(c, Style::default().fg(col).add_modifier(Modifier::BOLD))
}

/// Static slate-coloured span.
pub(super) fn slate(s: &'static str) -> Span<'static> {
    Span::styled(s, Style::default().fg(SLATE))
}

/// Slate-coloured span over an owned string (banner-side dynamic
/// labels like `canonical_slug`).
pub(super) fn slate_owned(s: String) -> Span<'static> {
    Span::styled(s, Style::default().fg(SLATE))
}

/// Wrap `text` to `body_w` columns and push one [`Line`] per chunk into
/// `out`, building each from `row(chunk, first)` where `first` marks the
/// opening chunk.  [`textwrap::wrap`] always yields at least one chunk —
/// an empty input wraps to a single empty chunk — so a blank value still
/// renders its marker faithfully via `row("", true)`.  This is the one
/// wrap-and-emit discipline every chrome builder in this module shares.
fn push_wrapped(
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
                push_field(&mut ls, &r.label, text, *style, label_w, width)
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

// ── Provider-error rendering ────────────────────────────────────────────────

/// Body keys subsumed by the rendered retry-after row, so a body
/// dump doesn't also print the raw retry-after value the wait field
/// already shows as a human duration + bar.
const WAIT_KEYS: &[&str] = &[
    "resets_in_seconds",
    "resets_at",
    "retry_after",
    "retry_after_seconds",
];

/// Render a [`ProviderErrorRecord`] as a structured multi-line block.
///
/// Header: blank line + bold-red `error: <kind>` (the `✗` shape lives in
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
                Some(b) => fs.extend(body_fields(b, WAIT_KEYS)),
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
            Span::raw(format!("{}  ", human_secs(secs))),
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

/// Format `s` seconds as a compact human duration: `12s`, `12m 46s`, or
/// `2h 05m`.  The rate-limit wait is the one quantity a reader acts on,
/// so it reads as a duration rather than a raw second count.
fn human_secs(s: u64) -> String {
    if s < 60 {
        format!("{s}s")
    } else if s < 3600 {
        format!("{}m {:02}s", s / 60, s % 60)
    } else {
        format!("{}h {:02}m", s / 3600, (s % 3600) / 60)
    }
}

/// The error payload object inside a parsed `body`.  Providers wrap
/// differently — OpenAI nests the detail under `error`, Anthropic sends
/// `{"type":"error","error":{…}}` — so prefer the inner `error` object,
/// falling back to the body itself when there is no such nesting.
fn error_object(body: &Value) -> Option<&Map<String, Value>> {
    body.get("error")
        .and_then(Value::as_object)
        .or_else(|| body.as_object())
}

/// The retry-after wait carried by a parsed `body`, if any: the first of
/// the recognised second-count keys whose value reads as a number.  Used
/// only when the response header didn't already supply the wait.
fn wait_from_body(body: &Value) -> Option<u64> {
    let obj = error_object(body)?;
    ["resets_in_seconds", "retry_after_seconds", "retry_after"]
        .iter()
        .find_map(|k| {
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
    let Some(obj) = error_object(body) else {
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

#[cfg(test)]
mod tests {
    use super::*;
    use unicode_width::UnicodeWidthStr;

    /// A hunk renders a numbered gutter: leading context, deletions,
    /// additions, then trailing context.  Removed rows keep their
    /// pre-edit numbers; additions and trailing context take their
    /// post-edit ones.  Replacing two lines (10–11) with one shifts the
    /// line that was at 12 down to 11 — so the trailing-context row must
    /// read `11`, not `12`, which pins the renumbering.
    #[test]
    fn patch_numbers_gutter_and_renumbers_after_edit() {
        let h = Hunk {
            start: 8,
            rows: vec![
                Row::Context("ctx8".into()),
                Row::Context("ctx9".into()),
                Row::Del("old10".into()),
                Row::Del("old11".into()),
                Row::Add("new10".into()),
                Row::Context("ctx11".into()),
                Row::Context("ctx12".into()),
            ],
        };
        let rows: Vec<String> = diff_body("src/foo.rs", &[h], 3).iter().map(plain).collect();
        let find = |needle: &str| {
            rows.iter()
                .find(|r| r.contains(needle))
                .unwrap_or_else(|| panic!("missing `{needle}` row in {rows:?}"))
                .clone()
        };
        // Leading context numbered just above the change.
        assert!(find("ctx8").contains('8'));
        assert!(find("ctx9").contains('9'));
        // Deletions keep their pre-edit numbers; additions take the
        // post-edit one (here both happen to be 10).
        assert!(find("- old10").contains("10"));
        assert!(find("- old11").contains("11"));
        assert!(find("+ new10").contains("10"));
        // The line that sat at 12 is renumbered to 11 in the new file.
        let after_first = find("ctx11");
        assert!(
            after_first.contains("11") && !after_first.contains("12"),
            "trailing context must renumber 12→11; got {after_first:?}"
        );
        assert!(find("ctx12").contains("12"));
        // Every hunk row sits two columns in, so the diff reads as indented
        // under the `▎ diff` header.
        for needle in ["ctx8", "ctx9", "old10", "old11", "new10", "ctx11", "ctx12"] {
            assert!(
                find(needle).starts_with("  "),
                "hunk row must indent two columns; got {:?}",
                find(needle)
            );
        }
    }

    /// The patch header carries a `log2`-scaled size-bar after the path:
    /// a large patch fills more cells than a small one, and the bar is
    /// always [`SIZE_BAR_W`] cells wide (filled `█` + empty `░`).  The
    /// bar is decorative — it must not perturb the numbered diff body.
    #[test]
    fn patch_header_size_bar_scales_with_magnitude() {
        let hunk = |del: usize, add: usize| Hunk {
            start: 1,
            rows: std::iter::repeat_n(Row::Del("x".to_string()), del)
                .chain(std::iter::repeat_n(Row::Add("y".to_string()), add))
                .collect(),
        };
        let bar = |hunks: &[Hunk]| -> String {
            // `diff_body` carries no leading blank, so the header is the
            // first row; the size-bar is its trailing `█`/`░` run.
            diff_body("src/foo.rs", hunks, 3)[0]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .chars()
                .filter(|c| *c == '█' || *c == '░')
                .collect()
        };
        let small = bar(&[hunk(1, 1)]); // magnitude 2
        let large = bar(&[hunk(250, 250)]); // magnitude 500
        assert_eq!(small.chars().count(), SIZE_BAR_W, "bar is fixed width");
        assert_eq!(large.chars().count(), SIZE_BAR_W, "bar is fixed width");
        let fill = |b: &str| b.chars().filter(|c| *c == '█').count();
        assert!(
            fill(&large) > fill(&small),
            "large patch fills more cells: {} vs {}",
            fill(&large),
            fill(&small),
        );
        assert_eq!(fill(&large), SIZE_BAR_W, "a 500-line patch fills the bar");
    }

    /// The patch header carries a grain run after the size-bar whose
    /// braille density encodes the addition ratio: a mostly-additions
    /// patch reads fuller (`⣿`) than a balanced one (`⣶`), which reads
    /// fuller than a mostly-deletions patch (`⣀`).
    #[test]
    fn patch_header_grain_tracks_addition_ratio() {
        let hunk = |del: usize, add: usize| Hunk {
            start: 1,
            rows: std::iter::repeat_n(Row::Del("x".to_string()), del)
                .chain(std::iter::repeat_n(Row::Add("y".to_string()), add))
                .collect(),
        };
        let grain = |hunks: &[Hunk]| -> char {
            diff_body("src/foo.rs", hunks, 3)[0]
                .spans
                .iter()
                .map(|s| s.content.as_ref())
                .collect::<String>()
                .chars()
                .find(|c| "⣿⣶⣤⣀".contains(*c))
                .expect("header must carry a grain cell")
        };
        assert_eq!(grain(&[hunk(0, 10)]), '⣿', "all additions reads full");
        assert_eq!(grain(&[hunk(5, 5)]), '⣶', "a balanced patch reads middle");
        assert_eq!(grain(&[hunk(10, 0)]), '⣀', "all deletions reads sparse");
        let denser = "⣿⣶⣤⣀";
        let pos = |c: char| denser.find(c).unwrap();
        assert!(
            pos(grain(&[hunk(0, 10)])) < pos(grain(&[hunk(5, 5)])),
            "more additions reads denser",
        );
        assert!(
            pos(grain(&[hunk(5, 5)])) < pos(grain(&[hunk(10, 0)])),
            "more deletions reads sparser",
        );
    }

    /// A 400-character `cause` must wrap into many rows and every row
    /// must fit inside `READ_W`.  Without wrapping the error would clip
    /// at the viewport edge — exactly the bug this renderer exists to
    /// fix.
    #[test]
    fn provider_error_wraps_long_cause() {
        let e = ProviderErrorRecord::Transient {
            cause: "x".repeat(400),
            attempts: 3,
            body: None,
        };
        let ls = provider_error(&e);
        assert!(ls.len() >= 5, "expected at least 5 lines, got {}", ls.len());
        for l in &ls {
            let w: usize = l
                .spans
                .iter()
                .map(|s| UnicodeWidthStr::width(s.content.as_ref()))
                .sum();
            assert!(
                w <= READ_W as usize,
                "line width {w} exceeds READ_W={}",
                READ_W
            );
        }
    }

    /// The queued strip uses the same rail and body styling as a sent user
    /// prompt while still wrapping and indenting continuations in-place.
    #[test]
    fn queued_prompt_marks_each_message_and_wraps() {
        let ls = queued_prompt(&["alpha".into(), "x".repeat(60)], 40, 20);
        let rows: Vec<String> = ls.iter().map(plain).collect();
        assert_eq!(ls[0].spans[0].content.as_ref(), RAIL);
        assert_eq!(rows[0], "alpha", "first message body: {rows:?}");
        // The 60-char second message wraps under a 40-col width, so it
        // spans more than its own first row: a continuation indents.
        assert!(rows.len() > 2, "second message must wrap: {rows:?}");
        assert_eq!(ls[1].spans[0].content.as_ref(), RAIL);
        assert!(rows[1].starts_with('x'), "second message body: {rows:?}");
        assert!(rows[2].starts_with("  "), "continuation indents: {rows:?}");
    }

    /// Past `max_rows` the strip closes with a `⋯ (N more)` line so a
    /// long queue can never grow without bound and crowd the transcript.
    #[test]
    fn queued_prompt_truncates_with_remainder() {
        let msgs: Vec<String> = (0..10).map(|i| format!("msg{i}")).collect();
        let ls = queued_prompt(&msgs, 40, 3);
        let rows: Vec<String> = ls.iter().map(plain).collect();
        assert_eq!(rows.len(), 3, "capped at max_rows: {rows:?}");
        // 10 messages, 2 shown, 8 folded into the remainder line.
        assert!(rows[2].contains("8 more"), "remainder count: {rows:?}");
    }

    /// The Bertin binding holds in both directions and never transposes: a
    /// nominal role binds identity to a hue (a `text` span), a `measure`
    /// binds magnitude to size (a fuller bar for a larger value), and a
    /// roled text span carries no size glyph.  The kit names a role or a
    /// magnitude; the renderer owns the one binding table.
    #[test]
    fn card_binds_identity_to_hue_and_magnitude_to_size() {
        let text_card = |role: Role, text: &str| {
            Card(vec![Mark::Text {
                spans: vec![CardSpan {
                    role: Some(role),
                    text: text.into(),
                }],
            }])
        };
        let fg_of = |ls: &[Line<'static>], needle: &str| {
            ls.iter()
                .flat_map(|l| &l.spans)
                .find(|s| s.content.contains(needle))
                .and_then(|s| s.style.fg)
        };
        let ok = render_card(&text_card(Role::Ok, "done"), 3);
        assert_eq!(fg_of(&ok, "done"), Some(LIME), "an `ok` role binds to lime");
        let bad = render_card(&text_card(Role::Bad, "boom"), 3);
        assert_eq!(fg_of(&bad, "boom"), Some(RED), "a `bad` role is a distinct hue");
        // Identity is the hue channel only — a roled text span never grows
        // a size bar (`█`/`░`), which is the measure/diff channel.
        assert!(
            !ok.iter()
                .flat_map(|l| &l.spans)
                .any(|s| s.content.contains('█') || s.content.contains('░')),
            "a text role must not render a magnitude bar"
        );

        // A larger bounded `measure` fills more cells than a smaller one —
        // magnitude on size, comparable across measures.
        let fill = |done: u32, total: u32| {
            let card = Card(vec![Mark::Measure(Measure {
                label: "m".into(),
                value: done,
                max: Some(total),
                unit: None,
            })]);
            render_card(&card, 3)
                .iter()
                .flat_map(|l| &l.spans)
                .flat_map(|s| s.content.chars())
                .filter(|c| *c == '█')
                .count()
        };
        assert!(fill(8, 10) > fill(2, 10), "a larger measure fills more cells");
    }

    /// A `fields` mark aligns every value to one shared label column —
    /// Bertin's selective alignment — regardless of label length.
    #[test]
    fn fields_align_to_one_label_column() {
        let inline = |text: &str| {
            FieldVal::Inline(vec![CardSpan {
                role: None,
                text: text.into(),
            }])
        };
        let card = Card(vec![Mark::Fields {
            rows: vec![
                CardField {
                    label: "a".into(),
                    value: inline("x"),
                },
                CardField {
                    label: "longer".into(),
                    value: inline("y"),
                },
            ],
        }]);
        let lines = render_card(&card, 3);
        let value_col = |needle: &str| {
            let line = lines
                .iter()
                .find(|l| l.spans.iter().any(|s| s.content.contains(needle)))
                .expect("a row carrying the value");
            // The leading span is the padded label; the value starts after it.
            UnicodeWidthStr::width(line.spans[0].content.as_ref())
        };
        assert_eq!(
            value_col("x"),
            value_col("y"),
            "both values start in the same column"
        );
    }
}
