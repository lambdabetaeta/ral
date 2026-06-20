//! Collapsible scrollback blocks.
//!
//! A viewport's scrollback is a sequence of [`Block`]s, not a flat line
//! buffer.  Three block kinds are *dialable* — tool calls, patches, and
//! markdown — each carrying a disclosure [`Block::level`] (0–3) that
//! grades how much it reveals: from the rail glyph alone (L0) up through a
//! one-line summary (L1) and a few lines of context (L2) to the full
//! source (L3).  Chrome is already 1–few lines, so it stays full.  Each
//! block memoises the lines it last produced, keyed by the width it was
//! asked for, so re-flattening the buffer each frame re-renders only the
//! block the user just dialed, or the whole buffer once on a resize.

use super::fidelity::Fidelity;
use super::group;
use super::line::{self, RAIL_GLYPHS, RAIL_W, READ_W, is_blank};
use super::md::{self, MD_INDENT};
use super::rail::{self, RailKind};
use crate::bus::Hunk;
use crate::card::{Card, Mark};
use ratatui::style::Style;
use ratatui::text::{Line, Span};
use std::time::Duration;
use unicode_width::UnicodeWidthChar;

/// Index into the agent rail palette (`line::AGENT_HUES`). Root is `0`;
/// each subagent takes the next slot at birth, wrapping modulo the
/// palette length. Carried by value on every [`Block`] so the rail
/// renders agent identity without a lookup on `App`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct AgentSlot(pub u8);

/// Coarse chrome sub-kind the rail dispatches on. Patches and tool calls
/// derive their rail shape from their own [`BlockKind`] variant; chrome
/// carries this discriminant so the rail need not re-parse built lines.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) enum RailShape {
    /// A step boundary — renders the `━` rail marker.
    Step,
    /// An error — renders `╳`.
    Error,
    /// Ambient chrome outside the transcript proper — no marginal rail.
    Plain,
    /// Everything else — renders the static `❖`.
    #[default]
    Generic,
}

/// Where a [`BlockKind::Card`] came from — the distinction the coalescing
/// projection ([`super::group`]) reads to tell an *effect* it may fold into
/// a ral block from a *barrier* that splits one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CardOrigin {
    /// A read / grep / exec the model's call produced — an observation
    /// effect, foldable into the call's coalesced block.
    Observation,
    /// A write redirect — an effect, but never folded: a write ends the
    /// current ral block exactly as a diff does, and renders standalone.
    Write,
    /// A diff (`edit`'s `▎ diff`) or a deliberately `surface`d rich card —
    /// the model's own communication, a barrier that splits the block and
    /// stays standalone.
    Surfaced,
}

/// What a block carries.  Each variant renders as a pure function of its
/// data, the target width, and the block's disclosure [`level`].
pub(super) enum BlockKind {
    /// A tool call worth revealing: `summary` is the one-line label
    /// shown reduced, `cmd` the full ral source shown revealed.
    /// Summary-less calls (the `fff` query, an invalid-input header) have
    /// nothing to reveal and arrive as [`BlockKind::Chrome`] instead.
    ToolCall {
        tool: &'static str,
        summary: String,
        cmd: String,
    },
    /// Streamed assistant prose; re-wrapped from source at every width.
    Markdown(String),
    /// An async subagent's final result, landed in root's scrollback.
    /// Dialable like a tool call: collapsed (L1) to a one-line header
    /// (`title` · `elapsed` · a size-bar for `text` length, plus an error
    /// suffix when `error` is set), dialed open (L3) to the full `text`
    /// rendered as markdown.  Carries its own `title`/`elapsed`/`error`
    /// because `Markdown` can't, and keeps the `↘` rail identity a `Card`
    /// would lose.
    Subagent {
        title: String,
        text: String,
        error: Option<String>,
        elapsed: Duration,
    },
    /// A render document a kit surfaced — an ordered stack of Bertin
    /// [`Card`] marks, re-rendered from data at every width and disclosure
    /// level.  A card holding a `diff` mark is dialable (L1 header ↔ L3
    /// full); one of only `text`/`fields`/`measure`/`raw` is chrome-level.
    /// `origin` tells the coalescing projection whether the card is a model
    /// *effect* it may fold into a ral block or a *barrier* that splits one.
    Card { card: Card, origin: CardOrigin },
    /// Pre-built chrome whose builder already wrapped to [`READ_W`] — a
    /// step separator, prompt echo, error, banner, subagent breadcrumb, or
    /// a summary-less tool call.  `shape` lets the rail (and the size/grain
    /// moves) dispatch on the chrome sub-kind without re-parsing the built
    /// lines.
    Chrome {
        shape: RailShape,
        lines: Vec<Line<'static>>,
    },
}

