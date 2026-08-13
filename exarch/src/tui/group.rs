//! The coalesced ral block — a render-time projection over arrival order.
//!
//! A contiguous run of observation-only `ral` calls (reads, greps, execs) reads
//! as one dialable object; a diff or a write is a barrier that ends the run and
//! renders as its own always-visible block.  A write lands at the redirect seam,
//! mid-call, so the effects behind it are still their call's: they render as a
//! [`continuation`], railless under the call the barrier split them from.
//! Nothing about how blocks are pushed or logged changes —
//! [`super::viewport`] gathers the run in arrival order and this module renders
//! its body at one of four [`Reveal`] rungs:
//!
//! - `Census` — one line tallying the run's `|>` effects by verb.  A run is the
//!   only object that reaches this floor, and only by being dialed *down* to it.
//! - `Summary` — the latest *settled* call's intent and effects, plus a
//!   sparkline of one bar per call; the bar count stands in for an `×N`.
//! - `Context` — every call: intent, bar, effects.
//! - `Full` — that, plus each call's ral `cmd` source.

use std::fmt::Write;

use super::block::Reveal;
use super::highlight::highlight_ral;
use super::line::{self, push_wrapped, wash, wrap_line};
use super::md;
use super::palette::{CODE_BG, RAIL_W, SLATE};
use crate::bus::card::ObservationKind;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// One call's scalars, borrowed off its [`super::block::Block`] rather than
/// copied.  [`super::viewport`] pairs them with the call's rendered effect rows
/// to build a [`Call`].
#[derive(Clone, Copy)]
pub(super) struct CallParts<'a> {
    pub(super) intent: &'a str,
    pub(super) cmd: &'a str,
    pub(super) magnitude: Option<u32>,
    pub(super) context: u8,
}

/// The run's `|>` effects by census bucket.  A write is a barrier, never a run
/// member, so it has no bucket; the script count is the call count, not a field.
#[derive(Clone, Copy, Default)]
pub(super) struct Tally {
    binaries: u32,
    files: u32,
    searches: u32,
}

impl Tally {
    /// Fold `n` effects of `kind` in, as `Viewport::group_calls` gathers a run.
    pub(super) fn add(&mut self, kind: ObservationKind, n: u32) {
        match kind {
            ObservationKind::Exec => self.binaries += n,
            ObservationKind::Read => self.files += n,
            ObservationKind::Grep => self.searches += n,
        }
    }

    fn merge(&mut self, other: Self) {
        self.binaries += other.binaries;
        self.files += other.files;
        self.searches += other.searches;
    }
}

/// One observation call as rendered: the magnitude drives its sparkline bar, the
/// context is the turn's floor, and the effect rows arrive already rail-less.
pub(super) struct Call {
    intent: String,
    cmd: String,
    magnitude: Option<u32>,
    context: u8,
    tally: Tally,
    effects: Vec<Line<'static>>,
}

impl Call {
    pub(super) fn new(parts: CallParts<'_>, tally: Tally, effects: Vec<Line<'static>>) -> Self {
        Self {
            intent: parts.intent.to_string(),
            cmd: parts.cmd.to_string(),
            magnitude: parts.magnitude,
            context: parts.context,
            tally,
            effects,
        }
    }
}

/// Columns held clear at the right edge, so the per-call bars and the tip's
/// sparkline stack into one comparable column whatever an intent's length.
const BAR_PAD: usize = 4;

/// Most bars the sparkline draws; a longer run keeps only its tail.
const MAX_SPARKLINE: usize = 30;

/// A list intent's indent, and the least gap between an intent and its bar.
const INTENT_INDENT: &str = "  ";
const GAP: usize = 2;

const BODY_INDENT: &str = "    ";

fn bar_col(width: usize) -> usize {
    width.saturating_sub(BAR_PAD)
}

/// The run's summed result magnitudes — what the rail's value step encodes.
/// `None` when no call carried a result, and the rail then renders at base hue.
pub(super) fn aggregate_magnitude(calls: &[Call]) -> Option<u32> {
    calls
        .iter()
        .filter_map(|c| c.magnitude)
        .reduce(|a, b| a + b)
}

