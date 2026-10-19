//! Colour and width constants shared by the line-builders ([`super::line`])
//! and the sibling render modules, kept apart so they read as one table.

use crate::card::{Role, Span as CardSpan};
use ratatui::{
    style::{Color, Modifier, Style},
    text::Span,
};
use unicode_width::UnicodeWidthStr;

// ── Color palette ────────────────────────────────────────────────────────────

/// Muted chrome hues; card spans reach them through a nominal
/// [`crate::card::Role`] rather than naming a colour.
pub(super) const CYAN: Color = Color::Rgb(135, 200, 215);
pub(super) const LIME: Color = Color::Rgb(165, 210, 155);
pub(super) const PURPLE: Color = Color::Rgb(175, 145, 210);
pub(super) const ORANGE: Color = Color::Rgb(215, 145, 115);
pub(super) const RED: Color = Color::Rgb(215, 110, 125);
pub(super) const SLATE: Color = Color::Rgb(140, 150, 170);
/// Brighter [`LIME`]/[`RED`] for a diff row's changed run, set against the
/// dimmed base hue of the unchanged remainder.
pub(super) const LIME_HOT: Color = Color::Rgb(196, 240, 182);
pub(super) const RED_HOT: Color = Color::Rgb(242, 142, 158);
/// A faint raised plane saying "queued, not yet delivered"; the prompt itself
/// still renders through the normal rail/fence/body path.
pub(super) const QUEUED_PROMPT_BG: Color = Color::Rgb(72, 78, 94);
/// Every modal overlay's plane ([`super::overlay`]) — the one areal mark that
/// means the modal holds the focus.
pub(super) const OVERLAY_BG: Color = Color::Rgb(28, 34, 66);
/// The silhouette of text a pending `/rewind` would cut: one ink for every
/// span, margin included, so the doomed stretch reads as a shape and nothing
/// in it competes with the kept text above.
pub(super) const GHOST: Color = Color::Rgb(66, 72, 90);
/// The human's ink — prompt body and the `❖` fence in the rail.  Neutral where
/// the agents own hues, so a prompt never aliases an agent's mark.
pub(super) const PROMPT_INK: Color = Color::Rgb(170, 180, 200);
/// The recessed machine-text panel: an areal mark, so background here means
/// "machine", as against the model's prose and the human's fence.
pub(super) const CODE_BG: Color = Color::Rgb(36, 38, 46);
/// A call's observed effects sit one stratum below its source: same areal
/// meaning, a darker plane, so what the script *was* and what it *did* are
/// told apart without a rule between them.
pub(super) const EFFECT_BG: Color = Color::Rgb(28, 30, 37);

/// Syntax inks for ral code washed into [`CODE_BG`] ([`super::highlight`]),
/// held apart from the chrome roles, [`PROMPT_INK`] and [`AGENT_HUES`] so a
/// token's colour never aliases a semantic one.  Punctuation reuses [`SLATE`];
/// every other token keeps the default white.
pub(super) const CODE_KEYWORD: Color = Color::Rgb(168, 154, 208);
pub(super) const CODE_STRING: Color = Color::Rgb(150, 186, 146);
pub(super) const CODE_VARIABLE: Color = Color::Rgb(206, 166, 130);
pub(super) const CODE_TAG: Color = Color::Rgb(202, 150, 178);
/// One hue per producing agent, indexed by [`AgentSlot`]; root
/// keeps [`CYAN`].  Hue is the sole identity channel — shape already carries
/// kind and value carries magnitude — so the six also descend an `L*` ladder
/// (≈77 → 47) to stay separable under deuteranopia and protanopia, which hue
/// alone is not.  A dedicated set: agent identity must never alias a semantic
/// colour such as [`RED`].
pub(super) const AGENT_AMBER: Color = Color::Rgb(230, 175, 90);
pub(super) const AGENT_MAGENTA: Color = Color::Rgb(205, 120, 190);
pub(super) const AGENT_BLUE: Color = Color::Rgb(95, 140, 225);
pub(super) const AGENT_OLIVE: Color = Color::Rgb(150, 130, 70);
pub(super) const AGENT_PLUM: Color = Color::Rgb(135, 95, 165);
pub(super) const AGENT_HUES: [Color; 6] = [
    CYAN,
    AGENT_AMBER,
    AGENT_MAGENTA,
    AGENT_BLUE,
    AGENT_OLIVE,
    AGENT_PLUM,
];

/// Index into [`AGENT_HUES`], wrapping: root is `0`, each
/// subagent the next slot at birth.  Carried by value, so the rail needs no
/// lookup on `App`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct AgentSlot(pub u8);