/// Number of source/content lines L2 reveals around the summary — the
/// `±N` context window for the partial views of every dialable kind.
const N: usize = 3;

/// A block paired with the lines it last rendered, memoised by width.
pub(super) struct Block {
    kind: BlockKind,
    /// Disclosure level, `0..=3`: L0 rail glyph alone, L1 summary, L2
    /// summary + [`N`] lines of context, L3 full source.  Set at
    /// construction per kind (conservative defaults preserve today's
    /// rendering), dialed by [`Self::dial`].  Inert on chrome, which
    /// always renders full.
    level: u8,
    /// The producing agent's palette slot, stamped at push.
    agent: AgentSlot,
    /// The epistemic signal this block carries — context pressure and echo
    /// similarity, set at markdown commit (Move 7).  Sound (`0/0`) on every
    /// other kind, so only assistant prose degrades its medium.
    fidelity: Fidelity,
    /// A tool call's result magnitude — `text.lines().count()` of its
    /// [`crate::bus::Event::ToolResult`], attached after the fact by
    /// [`super::viewport::Viewport::set_result_size`].  Feeds the
    /// collapsed header's size-bar; `None` until the result lands (and
    /// always, on non-`ToolCall` blocks).
    result_size: Option<u32>,
    /// Lines for the current state at [`Self::cache_w`], or `None` when
    /// stale — never rendered, toggled open/shut, or asked at a new
    /// width.
    cache: Option<Vec<Line<'static>>>,
    cache_w: u16,
}

impl Block {
    /// Build a block at its kind's default level — conservative so
    /// nothing changes visually until the user dials: `ToolCall` at L1
    /// (today's collapsed view), every other kind at L3 (today's full
    /// render).
    fn new(kind: BlockKind, agent: AgentSlot, fidelity: Fidelity) -> Self {
        let level = match kind {
            BlockKind::ToolCall { .. } | BlockKind::Subagent { .. } => 1,
            _ => 3,
        };
        Self {
            kind,
            level,
            agent,
            fidelity,
            result_size: None,
            cache: None,
            cache_w: 0,
        }
    }

