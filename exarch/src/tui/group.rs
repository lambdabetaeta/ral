//! The coalesced ral block — a render-time projection over arrival order.
//!
//! A contiguous run of *observation-only* `ral` calls (calls whose effects
//! are reads, greps, or execs — never a diff or a write) reads as one
//! dialable object instead of one block per call.  This is a projection in
//! the flatten ([`super::viewport::Viewport`]): nothing about how blocks are
//! pushed, logged, or aggregated changes — the grouping reads what arrival
//! order already adjoins.  A diff or a write is a *barrier*: it ends the
//! current block, renders as its own always-visible block, and a fresh
//! block starts after it.
//!
//! The block dials through three levels, with **no L0** and **L1 the
//! floor** — a coalesced block always shows at least its live tip:
//!
//! - **L1, the live tip** — one line: the latest call's intent and a
//!   *vertical* sparkline (one bar per call, height ∝ its result magnitude,
//!   left→right in call order), then that latest call's effects.  Earlier
//!   calls are just their bar; the text refreshes to the newest call as the
//!   block grows.  The bar count *is* the count — no `×N`.
//! - **L2, the full list** — every call: its intent, its effects, and its
//!   bar.
//! - **L3, everything** — L2 plus each call's full ral `cmd` source.

use super::line::{self, CODE_BG, RAIL_W, SLATE, push_wrapped};
use super::md;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// The scalar parts of one call, borrowed straight off its
/// [`super::block::Block`] — the projection reads these without copying the
/// script.  The viewport pairs them with the call's rendered effect rows to
/// build a [`Call`].
pub(super) struct CallParts<'a> {
    pub(super) intent: &'a str,
    pub(super) tool: &'a str,
    pub(super) cmd: &'a str,
    pub(super) magnitude: Option<u32>,
    pub(super) context: u8,
}

/// One observation call as the projection renders it: the model's stated
/// intent, the ral tool and script behind it, the call's result magnitude
/// (drives its sparkline bar), the turn's context floor (distress on the
/// intent line, never on a bar), and the pre-rendered, rail-less rows of
/// the reads/greps/execs it produced.
pub(super) struct Call {
    intent: String,
    tool: String,
    cmd: String,
    magnitude: Option<u32>,
    context: u8,
    effects: Vec<Line<'static>>,
}

impl Call {
    /// Build a call from its borrowed [`CallParts`] and the rail-less rows
    /// of the effects that followed it in arrival order.
    pub(super) fn new(parts: CallParts<'_>, effects: Vec<Line<'static>>) -> Self {
        Self {
            intent: parts.intent.to_string(),
            tool: parts.tool.to_string(),
            cmd: parts.cmd.to_string(),
            magnitude: parts.magnitude,
            context: parts.context,
            effects,
        }
    }
}

/// Columns reserved at the right edge for the bar column: the bars' last
/// glyph lands `BAR_PAD` columns shy of the content width, so the per-call
/// bars in the list and the whole-run sparkline at the tip stack into one
/// comparable column down the page regardless of intent length.
const BAR_PAD: usize = 4;

/// Two-space indent for a call's intent in the L2/L3 list, and the gap that
/// separates an intent from its right-pinned bars.
const INTENT_INDENT: &str = "  ";
const GAP: usize = 2;

/// Four-space indent for a call's effects and source under its intent.
const BODY_INDENT: &str = "    ";

/// The column an intent's last bar glyph lands in, given the content `width`.
fn bar_col(width: usize) -> usize {
    width.saturating_sub(BAR_PAD)
}

/// The run's aggregate magnitude — the summed result magnitudes of its
/// calls, the figure the data-encoding rail's value-step reads.  `None`
/// when no call carried a result (the rail then renders at the base hue).
pub(super) fn aggregate_magnitude(calls: &[Call]) -> Option<u32> {
    let mags: Vec<u32> = calls.iter().filter_map(|c| c.magnitude).collect();
    (!mags.is_empty()).then(|| mags.iter().sum())
}

/// Render a coalesced ral block's rail-less body at `level`.  The caller
/// ([`super::viewport`]) prepends the data-encoding rail — the disclosure
/// triangle `▸`/`▽`, the agent hue, the aggregate magnitude — to the first
/// content row, exactly as it does for a single block.  `calls` is the
/// run's calls in arrival order; it is never empty (a run is opened by its
/// first call).
pub(super) fn body(calls: &[Call], level: u8, width: usize) -> Vec<Line<'static>> {
    match level {
        1 => live_tip(calls, width),
        2 => full_list(calls, false, width),
        _ => full_list(calls, true, width),
    }
}

