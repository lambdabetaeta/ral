//! Collapsible scrollback blocks.
//!
//! A viewport's scrollback is a sequence of [`Block`]s, not a flat line
//! buffer.  Three block kinds are *dialable* — tool calls, patches, and
//! subagent results — each carrying a disclosure [`Block::level`] (1–3)
//! that grades how much it reveals: from a one-line summary (L1) through a
//! few lines of context (L2) to the full source (L3).  A block is dialable
//! only if it has a real summary to collapse to; model prose ([`BlockKind::
//! Markdown`]) has none — its answer is product to read, not process to
//! reduce — so it always renders full and is inert to the dial.  Chrome is
//! already 1–few lines, so it too stays full.  Each block memoises the lines
//! it last produced, keyed by the width it was asked for, so re-flattening
//! the buffer each frame re-renders only the block the user just dialed, or
//! the whole buffer once on a resize.

use super::fidelity::Fidelity;
use super::group;
use super::line::{self, RAIL_GLYPHS, RAIL_W, READ_W, is_blank};
use super::md::{self, MD_INDENT};
use super::rail::{self, RailKind};
use crate::bus::Hunk;
use crate::card::{Card, Mark, ObservationKind};
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
    /// Ambient chrome outside the transcript proper — no marginal rail. The
    /// default: a meta-notice (a model/mode switch, an export, a stop reason)
    /// is an annotation, not a navigable block, so it earns no shape.
    #[default]
    Plain,
    /// The human's submitted prompt — marked by its [`super::line::PROMPT_INK`]
    /// body tint and the `❖` rail fence, with a full-width rule above its first
    /// row (painted by the flatten).  No background band — background is the
    /// machine's.
    Prompt,
}

/// Where a [`BlockKind::Card`] came from — the distinction the coalescing
/// projection ([`super::group`]) reads to tell an *effect* it may fold into
/// a ral block from a *barrier* that splits one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CardOrigin {
    /// A read / grep / exec the model's call produced — an observation
    /// effect, foldable into the call's coalesced block.  Carries the `|>`
    /// effect `kind` and the `count` it folds (a grouped card comma-joins
    /// several of one kind), the census tally the coalesced run's L0 sums.
    Observation { kind: ObservationKind, count: u32 },
    /// A write redirect — an effect, but never folded: a write ends the
    /// current ral block exactly as a diff does, and renders standalone.
    Write,
    /// A diff (`edit`'s `▎ diff`) or a deliberately `surface`d rich card —
    /// the model's own communication, a barrier that splits the block and
    /// stays standalone.
    Surfaced,
}

/// The reasoning a turn produced. It is its own dialable block: the
/// collapsed form gives only a grain and size; higher rungs reveal the
/// drained trace. `answer_chars` is the whole turn's answer mass, the
/// deliberation grain's denominator.
pub(super) struct Thinking {
    pub(super) text: String,
    pub(super) answer_chars: u32,
}

