//! Line builders and their internal helpers.  Every function returns
//! `Vec<Line<'static>>` ready for the scrollback buffer.  Color constants
//! and layout constants live here and are used by sibling modules.
//!
//! These builders are the rendering arm of the typed [`crate::bus::Event`]
//! dispatch — producers send semantic events through the channel and
//! the consumer ([`super::App::handle`]) calls into here to turn them
//! into `Line`s.

use crate::bus::{Hunk, TaskStatus};
use crate::event::ProviderErrorRecord;
use crate::provider;
use ratatui::{
    style::{Color, Modifier, Style},
    text::{Line, Span},
};
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

/// Rail accent: the leading two columns that mark a chrome line (the
/// header row of a tool call, patch, wrote, task, meter, or the user-
/// prompt scrollback echo).  One glyph in [`CYAN`], one trailing space;
/// every body row drops it so dragging a selection through the block
/// copies as plain text.
pub(super) const RAIL: &str = "❖ ";

/// True when every span in `l` is empty or whitespace-only — i.e. the
/// line carries no glyphs and reads as a vertical separator rather
/// than a row of content.  Shared with `md` (trailing-blank collapse)
/// and `viewport` (chrome-boundary dedup) so the predicate has one
/// definition across the TUI.
pub(super) fn is_blank(l: &Line<'_>) -> bool {
    l.spans.iter().all(|s| s.content.trim().is_empty())
}

/// The rail markers a header row can open with: the static `❖` and the
/// `▸` / `▾` disclosure triangles.  [`plain`] drops one of these so
/// copied text carries the content, not the chrome glyph.
const RAIL_GLYPHS: [&str; 3] = [RAIL, "▸ ", "▾ "];

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

// ── Public line builders ─────────────────────────────────────────────────────

/// Step separator: one blank line.  The step number itself is recorded
/// in `events.json` / `user.log` for greppability; in the live TUI the
/// boundary is conveyed by vertical whitespace alone.
pub(super) fn step(_n: usize) -> Vec<Line<'static>> {
    vec![Line::default()]
}

/// Scrollback echo of the user's submitted prompt: a cyan `❖ ` accent
/// on the first line, two-space indent on continuations.  The typed
/// text itself renders in reverse video so the user's turn boundary is
/// unmistakable against the surrounding markdown/chrome; the `❖` glyph
/// stays in its normal cyan so the rail accent reads as before.
pub(super) fn user_prompt(s: &str) -> Vec<Line<'static>> {
    let arrow = Span::styled(RAIL, Style::default().fg(CYAN));
    let cont = Span::raw("  ");
    let body = Style::default().add_modifier(Modifier::REVERSED);
    let mut ls: Vec<Line<'static>> = vec![Line::default()];
    ls.extend(s.lines().enumerate().map(|(i, l)| {
        Line::from(vec![
            if i == 0 { arrow.clone() } else { cont.clone() },
            Span::styled(l.to_string(), body),
        ])
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

/// Tool-call header rows: a disclosure glyph in the rail column — `▸`
/// shut, `▾` open — then the slate tool name and the white one-line
/// `label`.  Long labels wrap under the label's own first column, so the
/// rail + tool prefix stays visually fixed while the comment reads as a
/// paragraph.
fn tool_call_header(
    glyph: &'static str,
    label: &str,
    tool: &str,
    width: u16,
) -> Vec<Line<'static>> {
    let prefix_w =
        UnicodeWidthStr::width(glyph) + UnicodeWidthStr::width(tool) + UnicodeWidthStr::width("  ");
    let body_w = (width as usize).saturating_sub(prefix_w).max(8);
    let mut out = Vec::new();
    push_wrapped(&mut out, label, body_w, |chunk, first| {
        if first {
            Line::from(vec![
                Span::styled(glyph, Style::default().fg(CYAN)),
                Span::styled(tool.to_string(), Style::default().fg(SLATE)),
                Span::raw("  "),
                Span::styled(chunk, Style::default().fg(Color::White)),
            ])
        } else {
            Line::from(vec![
                Span::raw(" ".repeat(prefix_w)),
                Span::styled(chunk, Style::default().fg(Color::White)),
            ])
        }
    });
    out
}

/// Collapsed tool call: a blank separator and the `▸` header alone.
/// Clicking the row swaps this for [`tool_call_expanded`].
pub(super) fn tool_call_collapsed(label: &str, tool: &str, width: u16) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header("▸ ", label, tool, width));
    ls
}

