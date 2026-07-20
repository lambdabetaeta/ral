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

    // ── elapsed-wait bar ──────────────────────────────────────────────
    // A single bar that grows with the current phase's elapsed wall-time
    // and resets when the next phase starts. Size and value both encode
    // elapsed (see `wait_bar`): a snappy phase is a short dim stub, a
    // dragging one a long bright bar — so the row differs turn to turn and
    // the exception flares rather than the constant baseline. The `Ns`
    // digit ticks once per second: a calm, unmistakable liveness signal,
    // and the bar ceasing to grow means the turn has wedged.
    let elapsed = wait_elapsed.unwrap_or(Duration::ZERO);
    spans.extend(wait_bar(elapsed));
    // ── phase label (fixed-width slot) ─────────────────────────────────
    let mut label = match phase {
        Some(p) => format!("{p}… "),
        None => String::new(),
    };
    // Pad by display width, not byte length: the `…` is 3 bytes but one
    // column, so padding by `label.len()` would leave the slot 2 columns
    // short and shift every field after it — the jitter the fixed slot exists
    // to prevent.
    let label_w = UnicodeWidthStr::width(label.as_str());
    label.push_str(&" ".repeat(PHASE_SLOT_W.saturating_sub(label_w)));
    spans.push(Span::styled(label, Style::default().fg(SLATE)));
    // ── status model ──────────────────────────────────────────────────
    spans.push(Span::styled(
        if status_model.is_empty() {
            "…".to_owned()
        } else {
            status_model.to_owned()
        },
        Style::default().fg(SLATE),
    ));
    spans.push(Span::styled(" · ", Style::default().fg(SLATE)));

    // ── ctx% value-ramp ───────────────────────────────────────────────
    // A fixed-width lightness ramp: filled cells step toward white as
    // `last_input / context_window` approaches 1.0, empty cells dim
    // slate.  The eye reads the fill level and notices the approach to
    // full; the `N%` digit stays as a precise readout after the bar.
    // `context_window = None` → no ctx segment at all (as today).
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
        spans.push(Span::styled(
            format!(" {pct}%"),
            Style::default().fg(SLATE),
        ));
    }

    // ── scroll position ───────────────────────────────────────────────
    // Where the window sits in the scrollback, as a fixed-position value —
    // the deleted right-margin scrollbar's datum, re-encoded as a magnitude
    // the doctrine permits.  `⇣ bot` at the tail, `⇣ N%` above it; absent
    // when the whole buffer fits.
    if let Some(pct) = scroll_pct {
        let text = if pct >= 100 {
            " · ⇣ end ".to_string()
        } else {
            format!(" · ⇣ {pct}% ")
        };
        spans.push(Span::styled(text, Style::default().fg(SLATE)));
    }

    // ── usage (right-aligned) ─────────────────────────────────────────
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

/// A value-ramp bar's cell run: `filled` lit cells in `fill_col`, the rest
/// dim slate — the shared body of the ctx and wait bars, so the two ramps
/// paint their cells one way.
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

/// Build the ctx% value-ramp: [`CTX_BAR_W`] cells whose filled run is
/// lightened toward white by [`rail::value_step`] of the percentage (so
/// near-full glows), the rest dim slate.  Reuses the rail's ramp so the bar
/// and the marginal rail share one value scale.
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
/// Bucket whole seconds of elapsed phase time into a `0..=3` value step
/// for the wait bar's colour: a normal sub-10s phase stays dim, a
/// dragging one flares toward white past ~30s. Deliberately distinct
/// from [`rail::value_step`], which is calibrated for line counts
/// (4/20/80) — feeding that ramp milliseconds is what saturated the old
/// duration ribbon to white on every turn.
pub(super) fn wait_step(secs: u64) -> u8 {
    match secs {
        0..=9 => 0,
        10..=19 => 1,
        20..=29 => 2,
        _ => 3,
    }
}

/// Build the elapsed-wait bar: [`WAIT_BAR_W`] cells whose filled run
/// grows on a `log2` scale with the current phase's elapsed seconds
/// (empty at 0s, ~7 cells near 16s, full near a minute), then a ` Ns `
/// readout. The fill colour is [`PURPLE`] lightened by [`wait_step`] —
/// dim while the wait is normal, bright when it drags — so size and
/// value agree, reusing the rail's [`rail::lighten`] ramp. PURPLE (not
/// the ctx ramp's CYAN) keeps the two bottom bars visually distinct.
pub(super) fn wait_bar(elapsed: Duration) -> Vec<Span<'static>> {
    let secs = elapsed.as_secs();
    // log2 fill, scaled so a minute-long wait reaches the right edge:
    // 0s → 0 cells, 3s → ~3, 16s → ~7, ~60s → full.
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
