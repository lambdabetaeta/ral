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

use super::dial::Detail;
use super::gauge;
use super::highlight::highlight_ral;
use super::md;
use super::palette::{self, CODE_BG, EFFECT_BG, RED, SLATE};
use super::text::{self, wash, wrap_line};
use crate::card::{self, Effect, Listing, Role, exec_listing, greps_listing, reads_listing};
use crate::record::{Seq, Verdict};
use ral_core::fact::Grep;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthStr;

/// One call's effects sorted by verb, the one partition both the tally and the
/// rendered rows read.  The facts are deduped on arrival, so a bucket's length
/// *is* its count and never a carried counter.
#[derive(Default)]
struct Buckets<'a> {
    reads: Vec<&'a str>,
    execs: Vec<&'a [String]>,
    greps: Vec<&'a Grep>,
}

/// One observation call as rendered: its verdict's magnitude drives its
/// sparkline bar and a failure reddens its script, the context is the turn's
/// floor, and its effects are the facts themselves, grouped into rows only at
/// render.
///
/// Named by the [`Seq`] of the record that opened it, which is how the result
/// patch addressed to that record finds the bar it earned.
pub(super) struct Call {
    at: Seq,
    intent: String,
    cmd: String,
    verdict: Option<Verdict>,
    context: u8,
    effects: Vec<Effect>,
}

impl Call {
    /// Open a call on its stated intent and the script behind it; its effects
    /// and its result's verdict arrive after.
    pub(super) fn open(at: Seq, intent: String, cmd: String, context: u8) -> Self {
        Self {
            at,
            intent,
            cmd,
            verdict: None,
            context,
            effects: Vec::new(),
        }
    }

    /// Fold one of this call's effects in, dropping a repeat: a call that read
    /// one file twice, ran one argv twice, or searched one pattern in one
    /// scope twice did the one thing worth showing.
    pub(super) fn absorb(&mut self, what: Effect) {
        if !self.effects.iter().any(|seen| same_effect(seen, &what)) {
            self.effects.push(what);
        }
    }

    /// Settle this call with the verdict the fold patched onto it.
    pub(super) fn settle(&mut self, verdict: Verdict) {
        self.verdict = Some(verdict);
    }

    fn magnitude(&self) -> Option<u32> {
        self.verdict.map(|v| v.lines)
    }

    fn failed(&self) -> bool {
        self.verdict.is_some_and(|v| v.failed)
    }

