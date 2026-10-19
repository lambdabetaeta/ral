//! The marginal rail: the left two columns of every block, one cell carrying
//! three variables at once — shape for the block's kind ([`RailKind`]), hue for
//! the producing agent ([`super::palette::AGENT_HUES`]), lightness for magnitude ([`value_step`]
//! then [`lighten`]).
//!
//! Hue is constant down a tab, since every block of one transcript shares its
//! agent; it is read on a tab-switch, not block to block. The human's prompt
//! fence is the exception, wearing a neutral [`PROMPT_INK`] so it never reads
//! as an agent.

use super::palette::{AgentSlot, PROMPT_INK, lighten};
use ratatui::style::Style;
use ratatui::text::Span;

/// The shape a block wears, one glyph per kind and no two kinds sharing one.
/// Derived from [`super::block::BlockKind`]; chrome's own [`super::block::Chrome`]
/// is lifted into this set by `Block::rail_kind`.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum RailKind {
    /// A file mutation, diff or whole-file write alike; the body says which.
    Patch,
    /// A tool call, `true` once dialled open to its context.
    ToolCall(bool),
    Markdown,
    /// The thinking lane. The answer it reached is a separate `Markdown`
    /// block, so deliberation and conclusion stay distinct in the rail.
    Thinking,
    Subagent,
    /// `spawn`, `cancel`, `message`, `reply` — an act on the fleet, landing now.
    FleetAct,
    /// `schedule`, `unschedule` — an act that lands on a clock instead.
    TimeAct,
    Turn,
    Error,
    /// The human's turn — the one kind not tinted by an agent's hue.
    Prompt,
    /// Any other chrome notice: a model switch, a stop reason, an eviction.
    /// A stall is not one of them; it wears [`Self::Error`].
    Note,
}
impl RailKind {
    /// The shape glyph. Every one is a single display column, so the rail keeps
    /// its fixed 2-column width (glyph plus space) across kinds — checked for
    /// the whole vocabulary by `super::row`'s gutter-width test.
    pub(super) fn glyph(self) -> &'static str {
        match self {
            Self::Patch => "▎",
            Self::ToolCall(true) => "▽",
            Self::ToolCall(false) => "▸",
            Self::Markdown => "·",
            Self::Thinking => "∴",
            Self::Subagent => "↘",
            Self::FleetAct => "↗",
            Self::TimeAct => "◷",
            Self::Turn => "━",
            Self::Error => "╳",
            Self::Prompt => "❖",
            Self::Note => "▪",
        }
    }
}

/// The shape vocabulary with its glosses: the rows `/legend` draws (in
/// `super::banner`), and the set `super::row`'s width invariant is checked
/// against.  Copy is no longer coupled to this list — a
/// [`super::row::Row`]'s margin is a field, not a span copy must recognise.
pub(super) const RAIL_SHAPES: &[(RailKind, &str)] = &[
    (RailKind::Patch, "file edit"),
    (RailKind::ToolCall(false), "shell script (collapsed)"),
    (RailKind::ToolCall(true), "shell script (expanded)"),
    (RailKind::Markdown, "assistant text"),
    (RailKind::Thinking, "thinking"),
    (RailKind::Subagent, "agent notification"),
    (
        RailKind::FleetAct,
        "agent action (spawn, message, cancel, reply)",
    ),
    (RailKind::TimeAct, "scheduling"),
    (RailKind::Turn, "turn boundary"),
    (RailKind::Error, "error/stop"),
    (RailKind::Prompt, "user prompt"),
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

/// The 2-column rail cell: the kind's glyph in the agent's hue, lightened by
/// magnitude, then a space. A slot past the end of [`super::palette::AGENT_HUES`] — callers
/// take the modulus, so it should not arise — falls back to the root's hue.
pub(super) fn span(kind: RailKind, agent: AgentSlot, magnitude: Option<u32>) -> Span<'static> {
    let base = match kind {
        RailKind::Prompt => PROMPT_INK,
        _ => agent.hue(),
    };
    let fg = lighten(base, value_step(magnitude));
    Span::styled(format!("{} ", kind.glyph()), Style::default().fg(fg))
}