/// Render the run's rail-less body at `level`.  [`super::viewport`] prepends the
/// data-encoding rail to the first content row, exactly as for a single block.
/// `calls` is in arrival order and never empty — a run is opened by a call.
pub(super) fn body(calls: &[Call], level: Reveal, width: usize) -> Vec<Line<'static>> {
    match level {
        Reveal::Census => census(calls, width),
        Reveal::Summary => live_tip(calls, width),
        Reveal::Context => full_list(calls, false, width),
        Reveal::Full => full_list(calls, true, width),
    }
}

/// Render a continuation — effects a barrier split from the call that issued
/// them — at that call's `level`.  They hang at the indent their level gives a
/// call's effects, so they read as the continuation they are, and
/// [`super::viewport`] seats no rail on them: the call above still wears it.
/// `Census` folds effects into a count, and a continuation shows nothing there.
pub(super) fn continuation(
    effects: &[Line<'static>],
    level: Reveal,
    width: usize,
) -> Vec<Line<'static>> {
    match level {
        Reveal::Census => Vec::new(),
        Reveal::Summary => indent_rows(effects, &" ".repeat(RAIL_W), width),
        Reveal::Context | Reveal::Full => indent_rows(effects, BODY_INDENT, width),
    }
}

/// `Summary`: the tip call's intent on the head row — the row the viewport seats
/// the rail on — the whole-run sparkline pinned right, then that call's effects.
fn live_tip(calls: &[Call], width: usize) -> Vec<Line<'static>> {
    // Anchor on the latest *settled* call, not `calls.last()`: a call still in
    // flight has no effects yet, so the tip would blank the previous call's
    // reads for a frame — a flicker.  The pending call still shows as its own
    // bar, so the count stays honest.
    let tip = calls
        .iter()
        .rev()
        .find(|c| c.magnitude.is_some())
        .unwrap_or_else(|| calls.last().expect("a run has at least one call"));
    let mut ls = vec![Line::default()];
    // The head row opens flush: the rail the viewport prepends is its margin.
    ls.extend(pinned_intent(
        &[],
        RAIL_W,
        &tip.intent,
        tip.context,
        &sparkline(calls),
        width,
    ));
    // The effects hang at RAIL_W — where the intent opens once the rail is
    // prepended — so each reads as belonging to the call above it.
    let effect_indent = " ".repeat(RAIL_W);
    ls.extend(indent_rows(&tip.effects, &effect_indent, width));
    ls
}

/// `Census`: the run in one slate line — its calls counted as scripts, its `|>`
/// effects summed by bucket and named by verb.  The viewport prepends the rail
/// to the census row, so a wrapped continuation hangs under [`RAIL_W`].
fn census(calls: &[Call], width: usize) -> Vec<Line<'static>> {
    let mut tally = Tally::default();
    for call in calls {
        tally.merge(call.tally);
    }
    #[allow(clippy::cast_possible_truncation, reason = "coalesced-run call count")]
    let text = census_line(calls.len() as u32, tally);
    let mut ls = vec![Line::default()];
    push_wrapped(
        &mut ls,
        &text,
        width.saturating_sub(RAIL_W),
        |chunk, first| {
            let mut spans = Vec::new();
            if !first {
                spans.push(Span::raw(" ".repeat(RAIL_W)));
            }
            spans.push(Span::styled(chunk, Style::default().fg(SLATE)));
            Line::from(spans)
        },
    );
    ls
}

/// "Ran N scripts" always, then the non-empty buckets in fixed order — binaries
/// share the "Ran"; reads and searches bring their own verb.
fn census_line(scripts: u32, tally: Tally) -> String {
    let mut s = format!("Ran {}", count(scripts, "script", "scripts"));
    if tally.binaries > 0 {
        let _ = write!(s, ", {}", count(tally.binaries, "binary", "binaries"));
    }
    if tally.files > 0 {
        let _ = write!(s, ", read {}", count(tally.files, "file", "files"));
    }
    if tally.searches > 0 {
        let _ = write!(s, ", searched {}", count(tally.searches, "time", "times"));
    }
    s.push('.');
    s
}

fn count(n: u32, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// `Context`/`Full`: every call as its own intent and right-aligned bar, its
/// effects below, and — when `source` — its ral `cmd` between the two.
fn full_list(calls: &[Call], source: bool, width: usize) -> Vec<Line<'static>> {
    let mut ls = vec![Line::default()];
    for (i, call) in calls.iter().enumerate() {
        if i > 0 {
            ls.push(Line::default());
        }
        ls.extend(intent_row(call, i == 0, width));
        if source {
            ls.extend(source_rows(call, width));
        }
        ls.extend(indent_rows(&call.effects, BODY_INDENT, width));
    }
    ls
}