/// L1: the latest call's intent on the `ral` head line, the whole-block
/// sparkline pinned to the right bar column, then the latest call's effects.
/// Only the newest call shows as text; every earlier call is just its bar in
/// the sparkline.
fn live_tip(calls: &[Call], width: usize) -> Vec<Line<'static>> {
    let latest = calls.last().expect("a run has at least one call");
    let head = head_span(calls);
    let lead_w = UnicodeWidthStr::width(head.content.as_ref()) + GAP;
    let mut ls = vec![Line::default()];
    ls.extend(pinned_intent(
        vec![head, Span::raw(INTENT_INDENT)],
        lead_w,
        // The viewport prepends the rail to this row alone, so continuations
        // bake in its width and the bars target `bar_col - RAIL_W` to land at
        // `bar_col` once the row shifts.
        RAIL_W + lead_w,
        &latest.intent,
        latest.context,
        sparkline(calls),
        calls.len(),
        bar_col(width).saturating_sub(RAIL_W),
    ));
    ls.extend(indent_rows(&latest.effects, INTENT_INDENT));
    ls
}

/// L2/L3: every call as its own intent + right-aligned bar, its effects
/// below, and — when `source` — its full ral `cmd` between the two.
fn full_list(calls: &[Call], source: bool, width: usize) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default(), Line::from(head_span(calls))];
    for (i, call) in calls.iter().enumerate() {
        if i > 0 {
            ls.push(Line::default());
        }
        ls.extend(intent_row(call, width));
        if source {
            ls.extend(source_rows(call));
        }
        ls.extend(indent_rows(&call.effects, BODY_INDENT));
    }
    ls
}

/// One call's intent rows in the list: the intent indented and wrapped under
/// its hanging indent, its single sparkline bar right-aligned to [`bar_col`]
/// so the bars form a comparable column.  These rows carry no rail (the head
/// line owns it), so the indent and bar column need no [`RAIL_W`] offset.
fn intent_row(call: &Call, width: usize) -> Vec<Line<'static>> {
    pinned_intent(
        vec![Span::raw(INTENT_INDENT)],
        GAP,
        GAP,
        &call.intent,
        call.context,
        bar(call.magnitude),
        1,
        bar_col(width),
    )
}

/// Lay one intent out as a left text block with its bar(s) pinned to the
/// right.  `lead` opens row 0 — the slate `ral` head at the tip, the bare
/// indent in the list — occupying `lead_w` columns; `cont_indent` is where the
/// wrapped continuations hang, baking in the [`RAIL_W`] the viewport prepends
/// to row 0 (the row the lead owns).  `bars` is the right-aligned sparkline —
/// the whole run at the tip, one glyph per row in the list — `bars_w` its
/// width and `bar_last` the column its final glyph lands in.  The intent wraps
/// to clear the bar band on every row, and the turn's `context` floor
/// distress-modulates each row (the bar's height stays the magnitude it
/// encodes; only the intent ink drains its saturation).
#[allow(clippy::too_many_arguments)]
fn pinned_intent(
    lead: Vec<Span<'static>>,
    lead_w: usize,
    cont_indent: usize,
    intent: &str,
    context: u8,
    bars: Span<'static>,
    bars_w: usize,
    bar_last: usize,
) -> Vec<Line<'static>> {
    let bars_left = (bar_last + 1).saturating_sub(bars_w);
    let body_w = bars_left.saturating_sub(lead_w + GAP).max(8);
    let ink = Style::default().fg(Color::White);
    let mut out: Vec<Line<'static>> = Vec::new();
    push_wrapped(&mut out, intent, body_w, |chunk, first| {
        let mut row = if first {
            let pad = bars_left
                .saturating_sub(lead_w + UnicodeWidthStr::width(chunk.as_str()))
                .max(GAP);
            let mut spans = lead.clone();
            spans.push(Span::styled(chunk, ink));
            spans.push(Span::raw(" ".repeat(pad)));
            spans.push(bars.clone());
            Line::from(spans)
        } else {
            Line::from(vec![
                Span::raw(" ".repeat(cont_indent)),
                Span::styled(chunk, ink),
            ])
        };
        md::apply_context(&mut row, context);
        row
    });
    out
}