/// What a block carries.  Each variant renders as a pure function of its
/// data, the target width, and the block's disclosure [`level`].
pub(super) enum BlockKind {
    /// A tool call worth revealing: `summary` is the one-line label
    /// shown reduced, `cmd` the full ral source shown revealed.
    /// Summary-less calls (the `fff` query, an invalid-input placeholder) have
    /// nothing to reveal and arrive as [`BlockKind::Query`] instead.
    ToolCall {
        tool: &'static str,
        summary: String,
        cmd: String,
    },
    /// A summary-less tool call — the `fff` query that coalesces with its
    /// neighbours into one `fff : q1, q2, …` line ([`super::group`]'s flat
    /// cousin, projected in [`super::viewport`]).  `query` is the text to show,
    /// or `None` for a parse-failure placeholder ([`crate::tools::INVALID_INPUT`]):
    /// such a call renders nothing, present only as the boundary a stray
    /// [`Block::set_result_size`] stops at so it cannot reach back to an earlier
    /// call.  Inert (nothing to dial); on the per-block log tee it renders alone
    /// as `tool  query`, wearing the shut tool-call triangle `▸`.
    Query {
        tool: &'static str,
        query: Option<String>,
    },
    /// Streamed assistant prose; re-wrapped from source at every width.
    /// Prose is product — always full, inert to the dial, wearing `·`.
    Markdown { src: String },
    /// A model reasoning trace, separate from the answer it produced.
    /// It is dialable: L1 is the deliberation header, L2 a few rows of
    /// drained trace, L3 the full trace.
    Thinking(Thinking),
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

/// How much of a dialable block the rail discloses, low to high — `Ord`
/// compares the rungs.  The reachable band is `[floor, Full]`; only a `|>` run
/// reaches `Census` (see [`Block::floor`]).  The wheel steps one rung and
/// stops at the band edge; the click steps one rung and wraps `Full → floor`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Reveal {
    /// L0: a run's `|>` effects tallied — "Ran 5 scripts, read 4 files, …".
    Census,
    /// L1: the live tip, or a collapsed one-line header.
    Summary,
    /// L2: the summary plus [`N`] lines of context.
    Context,
    /// L3: the full source.
    Full,
}

impl Reveal {
    /// The next rung up, saturating at `Full`.
    fn up(self) -> Self {
        match self {
            Self::Census => Self::Summary,
            Self::Summary => Self::Context,
            Self::Context | Self::Full => Self::Full,
        }
    }

    /// The next rung down, saturating at `Census`.
    fn down(self) -> Self {
        match self {
            Self::Full => Self::Context,
            Self::Context => Self::Summary,
            Self::Summary | Self::Census => Self::Census,
        }
    }
}

/// A block paired with the lines it last rendered, memoised by width.
pub(super) struct Block {
    kind: BlockKind,
    /// Disclosure rung on the [`Reveal`] ladder.  Set at construction per kind
    /// (conservative defaults preserve today's rendering), walked by
    /// [`Self::dial`] (wheel) and [`Self::cycle`] (click) within the band
    /// `[Self::floor, Full]`.  Inert on prose and chrome, which always render
    /// full.
    level: Reveal,
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
    /// nothing changes visually until the user dials: `ToolCall` and
    /// `Subagent` at L1 (their collapsed headers), every other kind at L3
    /// (today's full render).
    fn new(kind: BlockKind, fidelity: Fidelity) -> Self {
        let level = match kind {
            BlockKind::ToolCall { .. } | BlockKind::Subagent { .. } | BlockKind::Thinking(_) => {
                Reveal::Summary
            }
            _ => Reveal::Full,
        };
        Self {
            kind,
            level,
            fidelity,
            result_size: None,
            cache: None,
            cache_w: 0,
        }
    }