/// One call's intent rows, wrapped under a hanging indent with its bar pinned to
/// the shared column.  The `railed` row is the one the viewport seats the rail
/// on, so it drops its own indent and lets the rail be its margin.
fn intent_row(call: &Call, railed: bool, width: usize) -> Vec<Line<'static>> {
    let lead: Vec<Span<'static>> = if railed {
        Vec::new()
    } else {
        vec![Span::raw(INTENT_INDENT)]
    };
    pinned_intent(
        &lead,
        if railed { RAIL_W } else { 0 },
        &call.intent,
        call.context,
        &bar(call.magnitude),
        width,
    )
}

/// Lay one intent out as a left text block with its `bars` pinned right.  `lead`
/// and `rail_offset` together are the row's margin — the offset being what the
/// viewport will prepend to row 0 — so the bars target
/// `bar_col(width) - rail_offset` and land in the shared column once row 0
/// shifts.  The `context` floor drains a row's ink, never a bar's height.
fn pinned_intent(
    lead: &[Span<'static>],
    rail_offset: usize,
    intent: &str,
    context: u8,
    bars: &Span<'static>,
    width: usize,
) -> Vec<Line<'static>> {
    let lead_w: usize = lead.iter().map(Span::width).sum();
    let cont_indent = rail_offset + lead_w;
    let bar_last = bar_col(width).saturating_sub(rail_offset);
    let bars_w = bars.width();
    let bars_left = (bar_last + 1).saturating_sub(bars_w);
    let body_w = bars_left.saturating_sub(lead_w + GAP).max(8);
    // The intent is work-narration, not the answer: SLATE seats it below the
    // model's prose and gives the context drain a hue to desaturate.
    let ink = Style::default().fg(SLATE);
    let mut out: Vec<Line<'static>> = Vec::new();
    push_wrapped(&mut out, intent, body_w, |chunk, first| {
        let mut row = if first {
            let pad = bars_left
                .saturating_sub(lead_w + UnicodeWidthStr::width(chunk.as_str()))
                .max(GAP);
            let mut spans = lead.to_vec();
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

/// A call's ral `cmd` at the `Full` rung, syntax-highlighted and washed into the
/// recessed [`CODE_BG`] panel inset under [`BODY_INDENT`].
fn source_rows(call: &Call, width: usize) -> Vec<Line<'static>> {
    let mut ls = Vec::new();
    for line in highlight_ral(&call.cmd) {
        wash_inset(&mut ls, &line, BODY_INDENT, width);
    }
    ls
}

/// The whole-run sparkline: one [`line::spark_glyph`] per call in call order, as
/// one slate span — a bar chart of how much each call moved.
fn sparkline(calls: &[Call]) -> Span<'static> {
    let skip = calls.len().saturating_sub(MAX_SPARKLINE);
    let glyphs: String = calls
        .iter()
        .skip(skip)
        .map(|c| line::spark_glyph(c.magnitude))
        .collect();
    Span::styled(glyphs, Style::default().fg(SLATE))
}

fn bar(magnitude: Option<u32>) -> Span<'static> {
    Span::styled(
        line::spark_glyph(magnitude).to_string(),
        Style::default().fg(SLATE),
    )
}

/// Re-indent a call's effect rows — dropping the leading blank
/// [`line::render_card`] opens with — and wash each into the [`CODE_BG`] panel at
/// `indent`; the list passes the script's own margin, so the two read as one
/// rectangle.
fn indent_rows(rows: &[Line<'static>], indent: &str, width: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for l in rows.iter().filter(|l| !line::is_blank(l)) {
        wash_inset(&mut out, l, indent, width);
    }
    out
}

/// Inset `body` under `indent` and wash its content into the recessed
/// [`CODE_BG`] panel.  The indent stays unwashed so the panel's left edge aligns
/// with the content, but the wash runs to `width` so the region reads as a
/// stratum, not a swatch.
fn wash_inset(out: &mut Vec<Line<'static>>, body: &Line<'static>, indent: &str, width: usize) {
    let indent_w = UnicodeWidthStr::width(indent);
    let body_w = width.saturating_sub(indent_w).max(1);
    for vrow in wrap_line(body, body_w) {
        let mut spans = vec![Span::raw(indent.to_string())];
        spans.extend(wash(vrow, CODE_BG, Some(body_w)).spans);
        out.push(Line::from(spans));
    }
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
                cmd: "",
                magnitude,
                context: 0,
            },
            Tally::default(),
            Vec::new(),
        )
    }

    /// The script paints a left-inset panel: [`BODY_INDENT`] stays unwashed and
    /// every row is washed to the full width — no ragged right edge.
    #[test]
    fn source_rows_paint_an_inset_panel() {
        let c = Call::new(
            CallParts {
                intent: "x",
                cmd: "let x = 1\nlet y = 2",
                magnitude: None,
                context: 0,
            },
            Tally::default(),
            Vec::new(),
        );
        let rows = source_rows(&c, 60);
        assert_eq!(rows.len(), 2);
        for r in &rows {
            let w: usize = r.spans.iter().map(ratatui::prelude::Span::width).sum();
            assert_eq!(w, 60, "panel row padded to full width");
            assert!(
                r.spans.iter().any(|s| s.style.bg == Some(CODE_BG)),
                "the row is washed"
            );
            assert!(
                r.spans
                    .iter()
                    .filter(|s| s.style.bg.is_some())
                    .all(|s| s.style.bg == Some(CODE_BG)),
                "washed cells wear CODE_BG"
            );
        }
    }

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

    /// The head row opens flush and is pinned so the sparkline's last glyph
    /// lands at `bar_col` once the viewport shifts the row by [`RAIL_W`]; the
    /// continuation hangs under the intent, not back under the rail.
    #[test]
    fn live_tip_pins_sparkline_and_hangs_intent() {
        let width = 100;
        let calls = vec![call("short", Some(3)), call(LONG, Some(40))];
        let rows = nonblank(&body(&calls, Reveal::Summary, width));

        let head = &rows[0];
        assert_eq!(indent_of(head), 0);
        assert!(head.starts_with("Reading the definition"));
        assert!(head.ends_with(line::spark_glyph(Some(40))));
        assert_eq!(
            UnicodeWidthStr::width(head.as_str()),
            bar_col(width) - RAIL_W + 1
        );

        assert_eq!(indent_of(&rows[1]), RAIL_W);
    }

    /// A pending call carries no effects, so anchoring the tip on `calls.last()`
    /// would blank the prior call's reads; the tip narrates the latest settled
    /// call while the pending one still adds its bar.
    #[test]
    fn live_tip_anchors_on_latest_settled_call_not_a_pending_one() {
        let width = 100;
        let calls = vec![call("settled read", Some(7)), call("pending grep", None)];
        let rows = nonblank(&body(&calls, Reveal::Summary, width));

        let head = &rows[0];
        assert!(
            head.contains("settled read"),
            "tip narrates the settled call"
        );
        assert!(!head.contains("pending grep"), "not the in-flight call");
        // The pending call still counts toward the sparkline, as its shortest bar.
        assert!(head.ends_with(line::spark_glyph(None)));
        assert_eq!(
            UnicodeWidthStr::width(head.as_str()),
            bar_col(width) - RAIL_W + 1
        );
    }

    /// The run's first intent row is the railed one: it opens flush and targets
    /// `bar_col - RAIL_W`, landing in the same column as every later row, which
    /// carries [`INTENT_INDENT`] and pins to `bar_col` directly.
    #[test]
    fn intent_rows_pin_bars_in_one_column() {
        let width = 100;
        let calls = vec![call(LONG, Some(12)), call("short tail", Some(3))];
        let rows = nonblank(&body(&calls, Reveal::Context, width));

        assert_eq!(indent_of(&rows[0]), 0);
        assert_eq!(
            UnicodeWidthStr::width(rows[0].as_str()),
            bar_col(width) - RAIL_W + 1
        );
        assert_eq!(indent_of(&rows[1]), RAIL_W);

        let tail = rows.last().expect("the second call renders its intent row");
        assert!(tail.trim_start().starts_with("short tail"));
        assert_eq!(indent_of(tail), UnicodeWidthStr::width(INTENT_INDENT));
        assert_eq!(UnicodeWidthStr::width(tail.as_str()), bar_col(width) + 1);
    }
}
