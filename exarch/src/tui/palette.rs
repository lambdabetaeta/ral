//! The TUI colour + layout palette: cross-cutting colour and width
//! constants shared by the line-builders ([`super::line`]) and the sibling
//! render modules.  Kept apart from the builders so the vocabulary of hues
//! and widths reads as one table rather than being buried among the
//! `Line`-producing functions.

use ratatui::style::Color;

// ── Color palette ────────────────────────────────────────────────────────────

/// Muted vaporwave palette for the per-step chrome — dusty pastels so the
/// repeating chrome reads as accent rather than alarm.  The only louder
/// ink in the app is the splash's `BANNER_*` pair (wordmark + eagle), kept
/// saturated so the one-shot banner carries a neon punch without bleeding
/// into the session below — the metadata matrix and everything else draw
/// from this muted set through their nominal [`crate::card::Role`].
pub(super) const PINK: Color = Color::Rgb(220, 140, 175);
pub(super) const CYAN: Color = Color::Rgb(135, 200, 215);
pub(super) const LIME: Color = Color::Rgb(165, 210, 155);
pub(super) const PURPLE: Color = Color::Rgb(175, 145, 210);
pub(super) const ORANGE: Color = Color::Rgb(215, 145, 115);
pub(super) const RED: Color = Color::Rgb(215, 110, 125);
pub(super) const SLATE: Color = Color::Rgb(140, 150, 170);
/// Brighter siblings of [`LIME`]/[`RED`] for the *changed* run of a diff row —
/// the inline word-diff emphasis (rendered bold), set against the dimmed base
/// hue of the row's unchanged remainder.
pub(super) const LIME_HOT: Color = Color::Rgb(196, 240, 182);
pub(super) const RED_HOT: Color = Color::Rgb(242, 142, 158);
/// A faint raised plane for queued user prompts.  The prompt itself still
/// renders through the normal rail/fence/body path; this wash only says "not
/// yet delivered".
pub(super) const QUEUED_PROMPT_BG: Color = Color::Rgb(72, 78, 94);
/// The `/model` overlay's plane — the deep blue fill behind the floating,
/// bezel-framed picker ([`super::picker`]). A Norton-Commander indigo, but
/// pulled toward the app's muted set so the modal reads as a recessed panel
/// lifted *above* the session rather than a saturated intrusion. It is the
/// one areal mark that means "modal has the focus": the dimmed session shows
/// through nowhere the bezel covers.
pub(super) const OVERLAY_BG: Color = Color::Rgb(28, 34, 66);
/// The human's ink — the prompt body text and the `❖` fence marking a prompt
/// in the rail thumbnail.  A light cool neutral, distinct from the agent
/// rail's [`SLATE`] and dimmer than the machine's white prose: the human owns
/// the neutral tone, agents own the matrix hues, so a prompt reads as a quiet
/// island and its fence never aliases another agent's mark.
pub(super) const PROMPT_INK: Color = Color::Rgb(170, 180, 200);
/// The recessed machine-text panel — a grey fill behind a code block or a run
/// of observation output, marking it as a contiguous machine *region* (an
/// areal mark, matched to the data's nature).  Distinct from the model's base
/// prose and from the human's rule fence: background here means "machine".
pub(super) const CODE_BG: Color = Color::Rgb(36, 38, 46);

/// Syntax-highlight inks for ral code washed into the [`CODE_BG`] panel — one
/// low-saturation hue per token class ([`super::highlight`]).  Kept muted so
/// code reads calmly against the recessed panel rather than as alarm, and
/// held distinct from each other, from the chrome [`crate::card::Role`] palette, from the
/// human's [`PROMPT_INK`], and from the agent-rail identity set
/// ([`AGENT_HUES`]) so a token's colour never aliases a semantic one.
/// Punctuation reuses [`SLATE`]; every other token keeps the default code
/// ink (white).
pub(super) const CODE_KEYWORD: Color = Color::Rgb(168, 154, 208);
pub(super) const CODE_STRING: Color = Color::Rgb(150, 186, 146);
pub(super) const CODE_VARIABLE: Color = Color::Rgb(206, 166, 130);
pub(super) const CODE_TAG: Color = Color::Rgb(202, 150, 178);
/// Agent rail palette: one hue per producing agent, indexed by
/// [`super::block::AgentSlot`]. Root keeps [`CYAN`] — the existing rail
/// accent — so a root-only session is visually unchanged in hue. The
/// rail's value-step lightens a slot toward white with magnitude, so hue
/// stays the identity channel and value stays the magnitude channel.
///
/// Agent identity is hue-only on the rail (the cell already spends shape on
/// *kind* and value on *magnitude*, so no fourth channel is free), which a
/// red-green–blind reader cannot follow on hue alone. So the six are picked
/// to also separate by **lightness**: a descending `L*` ladder (≈77 → 75 →
/// 62 → 59 → 55 → 47) under which every pair stays distinct in simulated
/// deuteranopia *and* protanopia (worst-case ΔE76 ≈ 19, against ≈3 for a
/// hue-only set). Where two sit at near-equal `L*` (CYAN/MAGENTA) the
/// surviving blue–yellow axis holds them apart; no two warm hues share a
/// lightness, so the old orange/red confusion cannot recur. These are a
/// dedicated set, not the role palette above — agent identity must not alias
/// a semantic colour (e.g. `RED` the error hue).
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

/// Saturated splash-only palette — the wordmark (pink) and the eagle
/// (gold) of the one-shot startup banner.  These two are the only neon ink
/// in the app; all session data, the metadata matrix included, renders
/// through the muted palette above so nothing else competes with the
/// splash.
pub(super) const BANNER_PINK: Color = Color::Rgb(255, 20, 147);
pub(super) const BANNER_GOLD: Color = Color::Rgb(255, 191, 0);

// ── Layout constants ─────────────────────────────────────────────────────────

/// Maximum readable width in columns; markdown is wrapped to this.
pub(super) const READ_W: u16 = 100;

/// The prompt-fence glyph (`RailKind::Prompt`'s `❖`) plus its trailing
/// space — named because [`RAIL_GLYPHS`] reuses it as the last entry of the
/// shape vocabulary. Block content gets its rail from [`super::rail::span`],
/// prepended by [`super::block::Block::render`].
pub(super) const RAIL: &str = "❖ ";

/// Rail width in columns: one shape glyph plus one trailing space. Every
/// block's first content row carries a rail of this width; body rows do
/// not, so a selection through the block copies as plain text.  The full
/// rail is also the dial target — both the wheel and the click-cycle act
/// on a block when the pointer sits anywhere in these two columns.
pub(super) const RAIL_W: usize = 2;

/// The full rail shape vocabulary: one glyph + space per block kind.
/// [`super::line::plain`] drops a leading span whose content matches one of
/// these so copied text carries the content, not the chrome glyph;
/// [`super::line::wrap_line`] reuses the set to detect a rail-led row and
/// indent its continuations.
pub(super) const RAIL_GLYPHS: [&str; 9] = ["▎ ", "▸ ", "▽ ", "· ", "∴ ", "↘ ", "━ ", "╳ ", RAIL];