    /// `context` is the turn's degradation floor, stamped onto the call so
    /// its coalesced intent line dims under context pressure exactly as
    /// committed prose does (Move 7); echo does not apply — an intent is the
    /// model's stated purpose, not committed prose.
    pub(super) fn tool_call(
        tool: &'static str,
        summary: String,
        cmd: String,
        context: u8,
        agent: AgentSlot,
    ) -> Self {
        Self::new(
            BlockKind::ToolCall { tool, summary, cmd },
            agent,
            Fidelity { context, echo: 0 },
        )
    }
    pub(super) fn markdown(src: String, agent: AgentSlot, fidelity: Fidelity) -> Self {
        Self::new(BlockKind::Markdown(src), agent, fidelity)
    }
    /// An async subagent's final result. `fidelity` rides the existing
    /// [`Block::fidelity`] field so the revealed markdown degrades with
    /// root's context floor exactly as committing prose does.
    pub(super) fn subagent(
        title: String,
        text: String,
        error: Option<String>,
        elapsed: Duration,
        fidelity: Fidelity,
        agent: AgentSlot,
    ) -> Self {
        Self::new(
            BlockKind::Subagent {
                title,
                text,
                error,
                elapsed,
            },
            agent,
            fidelity,
        )
    }
    /// A surfaced render document — the model's own communication, a
    /// barrier the coalescing projection never folds.
    pub(super) fn card(card: Card, agent: AgentSlot) -> Self {
        Self::card_with(card, CardOrigin::Surfaced, agent)
    }
    /// A structural I/O effect: a read / grep / exec (foldable
    /// [`CardOrigin::Observation`]) or a write ([`CardOrigin::Write`], a
    /// barrier).  Distinct from [`Self::card`] so the projection can fold an
    /// observation into its call yet keep a write standalone.
    pub(super) fn io_card(card: Card, write: bool, agent: AgentSlot) -> Self {
        let origin = if write {
            CardOrigin::Write
        } else {
            CardOrigin::Observation
        };
        Self::card_with(card, origin, agent)
    }
    fn card_with(card: Card, origin: CardOrigin, agent: AgentSlot) -> Self {
        Self::new(
            BlockKind::Card { card, origin },
            agent,
            Fidelity::default(),
        )
    }
    /// A single-file diff, the common card the patch-aggregation path emits:
    /// one `card` carrying one `diff` mark, so the rail renders `▎` and the
    /// disclosure dial reveals the located hunks.  A diff is a barrier, so it
    /// carries [`CardOrigin::Surfaced`] — the projection never folds it.
    pub(super) fn patch(path: String, hunks: Vec<Hunk>, agent: AgentSlot) -> Self {
        Self::card(Card(vec![Mark::Diff { path, hunks }]), agent)
    }
    pub(super) fn chrome(shape: RailShape, lines: Vec<Line<'static>>, agent: AgentSlot) -> Self {
        Self::new(
            BlockKind::Chrome { shape, lines },
            agent,
            Fidelity::default(),
        )
    }

    /// The block's current disclosure level (`0..=3`).
    pub(super) fn level(&self) -> u8 {
        self.level
    }