    /// `context` is the turn's degradation floor, stamped onto the call so
    /// its coalesced intent line drains its saturation under context
    /// pressure exactly as committed prose does (Move 7); echo does not
    /// apply — an intent is the model's stated purpose, not committed prose.
    pub(super) fn tool_call(tool: &'static str, summary: String, cmd: String, context: u8) -> Self {
        Self::new(
            BlockKind::ToolCall { tool, summary, cmd },
            Fidelity { context, echo: 0 },
        )
    }
    pub(super) fn markdown(src: String, fidelity: Fidelity) -> Self {
        Self::new(BlockKind::Markdown { src }, fidelity)
    }
    /// A completed thinking trace. It is logged and rendered as its own
    /// dialable block; answer prose remains a separate markdown run.
    pub(super) fn thinking(text: String, answer_chars: u32) -> Self {
        Self::new(
            BlockKind::Thinking(Thinking { text, answer_chars }),
            Fidelity::default(),
        )
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
    ) -> Self {
        Self::new(
            BlockKind::Subagent {
                title,
                text,
                error,
                elapsed,
            },
            fidelity,
        )
    }
    /// A surfaced render document — the model's own communication, a
    /// barrier the coalescing projection never folds.
    pub(super) fn card(card: Card) -> Self {
        Self::card_with(card, CardOrigin::Surfaced)
    }
    /// A foldable observation effect: a read / grep / exec
    /// ([`CardOrigin::Observation`], carrying the census `count` it folds).
    /// Distinct from [`Self::card`] so the projection can fold it into its call.
    pub(super) fn observation_card(card: Card, kind: ObservationKind, count: u32) -> Self {
        Self::card_with(card, CardOrigin::Observation { kind, count })
    }
    /// A write effect ([`CardOrigin::Write`], a barrier): the `write <path>
    /// <outcome>` heading and a preview of what it wrote.  Like a diff, it ends
    /// the current ral block and renders standalone — never folded into a run.
    pub(super) fn write_card(card: Card) -> Self {
        Self::card_with(card, CardOrigin::Write)
    }
    fn card_with(card: Card, origin: CardOrigin) -> Self {
        Self::new(BlockKind::Card { card, origin }, Fidelity::default())
    }
    /// A single-file diff, the common card the patch-aggregation path emits:
    /// one `card` carrying one `diff` mark, so the rail renders `▎` and the
    /// disclosure dial reveals the located hunks.  A diff is a barrier, so it
    /// carries [`CardOrigin::Surfaced`] — the projection never folds it.
    pub(super) fn patch(path: String, hunks: Vec<Hunk>) -> Self {
        Self::card(Card(vec![Mark::Diff { path, hunks }]))
    }
    pub(super) fn chrome(shape: RailShape, lines: Vec<Line<'static>>) -> Self {
        Self::new(BlockKind::Chrome { shape, lines }, Fidelity::default())
    }
    /// A summary-less query call.  `query` is the text to coalesce into the
    /// `tool : …` line, or `None` for an invalid-input placeholder (an invisible
    /// call boundary).
    pub(super) fn query(tool: &'static str, query: Option<String>) -> Self {
        Self::new(BlockKind::Query { tool, query }, Fidelity::default())
    }

    /// The block's current disclosure rung.
    pub(super) fn level(&self) -> Reveal {
        self.level
    }