/// The call's full ral `cmd` source rows (L3), each inset under the call's
/// body column and styled as code, so the script reads beneath its intent.
fn source_rows(call: &Call) -> Vec<Line<'static>> {
    call.cmd
        .lines()
        .map(|l| {
            Line::from(vec![
                Span::raw(BODY_INDENT),
                Span::styled(l.to_string(), Style::default().fg(Color::White).bg(CODE_BG)),
            ])
        })
        .collect()
}

/// The block's head: the tool name the run's calls share, slate like every
/// other line label.  Coalesced calls are homogeneous in tool (the agent's
/// observation calls are all `ral`), so the latest call names the head.
fn head_span(calls: &[Call]) -> Span<'static> {
    let tool = calls
        .last()
        .map(|c| c.tool.as_str())
        .unwrap_or("ral")
        .to_string();
    Span::styled(tool, Style::default().fg(SLATE))
}

/// The whole-block sparkline: one [`line::spark_glyph`] per call, in call
/// order, as one slate span — decorative ink reading as a bar chart of how
/// much each call moved, the bar count standing in for an `×N`.
fn sparkline(calls: &[Call]) -> Span<'static> {
    let glyphs: String = calls.iter().map(|c| line::spark_glyph(c.magnitude)).collect();
    Span::styled(glyphs, Style::default().fg(SLATE))
}

/// One call's single sparkline bar — the same glyph as the whole-block
/// sparkline, for the right-aligned per-row column in the list views.
fn bar(magnitude: Option<u32>) -> Span<'static> {
    Span::styled(line::spark_glyph(magnitude).to_string(), Style::default().fg(SLATE))
}

/// Re-indent a call's pre-rendered effect rows by `indent`, dropping the
/// leading blank `render_card` opens with so the effects sit flush under
/// the intent rather than after a gap.
fn indent_rows(rows: &[Line<'static>], indent: &str) -> Vec<Line<'static>> {
    rows.iter()
        .filter(|l| !line::is_blank(l))
        .map(|l| {
            let mut spans = vec![Span::raw(indent.to_string())];
            spans.extend(l.spans.iter().cloned());
            Line::from(spans)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    const LONG: &str = "Reading the definition of poles, biorthogonal logical \
        relations, and the fundamental lemma for bimodels, which together span \
        rather more than a single rendered row at this width.";

    fn call(intent: &'static str, magnitude: Option<u32>) -> Call {
        Call::new(
            CallParts {
                intent,
                tool: "ral",
                cmd: "",
                magnitude,
                context: 0,
            },
            Vec::new(),
        )
    }

    /// The rail-less plain text of a line, span contents joined.
    fn plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn nonblank(lines: &[Line<'static>]) -> Vec<String> {
        lines
            .iter()
            .filter(|l| !line::is_blank(l))
            .map(plain)
            .collect()
    }

    fn indent_of(s: &str) -> usize {
        s.len() - s.trim_start().len()
    }

    /// L1 head row: `ral␠␠<intent>…<sparkline>`.  The row's display width is
    /// pinned so the sparkline's last glyph lands at `bar_col` once the
    /// viewport shifts the row by [`RAIL_W`]; the wrapped continuation hangs
    /// under the intent — at `RAIL_W + "ral  "` — not back under the rail.
    #[test]
    fn live_tip_pins_sparkline_and_hangs_intent() {
        let width = 100;
        let calls = vec![call("short", Some(3)), call(LONG, Some(40))];
        let rows = nonblank(&body(&calls, 1, width));

        let head = &rows[0];
        assert!(head.starts_with("ral  "));
        assert!(head.ends_with(line::spark_glyph(Some(40))));
        assert_eq!(
            UnicodeWidthStr::width(head.as_str()),
            bar_col(width) - RAIL_W + 1
        );

        let lead_w = UnicodeWidthStr::width("ral") + GAP;
        assert_eq!(indent_of(&rows[1]), RAIL_W + lead_w);
    }

    /// L2 list row: the bare `ral` head, then per-call intent rows whose
    /// single bar pins to `bar_col` directly — these rows carry no rail, so
    /// the indent and bar column take no [`RAIL_W`] offset.
    #[test]
    fn intent_row_pins_bar_without_rail_offset() {
        let width = 100;
        let rows = nonblank(&body(&[call(LONG, Some(12))], 2, width));

        assert_eq!(rows[0], "ral");
        assert_eq!(
            UnicodeWidthStr::width(rows[1].as_str()),
            bar_col(width) + 1
        );
        assert_eq!(indent_of(&rows[2]), GAP);
    }
}