    /// The block's magnitude, where defined: total changed lines
    /// (deletions + additions) for a patch, `None` elsewhere.  The rail's
    /// value-step and the header size-bar both read this.
    pub(super) fn magnitude(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Card { card, .. } => card.magnitude(),
            BlockKind::Subagent { text, .. } => Some(text.lines().count() as u32),
            _ => None,
        }
    }

    /// True for the block kinds whose disclosure [`Self::level`] the user
    /// can dial: tool calls, markdown, and a card carrying a `diff` mark.
    /// A diff-less card is chrome-level, and chrome is inert.
    pub(super) fn dialable(&self) -> bool {
        match &self.kind {
            BlockKind::ToolCall { .. } | BlockKind::Markdown(_) | BlockKind::Subagent { .. } => {
                true
            }
            BlockKind::Card { card, .. } => card.has_diff(),
            BlockKind::Chrome { .. } => false,
        }
    }

    /// True for a tool call — the one block kind a result magnitude
    /// attaches to via [`Self::set_result_size`].
    pub(super) fn is_tool_call(&self) -> bool {
        matches!(self.kind, BlockKind::ToolCall { .. })
    }

    /// True for a block the coalescing projection folds into a ral block —
    /// a tool call, or a read / grep / exec effect.  Everything else (a
    /// diff, a write, a surfaced card, markdown, chrome, a subagent result)
    /// is a *barrier* that splits one block from the next — except a step
    /// boundary interior to a run, which is neither content (it is not an
    /// observation here) nor a barrier: the viewport's run scan
    /// ([`super::viewport::Viewport::observation_run_end`]) bridges it as
    /// provider bookkeeping.
    pub(super) fn observation(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::ToolCall { .. }
                | BlockKind::Card {
                    origin: CardOrigin::Observation,
                    ..
                }
        )
    }

    /// This call's projected view for the coalesced ral block: its intent
    /// (`summary`), tool, script (`cmd`), result magnitude, and the turn's
    /// context floor (distress on the intent line).  `None` on any block
    /// that is not a tool call, so only a call opens a slot in the group.
    pub(super) fn call_view(&self) -> Option<group::CallParts<'_>> {
        match &self.kind {
            BlockKind::ToolCall { tool, summary, cmd } => Some(group::CallParts {
                intent: summary,
                tool,
                cmd,
                magnitude: self.result_size,
                context: self.fidelity.context,
            }),
            _ => None,
        }
    }

    /// An observation effect's rail-less rows, rendered as the io card it
    /// carries, for folding under its call's intent in the coalesced block.
    /// Empty for any non-observation block.
    pub(super) fn effect_lines(&self) -> Vec<Line<'static>> {
        match &self.kind {
            BlockKind::Card {
                card,
                origin: CardOrigin::Observation,
            } => line::render_card(card, 3),
            _ => Vec::new(),
        }
    }

    /// The `ral` script this block ran, if it is a `ral` tool call — the
    /// echo signal compares committing prose against it.  `None` for any
    /// other kind, including a non-`ral` tool call, so only a genuine
    /// just-run script can register as an echo.
    pub(super) fn ral_cmd(&self) -> Option<&str> {
        match &self.kind {
            BlockKind::ToolCall { tool, cmd, .. } if *tool == "ral" => Some(cmd),
            _ => None,
        }
    }

    /// True for a step-boundary chrome block — the column unit the
    /// matrix's per-agent step cells count.
    pub(super) fn is_step(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::Chrome {
                shape: RailShape::Step,
                ..
            }
        )
    }

    /// True for an error chrome block — drives the matrix's `╳` cell when
    /// the session's last block is a failure.
    pub(super) fn is_error(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::Chrome {
                shape: RailShape::Error,
                ..
            }
        )
    }

    /// Attach a tool call's result magnitude (`text.lines().count()`),
    /// dropping the memo so the collapsed header re-renders with its
    /// size-bar.  A no-op set on a non-tool-call block would never light
    /// a bar, but callers gate on [`Self::is_tool_call`].
    pub(super) fn set_result_size(&mut self, n: u32) {
        self.result_size = Some(n);
        self.cache = None;
    }

    /// The lowest level this block dials to.  A tool call is always the
    /// head of a coalesced ral block ([`super::group`]), whose floor is
    /// **L1, the live tip** — there is no L0 for a call block.  Every other
    /// dialable kind reduces to L0 (rail glyph alone).
    fn level_floor(&self) -> u8 {
        match self.kind {
            BlockKind::ToolCall { .. } => 1,
            _ => 0,
        }
    }

    /// Dial the disclosure level by `delta`, clamped to `level_floor..=3`,
    /// dropping the memo when it changed so the body re-renders at the new
    /// level.  A no-op on a non-dialable block or when already at the clamp.
    pub(super) fn dial(&mut self, delta: i8) {
        if !self.dialable() {
            return;
        }
        let next = (self.level as i8 + delta).clamp(self.level_floor() as i8, 3) as u8;
        if next != self.level {
            self.level = next;
            self.cache = None;
        }
    }

    /// Cycle a dialable block between its floor (reduced) and L3 (revealed) —
    /// the click-on-rail affordance, preserving today's click-to-expand.
    /// A tool call's floor is L1, every other kind's is L0.
    pub(super) fn cycle(&mut self) {
        if !self.dialable() {
            return;
        }
        let next = if self.level >= 3 {
            self.level_floor()
        } else {
            3
        };
        if next != self.level {
            self.level = next;
            self.cache = None;
        }
    }

    /// The block's lines at `width`, rebuilding the memo when it is cold
    /// or was filled at another width.
    pub(super) fn lines(&mut self, width: u16) -> &[Line<'static>] {
        if self.cache.is_none() || self.cache_w != width {
            self.cache = Some(self.render(width));
            self.cache_w = width;
        }
        self.cache.as_deref().expect("just filled")
    }

    /// The block as it belongs in the session log: full content,
    /// width-independent — every dialable block rendered at L3 regardless
    /// of its live level, so the script / diff / prose is on the record
    /// even while reduced on screen.  Routes through the same rendering
    /// path as [`Self::render`] (rail included) with the level forced full.
    pub(super) fn log_lines(&self) -> Vec<Line<'static>> {
        self.render_with(READ_W, true)
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        self.render_with(width, false)
    }

    /// The level at which to render: the live [`Self::level`], or L3 when
    /// `force_full` — the log path, which records the complete block.
    fn render_level(&self, force_full: bool) -> u8 {
        if force_full { 3 } else { self.level }
    }

    /// Build the block's body lines (rail-less) then prepend the
    /// data-encoding rail span to the first content row.  `force_full`
    /// renders every dialable block at L3 regardless of its live level —
    /// used only by [`Self::log_lines`] so the on-disk transcript is
    /// complete.
    fn render_with(&self, width: u16, force_full: bool) -> Vec<Line<'static>> {
        let level = self.render_level(force_full);
        let mut lines = self.body(width, level);
        // L0 reduces the body to nothing; the rail glyph alone remains, so
        // synthesise the single blank row it is prepended to.
        if lines.is_empty() {
            lines.push(Line::default());
        }
        if let Some(kind) = self.rail_kind(level) {
            let rail = rail::span(kind, self.agent, self.magnitude());
            // Markdown insets every row by `MD_INDENT`; the rail occupies the
            // first `RAIL_W` columns of that inset on the opening row, so shrink
            // the inset there to keep prose flush with the body.
            let shrink = matches!(self.kind, BlockKind::Markdown(_))
                .then_some(RAIL_W)
                .unwrap_or(0);
            let idx = lines.iter().position(|l| !is_blank(l)).unwrap_or(0);
            if shrink > 0 {
                shrink_leading_ws(&mut lines[idx], shrink);
            }
            lines[idx].spans.insert(0, rail);
        }
        lines
    }

    /// The rail-less body at `width`, graded by `level`.  L0 reveals
    /// nothing (only the rail survives, prepended by [`Self::render_with`]);
    /// L1 the one-line summary; L2 the summary plus [`N`] lines of context;
    /// L3 the full source.  Chrome ignores the level — it is always full.
    fn body(&self, width: u16, level: u8) -> Vec<Line<'static>> {
        if level == 0 && self.dialable() {
            return Vec::new();
        }
        match &self.kind {
            BlockKind::ToolCall { tool, summary, cmd } => match level {
                3 => line::tool_call_expanded(summary, tool, cmd, width),
                2 => line::tool_call_context(summary, tool, cmd, N, width),
                _ => line::tool_call_collapsed(summary, tool, self.result_size, width),
            },
            BlockKind::Markdown(src) => match level {
                3 => md::render_md(src, width, MD_INDENT, self.fidelity),
                2 => first_rows(md::render_md(src, width, MD_INDENT, self.fidelity), N),
                _ => first_rows(md::render_md(src, width, MD_INDENT, self.fidelity), 1),
            },
            BlockKind::Subagent {
                title,
                text,
                error,
                elapsed,
            } => {
                let size = text.lines().count() as u32;
                let mut ls = line::subagent_header(title, size, error.as_deref(), *elapsed);
                // L1 (and L0, handled above) is the header alone; L2/L3 extend
                // it with the rendered body. Build the header first so the
                // markdown rows append after it intact — the header is row 0
                // and the markdown's own first-rows/leading-blank logic never
                // touches it.
                match level {
                    3 => ls.extend(md::render_md(text, width, MD_INDENT, self.fidelity)),
                    2 => ls.extend(first_rows(
                        md::render_md(text, width, MD_INDENT, self.fidelity),
                        N,
                    )),
                    _ => {}
                }
                ls
            }
            BlockKind::Card { card, .. } => line::render_card(card, level),
            BlockKind::Chrome { lines, .. } => lines.clone(),
        }
    }

    /// The rail shape this block wears.  Chrome lifts its [`RailShape`]
    /// discriminant; patches, tool calls, and markdown derive theirs from
    /// the variant.  Plain chrome is ambient frame text and carries no
    /// rail.  A tool call's disclosure triangle tracks the level: `▽` once
    /// it reveals context (L2+), `▸` while reduced.
    fn rail_kind(&self, level: u8) -> Option<RailKind> {
        match &self.kind {
            BlockKind::ToolCall { .. } => Some(RailKind::ToolCall(level >= 2)),
            BlockKind::Markdown(_) => Some(RailKind::Markdown),
            // The `↘` keeps the delegated-result identity even on error; the
            // failure reads in the header suffix, not a swapped glyph.
            BlockKind::Subagent { .. } => Some(RailKind::Subagent),
            // A diff card wears the patch shape (`▎`); a diff-less card is
            // generic chrome (`❖`), the shape `wrote`/`task`/`meter` wore.
            BlockKind::Card { card, .. } => Some(if card.has_diff() {
                RailKind::Patch
            } else {
                RailKind::Generic
            }),
            BlockKind::Chrome { shape, .. } => match shape {
                RailShape::Step => Some(RailKind::Step),
                RailShape::Error => Some(RailKind::Error),
                RailShape::Plain => None,
                RailShape::Generic => Some(RailKind::Generic),
            },
        }
    }
}

