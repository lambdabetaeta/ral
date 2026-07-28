//! The marginal rail: the left two columns of every block, one cell carrying
//! three variables at once — shape for the block's kind ([`RailKind`]), hue for
//! the producing agent ([`AGENT_HUES`]), lightness for magnitude ([`value_step`]
//! then [`lighten`]).
//!
//! Hue is constant down a tab, since every block of one transcript shares its
//! agent; it is read on a tab-switch, not block to block. The human's prompt
//! fence is the exception, wearing a neutral [`PROMPT_INK`] so it never reads
//! as an agent.

use super::block::AgentSlot;
use super::palette::{AGENT_HUES, PROMPT_INK};
use ratatui::style::{Color, Style};
use ratatui::text::Span;

/// The shape a block wears, one glyph per kind and no two kinds sharing one.
/// Derived from [`super::block::BlockKind`]; chrome's coarser
/// [`super::block::RailShape`] is lifted into this set by `Block::rail_kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RailKind {
    /// A file mutation, diff or whole-file write alike; the body says which.
    Patch,
    /// A tool call, `true` once dialled open to its context.
    ToolCall(bool),
    Markdown,
    /// A thinking trace. The answer it reached is a separate `Markdown` block,
    /// so deliberation and conclusion stay distinct in the rail.
    Thinking,
    Subagent,
    /// `spawn`, `cancel`, `message`, `reply` — an act on the fleet, landing now.
    FleetAct,
    /// `schedule`, `unschedule` — an act that lands on a clock instead.
    TimeAct,
    Step,
    Error,
    /// The human's turn — the one kind not tinted by an agent's hue.
    Prompt,
    /// Any other chrome notice: a model switch, a stall, a compaction.
    Note,
}
impl RailKind {
    /// The shape glyph. Every one is a single display column, so the rail keeps
    /// its fixed 2-column width (glyph plus space) across kinds.
    fn glyph(self) -> &'static str {
        match self {
            Self::Patch => "▎",
            Self::ToolCall(true) => "▽",
            Self::ToolCall(false) => "▸",
            Self::Markdown => "·",
            Self::Thinking => "∴",
            Self::Subagent => "↘",
            Self::FleetAct => "↗",
            Self::TimeAct => "◷",
            Self::Step => "━",
            Self::Error => "╳",
            Self::Prompt => "❖",
            Self::Note => "▪",
        }
    }
}

/// The shape vocabulary with its glosses: both the rows `/legend` draws (in
/// `super::banner`) and the set [`is_rail_prefix`] recognises. A kind left out
/// here is one the rail draws but copy will not strip.
pub(super) const RAIL_SHAPES: &[(RailKind, &str)] = &[
    (RailKind::Patch, "file change — diff or write"),
    (RailKind::ToolCall(false), "tool call, shut"),
    (RailKind::ToolCall(true), "tool call, open"),
    (RailKind::Markdown, "model prose"),
    (RailKind::Thinking, "thinking trace"),
    (RailKind::Subagent, "subagent result"),
    (
        RailKind::FleetAct,
        "fleet act — spawn, cancel, message, reply",
    ),
    (RailKind::TimeAct, "time act — schedule, unschedule"),
    (RailKind::Step, "step boundary"),
    (RailKind::Error, "error"),
    (RailKind::Prompt, "your prompt — the fence"),
    (RailKind::Note, "system note"),
];

/// Bucket a magnitude onto a `0..=3` lightness step, thresholds tracking `log2`
/// of a line count. Absent and tiny both sit at the base hue, so a rail with no
/// magnitude to report reads as the quietest event rather than a missing one.
pub(super) fn value_step(magnitude: Option<u32>) -> u8 {
    match magnitude {
        None => 0,
        Some(n) if n <= 4 => 0,
        Some(n) if n <= 20 => 1,
        Some(n) if n <= 80 => 2,
        _ => 3,
    }
}

/// Channel-wise lerp of `from` toward `to` by `t`, clamped to `0.0..=1.0` — the
/// one definition of colour interpolation, which the value ramp, the fidelity
/// drain in `super::md`, and the picker's effort ramp all route through. A
/// non-RGB `from` passes through unchanged; the palette is RGB, so only themed
/// fallbacks take that path.
pub(super) fn mix(from: Color, to: Color, t: f32) -> Color {
    let (Color::Rgb(fr, fg, fb), Color::Rgb(tr, tg, tb)) = (from, to) else {
        return from;
    };
    let t = t.clamp(0.0, 1.0);
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "explicitly clamped to 0..=255 before cast"
    )]
    #[allow(
        clippy::suboptimal_flops,
        reason = "u8-rounded colour math; mul_add adds no precision and obscures the standard lerp/luma formula"
    )]
    let lerp = |a: u8, b: u8| -> u8 {
        (f32::from(a) + (f32::from(b) - f32::from(a)) * t)
            .round()
            .clamp(0.0, 255.0) as u8
    };
    Color::Rgb(lerp(fr, tr), lerp(fg, tg), lerp(fb, tb))
}

/// Carry `c` toward white by `step / 3` of the remaining distance, so step 3 is
/// white. Hue survives the ramp, so agent identity never collides with value.
pub(super) fn lighten(c: Color, step: u8) -> Color {
    mix(c, Color::Rgb(255, 255, 255), f32::from(step) / 3.0)
}

/// Drain `c` toward the grey of its own Rec. 601 luma by `t`: hue goes, and
/// lightness stays. Holding luminance is the point — this is the fidelity drain
/// `super::md` applies, and it must not disturb the lightness channel magnitude
/// rides, so a degraded passage stays as legible as a sound one.
pub(super) fn desaturate(c: Color, t: f32) -> Color {
    let Color::Rgb(r, g, b) = c else {
        return c;
    };
    #[allow(
        clippy::cast_possible_truncation,
        clippy::cast_sign_loss,
        reason = "Rec.601 luma weights sum to 1.0 over byte-range inputs, result stays in 0..=255"
    )]
    #[allow(
        clippy::suboptimal_flops,
        reason = "u8-rounded colour math; mul_add adds no precision and obscures the standard lerp/luma formula"
    )]
    let luma = (0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)).round() as u8;
    mix(c, Color::Rgb(luma, luma, luma), t)
}

/// True when `s` is one shape glyph and its trailing space — the chrome
/// [`super::line::plain`] strips on copy and [`super::line::wrap_line`] hangs a
/// wrapped line's continuations under.
pub(super) fn is_rail_prefix(s: &str) -> bool {
    s.strip_suffix(' ')
        .is_some_and(|glyph| RAIL_SHAPES.iter().any(|(k, _)| k.glyph() == glyph))
}

/// The 2-column rail cell: the kind's glyph in the agent's hue, lightened by
/// magnitude, then a space. A slot past the end of [`AGENT_HUES`] — callers
/// take the modulus, so it should not arise — falls back to the root's hue.
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

    #[allow(
        clippy::suboptimal_flops,
        reason = "u8-rounded colour math; mul_add adds no precision and obscures the standard lerp/luma formula"
    )]
    fn luma(c: Color) -> f32 {
        let Color::Rgb(r, g, b) = c else {
            unreachable!("test colours are RGB")
        };
        0.299 * f32::from(r) + 0.587 * f32::from(g) + 0.114 * f32::from(b)
    }

    /// The drain holds luminance, keeping the fidelity signal off the lightness
    /// channel magnitude rides — within a rounding step, at any `t`.
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

    #[test]
    fn desaturate_zero_is_identity() {
        assert_eq!(desaturate(AGENT_HUES[0], 0.0), AGENT_HUES[0]);
    }
}
