//! Collapsible scrollback blocks.
//!
//! A viewport's scrollback is a sequence of [`Block`]s, each on a rung of the
//! [`Reveal`] ladder.  Only a kind with a summary to collapse to is dialable —
//! tool calls, diffs, subagent results, acts, thinking; prose is product to
//! read rather than process to reduce, and chrome is already a line or two, so
//! both render full.  A block memoises the lines it last produced, keyed by the
//! width asked for, so a dial re-renders one block and a resize the buffer.

use super::fidelity::Fidelity;
use super::group;
use super::line::{self, is_blank};
use super::md::{self, MD_INDENT};
use super::palette::{QUEUED_PROMPT_BG, RAIL_W, READ_W, SLATE};
use super::rail::{self, RailKind};
use crate::bus::card::{Card, ObservationKind};
use ratatui::style::{Color, Modifier, Style};
use ratatui::text::{Line, Span};
use std::time::Duration;

/// Index into [`super::palette::AGENT_HUES`], wrapping: root is `0`, each
/// subagent the next slot at birth.  Carried by value, so the rail needs no
/// lookup on `App`.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) struct AgentSlot(pub u8);

/// Coarse chrome sub-kind, carried so the rail need not re-parse built lines.
/// Every other kind derives its shape from its [`BlockKind`] variant.
#[derive(Clone, Copy, Default, PartialEq, Eq, Debug)]
pub(super) enum RailShape {
    Step,
    /// A detached block that settled — the `` `done `` a deferred worker flushes
    /// at completion. It wears the `↘` of [`RailKind::Subagent`]: background
    /// work landing in root's scrollback turns after the run that spawned it is
    /// the same event as an agent's answer arriving, whatever produced it.
    Settled,
    Error,
    /// The turn the human stopped: it wears the `╳` an error does — the work
    /// broke off either way — but stays a separate shape so [`Block::is_error`]
    /// and the matrix cell it drives keep reporting failures only.
    Cancelled,
    /// A meta-notice — a model switch, an export, a stall: an annotation
    /// rather than a navigable block.
    #[default]
    Plain,
    /// The human's turn, tinted [`super::palette::PROMPT_INK`] and ruled
    /// full-width by the flatten.  No band — background is the machine's.
    Prompt,
}

/// Where a [`BlockKind::Card`] came from — what the coalescing projection
/// ([`super::group`]) reads to tell an *effect* it may fold into a ral block
/// from a *barrier* that splits one.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub(super) enum CardOrigin {
    /// A read / grep / exec a call produced: foldable, carrying its `|>` kind
    /// and how many it folds (one card may comma-join several) for the census.
    Observation { kind: ObservationKind, count: u32 },
    /// A write — an effect, but a barrier all the same: like a diff it ends the
    /// current ral block, and it wears the `▎` a mutation deserves rather than
    /// dissolving into the run's tally.
    Write,
    /// A diff or a deliberately `surface`d card — the model's own
    /// communication, a barrier.
    Surfaced,
}

/// One committed reasoning run.  `answer_chars` is the mass of the prose it
/// became, the deliberation grain's denominator, measured by the view that
/// draws it: the run commits ahead of that prose and so cannot carry it.
/// While the run is still streaming it has no block at all — the live edge is
/// `Viewport::thinking_seat`, a magnitude row.
pub(super) struct Thinking {
    pub(super) text: String,
    pub(super) answer_chars: u32,
}

/// What a block carries — each variant a pure function of its data, the target
/// width, and the block's rung.
pub(super) enum BlockKind {
    /// `summary` is the collapsed label, `details` the ral source behind it.  A
    /// summary-less call arrives as [`BlockKind::PlainTool`] instead.  `tool`
    /// is owned, not `&'static str`: a resumed or replayed commit hands this
    /// its tool name off the wire, with no static string to borrow.
    DiallableTool {
        #[allow(
            dead_code,
            reason = "kept for the resumed/replayed reader P6 wires up; today's live path already knows a call is `ral` before this block exists, off the record-side ToolCall, so nothing reads it back here yet"
        )]
        tool: String,
        summary: String,
        details: String,
    },
    /// A summary-less tool call, inert under the shut triangle.  `details` is
    /// `None` for a parse failure (`INVALID_INPUT`): such a call renders
    /// nothing, present only as the boundary a stray result stops at.
    PlainTool { details: Option<String> },
    /// A harness act — `spawn`, `cancel`, `message`, `reply`, `schedule`,
    /// `unschedule`.  It changes the world outside the turn, so it is no
    /// observation: never coalesced into a `ral` run, and carrying no magnitude.
    Act {
        verb: String,
        subject: Option<String>,
        payload: String,
        failed: bool,
    },
    /// Streamed assistant prose, re-wrapped from source at every width.
    Markdown { src: String },
    /// A reasoning trace, separate from the answer it produced.
    Thinking(Thinking),
    /// An async subagent's result, landed in root's scrollback.  Its own kind
    /// because `Markdown` cannot carry `name`/`elapsed`/`error` and a `Card`
    /// would lose the `↘` identity.
    Subagent {
        name: String,
        text: String,
        error: Option<String>,
        elapsed: Duration,
    },
    /// A render document a kit surfaced — a stack of [`Card`] marks re-rendered
    /// from data at every width.  Only one holding a `diff` mark is dialable.
    Card { card: Card, origin: CardOrigin },
    /// Pre-built chrome whose builder already wrapped to [`READ_W`].
    Chrome {
        shape: RailShape,
        lines: Vec<Line<'static>>,
    },
}