/// The first `k` rendered rows of `lines`, preserving leading blanks but
/// keeping at least one row so the rail always has somewhere to land.
/// Used for the partial markdown views (L1/L2): `render_md` lays out the
/// whole block, and truncating its rows keeps a code fence's opening
/// rows intact rather than re-parsing a prefix of the source.
fn first_rows(mut lines: Vec<Line<'static>>, k: usize) -> Vec<Line<'static>> {
    // Skip the leading blank `render_md` does not emit (markdown opens
    // flush), so `k` counts content rows; a blank-only block keeps one row.
    let lead = lines.iter().take_while(|l| is_blank(l)).count();
    lines.truncate((lead + k).max(1));
    lines
}

/// Shrink the leading whitespace of `line` by `n` cells, trimming the
/// first whitespace-only span(s) in place.  Used to reclaim the columns
/// the rail occupies on a markdown block's opening row so its prose stays
/// flush with the body inset.
fn shrink_leading_ws(line: &mut Line<'static>, n: usize) {
    let mut remaining = n;
    for span in &mut line.spans {
        if remaining == 0 {
            break;
        }
        let s = span.content.as_ref();
        if !s.chars().all(|c| c == ' ') {
            break;
        }
        let len = s.chars().count();
        if len <= remaining {
            remaining -= len;
            span.content = String::new().into();
        } else {
            let kept: String = s.chars().skip(remaining).collect();
            span.content = kept.into();
            remaining = 0;
        }
    }
}

/// Fold one logical line into visual rows no wider than `width`,
/// word-aware and preserving each span's style.  The line builders already
/// lay content out within [`READ_W`], so on a terminal at least that wide
/// this hands the line straight back; it only folds on a narrower one.
///
/// Continuations re-indent to the line's leading indentation — an optional
/// [`RAIL_GLYPHS`] rail glyph (prepended by [`Block::render_with`]) plus any
/// leading whitespace the builders inset content with — so a wrapped prompt
/// echo, code row, or io effect folds under its own indent rather than
/// sliding back to column zero.  A line with no leading indent wraps flush at
/// `0`.  The greedy placement breaks between words, dropping the inter-word
/// gap at the break; a single word wider than the body column is hard-broken
/// char-by-char.
pub(super) fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width {
        return vec![line.clone()];
    }
    // The hang column is the line's leading indentation: an optional rail
    // glyph (the first span, when it is one the rail prepends) followed by any
    // whitespace-only spans the builders inset with.  Carrying the indent into
    // the head — rather than leaving it in the body — is what keeps it on a
    // wrapped row 0: the body's leading whitespace would otherwise be dropped
    // as a row-leading gap.  The head spans ride row 0 verbatim (so the copy
    // contract still strips a leading rail glyph), and continuations re-indent
    // to their summed width.
    let spans = line.spans.as_slice();
    let rail = spans
        .first()
        .is_some_and(|s| RAIL_GLYPHS.contains(&s.content.as_ref())) as usize;
    let mut head_len = rail;
    while spans
        .get(head_len)
        .is_some_and(|s| !s.content.is_empty() && s.content.chars().all(|c| c == ' '))
    {
        head_len += 1;
    }
    let head = &spans[..head_len];
    let body = &spans[head_len..];
    let indent: usize = head
        .iter()
        .flat_map(|s| s.content.chars())
        .map(|c| UnicodeWidthChar::width(c).unwrap_or(0))
        .sum();

    let mut rows: Vec<Line<'static>> = Vec::new();
    // Row 0 opens carrying the head spans verbatim (the rail glyph and indent,
    // occupying columns `0..indent`); continuation rows are seeded with the
    // indent.
    let mut row: Vec<Span<'static>> = head.to_vec();
    let mut col = indent;
    // Whether a word has landed on the current row's body.  A pending gap
    // before the first body word of a row is leading whitespace and is
    // dropped; once a word lands the row may break and gaps become
    // inter-word separators.
    let mut started = false;
    // The whitespace pending between the last word and the next: carried as
    // style-runs so a styled gap survives, or dropped at a break / row start.
    let mut gap: Vec<(String, Style)> = Vec::new();
    let mut gap_w = 0;

    for (word, ww) in words(body) {
        // A whitespace run is held as the pending gap, never placed eagerly.
        if word.iter().all(|(s, _)| s.chars().all(char::is_whitespace)) {
            gap = word;
            gap_w = ww;
            continue;
        }
        // Break before a word that overflows once this row carries one; the
        // pending gap is dropped at the break.
        if started && col + gap_w + ww > width {
            rows.push(Line::from(std::mem::take(&mut row)));
            row = seed(indent);
            col = indent;
            started = false;
            gap.clear();
            gap_w = 0;
        }
        // Place the pending gap only between words on a started row; drop it
        // when it would lead the row.
        if started {
            for (s, style) in gap.drain(..) {
                row.push(Span::styled(s, style));
            }
            col += gap_w;
        } else {
            gap.clear();
        }
        gap_w = 0;
        // Place the word, hard-breaking it char-by-char when it alone is
        // wider than the body column.
        for (s, style) in word {
            for ch in s.chars() {
                let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
                if started && col + cw > width {
                    rows.push(Line::from(std::mem::take(&mut row)));
                    row = seed(indent);
                    col = indent;
                }
                push_char(&mut row, ch, style);
                col += cw;
                started = true;
            }
        }
    }
    if started || rows.is_empty() {
        rows.push(Line::from(row));
    }
    rows
}