    /// The block's magnitude, where defined: total changed lines
    /// (deletions + additions) for a patch, source-line count for prose (a
    /// markdown block or a subagent result), `None` elsewhere.  The rail's
    /// value-step and the header size-bar both read this, so prose volume
    /// lightens the rail the way line counts do for a diff.
    pub(super) fn magnitude(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Card { card, .. } => card.magnitude(),
            BlockKind::Markdown { src, .. } => Some(src.lines().count() as u32),
            BlockKind::Thinking(t) => Some(t.text.lines().count() as u32),
            BlockKind::Subagent { text, .. } => Some(text.lines().count() as u32),
            _ => None,
        }
    }

    /// The block's contribution to the session's *code* footprint — the
    /// changed lines of a diff card.  Distinct from [`Self::magnitude`],
    /// which also counts prose volume (markdown, a subagent result) for the
    /// rail's value channel: the matrix's "lines touched" readout is a
    /// write footprint, so prose must not inflate it.  `None` on every kind
    /// but a diff-bearing card.
    pub(super) fn lines_changed(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Card { card, .. } => card.magnitude(),
            _ => None,
        }
    }

    /// True for the block kinds whose disclosure [`Self::level`] the user
    /// can dial — those with something foldable: tool calls, subagent
    /// results, a thinking trace, and a card carrying a `diff` mark.
    /// Plain prose has only product to read, so it is inert; a diff-less
    /// card is chrome-level, and chrome is inert.
    pub(super) fn dialable(&self) -> bool {
        match &self.kind {
            BlockKind::ToolCall { .. } | BlockKind::Subagent { .. } | BlockKind::Thinking(_) => {
                true
            }
            BlockKind::Card { card, .. } => card.has_diff(),
            BlockKind::Markdown { .. } | BlockKind::Query { .. } | BlockKind::Chrome { .. } => {
                false
            }
        }
    }

    /// True for a tool call — the one block kind a result magnitude
    /// attaches to via [`Self::set_result_size`].
    pub(super) fn is_tool_call(&self) -> bool {
        matches!(self.kind, BlockKind::ToolCall { .. })
    }

    /// True for a summary-less query call — the flatten enters its coalescing
    /// branch here ([`super::viewport::Viewport::reflow`]).
    pub(super) fn is_query(&self) -> bool {
        matches!(self.kind, BlockKind::Query { .. })
    }

    /// True for a *call-bearing* block — a dialable tool call or a summary-less
    /// query.  [`Self::set_result_size`] walks back to the first of these so a
    /// query's result halts here rather than reaching past it to clobber an
    /// earlier dialable call's size bar.
    pub(super) fn is_call(&self) -> bool {
        self.is_tool_call() || self.is_query()
    }

    /// A query call's tool, the key a coalesced run groups by; `None` on any
    /// non-query block.  Set for both a shown query and an invisible
    /// placeholder, so the run scan bridges either.
    pub(super) fn query_tool(&self) -> Option<&'static str> {
        match self.kind {
            BlockKind::Query { tool, .. } => Some(tool),
            _ => None,
        }
    }

    /// The query text to render in a coalesced `tool : …` line — `None` for a
    /// parse-failure placeholder (rendered invisibly) or any non-query block.
    pub(super) fn query_text(&self) -> Option<&str> {
        match &self.kind {
            BlockKind::Query { query, .. } => query.as_deref(),
            _ => None,
        }
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
                    origin: CardOrigin::Observation { .. },
                    ..
                }
        )
    }

    /// This observation's census contribution: the `|>` effect kind it
    /// surfaces and how many it folds, for the coalesced run's L0 tally.
    /// `None` on every block but a folded observation card.
    pub(super) fn io_tally(&self) -> Option<(ObservationKind, u32)> {
        match &self.kind {
            BlockKind::Card {
                origin: CardOrigin::Observation { kind, count },
                ..
            } => Some((*kind, *count)),
            _ => None,
        }
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
                origin: CardOrigin::Observation { .. },
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

    /// The raw markdown source of a prose block, `None` on every other
    /// kind.  `/copy` walks the trailing run of these to reassemble the
    /// assistant's latest reply verbatim — each fence-safe paragraph
    /// commits as its own block, so the run is the multi-paragraph answer.
    pub(super) fn markdown_src(&self) -> Option<&str> {
        match &self.kind {
            BlockKind::Markdown { src, .. } => Some(src),
            _ => None,
        }
    }

    /// True for an assistant prose block.
    pub(super) fn is_markdown(&self) -> bool {
        matches!(self.kind, BlockKind::Markdown { .. })
    }

    /// True for a committed thinking block.
    pub(super) fn is_thinking(&self) -> bool {
        matches!(self.kind, BlockKind::Thinking(_))
    }

    /// Append `more` to an existing thinking block's trace, accumulating
    /// `answer_chars` into its deliberation-grain denominator.  A no-op on
    /// any non-`Thinking` block.
    pub(super) fn append_thinking(&mut self, more: &str, answer_chars: u32) {
        if let BlockKind::Thinking(t) = &mut self.kind {
            if !t.text.is_empty() && !more.is_empty() {
                t.text.push('\n');
            }
            t.text.push_str(more);
            t.answer_chars = t.answer_chars.saturating_add(answer_chars);
            self.cache = None;
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

    /// True for the human turn's prompt echo — the one block in the light
    /// stratum, banded full-width by the flatten ([`super::viewport`]) as a
    /// scrollback landmark.
    pub(super) fn is_prompt(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::Chrome {
                shape: RailShape::Prompt,
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

    /// The lowest rung this block reduces to.  A coalesced `|>` run (anchored
    /// on a tool call) bottoms out at [`Reveal::Census`] — its tallied effects;
    /// every other dialable kind floors one rung higher, at [`Reveal::Summary`],
    /// since a census of a lone diff or subagent is meaningless.
    fn floor(&self) -> Reveal {
        match self.kind {
            BlockKind::ToolCall { .. } => Reveal::Census,
            _ => Reveal::Summary,
        }
    }

    /// Dial the rung by one wheel notch — up reveals, down reduces — saturating
    /// at the band's edges.  A no-op on a non-dialable block.
    pub(super) fn dial(&mut self, delta: i8) {
        if self.dialable() {
            let next = if delta >= 0 {
                self.level.up()
            } else {
                self.level.down().max(self.floor())
            };
            self.set_level(next);
        }
    }

    /// Cycle the rung — the click affordance: one rung up, wrapping the ceiling
    /// back to the floor, so a click steps through every reachable rung rather
    /// than toggling the extremes.
    pub(super) fn cycle(&mut self) {
        if self.dialable() {
            let next = if self.level == Reveal::Full {
                self.floor()
            } else {
                self.level.up()
            };
            self.set_level(next);
        }
    }

    /// Move to `next`, dropping the memo so the body re-renders — the one seam
    /// both [`Self::dial`] and [`Self::cycle`] commit through.
    fn set_level(&mut self, next: Reveal) {
        if next != self.level {
            self.level = next;
            self.cache = None;
        }
    }

    /// The block's lines at `width`, rebuilding the memo when it is cold
    /// or was filled at another width.  `lead` says whether this block opens
    /// its rail-run (wears its glyph) or continues a prior prose paragraph's
    /// (blank gutter); it is fixed per block by arrival order — like
    /// `agent` — so it stays out of the width-keyed memo.
    pub(super) fn lines(&mut self, width: u16, agent: AgentSlot, lead: bool) -> &[Line<'static>] {
        if self.cache.is_none() || self.cache_w != width {
            self.cache = Some(self.render(width, agent, lead));
            self.cache_w = width;
        }
        self.cache.as_deref().expect("just filled")
    }

    /// The block as it belongs in the session log: full content,
    /// width-independent — every dialable block rendered at L3 regardless
    /// of its live level, so the script / diff / prose is on the record
    /// even while reduced on screen.  Routes through the same rendering
    /// path as [`Self::render`] (rail included) with the level forced full.
    /// `lead` matches the on-screen projection so the log marks a
    /// multi-paragraph response with one `·`, not one per paragraph.
    pub(super) fn log_lines(&self, agent: AgentSlot, lead: bool) -> Vec<Line<'static>> {
        self.render_with(READ_W, true, agent, lead)
    }

    fn render(&self, width: u16, agent: AgentSlot, lead: bool) -> Vec<Line<'static>> {
        self.render_with(width, false, agent, lead)
    }

    /// The rung at which to render: the live [`Self::level`], or [`Reveal::Full`]
    /// when `force_full` — the log path, which records the complete block.
    fn render_level(&self, force_full: bool) -> Reveal {
        if force_full { Reveal::Full } else { self.level }
    }

    /// Build the block's body lines (rail-less) then prepend the
    /// data-encoding rail span to the first content row.  `force_full`
    /// renders every dialable block at L3 regardless of its live level —
    /// used only by [`Self::log_lines`] so the on-disk transcript is
    /// complete.  `lead` is false for a prose paragraph that continues a
    /// prior one's response: it keeps the gutter (so the text stays in the
    /// same body column) but drops the `·`, so a multi-paragraph answer
    /// wears one rail mark, not one per paragraph.
    fn render_with(
        &self,
        width: u16,
        force_full: bool,
        agent: AgentSlot,
        lead: bool,
    ) -> Vec<Line<'static>> {
        let level = self.render_level(force_full);
        let mut lines = self.body(width, level);
        // Markdown is the one body that omits the opening blank every other
        // kind wears, so a lead prose answer would abut the tool call above it.
        // Restore that blank on the response head; continuation paragraphs
        // (lead = false) stay tight, and reflow folds it against any trailing
        // blank so the gap never doubles.
        if lead && self.markdown_src().is_some() && !lines.first().is_some_and(is_blank) {
            lines.insert(0, Line::default());
        }
        if let Some(kind) = self.rail_kind(level) {
            // A continuation prose paragraph keeps the gutter but blanks its
            // glyph — one response, one `·`, on its head row.
            let rail = if lead {
                rail::span(kind, agent, self.magnitude())
            } else {
                Span::raw(" ".repeat(RAIL_W))
            };
            // The common rail-seating path for every kind, so a body can never
            // hang inverted beneath the glyph again. Carve the rail's `RAIL_W`
            // gutter from the opening row — invisible where the row already
            // insets its content (markdown's `MD_INDENT`, a diff's two-column
            // gutter), a rightward push where it is flush — then hang every
            // continuation under that content by padding any row shy of the
            // gutter up to it. A diff's rows are already inset, so the hang is a
            // no-op for them; a flush surfaced card is the case it rescues.
            let idx = lines.iter().position(|l| !is_blank(l)).unwrap_or(0);
            // A block that renders no row at all — an invisible query
            // placeholder on the log tee — seats no rail.
            if idx < lines.len() {
                shrink_leading_ws(&mut lines[idx], RAIL_W);
                for (i, line) in lines.iter_mut().enumerate() {
                    let short = RAIL_W.saturating_sub(leading_ws(line));
                    if i != idx && short > 0 && !is_blank(line) {
                        line.spans.insert(0, Span::raw(" ".repeat(short)));
                    }
                }
                lines[idx].spans.insert(0, rail);
            }
        }
        lines
    }

    /// The rail-less body at `width`, graded by `level`: [`Reveal::Summary`]
    /// the one-line summary; [`Reveal::Context`] the summary plus [`N`] lines;
    /// [`Reveal::Full`] the full source.  (A tool call's [`Reveal::Census`] is
    /// rendered by [`super::group`], never here — a standalone call folds onto
    /// its summary.) Plain prose and chrome ignore the level — they are always
    /// full; thinking grades from header to partial trace to full trace.
    fn body(&self, width: u16, level: Reveal) -> Vec<Line<'static>> {
        match &self.kind {
            BlockKind::ToolCall { tool, summary, cmd } => match level {
                Reveal::Full => line::tool_call_expanded(summary, tool, cmd, width),
                Reveal::Context => line::tool_call_context(summary, tool, cmd, N, width),
                // A standalone tool call never renders below its summary: the
                // log tee forces full, and on screen a call is the head of a
                // coalesced run, whose Census is rendered by `group`, not here.
                Reveal::Summary | Reveal::Census => {
                    line::tool_call_collapsed(summary, tool, self.result_size, width)
                }
            },
            BlockKind::Markdown { src } => md::render_md(src, width, MD_INDENT, self.fidelity),
            BlockKind::Thinking(t) => {
                let mut ls = line::thinking_header(&t.text, t.answer_chars);
                if level >= Reveal::Context {
                    ls.push(Line::default());
                    let shadow = md::render_reasoning(&t.text, width, MD_INDENT);
                    ls.extend(if level >= Reveal::Full {
                        shadow
                    } else {
                        first_rows(shadow, N)
                    });
                }
                ls
            }
            BlockKind::Subagent {
                title,
                text,
                error,
                elapsed,
            } => {
                let size = text.lines().count() as u32;
                let mut ls = line::subagent_header(title, size, error.as_deref(), *elapsed);
                // L1 is the header alone; L2/L3 extend it with the rendered
                // body. Build the header first so the markdown rows append
                // after it intact — the header is row 0 and the markdown's own
                // first-rows/leading-blank logic never touches it.
                match level {
                    Reveal::Full => ls.extend(md::render_md(text, width, MD_INDENT, self.fidelity)),
                    Reveal::Context => ls.extend(first_rows(
                        md::render_md(text, width, MD_INDENT, self.fidelity),
                        N,
                    )),
                    Reveal::Summary | Reveal::Census => {}
                }
                ls
            }
            // A surfaced general card (no diff) is the model's deliberate
            // artifact — framed as a bounded object. Diff cards keep their
            // own rich rendering; folded observation/write cards stay plain.
            BlockKind::Card { card, origin } => {
                if !card.has_diff() && *origin == CardOrigin::Surfaced {
                    line::render_card_framed(card, width)
                } else {
                    // A diff honours L1/L2/L3; Census never reaches a card, so
                    // it folds onto the summary.
                    let diff_level = match level {
                        Reveal::Context => 2,
                        Reveal::Full => 3,
                        Reveal::Census | Reveal::Summary => 1,
                    };
                    line::render_card(card, diff_level)
                }
            }
            // The per-block log tee renders a query alone as `tool  query`,
            // matching a standalone tool call's header; an invisible
            // placeholder renders nothing.  On screen the flatten coalesces a
            // run of these into one `tool : …` line instead ([`super::viewport`]).
            BlockKind::Query { tool, query } => match query {
                Some(q) => line::tool_call_static(q, tool),
                None => Vec::new(),
            },
            BlockKind::Chrome { lines, .. } => lines.clone(),
        }
    }

    /// The rail shape this block wears.  Chrome lifts its [`RailShape`]
    /// discriminant; patches, tool calls, and markdown derive theirs from
    /// the variant.  Plain chrome is ambient frame text and carries no
    /// rail.  A tool call's disclosure triangle tracks the level: `▽` once
    /// it reveals context (L2+), `▸` while reduced.
    fn rail_kind(&self, level: Reveal) -> Option<RailKind> {
        match &self.kind {
            BlockKind::ToolCall { .. } => Some(RailKind::ToolCall(level >= Reveal::Context)),
            // A summary-less query is a tool call still — the shut triangle
            // `▸`, inert (nothing to dial open).  Only the per-block log tee
            // renders a query alone and reaches this; on screen the coalesced
            // run prepends its own rail.
            BlockKind::Query { .. } => Some(RailKind::ToolCall(false)),
            BlockKind::Markdown { .. } => Some(RailKind::Markdown),
            BlockKind::Thinking(_) => Some(RailKind::Thinking),
            // The `↘` keeps the delegated-result identity even on error; the
            // failure reads in the header suffix, not a swapped glyph.
            BlockKind::Subagent { .. } => Some(RailKind::Subagent),
            // A diff and a write are both file mutations, so both wear the
            // change-bar (`▎`); the body distinguishes located hunks from a
            // whole-file write summary. A surfaced general card is framed, and
            // the frame is its mark, so it wears no rail glyph (like the
            // prompt's band). An observation card folds into its ral group on
            // screen and so earns no rail glyph; only the per-block `user.log`
            // tee ever renders one alone, where an effect has no kind-mark.
            BlockKind::Card { card, origin } => {
                if card.has_diff() || *origin == CardOrigin::Write {
                    Some(RailKind::Patch)
                } else {
                    None
                }
            }
            BlockKind::Chrome { shape, .. } => match shape {
                RailShape::Step => Some(RailKind::Step),
                RailShape::Error => Some(RailKind::Error),
                RailShape::Plain => None,
                // The PROMPT_INK body tint is the prompt's body mark; the `❖`
                // fence is its margin mark — a rare landmark, on both axes.
                RailShape::Prompt => Some(RailKind::Prompt),
            },
        }
    }
}

