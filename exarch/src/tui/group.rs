//! The `▸` part of a group: one burst of `ral` work as a single dialable
//! object.
//!
//! A [`Call`] is opened by its tool call and grows as that call's effects —
//! reads, greps, execs — land on it; a diff or a write is a barrier that ends
//! the burst and renders as its own always-visible block.  An effect joins
//! the call that issued it whatever barrier landed at the seam between the two
//! (`Scrollback::absorb`), so a burst reads as one run and a call's effects
//! are never stranded past its end.  This module renders the run's body at
//! one of three [`Detail`] rungs:
//!
//! - `Tally` — one line counting the run's `|>` effects by verb.  A run is
//!   the only object that reaches this floor, and only by being dialled
//!   *down* to it.
//! - `Summary` — the latest *settled* call's intent and effects, plus a
//!   sparkline of one bar per call; the bar count stands in for an `×N`.
//! - `Full` — every call: intent, bar, effects, and its ral source.

use std::fmt::Write;

use super::block::Detail;
use super::highlight::highlight_ral;
use super::line::{self, push_wrapped, wash, wrap_line};
use super::md;
use super::palette::{CODE_BG, SLATE};
use crate::bus::card::{execs_card, greps_card, reads_card};
use crate::record::Seq;
use ral_core::types::Observed;
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// The run's `|>` effects by tally bucket, summed over its calls.  A write is a
/// barrier, never a run member, so it has no bucket; the script count is the
/// call count, not a field.
#[derive(Clone, Copy, Default)]
struct Tally {
    binaries: usize,
    files: usize,
    searches: usize,
}

/// One call's effects sorted by verb, the one partition both the tally and the
/// rendered rows read.  The facts are deduped on arrival, so a bucket's length
/// *is* its count and never a carried counter.
#[derive(Default)]
struct Buckets<'a> {
    reads: Vec<&'a str>,
    execs: Vec<&'a Observed>,
    greps: Vec<&'a Observed>,
}

/// One observation call as rendered: the magnitude drives its sparkline bar,
/// the context is the turn's floor, and its effects are the facts themselves,
/// grouped into rows only at render.
///
/// Named by the [`Seq`] of the commit that opened it, which is how the result
/// patch addressed to that commit finds the bar it earned.
pub(super) struct Call {
    at: Seq,
    intent: String,
    cmd: String,
    magnitude: Option<u32>,
    context: u8,
    effects: Vec<Observed>,
}

impl Call {
    /// Open a call on its stated intent and the script behind it; its effects
    /// and its result magnitude arrive after.
    pub(super) fn open(at: Seq, intent: String, cmd: String, context: u8) -> Self {
        Self {
            at,
            intent,
            cmd,
            magnitude: None,
            context,
            effects: Vec::new(),
        }
    }

    /// Fold one of this call's effects in, dropping a repeat: a call that read
    /// one file twice, ran one argv twice, or searched one pattern in one
    /// scope twice did the one thing worth showing.
    pub(super) fn absorb(&mut self, what: Observed) {
        if !self.effects.iter().any(|seen| same_effect(seen, &what)) {
            self.effects.push(what);
        }
    }

    /// Stamp the result magnitude the fold patched onto this call.
    pub(super) fn measure(&mut self, n: u32) {
        self.magnitude = Some(n);
    }

    fn buckets(&self) -> Buckets<'_> {
        let mut b = Buckets::default();
        for what in &self.effects {
            match what {
                Observed::Read { path } => b.reads.push(path),
                Observed::Command { .. } => b.execs.push(what),
                Observed::Grep { .. } => b.greps.push(what),
                _ => {}
            }
        }
        b
    }

    pub(super) fn at(&self) -> Seq {
        self.at
    }
}