/// Content lines L2 reveals past the summary, for every dialable kind.
const N: usize = 3;

/// How much of a dialable block is disclosed, low to high; `Ord` compares the
/// rungs.  The reachable band is `[Block::floor, Full]` — only a `|>` run
/// reaches `Census`.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub(super) enum Reveal {
    /// L0: a run's `|>` effects tallied, rendered by [`super::group`] alone.
    Census,
    /// L1: the live tip, or a collapsed one-line header.
    Summary,
    /// L2: the summary plus [`N`] lines of context.
    Context,
    /// L3: the full source.
    Full,
}

/// Rows for the prompts still waiting to be sent — the very chrome a committed
/// prompt wears, washed to read as pending and capped to `max_rows`.
pub(super) fn queued_prompt_rows(
    messages: &[String],
    width: u16,
    max_rows: usize,
) -> Vec<Line<'static>> {
    if width == 0 || max_rows == 0 {
        return Vec::new();
    }

    let mut out = Vec::new();
    for message in messages {
        let mut prompt = Block::chrome(RailShape::Prompt, line::user_prompt(message));
        let rows = prompt.lines(width, AgentSlot::default(), true);
        let first = if out.is_empty() || out.last().is_some_and(line::is_blank) {
            rows.iter().take_while(|row| line::is_blank(row)).count()
        } else {
            0
        };
        append_visual_rows(
            &mut out,
            &rows[first..],
            width,
            true,
            Some(QUEUED_PROMPT_BG),
        );
    }

    if out.len() > max_rows {
        let hidden = out.len() - (max_rows - 1);
        out.truncate(max_rows - 1);
        out.push(line::wash(
            Line::from(Span::styled(
                format!("⋯ ({hidden} more)"),
                Style::default().fg(SLATE).add_modifier(Modifier::ITALIC),
            )),
            QUEUED_PROMPT_BG,
            Some(width as usize),
        ));
    }
    out
}

