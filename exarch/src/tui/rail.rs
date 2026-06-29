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
//! Hue is a per-*view* tint, not a per-block variable: within one transcript
//! every block shares the tab's agent, so the whole rail glows that agent's
//! hue — constant here by design, and read on a tab-switch as "whose
//! transcript is this". The human's prompt fence is the exception, wearing a
//! neutral [`super::line::PROMPT_INK`] so it never reads as an agent.
//!
//! The rail is the keystone of the "transcript as graphic" re-encoding
//! ([[decisions/260618_tui-transcript-as-graphic]]): the variables live
//! per-`Block` rather than woven into prose, so the session's shape
//! reads at a glance and every later projection (matrix, codebase map)
//! composes on the same substrate.

use super::block::AgentSlot;
use super::line::{AGENT_HUES, PROMPT_INK};
use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// The rail glyph a block wears, derived from its [`super::block::BlockKind`].
/// Each variant maps to one shape cell, and every kind has its own — the
/// shape names the kind, with no two kinds sharing a glyph. Chrome's coarse
/// [`RailShape`] (`Step` / `Error` / `ToolCall`) is lifted into this set by
/// the block.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RailKind {
    /// A file mutation — the change-bar `▎`. Both an `edit`'s located diff
    /// (dialable to its hunks) and a whole-file write (atomic, a one-line
    /// summary) wear it; the body, not the shape, says which.
    Patch,
    /// A tool call, open or shut — the disclosure triangle *is* the
    /// tool-call shape, so no separate `◆`. A summary-less call (`fff`, an
    /// invalid-input header) is the shut triangle, inert: there is nothing
    /// to dial, but a tool call is a tool call.
    ToolCall(bool),
    Markdown,
    /// A model thinking trace — the `∴` therefore shape, dialable as its own
    /// block. The answer it produced remains a separate plain prose `·`
    /// block, so deliberation and conclusion stay distinct in the rail.
    Thinking,
    /// An async subagent's landed result — the `↘` delegated-result shape.
    Subagent,
    Step,
    Error,
    /// The human turn's fence — a `❖` in the human's [`PROMPT_INK`], beside
    /// the raised band, so the rail thumbnail still shows where each turn
    /// opens. The sole wearer of `❖`, so the glyph reads as "the human".
    Prompt,
}

impl RailKind {
    /// The single-cell shape glyph. Every glyph is one display column so
    /// the rail stays a fixed 2-col width (glyph + space) across kinds.
    fn glyph(self) -> &'static str {
        match self {
            RailKind::Patch => "▎",
            RailKind::ToolCall(true) => "▽",
            RailKind::ToolCall(false) => "▸",
            RailKind::Markdown => "·",
            RailKind::Thinking => "∴",
            RailKind::Subagent => "↘",
            RailKind::Step => "━",
            RailKind::Error => "╳",
            RailKind::Prompt => "❖",
        }
    }
}

/// The shape vocabulary, each variant paired with the block kind it names —
/// the row source the `/legend` panel draws its shape samples from (via
/// [`span`]), so the legend enumerates the same set the rail dispatches on
/// and can never list a glyph the rail does not draw. A shut / open tool
/// call is one shape under disclosure, so both triangles appear.
pub(super) const RAIL_SHAPES: &[(RailKind, &str)] = &[
    (RailKind::Patch, "file change — diff or write"),
    (RailKind::ToolCall(false), "tool call, shut"),
    (RailKind::ToolCall(true), "tool call, open"),
    (RailKind::Markdown, "model prose"),
    (RailKind::Thinking, "thinking trace"),
    (RailKind::Subagent, "subagent result"),
    (RailKind::Step, "step boundary"),
    (RailKind::Error, "error"),
    (RailKind::Prompt, "your prompt — the fence"),
];

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

/// Drain an RGB colour's saturation toward grey by `t` (clamped to
/// `0.0..=1.0`), holding its luminance: mix toward the grey of the
/// colour's own Rec. 601 luma, so the result keeps its lightness but loses
/// its hue. This is the fidelity drain ([`super::md`]) — distrust reads as
/// "the colour has gone out of it" without touching the value (lightness)
/// channel magnitude rides, so a degraded passage stays as legible as a
/// sound one. Non-RGB colours pass through unchanged.
pub(super) fn desaturate(c: Color, t: f32) -> Color {
    let Color::Rgb(r, g, b) = c else {
        return c;
    };
    let luma = (0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32).round() as u8;
    mix(c, Color::Rgb(luma, luma, luma), t)
}

/// Build the 2-column rail span — one shape glyph in the producing agent's
/// hue, lightened by its magnitude's value-step, then a space. The human's
/// prompt fence wears its own [`PROMPT_INK`] so it never reads as an agent.
/// One cell, three variables: shape, hue, and value.
pub(super) fn span(kind: RailKind, agent: AgentSlot, magnitude: Option<u32>) -> Span<'static> {
    let base = match kind {
        RailKind::Prompt => PROMPT_INK,
        _ => AGENT_HUES
            .get(agent.0 as usize)
            .copied()
            .unwrap_or(AGENT_HUES[0]),
    };
    let fg = lighten(base, value_step(magnitude));
    Span::styled(format!("{} ", kind.glyph()), Style::default().fg(fg))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn luma(c: Color) -> f32 {
        let Color::Rgb(r, g, b) = c else {
            unreachable!("test colours are RGB")
        };
        0.299 * r as f32 + 0.587 * g as f32 + 0.114 * b as f32
    }

    /// The drain holds luminance — the property that keeps the fidelity
    /// signal off the value (lightness) channel magnitude rides. A fully
    /// drained colour collapses to its own grey; a partially drained one
    /// stays within a rounding step of its original luma.
    #[test]
    fn desaturate_holds_luminance() {
        for &c in &AGENT_HUES {
            let before = luma(c);
            assert!(
                (luma(desaturate(c, 0.45)) - before).abs() <= 1.0,
                "partial drain shifted luma of {c:?}"
            );
            let full = desaturate(c, 1.0);
            assert!(
                (luma(full) - before).abs() <= 1.0,
                "full drain shifted luma"
            );
            let Color::Rgb(r, g, b) = full else {
                unreachable!()
            };
            assert_eq!((r, r), (g, b), "full drain is grey");
        }
    }

    /// `t == 0` is a no-op: a sound passage's ink is untouched.
    #[test]
    fn desaturate_zero_is_identity() {
        assert_eq!(desaturate(AGENT_HUES[0], 0.0), AGENT_HUES[0]);
    }
}
