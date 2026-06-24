//! Line builders and their internal helpers.  Every function returns
//! `Vec<Line<'static>>` ready for the scrollback buffer.  Color constants
//! and layout constants live here and are used by sibling modules.
//!
//! These builders are the rendering arm of the typed [`crate::bus::Event`]
//! dispatch — producers send semantic events through the channel and
//! the consumer ([`super::App::handle`]) calls into here to turn them
//! into `Line`s.

use super::block::wrap_line;
use super::highlight::highlight_ral;
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
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

// ── Color palette ────────────────────────────────────────────────────────────

/// Muted vaporwave palette for the per-step chrome — dusty pastels so the
/// repeating chrome reads as accent rather than alarm.  The only louder
/// ink in the app is the splash's `BANNER_*` pair (wordmark + eagle), kept
/// saturated so the one-shot banner carries a neon punch without bleeding
/// into the session below — the metadata matrix and everything else draw
/// from this muted set through their nominal [`Role`].
pub(super) const PINK: Color = Color::Rgb(220, 140, 175);
pub(super) const CYAN: Color = Color::Rgb(135, 200, 215);
pub(super) const LIME: Color = Color::Rgb(165, 210, 155);
pub(super) const PURPLE: Color = Color::Rgb(175, 145, 210);
pub(super) const ORANGE: Color = Color::Rgb(215, 145, 115);
pub(super) const RED: Color = Color::Rgb(215, 110, 125);
pub(super) const SLATE: Color = Color::Rgb(140, 150, 170);
/// The pending-prompt band — the raised, faintly cool fill behind the
/// queued-prompt strip in the input area ([`queued_prompt`]), a "your text,
/// still queued" affordance.  The *committed* prompt echo in the transcript
/// wears no band: it reads by its [`PROMPT_INK`] body tint and rule fence,
/// leaving the background plane to the machine's recessed [`CODE_BG`] panel.
pub(super) const PROMPT_BG: Color = Color::Rgb(72, 78, 94);
/// The human's ink — the prompt body text and the `❖` fence marking a prompt
/// in the rail thumbnail.  A light cool neutral, distinct from the agent
/// rail's [`SLATE`] and dimmer than the machine's white prose: the human owns
/// the neutral tone, agents own the matrix hues, so a prompt reads as a quiet
/// island and its fence never aliases another agent's mark.
pub(super) const PROMPT_INK: Color = Color::Rgb(170, 180, 200);
/// The recessed machine-text panel — a grey fill behind a code block or a run
/// of observation output, marking it as a contiguous machine *region* (an
/// areal mark, matched to the data's nature).  Distinct from the model's base
/// prose and from the human's rule fence: background here means "machine".
pub(super) const CODE_BG: Color = Color::Rgb(36, 38, 46);