/// Expanded tool call: the `▾` header followed by the full ral `cmd`.
/// Both the header comment and source body wrap before the viewport edge:
/// header continuations align under the comment, and source continuations
/// align under the line's own opening indentation.
pub(super) fn tool_call_expanded(
    label: &str,
    tool: &str,
    cmd: &str,
    width: u16,
) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    ls.extend(tool_call_header("▾ ", label, tool, width));
    ls.push(Line::default());
    for l in cmd.lines() {
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
        RAIL,
        cmd.lines().next().unwrap_or(""),
        tool,
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

/// Error line: bold red `✗ error` prefix.
pub(super) fn error(msg: &str) -> Vec<Line<'static>> {
    vec![
        Line::default(),
        Line::from(vec![
            Span::styled(
                "✗ error ",
                Style::default().fg(RED).add_modifier(Modifier::BOLD),
            ),
            Span::raw(msg.to_string()),
        ]),
    ]
}

/// Patch event: a header line naming the path, then one located hunk per
/// `edit` — leading context (no sign), removed lines (red `-`), added
/// lines (lime `+`), and trailing context — each row indented two columns,
/// then prefixed with a right-aligned line number in [`SLATE`] so the diff
/// sits under the header like a `wrote` block's preview.  Removed rows carry
/// their pre-edit numbers; added and context rows carry their post-edit ones.
/// Several hunks on one path are separated by an elision marker.  No rail
/// glyph on the body, so dragging a selection through the block copies as
/// plain text.  Patches are the canonical user-visible side effect of a
/// tool call, so they always render.
pub(super) fn patch(path: &str, hunks: &[Hunk]) -> Vec<Line<'static>> {
    let mut ls: Vec<Line<'static>> = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(RAIL, Style::default().fg(CYAN)),
            Span::styled("patch", Style::default().fg(SLATE)),
            Span::raw("  "),
            Span::styled(path.to_string(), Style::default().fg(Color::White)),
        ]),
    ];
    // One gutter width for the whole block — the widest number any hunk
    // will show — so every row's text column lines up under the header.
    let gutter = hunks
        .iter()
        .map(hunk_max_lineno)
        .max()
        .unwrap_or(0)
        .to_string()
        .len()
        .max(3);
    for (i, h) in hunks.iter().enumerate() {
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

/// The largest line number [`patch`] will render for `h`, used to size
/// the gutter: the last trailing-context row when present, else the last
/// added or removed row.
fn hunk_max_lineno(h: &Hunk) -> u32 {
    let mut m = h.start;
    if !h.del.is_empty() {
        m = m.max(h.start + h.del.len() as u32 - 1);
    }
    let add_base = h.start + h.add.len() as u32;
    if !h.after.is_empty() {
        m = m.max(add_base + h.after.len() as u32 - 1);
    } else if !h.add.is_empty() {
        m = m.max(add_base - 1);
    }
    m
}

/// Render one hunk's rows into `ls`: leading context, deletions,
/// additions, then trailing context.  Context and additions are numbered
/// in the post-edit file (`before` sits just above `start`, `after` just
/// below the inserted block); deletions keep their pre-edit numbers from
/// `start`.
fn push_hunk(ls: &mut Vec<Line<'static>>, h: &Hunk, gutter: usize) {
    let cb = h.before.len() as u32;
    for (i, line) in h.before.iter().enumerate() {
        push_gutter_row(
            ls,
            gutter,
            h.start.saturating_sub(cb) + i as u32,
            ' ',
            line,
            SLATE,
        );
    }
    for (j, line) in h.del.iter().enumerate() {
        push_gutter_row(ls, gutter, h.start + j as u32, '-', line, RED);
    }
    for (k, line) in h.add.iter().enumerate() {
        push_gutter_row(ls, gutter, h.start + k as u32, '+', line, LIME);
    }
    let after_base = h.start + h.add.len() as u32;
    for (m, line) in h.after.iter().enumerate() {
        push_gutter_row(ls, gutter, after_base + m as u32, ' ', line, SLATE);
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

/// Whole-file write: header naming the path and total line count,
/// followed by a dim-slate preview of the head.  The preview list is
/// the producer's responsibility — typically the first half-dozen
/// lines of the file with a `... (N more)` sentinel as the last entry.
pub(super) fn wrote(path: &str, lines: u32, preview: &[String]) -> Vec<Line<'static>> {
    let mut ls: Vec<Line<'static>> = vec![
        Line::default(),
        Line::from(vec![
            Span::styled(RAIL, Style::default().fg(CYAN)),
            Span::styled("wrote", Style::default().fg(SLATE)),
            Span::raw("  "),
            Span::styled(path.to_string(), Style::default().fg(Color::White)),
            Span::styled(
                format!("  ({lines} lines)"),
                Style::default().fg(SLATE).add_modifier(Modifier::DIM),
            ),
        ]),
    ];
    for line in preview {
        ls.push(Line::from(vec![
            Span::raw("  "),
            Span::styled(
                format!("> {line}"),
                Style::default().fg(SLATE).add_modifier(Modifier::DIM),
            ),
        ]));
    }
    ls
}

/// Task transition: a single line `❖ task <status> <desc>` with the
/// status coloured by role — open=slate, doing=cyan, blocked=orange,
/// done=lime.  The status is a closed [`TaskStatus`], so every role has a
/// colour and there is no unknown case to degrade.
pub(super) fn task(status: TaskStatus, desc: &str) -> Vec<Line<'static>> {
    let (col, style) = match status {
        TaskStatus::Doing => (CYAN, Style::default().fg(CYAN).add_modifier(Modifier::BOLD)),
        TaskStatus::Done => (LIME, Style::default().fg(LIME).add_modifier(Modifier::BOLD)),
        TaskStatus::Blocked => (
            ORANGE,
            Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        ),
        TaskStatus::Open => (SLATE, Style::default().fg(SLATE)),
    };
    let _ = col;
    vec![Line::from(vec![
        Span::styled(RAIL, Style::default().fg(CYAN)),
        Span::styled("task", Style::default().fg(SLATE)),
        Span::raw("  "),
        Span::styled(status.tag().to_string(), style),
        Span::raw("  "),
        Span::styled(desc.to_string(), Style::default().fg(Color::White)),
    ])]
}