/// The first `k` rendered rows of `lines`, preserving leading blanks but
/// keeping at least one row so the rail always has somewhere to land.
/// Used for the subagent result's L2 context view: `render_md` lays out the
/// whole result, and truncating its rows keeps a code fence's opening
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
/// The width of a line's leading run of all-space spans — the indent the
/// rail's gutter is carved from or hung under. Counted span-wise to match
/// [`shrink_leading_ws`], which only trims leading spans the builders emit
/// as their own span (markdown's inset, a wrapped continuation's hang).
fn leading_ws(line: &Line<'static>) -> usize {
    let mut w = 0;
    for span in &line.spans {
        let s = span.content.as_ref();
        if s.is_empty() {
            continue;
        }
        if !s.chars().all(|c| c == ' ') {
            break;
        }
        w += s.chars().count();
    }
    w
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::{Row, Seg};

    fn plain(line: &Line<'_>) -> String {
        line.spans.iter().map(|s| s.content.as_ref()).collect()
    }

    fn indent_of(s: &str) -> usize {
        s.len() - s.trim_start().len()
    }

    fn diff_block() -> Block {
        let hunks = vec![Hunk {
            start: 1,
            rows: vec![
                Row::Add(vec![Seg::plain("a new line")]),
                Row::Add(vec![Seg::plain("another")]),
            ],
        }];
        Block::patch("src/lib.rs".into(), hunks)
    }

    fn subagent_block() -> Block {
        Block::subagent(
            "delegate".into(),
            "the result\nspanning\na few lines".into(),
            None,
            Duration::from_secs(2),
            Fidelity::default(),
        )
    }

    /// Model prose is not dialable: it has no summary to collapse to, so the
    /// dial and click gestures are inert and it renders identically whatever
    /// level is asked of it.
    #[test]
    fn markdown_is_inert_prose() {
        let mut block = Block::markdown(
            "# heading\n\nA paragraph of prose that the answer is to read.".into(),
            Fidelity::default(),
        );
        assert!(!block.dialable());

        let full = block.body(READ_W, Reveal::Full);
        assert_eq!(
            block.body(READ_W, Reveal::Summary),
            full,
            "L1 must render full prose"
        );
        assert_eq!(
            block.body(READ_W, Reveal::Context),
            full,
            "L2 must render full prose"
        );

        let before = block.level();
        block.dial(-1);
        block.cycle();
        assert_eq!(block.level(), before, "gestures are inert on prose");
    }

    /// A coalesced run's anchor (a tool call) floors at L0, the census; every
    /// other dialable kind — a diff, a subagent — floors one rung higher, at
    /// the summary.  The wheel saturates at each kind's floor.
    #[test]
    fn the_floor_is_census_for_a_run_summary_otherwise() {
        let cases = [
            (
                Block::tool_call("ral", "read lib".into(), "read src/lib.rs".into(), 0),
                Reveal::Census,
            ),
            (diff_block(), Reveal::Summary),
            (subagent_block(), Reveal::Summary),
        ];
        for (mut block, floor) in cases {
            assert!(block.dialable());
            // Each notch is one rung; dialing past the floor pins there.
            for _ in 0..4 {
                block.dial(-1);
            }
            assert_eq!(block.level(), floor);
            // Dialing up past the ceiling pins at Full.
            for _ in 0..4 {
                block.dial(1);
            }
            assert_eq!(block.level(), Reveal::Full);
            // From the ceiling, one click wraps straight to that same floor.
            block.cycle();
            assert_eq!(block.level(), floor);
        }
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
            assert_eq!(
                indent_of(&plain(row)),
                4,
                "row lost its indent: {:?}",
                plain(row)
            );
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
        let line = Line::from(Span::raw(
            "one two three four five six seven eight nine ten",
        ));
        let rows = wrap_line(&line, 16);
        assert!(rows.len() > 1);
        for row in &rows {
            assert_eq!(indent_of(&plain(row)), 0);
        }
    }
}