/// Syntax-highlight inks for ral code washed into the [`CODE_BG`] panel — one
/// low-saturation hue per token class ([`super::highlight`]).  Kept muted so
/// code reads calmly against the recessed panel rather than as alarm, and
/// held distinct from each other, from the chrome [`Role`] palette, from the
/// human's [`PROMPT_INK`], and from the agent-rail identity set
/// ([`AGENT_HUES`]) so a token's colour never aliases a semantic one.
/// Punctuation reuses [`SLATE`]; every other token keeps the default code
/// ink (white).
pub(super) const CODE_KEYWORD: Color = Color::Rgb(168, 154, 208);
pub(super) const CODE_STRING: Color = Color::Rgb(150, 186, 146);
pub(super) const CODE_VARIABLE: Color = Color::Rgb(206, 166, 130);
pub(super) const CODE_TAG: Color = Color::Rgb(202, 150, 178);
/// Agent rail palette: one hue per producing agent, indexed by
/// [`super::block::AgentSlot`]. Root keeps [`CYAN`] — the existing rail
/// accent — so a root-only session is visually unchanged in hue. The
/// rail's value-step lightens a slot toward white with magnitude, so hue
/// stays the identity channel and value stays the magnitude channel.
///
/// Agent identity is hue-only on the rail (the cell already spends shape on
/// *kind* and value on *magnitude*, so no fourth channel is free), which a
/// red-green–blind reader cannot follow on hue alone. So the six are picked
/// to also separate by **lightness**: a descending `L*` ladder (≈77 → 75 →
/// 62 → 59 → 55 → 47) under which every pair stays distinct in simulated
/// deuteranopia *and* protanopia (worst-case ΔE76 ≈ 19, against ≈3 for a
/// hue-only set). Where two sit at near-equal `L*` (CYAN/MAGENTA) the
/// surviving blue–yellow axis holds them apart; no two warm hues share a
/// lightness, so the old orange/red confusion cannot recur. These are a
/// dedicated set, not the role palette above — agent identity must not alias
/// a semantic colour (e.g. `RED` the error hue).
pub(super) const AGENT_AMBER: Color = Color::Rgb(230, 175, 90);
pub(super) const AGENT_MAGENTA: Color = Color::Rgb(205, 120, 190);
pub(super) const AGENT_BLUE: Color = Color::Rgb(95, 140, 225);
pub(super) const AGENT_OLIVE: Color = Color::Rgb(150, 130, 70);
pub(super) const AGENT_PLUM: Color = Color::Rgb(135, 95, 165);
pub(super) const AGENT_HUES: [Color; 6] = [
    CYAN,
    AGENT_AMBER,
    AGENT_MAGENTA,
    AGENT_BLUE,
    AGENT_OLIVE,
    AGENT_PLUM,
];

/// Saturated splash-only palette — the wordmark (pink) and the eagle
/// (gold) of the one-shot startup banner.  These two are the only neon ink
/// in the app; all session data, the metadata matrix included, renders
/// through the muted palette above so nothing else competes with the
/// splash.
pub(super) const BANNER_PINK: Color = Color::Rgb(255, 20, 147);
pub(super) const BANNER_GOLD: Color = Color::Rgb(255, 191, 0);

// ── Layout constants ─────────────────────────────────────────────────────────

/// Maximum readable width in columns; markdown is wrapped to this.
pub(super) const READ_W: u16 = 100;

/// The prompt-fence glyph (`RailKind::Prompt`'s `❖`) plus its trailing
/// space — named because [`RAIL_GLYPHS`] reuses it as the last entry of the
/// shape vocabulary. Block content gets its rail from [`super::rail::span`],
/// prepended by [`super::block::Block::render`].
pub(super) const RAIL: &str = "❖ ";

/// Rail width in columns: one shape glyph plus one trailing space. Every
/// block's first content row carries a rail of this width; body rows do
/// not, so a selection through the block copies as plain text.
pub(super) const RAIL_W: usize = 2;

/// Width of the wheel-dialable rail target: the shape glyph alone, not
/// the trailing space. The glyph is the cell that *bears* the mark, so
/// the wheel dials the block's disclosure level only when it sits on it;
/// the blank second column is inert margin that falls through to
/// page-scroll, so resting the pointer there never traps the wheel.
pub(super) const RAIL_DIAL_W: usize = 1;

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
pub(super) const RAIL_GLYPHS: [&str; 8] = ["▎ ", "▸ ", "▽ ", "· ", "↘ ", "━ ", "╳ ", RAIL];

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
    let step =
        (((magnitude.unwrap_or(0) + 1) as f32).log2().round() as usize).min(SPARK_GLYPHS.len() - 1);
    SPARK_GLYPHS[step]
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