/// A fresh continuation row seeded with `indent` spaces, or empty when the
/// line wraps flush — the body column every wrapped row re-indents to.
fn seed(indent: usize) -> Vec<Span<'static>> {
    if indent == 0 {
        Vec::new()
    } else {
        vec![Span::raw(" ".repeat(indent))]
    }
}

/// Append `ch` to `row`, extending the trailing span when it shares `ch`'s
/// style so a word does not fragment into one span per character.
fn push_char(row: &mut Vec<Span<'static>>, ch: char, style: Style) {
    match row.last_mut() {
        Some(last) if last.style == style => last.content.to_mut().push(ch),
        _ => row.push(Span::styled(ch.to_string(), style)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn indent_of(s: &str) -> usize {
        s.len() - s.trim_start().len()
    }

    /// A wrapped, indented, rail-less line (a source or io-effect row) keeps
    /// its indent on row 0 and hangs every continuation under it — the leading
    /// whitespace is the hang column, not a row-leading gap to drop.
    #[test]
    fn wrap_hangs_indented_line_under_its_indent() {
        let line = Line::from(vec![
            Span::raw("    "),
            Span::raw("let paper_candidates = filter re-match candidates over the files"),
        ]);
        let rows = wrap_line(&line, 24);
        assert!(rows.len() > 1, "expected a fold at width 24");
        for row in &rows {
            assert_eq!(indent_of(&plain(row)), 4, "row lost its indent: {:?}", plain(row));
        }
    }

    /// A rail-led line still hangs continuations two columns under the glyph,
    /// and the colour-styled glyph rides row 0 as its own span — what the copy
    /// contract ([`plain`]) keys off to strip the chrome.
    #[test]
    fn wrap_hangs_rail_led_line_under_the_glyph() {
        let rail = Span::styled("▸ ", Style::default().fg(ratatui::style::Color::Cyan));
        let line = Line::from(vec![
            rail,
            Span::raw("alpha beta gamma delta epsilon zeta eta theta iota"),
        ]);
        let rows = wrap_line(&line, 20);
        assert!(rows.len() > 1);
        assert!(RAIL_GLYPHS.contains(&rows[0].spans[0].content.as_ref()));
        for row in &rows[1..] {
            let text = plain(row);
            assert_eq!(indent_of(&text), 2);
            assert!(!text.starts_with('▸'));
        }
    }

    /// A flush, unindented line wraps back to column zero — no spurious indent.
    #[test]
    fn wrap_keeps_flush_line_flush() {
        let line = Line::from(Span::raw("one two three four five six seven eight nine ten"));
        let rows = wrap_line(&line, 16);
        assert!(rows.len() > 1);
        for row in &rows {
            assert_eq!(indent_of(&plain(row)), 0);
        }
    }
}

/// Tokenise a span stream into maximal whitespace / non-whitespace runs,
/// paired with each run's display width.  A run carries its style-fragments
/// so a word that crosses a span seam (a style change mid-word) keeps each
/// fragment's [`Style`].  Mirrors `md`'s word/space split, but span-aware.
fn words(spans: &[Span<'static>]) -> Vec<(Vec<(String, Style)>, usize)> {
    let mut out: Vec<(Vec<(String, Style)>, usize)> = Vec::new();
    let mut run: Vec<(String, Style)> = Vec::new();
    let mut run_w = 0;
    // Whether the run accumulated so far is whitespace — `None` until the
    // first char fixes its kind.
    let mut ws: Option<bool> = None;
    let mut flush = |run: &mut Vec<(String, Style)>, run_w: &mut usize, ws: &mut Option<bool>| {
        if !run.is_empty() {
            out.push((std::mem::take(run), std::mem::replace(run_w, 0)));
        }
        *ws = None;
    };
    for span in spans {
        for ch in span.content.chars() {
            let is_ws = ch.is_whitespace();
            if ws.is_some_and(|prev| prev != is_ws) {
                flush(&mut run, &mut run_w, &mut ws);
            }
            ws = Some(is_ws);
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            match run.last_mut() {
                Some((s, st)) if *st == span.style => s.push(ch),
                _ => run.push((ch.to_string(), span.style)),
            }
            run_w += cw;
        }
    }
    flush(&mut run, &mut run_w, &mut ws);
    out
}