/// When two effects are the one fact: a path read again, an argv run again, a
/// pattern searched again in the same scope.
fn same_effect(a: &Observed, b: &Observed) -> bool {
    match (a, b) {
        (Observed::Read { path: p }, Observed::Read { path: q }) => p == q,
        (Observed::Command { argv: p, .. }, Observed::Command { argv: q, .. }) => p == q,
        (
            Observed::Grep {
                scope: s,
                pattern: p,
            },
            Observed::Grep {
                scope: t,
                pattern: q,
            },
        ) => (s, p) == (t, q),
        _ => false,
    }
}

/// One call's effects as rail-less rows at `width`: its reads, then its execs,
/// then its greps, each bucket one comma-joined card.  The order is fixed
/// rather than the arrival order — the user does not care in what order a burst
/// interleaved, only what the call touched.
fn effect_rows(call: &Call, width: usize) -> Vec<Line<'static>> {
    let Buckets {
        reads,
        execs,
        greps,
    } = call.buckets();
    [reads_card(&reads), execs_card(&execs), greps_card(&greps)]
        .into_iter()
        .flatten()
        .flat_map(|card| line::render_card_unframed(&card, width, Detail::Full))
        .collect()
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

/// Render the run's rail-less body at `at`.  [`super::block`] seats the
/// data-encoding rail on the first content row, exactly as for a single part.
/// `calls` is in arrival order and never empty — a run is opened by a call.
pub(super) fn body(calls: &[Call], at: Detail, width: usize) -> Vec<Line<'static>> {
    match at {
        Detail::Tally => tally(calls, width),
        Detail::Summary => live_tip(calls, width),
        Detail::Full => full_list(calls, width),
    }
}

/// `Summary`: the tip call's intent on the head row — the row the scrollback seats
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
    ls.extend(pinned_intent(
        &[],
        &tip.intent,
        tip.context,
        &sparkline(calls),
        width,
    ));
    // The effects open in the intent's own column, so each reads as belonging
    // to the call above it.
    ls.extend(indent_rows(effect_rows(tip, width), "", width));
    ls
}

/// `Tally`: the run in one slate line — its calls counted as scripts, its `|>`
/// effects summed by bucket and named by verb.
fn tally(calls: &[Call], width: usize) -> Vec<Line<'static>> {
    let mut total = Tally::default();
    for call in calls {
        let b = call.buckets();
        total.binaries += b.execs.len();
        total.files += b.reads.len();
        total.searches += b.greps.len();
    }
    let text = tally_line(calls.len(), total);
    let mut ls = vec![Line::default()];
    push_wrapped(&mut ls, &text, width, |chunk, _| {
        Line::from(Span::styled(chunk, Style::default().fg(SLATE)))
    });
    ls
}

/// "Ran N scripts" always, then the non-empty buckets in fixed order — binaries
/// share the "Ran"; reads and searches bring their own verb.
fn tally_line(scripts: usize, tally: Tally) -> String {
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

fn count(n: usize, singular: &str, plural: &str) -> String {
    format!("{n} {}", if n == 1 { singular } else { plural })
}

/// `Full`: every call as its own intent and right-aligned bar, its ral `cmd`
/// below that, and its effects below that.
fn full_list(calls: &[Call], width: usize) -> Vec<Line<'static>> {
    let body_w = inset_w(BODY_INDENT, width);
    let mut ls = vec![Line::default()];
    for (i, call) in calls.iter().enumerate() {
        if i > 0 {
            ls.push(Line::default());
        }
        ls.extend(intent_row(call, i == 0, width));
        ls.extend(source_rows(call, width));
        ls.extend(indent_rows(effect_rows(call, body_w), BODY_INDENT, body_w));
    }
    ls
}

/// One call's intent rows, wrapped under a hanging indent with its bar pinned to
/// the shared column.  The `railed` row is the one the scrollback seats the glyph
/// on, so it drops its own indent and lets the margin be its indent.
fn intent_row(call: &Call, railed: bool, width: usize) -> Vec<Line<'static>> {
    let lead: Vec<Span<'static>> = if railed {
        Vec::new()
    } else {
        vec![Span::raw(INTENT_INDENT)]
    };
    pinned_intent(
        &lead,
        &call.intent,
        call.context,
        &bar(call.magnitude),
        width,
    )
}

