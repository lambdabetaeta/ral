//! The data-encoding marginal rail.
//!
//! The left two columns of every block are re-projected as a scannable
//! thumbnail of the session: one cell carries three of Bertin's visual
//! variables at once —
//!
//! - **shape** (associative) → block *kind*, via [`RailKind`];
//! - **hue** (associative) → the *producing agent*, via [`AGENT_HUES`];
//! - **value** (the ordered lightness ramp) → *magnitude*, via
//!   [`value_step`] + [`lighten`].
//!
//! The rail is the keystone of the "transcript as graphic" re-encoding
//! ([[decisions/260618_tui-transcript-as-graphic]]): the variables live
//! per-`Block` rather than woven into prose, so the session's shape
//! reads at a glance and every later projection (matrix, codebase map)
//! composes on the same substrate.

use super::block::AgentSlot;
use super::line::AGENT_HUES;
use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// The rail glyph a block wears, derived from its [`super::block::BlockKind`].
/// Each variant maps to one shape cell; chrome's coarse [`RailShape`]
/// (`Step` / `Error` / `Generic`) is lifted into this set by the block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RailKind {
    Patch,
    /// A tool call, open or shut — the disclosure triangle *is* the
    /// tool-call shape, so no separate `◆`.
    ToolCall(bool),
    Markdown,
    /// An async subagent's landed result — the `↘` delegated-result shape.
    Subagent,
    Step,
    Error,
    Generic,
}

impl RailKind {
    /// The single-cell shape glyph. Every glyph is one display column so
    /// the rail stays a fixed 2-col width (glyph + space) across kinds.
    fn glyph(self) -> &'static str {
        match self {
            RailKind::Patch => "▎",
            RailKind::ToolCall(true) => "▾",
            RailKind::ToolCall(false) => "▸",
            RailKind::Markdown => "·",
            RailKind::Subagent => "↘",
            RailKind::Step => "━",
            RailKind::Error => "✗",
            RailKind::Generic => "❖",
        }
    }
}

/// Bucket a magnitude into a `0..=3` lightness step: `None` and tiny
/// changes read at the base hue (step 0); larger events step toward
/// white so brighter rail = larger event, comparable across the whole
/// buffer at rest. The thresholds roughly track `log2` of line count.
pub(super) fn value_step(magnitude: Option<u32>) -> u8 {
    match magnitude {
        None | Some(0) => 0,
        Some(n) if n <= 4 => 0,
        Some(n) if n <= 20 => 1,
        Some(n) if n <= 80 => 2,
        _ => 3,
    }
}

/// Linearly interpolate `from` toward `to` by `t` (clamped to
/// `0.0..=1.0`), channel by channel. The value ramp ([`lighten`]) and the
/// fidelity modulations ([`super::md`]) both express themselves in terms
/// of it, so colour interpolation has one definition. A non-RGB `from`
/// passes through unchanged (the palette is RGB, so this only matters for
/// themed fallbacks).
pub(super) fn mix(from: Color, to: Color, t: f32) -> Color {
    let (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) = (from, to) else {
        return from;
    };
    let t = t.clamp(0.0, 1.0);
    let lerp = |a: u8, b: u8| -> u8 {
        (a as f32 + (b as f32 - a as f32) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::Rgb(lerp(fr, tr), lerp(fg, tg), lerp(fb, tb))
}

/// Interpolate an RGB colour toward white (255) by `step / 3` of the
/// remaining distance, so step 3 is white. Non-RGB colours pass through
/// unchanged. Brighter = larger magnitude; hue is preserved on the way,
/// so agent identity never collides with the value ramp.
pub(super) fn lighten(c: Color, step: u8) -> Color {
    mix(c, Color::Rgb(255, 255, 255), step as f32 / 3.0)
}

/// Build the 2-column rail span — one shape glyph styled with the
/// agent's hue lightened by its magnitude's value-step, then a space.
/// This is the keystone: one cell, three variables.
pub(super) fn span(kind: RailKind, agent: AgentSlot, magnitude: Option<u32>) -> Span<'static> {
    let hue = AGENT_HUES
        .get(agent.0 as usize)
        .copied()
        .unwrap_or(AGENT_HUES[0]);
    let fg = lighten(hue, value_step(magnitude));
    Span::styled(format!("{} ", kind.glyph()), Style::default().fg(fg))
}