    fn buckets(&self) -> Buckets<'_> {
        let mut b = Buckets::default();
        for what in &self.effects {
            match what {
                Effect::Read(r) => b.reads.push(&r.path),
                Effect::Command(c) => b.execs.push(&c.argv),
                Effect::Grep(g) => b.greps.push(g),
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
fn same_effect(a: &Effect, b: &Effect) -> bool {
    match (a, b) {
        (Effect::Read(p), Effect::Read(q)) => p.path == q.path,
        (Effect::Command(p), Effect::Command(q)) => p.argv == q.argv,
        (Effect::Grep(p), Effect::Grep(q)) => (&p.scope, &p.pattern) == (&q.scope, &q.pattern),
        _ => false,
    }
}

/// One call's effects as rail-less rows at `width`: its reads, then each exec,
/// then its greps, each as a [`Listing`].  The order is fixed rather than the
/// arrival order — the user does not care in what order a burst interleaved,
/// only what the call touched.
fn effect_rows(call: &Call, at: Detail, width: usize) -> Vec<Line<'static>> {
    let Buckets {
        reads,
        execs,
        greps,
    } = call.buckets();
    (!reads.is_empty())
        .then(|| reads_listing(&reads))
        .into_iter()
        .chain(execs.into_iter().map(exec_listing))
        .chain((!greps.is_empty()).then(|| greps_listing(&greps)))
        .flat_map(|l| listing_rows(&l, at, width))
        .collect()
}

/// A listing as `head item…`, with no exec ` → status` tail: a run reads as the
/// commands it ran, and each status rides its own observation.  At `Full` every
/// item hangs under the first; at `Summary`, the items that fit one row and a
/// count of the rest, as a diff shows its peek.
fn listing_rows(l: &Listing, at: Detail, width: usize) -> Vec<Line<'static>> {
    let mut head: Vec<_> = l.head.iter().map(palette::card_span).collect();
    head.push(Span::raw(" "));
    let items: Vec<Vec<Span<'static>>> = l
        .items
        .iter()
        .map(|i| i.iter().map(palette::card_span).collect())
        .collect();
    let n = items.len();
    let sep_w = l.sep.width();
    let head_w: usize = head.iter().map(Span::width).sum();
    // ends[k]: the row's width carrying the first k items.
    let ends: Vec<usize> = std::iter::once(head_w)
        .chain(items.iter().enumerate().scan(head_w, |w, (i, item)| {
            *w += (if i > 0 { sep_w } else { 0 }) + item.iter().map(Span::width).sum::<usize>();
            Some(*w)
        }))
        .collect();
    let more =
        |k: usize| palette::card_span(&card::Span::new(Role::Muted, format!("… +{} more", n - k)));
    let kept = if at >= Detail::Full || ends[n] <= width {
        n
    } else {
        let fit = ends.partition_point(|&e| e <= width).saturating_sub(1);
        (0..=fit)
            .rev()
            .find(|&k| ends[k] + (if k > 0 { sep_w } else { 0 }) + more(k).width() <= width)
            .unwrap_or(0)
    };
    // The count rides as one more item, so it joins on the listing's own separator.
    let shown = items
        .into_iter()
        .take(kept)
        .chain((kept < n).then(|| vec![more(kept)]));
    let mut body = Vec::new();
    for (i, item) in shown.enumerate() {
        if i > 0 {
            body.push(Span::raw(l.sep));
        }
        body.extend(item);
    }
    text::hang(&head, body, width)
}

/// Columns held clear at the right edge, so the per-call bars and the tip's
/// sparkline stack into one comparable column whatever an intent's length.
const BAR_PAD: usize = 4;

/// Most bars the sparkline draws; a longer run keeps only its tail.
const MAX_SPARKLINE: usize = 30;

/// The least gap between an intent and its bar.
const GAP: usize = 2;

/// Every intent opens in the content column the rail glyph addresses; what a
/// call *is made of* — its source, then its effects — steps in by one indent.
const BODY_INDENT: &str = "  ";

fn bar_col(width: usize) -> usize {
    width.saturating_sub(BAR_PAD)
}

/// The run's summed result magnitudes — what the rail's value step encodes.
/// `None` when no call carried a result, and the rail then renders at base hue.
pub(super) fn aggregate_magnitude(calls: &[Call]) -> Option<u32> {
    calls
        .iter()
        .filter_map(Call::magnitude)
        .reduce(|a, b| a + b)
}

/// Render the run's rail-less body at `at`.  [`super::block`] sets the
/// data-encoding rail on the first content row, exactly as for a single part.
/// `calls` is in arrival order and never empty — a run is opened by a call.
pub(super) fn body(calls: &[Call], at: Detail, width: usize) -> Vec<Line<'static>> {
    match at {
        Detail::Tally => tally(calls, width),
        Detail::Summary => live_tip(calls, width),
        Detail::Full => full_list(calls, width),
    }
}

/// `Summary`: the tip call's intent on the head row — the row that wears
/// the rail mark — the whole-run sparkline pinned right, then that call's effects.
fn live_tip(calls: &[Call], width: usize) -> Vec<Line<'static>> {
    // Anchor on the latest *settled* call, not `calls.last()`: a call still in
    // flight has no effects yet, so the tip would blank the previous call's
    // reads for a frame — a flicker.  The pending call still shows as its own
    // bar, so the count stays honest.
    let tip = calls
        .iter()
        .rev()
        .find(|c| c.verdict.is_some())
        .unwrap_or_else(|| calls.last().expect("a run has at least one call"));
    let mut ls = vec![Line::default()];
    ls.extend(pinned_intent(
        &tip.intent,
        tip.context,
        &sparkline(calls),
        width,
    ));
    // The effects open in the intent's own column, so each reads as belonging
    // to the call above it.
    ls.extend(
        effect_rows(tip, Detail::Summary, width)
            .into_iter()
            .map(|l| wash_inset(l, "", width, EFFECT_BG)),
    );
    ls
}

/// `Tally`: the run in one slate line — its calls counted as scripts, its `|>`
/// effects summed by bucket and named by verb.
fn tally(calls: &[Call], width: usize) -> Vec<Line<'static>> {
    let (execs, reads, greps) = calls
        .iter()
        .map(Call::buckets)
        .fold((0, 0, 0), |(e, r, g), b| {
            (e + b.execs.len(), r + b.reads.len(), g + b.greps.len())
        });
    let mut ls = vec![Line::default()];
    ls.extend(wrap_line(
        &Line::from(Span::styled(
            tally_line(calls.len(), execs, reads, greps),
            Style::default().fg(SLATE),
        )),
        width,
    ));
    ls
}

