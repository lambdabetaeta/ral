//! Magnitude glyphs: the size bar, the sparkline, the grain run.

use super::palette::SLATE;
use ratatui::{style::Style, text::Span};

/// Width in cells of the header size-bar — the size channel, ordered beside
/// the rail's lightness.
pub(super) const SIZE_BAR_W: usize = 8;

/// Bucket `magnitude` onto a `log2` scale clamped to `0..=cap`: step `0` at
/// zero, one step per doubling.  The same scale [`super::rail::value_step`]
/// tracks, at coarser resolution.
fn log2_step(magnitude: u32, cap: usize) -> usize {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "log2 of a small positive count, then min-clamped"
    )]
    let step = ((magnitude as f32 + 1.0).log2().round() as usize).min(cap);
    step
}

/// Filled cells for `magnitude`, log2-scaled: a 1-line event lights one cell;
/// the bar fills around 180 lines.
fn size_cells(magnitude: u32) -> usize {
    log2_step(magnitude, SIZE_BAR_W)
}

/// The header size-bar: [`SIZE_BAR_W`] cells, `█` filled and `░` empty, in
/// [`SLATE`] so it reads as chrome beside the path or summary, not as content.
pub(super) fn size_bar(magnitude: u32) -> Span<'static> {
    Span::styled(size_bar_text(magnitude), Style::default().fg(SLATE))
}

/// The bar's glyphs alone, for a caller that paints them in its own hue — the
/// metadata matrix's per-agent bar.
pub(super) fn size_bar_text(magnitude: u32) -> String {
    let filled = size_cells(magnitude);
    "█"
        .repeat(filled)
        .chars()
        .chain("░".repeat(SIZE_BAR_W - filled).chars())
        .collect()
}

/// Vertical-sparkline glyphs, lowest to highest — one per call in a coalesced
/// run, so the run reads as a bar chart of what each call moved.
const SPARK_GLYPHS: [char; 8] = ['▁', '▂', '▃', '▄', '▅', '▆', '▇', '█'];

/// One call's sparkline glyph, on [`size_cells`]'s scale at eight steps.  A
/// `None` or empty result still lights the shortest bar, because a call ran.
pub(super) fn spark_glyph(magnitude: Option<u32>) -> char {
    SPARK_GLYPHS[log2_step(magnitude.unwrap_or(0), SPARK_GLYPHS.len() - 1)]
}

/// Width in cells of the header grain run — the grain channel, reading *what
/// kind* of change beside the size-bar's *how much*.
const GRAIN_W: usize = 4;

/// Grain: [`GRAIN_W`] braille cells whose density reads `a / (a + b)` on the
/// ramp `⣿⣶⣤⣀`, in [`SLATE`] so it never collides with a data hue like the
/// `+`/`-` line colours.  Read by the patch header (added over changed) and the
/// thinking header (thought over said — how dearly the answer was bought).
pub(super) fn grain_run(a: u32, b: u32) -> Span<'static> {
    let total = a + b;
    #[allow(
        clippy::cast_precision_loss,
        reason = "changed-line counts far below f32 precision limit"
    )]
    let cell = if total == 0 {
        ' '
    } else {
        grain_cell(a as f32 / total as f32)
    };
    Span::styled(cell.to_string().repeat(GRAIN_W), Style::default().fg(SLATE))
}

fn grain_cell(ratio: f32) -> char {
    #[allow(
        clippy::cast_precision_loss,
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "ratio in [0,1] times a small constant GRAIN_W"
    )]
    let bucket = (ratio * GRAIN_W as f32) as usize;
    match bucket {
        3.. => '⣿',
        2 => '⣶',
        1 => '⣤',
        _ => '⣀',
    }
}