/// Progress meter: `❖ <label>  <done>/<total>  [██████░░░░]`.  The bar
/// is 10 cells wide, lime for the filled portion and dim slate for the
/// empty.  `total = 0` is treated as a degenerate "no progress yet"
/// state (full slate bar) rather than a divide-by-zero error.
pub(super) fn meter(done: u32, total: u32, label: &str) -> Vec<Line<'static>> {
    const W: u32 = 10;
    let filled = if total == 0 {
        0
    } else {
        ((done as u64 * W as u64) / total as u64).min(W as u64) as u32
    };
    let bar_filled: String = "█".repeat(filled as usize);
    let bar_empty: String = "░".repeat((W - filled) as usize);
    vec![Line::from(vec![
        Span::styled(RAIL, Style::default().fg(CYAN)),
        Span::styled(label.to_string(), Style::default().fg(SLATE)),
        Span::raw("  "),
        Span::styled(format!("{done}/{total}"), Style::default().fg(Color::White)),
        Span::raw("  "),
        Span::styled(bar_filled, Style::default().fg(LIME)),
        Span::styled(
            bar_empty,
            Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        ),
    ])]
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

// ── Provider-error rendering ────────────────────────────────────────────────

/// Render a [`ProviderErrorRecord`] as a wrapped multi-line block.
///
/// Header: blank line + bold red `✗ error <kind>`.  Body: flush-left
/// rows for each populated field (bold slate label, plain value), with
/// `cause` text wrapped to `READ_W` columns so long URLs and stack-like
/// strings don't clip at the viewport edge.
pub(super) fn provider_error(e: &ProviderErrorRecord) -> Vec<Line<'static>> {
    let mut ls: Vec<Line<'static>> = vec![Line::default()];
    let kind = error_kind(e);
    ls.push(Line::from(vec![
        Span::styled(
            "✗ error ",
            Style::default().fg(RED).add_modifier(Modifier::BOLD),
        ),
        bold(kind.into(), RED),
    ]));
    let width = READ_W as usize;
    match e {
        ProviderErrorRecord::Cancelled { where_ } => {
            push_field(&mut ls, "where", where_, width);
        }
        ProviderErrorRecord::Transient { cause, attempts } => {
            push_field(&mut ls, "attempts", &attempts.to_string(), width);
            push_field(&mut ls, "cause", cause, width);
        }
        ProviderErrorRecord::RateLimited {
            retry_after_secs,
            cause,
        } => {
            if let Some(secs) = retry_after_secs {
                push_field(&mut ls, "retry-after", &format!("{secs}s"), width);
            }
            push_field(&mut ls, "cause", cause, width);
        }
        ProviderErrorRecord::Api {
            status,
            model,
            message,
            url,
        } => {
            if let Some(s) = status {
                push_field(&mut ls, "status", &s.to_string(), width);
            }
            push_field(&mut ls, "model", model, width);
            if let Some(u) = url {
                push_field(&mut ls, "url", u, width);
            }
            push_field(&mut ls, "message", message, width);
        }
        ProviderErrorRecord::Truncated { reason } => {
            push_field(&mut ls, "stop_reason", reason, width);
            push_field(
                &mut ls,
                "remedy",
                "raise `--max-tokens N` or split the turn into smaller writes",
                width,
            );
        }
        ProviderErrorRecord::Other { cause } => {
            push_field(&mut ls, "cause", cause, width);
        }
    }
    ls
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