/// Wrap block-rendered logical lines into visual rows — the shared last step of
/// the transcript flatten and the queued-prompt projection.  With `prompt` set
/// the fence goes in above the first visible row and outside any `wash`: a
/// boundary marks the plane's edge rather than lying within it, so a prompt's
/// rule reads the same committed or queued.
pub(super) fn append_visual_rows(
    out: &mut Vec<Line<'static>>,
    lines: &[Line<'static>],
    width: u16,
    prompt: bool,
    wash: Option<Color>,
) -> usize {
    let before = out.len();
    let mut fenced = false;
    for line in lines {
        for vrow in line::wrap_line(line, width as usize) {
            if prompt && !fenced && !line::is_blank(&vrow) {
                out.push(line::prompt_fence(width));
                fenced = true;
            }
            out.push(wash_row(vrow, width, wash));
        }
    }
    out.len() - before
}

fn wash_row(row: Line<'static>, width: u16, wash: Option<Color>) -> Line<'static> {
    match wash {
        Some(bg) => line::wash(row, bg, Some(width as usize)),
        None => row,
    }
}

impl Reveal {
    fn up(self) -> Self {
        match self {
            Self::Census => Self::Summary,
            Self::Summary => Self::Context,
            Self::Context | Self::Full => Self::Full,
        }
    }

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
    level: Reveal,
    /// The epistemic signal — context pressure and echo, set at markdown
    /// commit.  Sound (`0/0`) elsewhere, so only prose degrades its medium.
    fidelity: Fidelity,
    /// A tool call's result magnitude — the line count from its
    /// `Display::ToolCall`'s `result_lines`, attached after the fact by
    /// `Viewport::set_result_size`.
    result_size: Option<u32>,
    /// Lines for the current state at `cache_w`, `None` once stale.
    cache: Option<Vec<Line<'static>>>,
    cache_w: u16,
}

impl Block {
    /// Build at the kind's default rung: tool calls, subagent results and acts
    /// arrive collapsed to their headers, every other kind — thinking
    /// included, so a trace streams in the open — full.
    fn new(kind: BlockKind, fidelity: Fidelity) -> Self {
        let level = match kind {
            BlockKind::DiallableTool { .. }
            | BlockKind::Subagent { .. }
            | BlockKind::Act { .. } => Reveal::Summary,
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

    /// `context` is the turn's degradation floor, so the coalesced intent line
    /// drains as committed prose does; echo cannot apply to a stated purpose.
    pub(super) fn tool_call(
        tool: impl Into<String>,
        summary: String,
        details: String,
        context: u8,
    ) -> Self {
        Self::new(
            BlockKind::DiallableTool {
                tool: tool.into(),
                summary,
                details,
            },
            Fidelity { context, echo: 0 },
        )
    }
    pub(super) fn act(
        verb: impl Into<String>,
        subject: Option<String>,
        payload: String,
        failed: bool,
    ) -> Self {
        Self::new(
            BlockKind::Act {
                verb: verb.into(),
                subject,
                payload,
                failed,
            },
            Fidelity::default(),
        )
    }
    pub(super) fn markdown(src: String, fidelity: Fidelity) -> Self {
        Self::new(BlockKind::Markdown { src }, fidelity)
    }
    pub(super) fn thinking(text: String, answer_chars: u32) -> Self {
        Self::new(
            BlockKind::Thinking(Thinking { text, answer_chars }),
            Fidelity::default(),
        )
    }
    /// `fidelity` is root's, so the revealed markdown degrades as its prose does.
    pub(super) fn subagent(
        name: String,
        text: String,
        error: Option<String>,
        elapsed: Duration,
        fidelity: Fidelity,
    ) -> Self {
        Self::new(
            BlockKind::Subagent {
                name,
                text,
                error,
                elapsed,
            },
            fidelity,
        )
    }
    pub(super) fn card(card: Card) -> Self {
        Self::card_with(card, CardOrigin::Surfaced)
    }
    pub(super) fn observation_card(card: Card, kind: ObservationKind, count: u32) -> Self {
        Self::card_with(card, CardOrigin::Observation { kind, count })
    }
    pub(super) fn write_card(card: Card) -> Self {
        Self::card_with(card, CardOrigin::Write)
    }
    fn card_with(card: Card, origin: CardOrigin) -> Self {
        Self::new(BlockKind::Card { card, origin }, Fidelity::default())
    }
    pub(super) fn chrome(shape: RailShape, lines: Vec<Line<'static>>) -> Self {
        Self::new(BlockKind::Chrome { shape, lines }, Fidelity::default())
    }
    pub(super) fn plain_call(details: Option<String>) -> Self {
        Self::new(BlockKind::PlainTool { details }, Fidelity::default())
    }

    pub(super) fn level(&self) -> Reveal {
        self.level
    }

    /// Changed lines for a patch, source lines for prose (markdown, thinking, a
    /// subagent result).  The rail's value-step reads it, so prose volume
    /// lightens the rail as a diff's does.
    #[allow(
        clippy::cast_possible_truncation,
        reason = "transcript-block line count; u32 headroom far exceeds any in-memory transcript"
    )]
    pub(super) fn magnitude(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Card { card, .. } => card.magnitude(),
            BlockKind::Markdown { src, .. } => Some(src.lines().count() as u32),
            BlockKind::Thinking(t) => Some(t.text.lines().count() as u32),
            BlockKind::Subagent { text, .. } => Some(text.lines().count() as u32),
            _ => None,
        }
    }

    /// The session's *code* footprint — a diff card's changed lines and nothing
    /// else.  Distinct from [`Self::magnitude`], which counts prose too: the
    /// matrix's "lines touched" is a write footprint, not a volume.
    pub(super) fn lines_changed(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Card { card, .. } => card.magnitude(),
            _ => None,
        }
    }

    pub(super) fn dialable(&self) -> bool {
        match &self.kind {
            BlockKind::DiallableTool { .. }
            | BlockKind::Subagent { .. }
            | BlockKind::Act { .. }
            | BlockKind::Thinking(_) => true,
            BlockKind::Card { card, .. } => card.has_diff(),
            BlockKind::Markdown { .. } | BlockKind::PlainTool { .. } | BlockKind::Chrome { .. } => {
                false
            }
        }
    }

    /// The one kind [`Self::set_result_size`] attaches a magnitude to.
    pub(super) fn is_tool_call(&self) -> bool {
        matches!(self.kind, BlockKind::DiallableTool { .. })
    }

    /// True for a block the coalescing projection folds into a ral block — a
    /// tool call, or a read / grep / exec effect.  Everything else is a
    /// *barrier* splitting one block from the next, save a step boundary
    /// interior to a run, which the viewport's run scan bridges as bookkeeping.
    pub(super) fn observation(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::DiallableTool { .. }
                | BlockKind::Card {
                    origin: CardOrigin::Observation { .. },
                    ..
                }
        )
    }

    /// This observation's census contribution — its `|>` kind and count.
    pub(super) fn io_tally(&self) -> Option<(ObservationKind, u32)> {
        match &self.kind {
            BlockKind::Card {
                origin: CardOrigin::Observation { kind, count },
                ..
            } => Some((*kind, *count)),
            _ => None,
        }
    }

    /// This call's parts for the coalesced ral block.  `None` on anything but a
    /// tool call, so only a call opens a slot in the group.
    pub(super) fn call_view(&self) -> Option<group::CallParts<'_>> {
        match &self.kind {
            BlockKind::DiallableTool {
                summary, details, ..
            } => Some(group::CallParts {
                intent: summary,
                cmd: details,
                magnitude: self.result_size,
                context: self.fidelity.context,
            }),
            _ => None,
        }
    }

    /// An effect's rail-less rows, to fold under its call's intent.
    pub(super) fn effect_lines(&self) -> Vec<Line<'static>> {
        match &self.kind {
            BlockKind::Card {
                card,
                origin: CardOrigin::Observation { .. },
            } => line::render_card(card, 3),
            _ => Vec::new(),
        }
    }

    /// `/copy` walks the trailing run of these — each fence-safe paragraph is
    /// its own block, so the run is the whole reply.
    pub(super) fn markdown_src(&self) -> Option<&str> {
        match &self.kind {
            BlockKind::Markdown { src, .. } => Some(src),
            _ => None,
        }
    }

    /// [`Self::markdown_src`] for the reasoning lane.
    pub(super) fn thinking_src(&self) -> Option<&str> {
        match &self.kind {
            BlockKind::Thinking(t) => Some(&t.text),
            _ => None,
        }
    }

    /// The epistemic signal this block was built with — what a live tail
    /// re-renders under, so growing prose keeps the ink it commits in.
    pub(super) fn fidelity(&self) -> Fidelity {
        self.fidelity
    }

    pub(super) fn is_thinking(&self) -> bool {
        matches!(self.kind, BlockKind::Thinking(_))
    }

    /// True for a step boundary — what the matrix's per-agent step cells count.
    pub(super) fn is_step(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::Chrome {
                shape: RailShape::Step,
                ..
            }
        )
    }

    /// Drives the matrix's `╳` cell when the session's last block is a failure.
    pub(super) fn is_error(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::Chrome {
                shape: RailShape::Error,
                ..
            }
        )
    }

    /// The human turn's echo — the one block the flatten rules full-width.
    pub(super) fn is_prompt(&self) -> bool {
        matches!(
            self.kind,
            BlockKind::Chrome {
                shape: RailShape::Prompt,
                ..
            }
        )
    }

    /// Drops the memo, so the collapsed header re-renders with its size-bar.
    pub(super) fn set_result_size(&mut self, n: u32) {
        self.result_size = Some(n);
        self.cache = None;
    }

    /// The lowest rung this block reduces to: a `|>` run, anchored on a tool
    /// call, bottoms out at its census; every other kind floors a rung higher,
    /// a census of a lone diff or subagent being meaningless.
    fn floor(&self) -> Reveal {
        match self.kind {
            BlockKind::DiallableTool { .. } => Reveal::Census,
            _ => Reveal::Summary,
        }
    }

    /// The rung above this block's, kind-aware: thinking has no `Context`
    /// reading — a trace is one thing, shown whole or as its header — so the
    /// dial hops straight between `Summary` and `Full`.
    fn rung_up(&self) -> Reveal {
        match (self.is_thinking(), self.level.up()) {
            (true, Reveal::Context) => Reveal::Full,
            (_, next) => next,
        }
    }

    /// The rung below, floored per kind and hopping `Context` like
    /// [`Self::rung_up`].
    fn rung_down(&self) -> Reveal {
        let next = self.level.down().max(self.floor());
        match (self.is_thinking(), next) {
            (true, Reveal::Context) => Reveal::Summary,
            _ => next,
        }
    }

    /// One wheel notch — up reveals, down reduces — saturating at the band's edges.
    pub(super) fn dial(&mut self, delta: i8) {
        if self.dialable() {
            let next = if delta >= 0 {
                self.rung_up()
            } else {
                self.rung_down()
            };
            self.set_level(next);
        }
    }

    /// One click: a rung up, the ceiling wrapping to the floor, so clicking
    /// walks every reachable rung rather than toggling the extremes.
    pub(super) fn cycle(&mut self) {
        if self.dialable() {
            let next = if self.level == Reveal::Full {
                self.floor()
            } else {
                self.rung_up()
            };
            self.set_level(next);
        }
    }

    /// The one seam [`Self::dial`] and [`Self::cycle`] commit through.
    fn set_level(&mut self, next: Reveal) {
        if next != self.level {
            self.level = next;
            self.cache = None;
        }
    }

    /// Restore a rung a prior sync's side table remembered — a printer
    /// rebuilds this block fresh from the fold's memo every sync, so its own
    /// dial state cannot ride inside the block the way a live mutation would.
    pub(super) fn set_reveal(&mut self, level: Reveal) {
        self.set_level(level);
    }

    /// The block's lines at `width`, rebuilding the memo when it is cold or was
    /// filled at another width.  `lead` says whether this block opens its
    /// rail-run or continues a prior paragraph's; like `agent`, arrival order
    /// fixes it, so it stays out of the width-keyed memo.
    pub(super) fn lines(&mut self, width: u16, agent: AgentSlot, lead: bool) -> &[Line<'static>] {
        if self.cache.is_none() || self.cache_w != width {
            self.cache = Some(self.render(width, agent, lead));
            self.cache_w = width;
        }
        self.cache.as_deref().expect("just filled")
    }

    /// The block as it belongs in the session log: width-independent and forced
    /// to L3, so the script / diff / prose is on the record even while reduced
    /// on screen.  `lead` matches the screen, so one response keeps one `·`.
    pub(super) fn log_lines(&self, agent: AgentSlot, lead: bool) -> Vec<Line<'static>> {
        self.render_with(READ_W, true, agent, lead)
    }

    fn render(&self, width: u16, agent: AgentSlot, lead: bool) -> Vec<Line<'static>> {
        self.render_with(width, false, agent, lead)
    }

    fn render_level(&self, force_full: bool) -> Reveal {
        if force_full { Reveal::Full } else { self.level }
    }

    /// Build the rail-less body, then seat the rail span on its first content
    /// row.  `lead` is false for a paragraph continuing a prior one: it keeps
    /// the gutter but drops the glyph, so one answer wears one rail mark.
    fn render_with(
        &self,
        width: u16,
        force_full: bool,
        agent: AgentSlot,
        lead: bool,
    ) -> Vec<Line<'static>> {
        let level = self.render_level(force_full);
        let mut lines = self.body(width, level);
        // Markdown is the one body that opens flush, so a lead answer would abut
        // the call above it; the flatten folds this blank against any trailing
        // one, so the gap never doubles.
        if lead && self.markdown_src().is_some() && !lines.first().is_some_and(is_blank) {
            lines.insert(0, Line::default());
        }
        if let Some(kind) = self.rail_kind(level) {
            let rail = if lead {
                rail::span(kind, agent, self.magnitude())
            } else {
                Span::raw(" ".repeat(RAIL_W))
            };
            // Carve the rail's gutter out of the opening row — invisible where
            // the row already insets (markdown's `MD_INDENT`, a diff's gutter),
            // a rightward push where it is flush — then hang every shorter row
            // under that content. A flush surfaced card is what this rescues.
            let idx = lines.iter().position(|l| !is_blank(l)).unwrap_or(0);
            // A block that renders no row at all seats no rail.
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

    /// The rail-less body at `width`, graded by `level`.  A run's census is
    /// rendered by [`super::group`], never here, so a standalone call folds onto
    /// its summary; prose and chrome ignore the level altogether.
    fn body(&self, width: u16, level: Reveal) -> Vec<Line<'static>> {
        match &self.kind {
            BlockKind::DiallableTool {
                summary, details, ..
            } => match level {
                Reveal::Full => line::tool_call_body(summary, details, None, width),
                Reveal::Context => line::tool_call_body(summary, details, Some(N), width),
                Reveal::Summary | Reveal::Census => {
                    line::tool_call_collapsed(summary, self.result_size, width)
                }
            },
            // The row *is* the act, so there are only two readings: the payload
            // cut to its column, or laid out whole. L2 is that layout capped.
            BlockKind::Act {
                verb,
                subject,
                payload,
                failed,
            } => {
                let row =
                    |full| line::act_row(verb, subject.as_deref(), payload, *failed, width, full);
                match level {
                    Reveal::Full => row(true),
                    Reveal::Context => first_rows(row(true), N),
                    Reveal::Summary | Reveal::Census => row(false),
                }
            }
            BlockKind::Markdown { src } => md::render_md(src, width, MD_INDENT, self.fidelity),
            BlockKind::Thinking(t) => {
                let think_chars = u32::try_from(t.text.chars().count()).unwrap_or(u32::MAX);
                let think_lines = u32::try_from(t.text.lines().count()).unwrap_or(u32::MAX);
                let mut ls = line::thinking_header(think_chars, think_lines, t.answer_chars);
                // Two rungs only: the header alone, or the whole trace.
                if level >= Reveal::Context {
                    ls.push(Line::default());
                    ls.extend(md::render_reasoning(&t.text, width, MD_INDENT));
                }
                ls
            }
            BlockKind::Subagent {
                name,
                text,
                error,
                elapsed,
            } => {
                #[allow(
                    clippy::cast_possible_truncation,
                    reason = "transcript-block line count; u32 headroom far exceeds any in-memory transcript"
                )]
                let size = text.lines().count() as u32;
                let mut ls = line::subagent_header(name, size, error.as_deref(), *elapsed);
                // The header is built first so it stays row 0, out of reach of
                // the markdown's own leading-blank and first-rows handling.
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
            // A surfaced general card is the model's deliberate artifact, framed
            // as a bounded object; diff and effect cards render plain.
            BlockKind::Card { card, origin } => {
                if !card.has_diff() && *origin == CardOrigin::Surfaced {
                    line::render_card_framed(card, width)
                } else {
                    // Census never reaches a card, so it folds onto the summary.
                    let diff_level = match level {
                        Reveal::Context => 2,
                        Reveal::Full => 3,
                        Reveal::Census | Reveal::Summary => 1,
                    };
                    line::render_card(card, diff_level)
                }
            }
            // Only the log tee renders a query alone; on screen the flatten
            // coalesces a run of these into one `tool : …` line instead.
            BlockKind::PlainTool { details, .. } => match details {
                Some(q) => line::tool_call_static(q),
                None => Vec::new(),
            },
            // A notice is not prose: it sits in its own gap, one blank row
            // above and below, whatever blanks its builder happened to bring.
            // `Viewport::reflow` collapses the gap against a blank tail, so
            // framing here reads as one row between neighbours, never two.
            BlockKind::Chrome { lines, .. } => {
                let body = trim_blanks(lines);
                if body.is_empty() {
                    // The step rule *is* a gap; there is nothing to frame.
                    vec![Line::default()]
                } else {
                    std::iter::once(Line::default())
                        .chain(body.iter().cloned())
                        .chain(std::iter::once(Line::default()))
                        .collect()
                }
            }
        }
    }
    /// The rail shape this block wears, `None` for one that seats no rail.  A
    /// tool call's triangle tracks the rung: open once it reveals context.
    fn rail_kind(&self, level: Reveal) -> Option<RailKind> {
        match &self.kind {
            BlockKind::DiallableTool { .. } => Some(RailKind::ToolCall(level >= Reveal::Context)),
            // A summary-less query is a tool call still, shut; only the log tee
            // renders one alone and so reaches this.
            BlockKind::PlainTool { .. } => Some(RailKind::ToolCall(false)),
            // The shape says when the act lands — `◷` on a clock, `↗` now —
            // and holds across every rung: an act is one thing disclosed.
            BlockKind::Act { verb, .. } => Some(match verb.as_str() {
                "schedule" | "unschedule" => RailKind::TimeAct,
                _ => RailKind::FleetAct,
            }),
            BlockKind::Markdown { .. } => Some(RailKind::Markdown),
            BlockKind::Thinking(_) => Some(RailKind::Thinking),
            // The `↘` holds even on error; the failure reads in the header.
            BlockKind::Subagent { .. } => Some(RailKind::Subagent),
            // A diff and a write are both file mutations, so both wear `▎` and
            // the body says which. A framed card's frame is its own mark, and an
            // observation folds into its group, so neither seats a glyph.
            BlockKind::Card { card, origin } => {
                if card.has_diff() || *origin == CardOrigin::Write {
                    Some(RailKind::Patch)
                } else {
                    None
                }
            }
            BlockKind::Chrome { shape, .. } => match shape {
                RailShape::Step => Some(RailKind::Step),
                RailShape::Settled => Some(RailKind::Subagent),
                RailShape::Error | RailShape::Cancelled => Some(RailKind::Error),
                RailShape::Plain => Some(RailKind::Note),
                RailShape::Prompt => Some(RailKind::Prompt),
            },
        }
    }
}