/// "Ran N scripts" always, then the non-empty buckets in fixed order: binaries
/// share the "Ran"; reads and searches bring their own verb.
fn tally_line(scripts: usize, execs: usize, reads: usize, greps: usize) -> String {
    let mut s = format!("Ran {}", count(scripts, "script", "scripts"));
    if execs > 0 {
        let _ = write!(s, ", {}", count(execs, "binary", "binaries"));
    }
    if reads > 0 {
        let _ = write!(s, ", read {}", count(reads, "file", "files"));
    }
    if greps > 0 {
        let _ = write!(s, ", searched {}", count(greps, "time", "times"));
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
        ls.extend(intent_row(call, width));
        ls.extend(source_rows(call, width));
        ls.extend(
            effect_rows(call, Detail::Full, body_w)
                .into_iter()
                .map(|l| wash_inset(l, BODY_INDENT, body_w, EFFECT_BG)),
        );
    }
    ls
}

/// One call's intent rows, with its bar pinned to the shared column.  Every
/// intent opens flush in content space — the margin is the only indent, and the
/// rail glyph the first call wears sits in it.
fn intent_row(call: &Call, width: usize) -> Vec<Line<'static>> {
    pinned_intent(&call.intent, call.context, &bar(call.magnitude()), width)
}

/// Lay one intent out as a left text block with its `bars` pinned right, in
/// content columns: the bars target [`bar_col`] directly.  The `context`
/// floor drains a row's ink, never a bar's height.
fn pinned_intent(
    intent: &str,
    context: u8,
    bars: &Span<'static>,
    width: usize,
) -> Vec<Line<'static>> {
    let bars_left = (bar_col(width) + 1).saturating_sub(bars.width());
    let body_w = bars_left.saturating_sub(GAP).max(8);
    // The intent is work-narration, not the answer: SLATE sets it below the
    // model's prose and gives the context drain a hue to desaturate.
    let mut rows = wrap_line(
        &Line::from(Span::styled(intent.to_string(), Style::default().fg(SLATE))),
        body_w,
    );
    let pad = bars_left.saturating_sub(rows[0].width()).max(GAP);
    rows[0]
        .spans
        .extend([Span::raw(" ".repeat(pad)), bars.clone()]);
    for row in &mut rows {
        md::apply_context(row, context);
    }
    rows
}

/// A call's ral `cmd` at the `Full` rung, syntax-highlighted — or solid red
/// when the run failed — folded to the panel's own columns and washed into
/// the recessed [`CODE_BG`] panel inset under [`BODY_INDENT`].
fn source_rows(call: &Call, width: usize) -> Vec<Line<'static>> {
    let body_w = inset_w(BODY_INDENT, width);
    let lines: Vec<Line<'static>> = if call.failed() {
        call.cmd
            .lines()
            .map(|l| Line::from(Span::styled(l.to_owned(), Style::default().fg(RED))))
            .collect()
    } else {
        highlight_ral(&call.cmd)
    };
    lines
        .iter()
        .flat_map(|l| wrap_line(l, body_w))
        .map(|v| wash_inset(v, BODY_INDENT, body_w, CODE_BG))
        .collect()
}

/// The whole-run sparkline: one [`gauge::spark_glyph`] per call in call order, as
/// one slate span — a bar chart of how much each call moved.
fn sparkline(calls: &[Call]) -> Span<'static> {
    let skip = calls.len().saturating_sub(MAX_SPARKLINE);
    let glyphs: String = calls
        .iter()
        .skip(skip)
        .map(|c| gauge::spark_glyph(c.magnitude()))
        .collect();
    Span::styled(glyphs, Style::default().fg(SLATE))
}

fn bar(magnitude: Option<u32>) -> Span<'static> {
    Span::styled(
        gauge::spark_glyph(magnitude).to_string(),
        Style::default().fg(SLATE),
    )
}

/// The columns a row has left once `indent` is paid — the one subtraction
/// between a body's width and the width its content was laid out at.
fn inset_w(indent: &str, width: usize) -> usize {
    width.saturating_sub(UnicodeWidthStr::width(indent)).max(1)
}

/// Inset `body` under `indent` and wash its content into the recessed `bg`
/// plane: one row in, one row out.  The indent stays unwashed so the panel's
/// left edge aligns with the content, but the wash runs the whole `body_w` so
/// the region reads as a stratum, not a swatch.
fn wash_inset(body: Line<'static>, indent: &str, body_w: usize, bg: Color) -> Line<'static> {
    let mut spans = vec![Span::raw(indent.to_string())];
    spans.extend(wash(body, bg, Some(body_w)).spans);
    Line::from(spans)
}

#[cfg(test)]
mod tests;
