//! The status rule under the transcript: two value-ramp bars — elapsed wait and
//! context fill — around the phase label, with usage right-aligned. `/legend` in
//! `super::banner` draws the same two bars as samples.

use super::line::usage_text;
use super::palette::{CYAN, PURPLE, SLATE};
use super::rail;
use crate::provider::Usage;
use ratatui::style::{Color, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;
use unicode_width::UnicodeWidthStr;

#[allow(clippy::too_many_arguments)]
pub(super) fn rule_line(
    width: usize,
    phase: Option<&str>,
    wait_elapsed: Option<Duration>,
    scroll_pct: Option<u16>,
    usage: &Usage,
    last_input: u64,
    context_window: Option<u64>,
    status_model: &str,
) -> Line<'static> {
    let mut spans: Vec<Span<'static>> = Vec::new();

    // The `Ns` digit ticks once a second: a bar that stops growing is a wedged turn.
    let elapsed = wait_elapsed.unwrap_or(Duration::ZERO);
    spans.extend(wait_bar(elapsed));
    let mut label = match phase {
        Some(p) => format!("{p}… "),
        None => String::new(),
    };
    // Pad by display width: `…` is three bytes and one column, so `label.len()`
    // would leave the slot short and shift every field after it.
    let label_w = UnicodeWidthStr::width(label.as_str());
    label.push_str(&" ".repeat(PHASE_SLOT_W.saturating_sub(label_w)));
    spans.push(Span::styled(label, Style::default().fg(SLATE)));
    spans.push(Span::styled(
        if status_model.is_empty() {
            "…".to_owned()
        } else {
            status_model.to_owned()
        },
        Style::default().fg(SLATE),
    ));
    spans.push(Span::styled(" · ", Style::default().fg(SLATE)));

    // `None` is a catalog miss in `provider::pricing`, not a zero-size window:
    // drop the segment rather than ramp against a guessed denominator.
    if let Some(cap) = context_window
        && cap > 0
    {
        #[allow(
            clippy::cast_possible_truncation,
            clippy::cast_sign_loss,
            clippy::cast_precision_loss,
            reason = "float division of counts, then rounded and min-clamped; cast saturates regardless"
        )]
        let pct = ((last_input as f64 / cap as f64) * 100.0).round() as u64;
        let pct = pct.min(999);
        spans.extend(ctx_ramp(pct));
        spans.push(Span::styled(format!(" {pct}%"), Style::default().fg(SLATE)));
    }

    // A magnitude in a fixed slot, standing in for a right-margin scrollbar;
    // absent when the whole buffer fits, which is what `None` means here.
    if let Some(pct) = scroll_pct {
        let text = if pct >= 100 {
            " · ⇣ end ".to_string()
        } else {
            format!(" · ⇣ {pct}% ")
        };
        spans.push(Span::styled(text, Style::default().fg(SLATE)));
    }

    let left_w: usize = spans.iter().map(Span::width).sum();
    let right = usage_text(usage);
    let rw: usize = right.iter().map(Span::width).sum();
    let gap = width.saturating_sub(left_w + rw);
    if gap > 0 {
        spans.push(Span::styled(" ".repeat(gap), Style::default().fg(SLATE)));
    }
    spans.extend(right);
    Line::from(spans)
}

/// Width of the ctx% value-ramp bar, in cells.
pub(super) const CTX_BAR_W: usize = 10;

/// The body both bars share: `filled` lit cells in `fill_col`, the rest dim slate.
fn bar_cells(filled: usize, bar_w: usize, fill_col: Color) -> Vec<Span<'static>> {
    let mut spans = Vec::with_capacity(bar_w);
    for _ in 0..filled {
        spans.push(Span::styled("█", Style::default().fg(fill_col)));
    }
    for _ in filled..bar_w {
        spans.push(Span::styled("░", Style::default().fg(SLATE)));
    }
    spans
}

/// The ctx% bar: [`CTX_BAR_W`] cells filled and lightened by `pct`, so near-full
/// glows. Lightness runs through [`rail::value_step`], so the bar and the
/// marginal rail share one value scale.
pub(super) fn ctx_ramp(pct: u64) -> Vec<Span<'static>> {
    let pct = pct.min(100) as usize;
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "pct already clamped to 0..=100, result additionally clamped to CTX_BAR_W"
    )]
    let filled = ((pct as f64 / 100.0) * CTX_BAR_W as f64).round() as usize;
    let filled = filled.min(CTX_BAR_W);
    #[allow(
        clippy::cast_possible_truncation,
        reason = "pct already clamped to 0..=100"
    )]
    let pct_u32 = pct as u32;
    let fill_col = rail::lighten(CYAN, rail::value_step(Some(pct_u32)));
    let mut spans = vec![Span::styled("ctx ", Style::default().fg(SLATE))];
    spans.extend(bar_cells(filled, CTX_BAR_W, fill_col));
    spans.push(Span::styled(" ", Style::default().fg(SLATE)));
    spans
}

/// Width of the elapsed-wait bar, in cells.
pub(super) const WAIT_BAR_W: usize = 10;
pub(super) const PHASE_SLOT_W: usize = 16; // fixed phase-label slot: stops status-line jitter
/// Elapsed seconds to a `0..=3` lightness step. Not [`rail::value_step`], whose
/// 4/20/80 thresholds are calibrated for line counts and would burn this bar
/// white on nearly every turn.
pub(super) fn wait_step(secs: u64) -> u8 {
    match secs {
        0..=9 => 0,
        10..=19 => 1,
        20..=29 => 2,
        _ => 3,
    }
}

/// The elapsed-wait bar: [`WAIT_BAR_W`] cells growing with the current phase's
/// seconds and lightening as the wait drags, then a ` Ns ` readout. [`PURPLE`]
/// rather than the ctx ramp's cyan, so the two bars stay apart.
pub(super) fn wait_bar(elapsed: Duration) -> Vec<Span<'static>> {
    let secs = elapsed.as_secs();
    // log2 fill, scaled so a minute reaches the edge: 0s → 0 cells, 16s → ~7.
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        clippy::cast_precision_loss,
        reason = "log2 of a small positive count, then min-clamped"
    )]
    let filled = ((((secs + 1) as f64).log2() * 1.7).round() as usize).min(WAIT_BAR_W);
    let fill_col = rail::lighten(PURPLE, wait_step(secs));
    let mut spans = bar_cells(filled, WAIT_BAR_W, fill_col);
    spans.push(Span::styled(
        format!(" {secs}s "),
        Style::default().fg(SLATE),
    ));
    spans
}