/// The pending-prompt strip shown above the input while a turn runs: each
/// message the user submitted mid-turn, oldest first.  Pending prompts wear a
/// raised [`PROMPT_BG`] band ([`wash`]) — a "your text, still queued"
/// affordance in the input area, distinct from the rule fence the committed
/// prompt gets in the transcript — and, like that echo, leaves reverse
/// video to an active selection alone.  Flush-left at regular weight, wrapped
/// to `width` columns.  Capped at `max_rows` total — a longer queue closes with
/// a `⋯ (N more)` line so it can never crowd the transcript off-screen.
pub(super) fn queued_prompt(
    messages: &[String],
    width: u16,
    max_rows: usize,
) -> Vec<Line<'static>> {
    let w = width as usize;
    let more = Style::default().fg(SLATE).add_modifier(Modifier::ITALIC);
    let mut out: Vec<Line<'static>> = Vec::new();
    for msg in messages {
        for raw in msg.lines() {
            push_wrapped(&mut out, raw, w, |chunk, _first| {
                wash(Line::from(Span::raw(chunk)), PROMPT_BG, Some(w))
            });
        }
    }
    if out.len() > max_rows {
        let hidden = out.len() - (max_rows - 1);
        out.truncate(max_rows - 1);
        out.push(wash(
            Line::from(Span::styled(format!("⋯ ({hidden} more)"), more)),
            PROMPT_BG,
            Some(w),
        ));
    }
    out
}

/// Tool-call header rows: the slate tool name then the white one-line
/// `label`. The disclosure triangle (`▸`/`▽`) lives in the lifted rail,
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
                Span::styled(chunk, Style::default().fg(SLATE)),
            ];
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

/// Expanded tool call (L3): the `▽` header followed by the full ral `cmd`.
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
    for line in highlight_ral(cmd).into_iter().take(take) {
        push_code_row(&mut ls, line, width);
    }
    ls
}

/// Wash `row` with the background `bg`, preserving every span's foreground
/// and modifiers — the single place a background stratum is painted: the
/// recessed code panel, the pending-prompt band, and the `/legend` swatches.
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

/// A tool call with no separate summary — the `fff` query, an
/// invalid-input header.  There is nothing to dial open, so it is pushed as
/// inert chrome (`RailShape::ToolCall`) wearing the shut triangle `▸` — a
/// tool call still, not the prompt's `❖`: `cmd`'s first line is the label,
/// any remainder follows 2-space indented.
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
            Span::styled(l.to_string(), Style::default().fg(SLATE)),
        ]));
    }
    ls
}

/// The one-line header for an async subagent's landed result: a leading
/// blank (like [`tool_call_collapsed`]), then the bold `title` (LIME, or
/// the error hue when `error` is set), a [`SLATE`]-dim ` {elapsed}s `
/// readout, a [`size_bar`] for the result `size` (lines of `text`), and an
/// error suffix when one applies.  The `↘` shape arrives via the lifted
/// rail ([`super::block::Block::render`], `Subagent` shape), so this
/// builder is rail-less.
pub(super) fn subagent_header(
    title: &str,
    size: u32,
    error: Option<&str>,
    elapsed: Duration,
) -> Vec<Line<'static>> {
    let secs = elapsed.as_secs();
    let title_color = if error.is_some() { ORANGE } else { LIME };
    let mut spans = vec![
        bold(title.to_string(), title_color),
        Span::styled(
            format!(" {secs}s "),
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ),
        size_bar(size),
    ];
    // The error / empty suffix the breadcrumb carried, less the `[done in
    // Ns]` case the elapsed readout now subsumes.
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
    )
}

/// Render a pinned register card: framed in its producing agent's `hue`, flush
/// to the register column (no transcript indent), bounded by the column
/// `width`.  The hue is the register's only departure from a surfaced card —
/// identity that the transcript reads from the matrix, a side column must carry
/// itself.
pub(super) fn render_pin(card: &Card, width: u16, hue: Color) -> Vec<Line<'static>> {
    render_framed(card, 0, Style::default().fg(hue), width)
}

