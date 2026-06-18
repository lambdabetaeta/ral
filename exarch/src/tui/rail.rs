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

/// Interpolate an RGB colour toward white (255) by `step / 3` of the
/// remaining distance, so step 3 is near-white. Non-RGB colours pass
/// through unchanged (the palette is RGB, so this only matters for
/// themed fallbacks). Brighter = larger magnitude; hue is preserved, so
/// agent identity never collides with the value ramp.
pub(super) fn lighten(c: Color, step: u8) -> Color {
    let Color::Rgb(r, g, b) = c else {
        return c;
    };
    if step == 0 {
        return c;
    }
    let t = (step as f32) / 3.0;
    let lift = |ch: u8| -> u8 {
        let d = 255.0 - ch as f32;
        (ch as f32 + d * t).round().clamp(0.0, 255.0) as u8
    };
    Color::Rgb(lift(r), lift(g), lift(b))
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn value_step_buckets_log2() {
        assert_eq!(value_step(None), 0);
        assert_eq!(value_step(Some(0)), 0);
        assert_eq!(value_step(Some(1)), 0);
        assert_eq!(value_step(Some(4)), 0);
        assert_eq!(value_step(Some(5)), 1);
        assert_eq!(value_step(Some(20)), 1);
        assert_eq!(value_step(Some(21)), 2);
        assert_eq!(value_step(Some(80)), 2);
        assert_eq!(value_step(Some(81)), 3);
        assert_eq!(value_step(Some(500)), 3);
    }

    /// Step 0 is the identity; step 3 pulls each channel most of the way
    /// to white but keeps the hue (R≠G≠B for the pink palette entry, so
    /// identity is non-trivial).
    #[test]
    fn lighten_preserves_hue_and_steps_toward_white() {
        let base = AGENT_HUES[1]; // PINK, R>G>B
        let Color::Rgb(r0, g0, b0) = base else {
            panic!("palette must be RGB");
        };
        assert_eq!(lighten(base, 0), base);
        let Color::Rgb(r3, g3, b3) = lighten(base, 3) else {
            panic!("lighten must return RGB");
        };
        assert!(r3 >= r0 && g3 >= g0 && b3 >= b0, "step 3 must not darken");
        assert!(r3 > r0 || g3 > g0 || b3 > b0, "step 3 must shift toward white");
        // Hue preserved: the channel ordering stays (pink stays red-dominant).
        assert!(r3 >= g3 && r3 >= b3);
    }

    /// A large patch renders a brighter rail glyph than a small one at
    /// the same agent hue — the three-variable contract: same hue
    /// (agent), different value (magnitude).
    #[test]
    fn span_encodes_magnitude_in_value_not_hue() {
        let agent = AgentSlot(0);
        let small = span(RailKind::Patch, agent, Some(2));
        let large = span(RailKind::Patch, agent, Some(500));
        // Same shape glyph, same agent → hue equal, but the large event's
        // foreground is lightened further.
        let s = |sp: Span<'_>| {
            let Style { fg, .. } = sp.style;
            fg
        };
        assert_eq!(s(small.clone()), s(span(RailKind::Patch, agent, Some(2))));
        // The large patch's colour is the small one's lightened by step 3
        // vs step 0, so they differ.
        assert_ne!(s(small), s(large));
    }

    /// Every kind maps to a distinct, single-cell glyph so the rail
    /// thumbnail is unambiguous at one column.
    #[test]
    fn glyphs_are_distinct_and_single_cell() {
        let kinds = [
            RailKind::Patch,
            RailKind::ToolCall(false),
            RailKind::ToolCall(true),
            RailKind::Markdown,
            RailKind::Step,
            RailKind::Error,
            RailKind::Generic,
        ];
        let glyphs: Vec<&str> = kinds.iter().map(|k| k.glyph()).collect();
        let unique: std::collections::HashSet<&str> = glyphs.iter().copied().collect();
        assert_eq!(unique.len(), kinds.len(), "glyphs must be distinct: {glyphs:?}");
        for g in &glyphs {
            assert_eq!(unicode_width::UnicodeWidthStr::width(*g), 1, "{g:?} must be 1 cell");
        }
    }
}
