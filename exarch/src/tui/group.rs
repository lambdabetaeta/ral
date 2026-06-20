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

use super::line::{self, CODE_BG, READ_W, SLATE};
use super::md;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};

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

/// Column the per-call sparkline bar right-aligns to in the L2/L3 list, so
/// the bars stack into a comparable column down the page regardless of
/// intent length.
const BAR_COL: usize = (READ_W - 4) as usize;

/// Two-space indent for a call's intent in the L2/L3 list.
const INTENT_INDENT: &str = "  ";

/// Four-space indent for a call's effects and source under its intent.
const BODY_INDENT: &str = "    ";

/// The run's aggregate magnitude — the summed result magnitudes of its
/// calls, the figure the data-encoding rail's value-step reads.  `None`
/// when no call carried a result (the rail then renders at the base hue).
pub(super) fn aggregate_magnitude(calls: &[Call]) -> Option<u32> {
    let mags: Vec<u32> = calls.iter().filter_map(|c| c.magnitude).collect();
    (!mags.is_empty()).then(|| mags.iter().sum())
}

/// Render a coalesced ral block's rail-less body at `level`.  The caller
/// ([`super::viewport`]) prepends the data-encoding rail — the disclosure
/// triangle `▸`/`▾`, the agent hue, the aggregate magnitude — to the first
/// content row, exactly as it does for a single block.  `calls` is the
/// run's calls in arrival order; it is never empty (a run is opened by its
/// first call).
pub(super) fn body(calls: &[Call], level: u8) -> Vec<Line<'static>> {
    match level {
        1 => live_tip(calls),
        2 => full_list(calls, false),
        _ => full_list(calls, true),
    }
}

/// L1: the latest call's intent beside the whole-block sparkline, then the
/// latest call's effects.  Only the newest call shows as text; every earlier
/// call is just its bar in the sparkline.
fn live_tip(calls: &[Call]) -> Vec<Line<'static>> {
    let latest = calls.last().expect("a run has at least one call");
    let mut head = vec![
        head_span(calls),
        Span::raw(" "),
        sparkline(calls),
        Span::raw("  "),
        intent_span(latest),
    ];
    let mut line0 = Line::from(std::mem::take(&mut head));
    md::apply_context(&mut line0, latest.context);
    let mut ls = vec![Line::default(), line0];
    ls.extend(indent_rows(&latest.effects, INTENT_INDENT));
    ls
}

/// L2/L3: every call as its own intent + right-aligned bar, its effects
/// below, and — when `source` — its full ral `cmd` between the two.
fn full_list(calls: &[Call], source: bool) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default(), Line::from(head_span(calls))];
    for call in calls {
        ls.push(intent_row(call));
        if source {
            ls.extend(source_rows(call));
        }
        ls.extend(indent_rows(&call.effects, BODY_INDENT));
    }
    ls
}

/// One call's intent row in the list: the intent indented, then its single
/// sparkline bar right-aligned to [`BAR_COL`] so the bars form a comparable
/// column.  Distress modulates the intent, never the bar.
fn intent_row(call: &Call) -> Line<'static> {
    let intent = intent_span(call);
    let used = INTENT_INDENT.len() + intent.content.chars().count();
    let pad = BAR_COL.saturating_sub(used).max(2);
    let mut row = Line::from(vec![
        Span::raw(INTENT_INDENT),
        intent,
        Span::raw(" ".repeat(pad)),
        bar(call.magnitude),
    ]);
    md::apply_context(&mut row, call.context);
    row
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

/// The intent text as a white content span — the one span
/// [`md::apply_context`] then degrades by the turn's context floor.
fn intent_span(call: &Call) -> Span<'static> {
    Span::styled(call.intent.clone(), Style::default().fg(Color::White))
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
