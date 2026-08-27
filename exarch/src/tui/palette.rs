//! Colour and width constants shared by the line-builders ([`super::line`])
//! and the sibling render modules, kept apart so they read as one table.

use ratatui::style::Color;

// ── Color palette ────────────────────────────────────────────────────────────

/// Muted chrome hues; card spans reach them through a nominal
/// [`crate::bus::card::Role`] rather than naming a colour.
pub(super) const PINK: Color = Color::Rgb(220, 140, 175);
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
/// The `/model` overlay's plane ([`super::picker`]) — the one areal mark that
/// means the modal holds the focus.
pub(super) const OVERLAY_BG: Color = Color::Rgb(28, 34, 66);
/// The human's ink — prompt body and the `❖` fence in the rail.  Neutral where
/// the agents own hues, so a prompt never aliases an agent's mark.
pub(super) const PROMPT_INK: Color = Color::Rgb(170, 180, 200);
/// The recessed machine-text panel: an areal mark, so background here means
/// "machine", as against the model's prose and the human's fence.
pub(super) const CODE_BG: Color = Color::Rgb(36, 38, 46);

/// Syntax inks for ral code washed into [`CODE_BG`] ([`super::highlight`]),
/// held apart from the chrome roles, [`PROMPT_INK`] and [`AGENT_HUES`] so a
/// token's colour never aliases a semantic one.  Punctuation reuses [`SLATE`];
/// every other token keeps the default white.
pub(super) const CODE_KEYWORD: Color = Color::Rgb(168, 154, 208);
pub(super) const CODE_STRING: Color = Color::Rgb(150, 186, 146);
pub(super) const CODE_VARIABLE: Color = Color::Rgb(206, 166, 130);
pub(super) const CODE_TAG: Color = Color::Rgb(202, 150, 178);
/// One hue per producing agent, indexed by [`super::block::AgentSlot`]; root
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

/// The startup banner's wordmark and eagle — the only saturated ink in the app,
/// so nothing in the session below competes with the splash.
pub(super) const BANNER_PINK: Color = Color::Rgb(255, 20, 147);
pub(super) const BANNER_GOLD: Color = Color::Rgb(255, 191, 0);

// ── Layout constants ─────────────────────────────────────────────────────────

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

/// Content columns at the readable width — the width builders wrap to.
pub(super) const READ_CONTENT_W: u16 = content_w(READ_W);