/// Core framed-card renderer shared by the transcript's surfaced cards and the
/// register's pins: a bordered box `indent_w` columns in, drawn in `border`
/// ink, content wrapped to a budget derived from `width` (the caller caps it —
/// the transcript at [`READ_W`], the register at its column width).
fn render_framed(card: &Card, indent_w: usize, border: Style, width: u16) -> Vec<Line<'static>> {
    let indent = " ".repeat(indent_w);
    // Inner content budget: the content column less the indent and the four
    // frame columns (`│ ` … ` │`).
    let max_inner = (width as usize).saturating_sub(indent_w + 4).max(8);

    // Lift a single-line leading heading into the top rule; everything else
    // renders inside.  A multi-line or non-text first mark leaves no title.
    let marks = card.marks();
    let (title, body_marks): (Option<Vec<Span<'static>>>, &[Mark]) = match marks.first() {
        Some(Mark::Text { spans }) => {
            let head = render_text(spans);
            if head.len() == 1 {
                (Some(head[0].spans.clone()), &marks[1..])
            } else {
                (None, marks)
            }
        }
        _ => (None, marks),
    };

    // Body marks → logical lines → wrapped to the inner budget.
    let mut body: Vec<Line<'static>> = Vec::new();
    for mark in body_marks {
        match mark {
            Mark::Text { spans } => body.extend(render_text(spans)),
            Mark::Measure(m) => body.push(render_measure(m)),
            Mark::Fields { rows } => body.extend(render_fields(rows)),
            Mark::Raw { bytes } => body.extend(render_raw(bytes)),
            // A diff never reaches here — diff-bearing cards take the diff path.
            Mark::Diff { .. } => {}
        }
    }
    let wrapped: Vec<Line<'static>> = body.iter().flat_map(|l| wrap_line(l, max_inner)).collect();

    // Inner width: the widest row, and at least one column past the title so
    // the top rule's `╭─ title ─╮` always closes.  Capped at the budget.
    let title_w = title.as_deref().map_or(0, span_run_width);
    let title_min = if title.is_some() { title_w + 1 } else { 0 };
    let inner_w = wrapped
        .iter()
        .map(|l| span_run_width(&l.spans))
        .max()
        .unwrap_or(0)
        .max(title_min)
        .clamp(1, max_inner);
    let interior = inner_w + 2; // one padding column each side

    let mut out: Vec<Line<'static>> = vec![Line::default()];

    // Top rule, with the heading set into it.
    let mut top = vec![Span::raw(indent.clone())];
    match &title {
        Some(spans) => {
            top.push(Span::styled("╭─ ", border));
            top.extend(spans.iter().cloned());
            let fill = interior.saturating_sub(3 + title_w); // "─ " + title + " "
            top.push(Span::styled(format!(" {}", "─".repeat(fill)), border));
            top.push(Span::styled("╮", border));
        }
        None => {
            top.push(Span::styled("╭", border));
            top.push(Span::styled("─".repeat(interior), border));
            top.push(Span::styled("╮", border));
        }
    }
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

/// One pin reduced to its first non-blank rendered row — the digest the
/// collapsed pin band shows when the terminal is too narrow for the column.
fn pin_digest(card: &Card) -> Vec<Span<'static>> {
    render_card(card, 3)
        .into_iter()
        .find(|l| !is_blank(l))
        .map(|l| l.spans)
        .unwrap_or_default()
}

/// The collapsed register: every pin's digest on one row, separated by a gap —
/// the narrow-terminal fallback for the register column.  Empty (no row) when
/// there are no pins; overflow past the strip is clipped by the paragraph.
pub(super) fn pin_band(pins: &[(String, Card)]) -> Vec<Line<'static>> {
    if pins.is_empty() {
        return Vec::new();
    }
    let mut spans: Vec<Span<'static>> = Vec::new();
    for (_key, card) in pins {
        if !spans.is_empty() {
            spans.push(Span::styled("   ", Style::default().fg(SLATE)));
        }
        spans.extend(pin_digest(card));
    }
    vec![Line::from(spans)]
}

/// Total display width of a span run, unicode-aware.
fn span_run_width(spans: &[Span<'static>]) -> usize {
    spans.iter().map(|s| s.width()).sum()
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
}