/// `lines` without its leading and trailing blank rows.
fn trim_blanks<'a>(lines: &'a [Line<'static>]) -> &'a [Line<'static>] {
    let start = lines.iter().take_while(|l| line::is_blank(l)).count();
    let tail = lines[start..]
        .iter()
        .rev()
        .take_while(|l| line::is_blank(l))
        .count();
    &lines[start..lines.len() - tail]
}

/// The first `k` rendered rows of `lines`, always at least one so the rail has
/// somewhere to land.  Truncating rendered rows rather than source keeps a code
/// fence's opening intact.
fn first_rows(mut lines: Vec<Line<'static>>, k: usize) -> Vec<Line<'static>> {
    // Leading blanks are free, so `k` counts content rows.
    let lead = lines.iter().take_while(|l| is_blank(l)).count();
    lines.truncate((lead + k).max(1));
    lines
}

/// The width of a line's leading run of all-space spans — counted span-wise to
/// match [`shrink_leading_ws`], which trims only the spans the builders emit as
/// their own (markdown's inset, a wrapped row's hang).
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

/// Reclaim `n` cells of `line`'s leading whitespace for the rail, so an inset
/// body's opening row keeps its own column.
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::bus::card::{Hunk, Mark, Row, Seg};

    fn diff_block() -> Block {
        let hunks = vec![Hunk {
            start: 1,
            rows: vec![
                Row::Add(vec![Seg::plain("a new line")]),
                Row::Add(vec![Seg::plain("another")]),
            ],
        }];
        Block::card(Card(vec![Mark::Diff {
            path: "src/lib.rs".into(),
            hunks,
        }]))
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

    /// Prose has no summary to collapse to: every rung renders the same.
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

    /// A run's anchor floors at the census, every other dialable kind at the
    /// summary.  The wheel saturates there; the click wraps to it.
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
            for _ in 0..4 {
                block.dial(-1);
            }
            assert_eq!(block.level(), floor);
            for _ in 0..4 {
                block.dial(1);
            }
            assert_eq!(block.level(), Reveal::Full);
            block.cycle();
            assert_eq!(block.level(), floor);
        }
    }

    /// An act's first two columns are pinned, so verbs align down the page and
    /// every payload opens in the same column — the alignment `render_field_rows`
    /// cannot supply, since each act is a row that would only align with itself.
    #[test]
    fn act_columns_are_pinned_across_blocks() {
        let rendered = |verb, subject: Option<&str>, payload: &str| {
            let block = Block::act(verb, subject.map(str::to_string), payload.into(), false);
            let lines = block.body(READ_W, Reveal::Summary);
            line::plain(lines.last().expect("an act renders one content row"))
        };
        assert_eq!(
            rendered(
                "spawn",
                Some("hunter"),
                "audit every unwrap() in exarch/src"
            ),
            "spawn      hunter              audit every unwrap() in exarch/src"
        );
        assert_eq!(
            rendered("unschedule", Some("nightly"), ""),
            "unschedule nightly",
            "a landed act with no argument leaves the payload cell empty"
        );
        assert_eq!(
            rendered("schedule", Some("nightly"), "0 9 * * 1-5"),
            "schedule   nightly             0 9 * * 1-5"
        );
        assert_eq!(
            rendered("reply", None, "[status: \"clean\", findings: 0]"),
            "reply                          [status: \"clean\", findings: 0]",
            "a subject-less act leaves the cell blank, not the column"
        );
    }

    /// An act changes the world; it does not measure it.  So: no magnitude, no
    /// size-bar, and no `call_view` for the projection to fold.
    #[test]
    fn an_act_carries_no_magnitude_and_no_bar() {
        let block = Block::act(
            "message",
            Some("hunter".into()),
            "focus on it".into(),
            false,
        );
        assert!(block.magnitude().is_none(), "an act ranks nothing");
        assert!(block.call_view().is_none(), "an act opens no group slot");
        assert!(!block.is_tool_call());
        for level in [Reveal::Summary, Reveal::Context, Reveal::Full] {
            let text: String = block.body(READ_W, level).iter().map(line::plain).collect();
            assert!(
                !text.contains('\u{2588}') && !text.contains('\u{2591}'),
                "no size-bar on an act row at {level:?}: {text:?}"
            );
        }
    }

    /// A refusal reads hot on the very row that names the attempt; the long
    /// form is the raise, and the raise is the model's.
    #[test]
    fn a_refused_act_tiers_its_outcome_hot() {
        let block = Block::act(
            "cancel",
            Some("hunter".into()),
            "refused: not a descendant".into(),
            true,
        );
        let lines = block.body(READ_W, Reveal::Summary);
        let row = lines.last().expect("an act renders one content row");
        assert_eq!(
            line::plain(row),
            "cancel     hunter              refused: not a descendant"
        );
        let outcome = row.spans.last().expect("the payload span");
        assert_eq!(outcome.style.fg, Some(super::super::palette::RED_HOT));
        assert!(outcome.style.add_modifier.contains(Modifier::BOLD));

        // A landed act of the same verb wears the ordinary body ink.
        let landed = Block::act(
            "cancel",
            Some("hunter".into()),
            "no live agent by that name".into(),
            false,
        );
        let landed = landed.body(READ_W, Reveal::Summary);
        assert_eq!(
            landed
                .last()
                .expect("a row")
                .spans
                .last()
                .expect("payload")
                .style
                .fg,
            Some(SLATE)
        );
    }

    /// Reduced, the payload is cut to its column; the dial is what gets the rest
    /// back, wrapped and hanging at the head row's own offset.
    #[test]
    fn a_long_payload_truncates_reduced_and_returns_whole_on_the_dial() {
        let payload = "audit every unwrap() in exarch/src and report the ones that can \
            actually fire, with the file and line and a one-sentence argument for each";
        let mut block = Block::act("spawn", Some("hunter".into()), payload.into(), false);
        assert_eq!(block.level(), Reveal::Summary, "an act arrives reduced");
        assert!(
            block.dialable(),
            "the dial is what keeps the rest reachable"
        );

        let reduced = block.body(READ_W, Reveal::Summary);
        assert_eq!(reduced.len(), 2, "reduced, an act is one row and its blank");
        let head = line::plain(&reduced[1]);
        assert!(
            head.ends_with('\u{2026}'),
            "the cut payload ends in an ellipsis: {head:?}"
        );
        assert!(
            head.chars().count() < payload.chars().count(),
            "the payload was cut to its column"
        );

        block.dial(1);
        block.dial(1);
        assert_eq!(block.level(), Reveal::Full);
        let full: String = block
            .body(READ_W, Reveal::Full)
            .iter()
            .map(|l| line::plain(l).trim_start().to_string())
            .collect::<Vec<_>>()
            .join(" ");
        for word in ["one-sentence", "argument", "for", "each"] {
            assert!(
                full.contains(word),
                "L3 restores the whole payload: {full:?}"
            );
        }

        // Measured on the seated rows, rail span included: the rail lands on
        // the head alone, so every wrapped row must carry that width itself.
        let payload_col = RAIL_W + line::ACT_VERB_W + line::ACT_SUBJECT_W;
        let rows: Vec<String> = block
            .render(READ_W, AgentSlot::default(), true)
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .filter(|row: &String| !row.trim().is_empty())
            .collect();
        assert!(rows.len() > 1, "the payload wraps at L3: {rows:?}");
        assert_eq!(
            rows[0].find("audit").map(|i| rows[0][..i].chars().count()),
            Some(payload_col),
            "the head row opens its payload in the payload column: {:?}",
            rows[0]
        );
        for row in &rows[1..] {
            assert_eq!(
                row.chars().take_while(|c| *c == ' ').count(),
                payload_col,
                "a wrapped row hangs under the payload column: {row:?}"
            );
        }
    }

    /// An act never joins a run of reads, and its shape says when it lands.
    #[test]
    fn acts_are_barriers_wearing_their_own_shapes() {
        for (verb, shape) in [
            ("spawn", RailKind::FleetAct),
            ("cancel", RailKind::FleetAct),
            ("message", RailKind::FleetAct),
            ("reply", RailKind::FleetAct),
            ("schedule", RailKind::TimeAct),
            ("unschedule", RailKind::TimeAct),
        ] {
            let block = Block::act(verb, Some("subject".into()), "payload".into(), false);
            assert!(!block.observation(), "`{verb}` must not coalesce");
            for level in [Reveal::Summary, Reveal::Context, Reveal::Full] {
                assert_eq!(block.rail_kind(level), Some(shape), "`{verb}` at {level:?}");
            }
        }
    }
}
