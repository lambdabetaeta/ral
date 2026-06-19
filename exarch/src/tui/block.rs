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
use super::line::{self, RAIL_W, READ_W, is_blank};
use super::md::{self, MD_INDENT};
use super::rail::{self, RailKind};
use crate::bus::Hunk;
use crate::card::{Card, Mark};
use ratatui::text::{Line, Span};
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
    /// An error — renders `✗`.
    Error,
    /// Ambient chrome outside the transcript proper — no marginal rail.
    Plain,
    /// Everything else — renders the static `❖`.
    #[default]
    Generic,
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
    /// A render document a kit surfaced — an ordered stack of Bertin
    /// [`Card`] marks, re-rendered from data at every width and disclosure
    /// level.  A card holding a `diff` mark is dialable (L1 header ↔ L3
    /// full); one of only `text`/`fields`/`measure`/`raw` is chrome-level.
    Card(Card),
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
            BlockKind::ToolCall { .. } => 1,
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

    pub(super) fn tool_call(
        tool: &'static str,
        summary: String,
        cmd: String,
        agent: AgentSlot,
    ) -> Self {
        Self::new(
            BlockKind::ToolCall { tool, summary, cmd },
            agent,
            Fidelity::default(),
        )
    }
    pub(super) fn markdown(src: String, agent: AgentSlot, fidelity: Fidelity) -> Self {
        Self::new(BlockKind::Markdown(src), agent, fidelity)
    }
    /// A surfaced render document.
    pub(super) fn card(card: Card, agent: AgentSlot) -> Self {
        Self::new(BlockKind::Card(card), agent, Fidelity::default())
    }
    /// A single-file diff, the common card the patch-aggregation path emits:
    /// one `card` carrying one `diff` mark, so the rail renders `▎` and the
    /// disclosure dial reveals the located hunks.
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
            BlockKind::Card(card) => card.magnitude(),
            _ => None,
        }
    }

    /// True for the block kinds whose disclosure [`Self::level`] the user
    /// can dial: tool calls, markdown, and a card carrying a `diff` mark.
    /// A diff-less card is chrome-level, and chrome is inert.
    pub(super) fn dialable(&self) -> bool {
        match &self.kind {
            BlockKind::ToolCall { .. } | BlockKind::Markdown(_) => true,
            BlockKind::Card(card) => card.has_diff(),
            BlockKind::Chrome { .. } => false,
        }
    }

    /// True for a tool call — the one block kind a result magnitude
    /// attaches to via [`Self::set_result_size`].
    pub(super) fn is_tool_call(&self) -> bool {
        matches!(self.kind, BlockKind::ToolCall { .. })
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

    /// True for an error chrome block — drives the matrix's `✗` cell when
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

    /// Dial the disclosure level by `delta`, clamped to `0..=3`, dropping
    /// the memo when it changed so the body re-renders at the new level.
    /// A no-op on a non-dialable block or when already at the clamp.
    pub(super) fn dial(&mut self, delta: i8) {
        if !self.dialable() {
            return;
        }
        let next = (self.level as i8 + delta).clamp(0, 3) as u8;
        if next != self.level {
            self.level = next;
            self.cache = None;
        }
    }

    /// Cycle a dialable block between L1 (reduced) and L3 (revealed) —
    /// the click-on-rail affordance, preserving today's click-to-expand.
    pub(super) fn cycle(&mut self) {
        if !self.dialable() {
            return;
        }
        let next = if self.level >= 3 { 1 } else { 3 };
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
            BlockKind::Card(card) => line::render_card(card, level),
            BlockKind::Chrome { lines, .. } => lines.clone(),
        }
    }

    /// The rail shape this block wears.  Chrome lifts its [`RailShape`]
    /// discriminant; patches, tool calls, and markdown derive theirs from
    /// the variant.  Plain chrome is ambient frame text and carries no
    /// rail.  A tool call's disclosure triangle tracks the level: `▾` once
    /// it reveals context (L2+), `▸` while reduced.
    fn rail_kind(&self, level: u8) -> Option<RailKind> {
        match &self.kind {
            BlockKind::ToolCall { .. } => Some(RailKind::ToolCall(level >= 2)),
            BlockKind::Markdown(_) => Some(RailKind::Markdown),
            // A diff card wears the patch shape (`▎`); a diff-less card is
            // generic chrome (`❖`), the shape `wrote`/`task`/`meter` wore.
            BlockKind::Card(card) => Some(if card.has_diff() {
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
/// preserving each span's style.  The line builders already lay content
/// out within [`READ_W`], so on a terminal at least that wide this hands
/// the line straight back; it only folds — at the column, since the
/// content is already-laid-out chrome or source — on a narrower one.
pub(super) fn wrap_line(line: &Line<'static>, width: usize) -> Vec<Line<'static>> {
    if width == 0 || line.width() <= width {
        return vec![line.clone()];
    }
    let mut rows: Vec<Line<'static>> = Vec::new();
    let mut row: Vec<Span<'static>> = Vec::new();
    let mut col = 0;
    for span in &line.spans {
        let mut buf = String::new();
        for ch in span.content.chars() {
            let cw = UnicodeWidthChar::width(ch).unwrap_or(0);
            if col + cw > width && col > 0 {
                if !buf.is_empty() {
                    row.push(Span::styled(std::mem::take(&mut buf), span.style));
                }
                rows.push(Line::from(std::mem::take(&mut row)));
                col = 0;
            }
            buf.push(ch);
            col += cw;
        }
        if !buf.is_empty() {
            row.push(Span::styled(buf, span.style));
        }
    }
    if !row.is_empty() || rows.is_empty() {
        rows.push(Line::from(row));
    }
    rows
}