impl AgentSlot {
    pub(super) fn hue(self) -> Color {
        AGENT_HUES
            .get(usize::from(self.0))
            .copied()
            .unwrap_or(AGENT_HUES[0])
    }
}

/// The startup banner's wordmark and eagle — the only saturated ink in the app,
/// so nothing in the session below competes with the splash.
pub(super) const BANNER_GOLD: Color = Color::Rgb(255, 210, 74);
pub(super) const BANNER_CRIMSON: Color = Color::Rgb(224, 20, 60);

// ── Layout constants ─────────────────────────────────────────────────────────

/// A column that rows share: its width in display cells, and the padding that
/// sets a cell in it.  A column is either *declared* ([`Col::wide`], where the
/// rows arrive one at a time and could only align with themselves) or
/// *measured* ([`Col::of`], where they arrive together); the two differ in
/// provenance alone, so every gutter in the TUI that holds a cell is one of
/// these and none pads by hand.  Pure indentation — the rail margin, a card's
/// inset — is not a column: it holds nothing.
///
/// Padding is by display width, where `format!("{cell:<w$}")` counts chars and
/// leaves a row carrying a wide glyph short of its neighbours.
#[derive(Clone, Copy, Default)]
pub(super) struct Col(usize);

impl Col {
    /// A column of a declared width.
    pub(super) const fn wide(cells: usize) -> Self {
        Self(cells)
    }

    /// The column's width, for a budget that must subtract it.
    pub(super) const fn cells(self) -> usize {
        self.0
    }

    /// The column every one of `cells` fits in.
    pub(super) fn of<'a>(cells: impl IntoIterator<Item = &'a str>) -> Self {
        cells.into_iter().fold(Self::default(), Self::seeing)
    }

    /// Widened to fit `cell` too — measuring on from a declared floor, or over
    /// rows that are not in one collection.
    pub(super) fn seeing(self, cell: &str) -> Self {
        Self(self.0.max(UnicodeWidthStr::width(cell)))
    }

    /// `cell` flush left in the column — words, which read from the left.
    pub(super) fn left(self, cell: &str) -> String {
        format!("{cell}{}", self.air(cell))
    }

    /// `cell` flush right — figures, whose digits must end level.
    pub(super) fn right(self, cell: &str) -> String {
        format!("{}{cell}", self.air(cell))
    }

    fn air(self, cell: &str) -> String {
        " ".repeat(self.0.saturating_sub(UnicodeWidthStr::width(cell)))
    }
}

/// Maximum readable width in columns; markdown is wrapped to this.
pub(super) const READ_W: u16 = 100;

/// Rail gutter: one shape glyph plus a trailing space.  Every transcript row
/// carries exactly this margin — a glyph on a block's first content row, blank
/// elsewhere — so content columns are the same on every row and no selection
/// can reach the chrome.  See [`super::row::Row`].
pub(super) const RAIL_W: usize = 2;

/// Content columns available in a row `width` columns wide — the one place the
/// margin is subtracted, and the only cast of [`RAIL_W`].
#[allow(
    clippy::cast_possible_truncation,
    reason = "RAIL_W is a small compile-time constant"
)]
pub(super) const fn content_w(width: u16) -> u16 {
    width.saturating_sub(RAIL_W as u16)
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

/// Cards name roles, never colours: every card hue lives here.
fn role_style(role: Role) -> Style {
    match role {
        Role::Path => Style::default().fg(CYAN),
        Role::Code => Style::default().fg(Color::White),
        Role::Ok => Style::default().fg(LIME).add_modifier(Modifier::BOLD),
        Role::Warn => Style::default().fg(ORANGE).add_modifier(Modifier::BOLD),
        Role::Bad => Style::default().fg(RED).add_modifier(Modifier::BOLD),
        Role::Muted => Style::default().fg(SLATE).add_modifier(Modifier::DIM),
        Role::Strong => Style::default()
            .fg(Color::White)
            .add_modifier(Modifier::BOLD),
    }
}

/// Roled spans bind through [`role_style`]; roleless ones take plain white ink.
pub(super) fn span_style(role: Option<Role>) -> Style {
    role.map_or_else(|| Style::default().fg(Color::White), role_style)
}

/// One card span in the ink its role binds.
pub(super) fn card_span(cs: &CardSpan) -> Span<'static> {
    Span::styled(cs.text.clone(), span_style(cs.role))
}

#[cfg(test)]
mod tests;