/// Append one labelled field as one-or-more flush-left Lines.
///
/// The first wrapped row carries `<label>  <value>`; continuation rows
/// drop the label so the value column lines up under itself.  `value`
/// is wrapped to `width` columns via [`textwrap::wrap`] so anything
/// longer than the viewport — long URLs, stack traces, JSON blobs —
/// wraps cleanly instead of clipping.
fn push_field(ls: &mut Vec<Line<'static>>, label: &str, value: &str, width: usize) {
    let label_w = label.len() + 2; // "<label>  "
    let body_w = width.saturating_sub(label_w).max(8);
    push_wrapped(ls, value, body_w, |chunk, first| {
        let lead = if first {
            Span::styled(
                format!("{label}  "),
                Style::default().fg(SLATE).add_modifier(Modifier::BOLD),
            )
        } else {
            Span::raw(" ".repeat(label_w))
        };
        Line::from(vec![lead, Span::raw(chunk)])
    });
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
            start: 10,
            before: vec!["ctx8".into(), "ctx9".into()],
            del: vec!["old10".into(), "old11".into()],
            add: vec!["new10".into()],
            after: vec!["ctx11".into(), "ctx12".into()],
        };
        let rows: Vec<String> = patch("src/foo.rs", &[h]).iter().map(plain).collect();
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
        // under the `❖ patch` header — the same body offset as `wrote`.
        for needle in ["ctx8", "ctx9", "old10", "old11", "new10", "ctx11", "ctx12"] {
            assert!(
                find(needle).starts_with("  "),
                "hunk row must indent two columns; got {:?}",
                find(needle)
            );
        }
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
}