/// Lay one intent out as a left text block with its `bars` pinned right, in
/// content columns: `lead` is the row's whole indent and the bars target
/// [`bar_col`] directly.  The `context` floor drains a row's ink, never a bar's
/// height.
fn pinned_intent(
    lead: &[Span<'static>],
    intent: &str,
    context: u8,
    bars: &Span<'static>,
    width: usize,
) -> Vec<Line<'static>> {
    let lead_w: usize = lead.iter().map(Span::width).sum();
    let cont_indent = lead_w;
    let bar_last = bar_col(width);
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

/// A call's ral `cmd` at the `Full` rung, syntax-highlighted, folded to the
/// panel's own columns and washed into the recessed [`CODE_BG`] panel inset
/// under [`BODY_INDENT`].
fn source_rows(call: &Call, width: usize) -> Vec<Line<'static>> {
    let body_w = inset_w(BODY_INDENT, width);
    let mut ls = Vec::new();
    for line in highlight_ral(&call.cmd) {
        for vrow in wrap_line(&line, body_w) {
            wash_inset(&mut ls, vrow, BODY_INDENT, body_w);
        }
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

/// The columns a row has left once `indent` is paid — the one subtraction
/// between a body's width and the width its content was laid out at.
fn inset_w(indent: &str, width: usize) -> usize {
    width.saturating_sub(UnicodeWidthStr::width(indent)).max(1)
}

/// Re-indent a call's effect rows — dropping the leading blank
/// [`line::render_card_unframed`] opens with — and wash each into the
/// [`CODE_BG`] panel at `indent`; the list passes the script's own margin, so
/// the two read as one rectangle.  `body_w` is the width the rows were built
/// at, so each is already one visual row.
fn indent_rows(rows: Vec<Line<'static>>, indent: &str, body_w: usize) -> Vec<Line<'static>> {
    let mut out = Vec::new();
    for l in rows.into_iter().filter(|l| !line::is_blank(l)) {
        wash_inset(&mut out, l, indent, body_w);
    }
    out
}

/// Inset `body` under `indent` and wash its content into the recessed
/// [`CODE_BG`] panel: one row in, one row out.  The indent stays unwashed so
/// the panel's left edge aligns with the content, but the wash runs the whole
/// `body_w` so the region reads as a stratum, not a swatch.
fn wash_inset(out: &mut Vec<Line<'static>>, body: Line<'static>, indent: &str, body_w: usize) {
    let mut spans = vec![Span::raw(indent.to_string())];
    spans.extend(wash(body, CODE_BG, Some(body_w)).spans);
    out.push(Line::from(spans));
}

#[cfg(test)]
mod tests {
    use super::*;

    fn call(intent: &str, magnitude: Option<u32>) -> Call {
        let mut call = Call::open(Seq::new(1), intent.into(), String::new(), 0);
        if let Some(n) = magnitude {
            call.measure(n);
        }
        call
    }

    /// The script paints a left-inset panel: [`BODY_INDENT`] stays unwashed and
    /// every row is washed to the full width — no ragged right edge.
    #[test]
    fn source_rows_paint_an_inset_panel() {
        let c = Call::open(
            Seq::new(1),
            "x".into(),
            "let x = 1\nlet y = 2".into(),
            0,
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

    #[test]
    fn live_tip_anchors_on_latest_settled_call_not_a_pending_one() {
        let width = 100;
        let calls = vec![call("settled read", Some(7)), call("pending grep", None)];
        let rows = nonblank(&body(&calls, Detail::Summary, width));

        let head = &rows[0];
        assert!(
            head.contains("settled read"),
            "tip narrates the settled call"
        );
        assert!(!head.contains("pending grep"), "not the in-flight call");
        // The pending call still counts toward the sparkline, as its shortest bar.
        assert!(head.ends_with(line::spark_glyph(None)));
    }
}
