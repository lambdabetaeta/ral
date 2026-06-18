//! Collapsible scrollback blocks.
//!
//! A viewport's scrollback is a sequence of [`Block`]s, not a flat line
//! buffer.  A tool call is the one interactive block — it renders as its
//! summary when shut and as the full ral script when open — and every
//! other block carries content that renders the same way every time:
//! streamed markdown source, a diff, or pre-built chrome lines.  Each
//! block memoises the lines it last produced, keyed by the width it was
//! asked for, so re-flattening the buffer each frame re-renders only the
//! block the user just toggled, or the whole buffer once on a resize.

use super::line::{self, READ_W, RAIL_W, is_blank};
use super::md::{self, MD_INDENT};
use super::rail::{self, RailKind};
use crate::bus::Hunk;
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
    /// Everything else — renders the static `❖`.
    #[default]
    Generic,
}

/// What a block carries.  Each variant renders as a pure function of its
/// data and the target width — and, for a tool call, its open state.
pub(super) enum BlockKind {
    /// A tool call worth revealing: `summary` is the one-line label
    /// shown shut, `cmd` the full ral source shown open.  Summary-less
    /// calls (the `fff` query, an invalid-input header) have nothing to
    /// reveal and arrive as [`BlockKind::Chrome`] instead.
    ToolCall {
        tool: &'static str,
        summary: String,
        cmd: String,
        open: bool,
    },
    /// Streamed assistant prose; re-wrapped from source at every width.
    Markdown(String),
    /// A diff; re-rendered from its located hunks at every width.
    Patch { path: String, hunks: Vec<Hunk> },
    /// Pre-built chrome whose builder already wrapped to [`READ_W`] — a
    /// step separator, prompt echo, error, write, task, meter, banner,
    /// subagent breadcrumb, or a summary-less tool call.  `shape` lets
    /// the rail (and the size/grain moves) dispatch on the chrome
    /// sub-kind without re-parsing the built lines.
    Chrome {
        shape: RailShape,
        lines: Vec<Line<'static>>,
    },
}

/// A block paired with the lines it last rendered, memoised by width.
pub(super) struct Block {
    kind: BlockKind,
    /// The producing agent's palette slot, stamped at push.
    agent: AgentSlot,
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
    fn new(kind: BlockKind, agent: AgentSlot) -> Self {
        Self {
            kind,
            agent,
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
            BlockKind::ToolCall {
                tool,
                summary,
                cmd,
                open: false,
            },
            agent,
        )
    }
    pub(super) fn markdown(src: String, agent: AgentSlot) -> Self {
        Self::new(BlockKind::Markdown(src), agent)
    }
    pub(super) fn patch(path: String, hunks: Vec<Hunk>, agent: AgentSlot) -> Self {
        Self::new(BlockKind::Patch { path, hunks }, agent)
    }
    pub(super) fn chrome(shape: RailShape, lines: Vec<Line<'static>>, agent: AgentSlot) -> Self {
        Self::new(BlockKind::Chrome { shape, lines }, agent)
    }

    /// The producing agent's palette slot.
    pub(super) fn agent(&self) -> AgentSlot {
        self.agent
    }

    /// The block's magnitude, where defined: total changed lines
    /// (deletions + additions) for a patch, `None` elsewhere.  The rail's
    /// value-step and the header size-bar both read this.
    pub(super) fn magnitude(&self) -> Option<u32> {
        match &self.kind {
            BlockKind::Patch { hunks, .. } => Some(line::patch_magnitude(hunks)),
            _ => None,
        }
    }

    /// True for the one block kind a click opens.
    pub(super) fn expandable(&self) -> bool {
        matches!(self.kind, BlockKind::ToolCall { .. })
    }

    /// True for a tool call — the one block kind a result magnitude
    /// attaches to via [`Self::set_result_size`].
    pub(super) fn is_tool_call(&self) -> bool {
        matches!(self.kind, BlockKind::ToolCall { .. })
    }

    /// Attach a tool call's result magnitude (`text.lines().count()`),
    /// dropping the memo so the collapsed header re-renders with its
    /// size-bar.  A no-op set on a non-tool-call block would never light
    /// a bar, but callers gate on [`Self::is_tool_call`].
    pub(super) fn set_result_size(&mut self, n: u32) {
        self.result_size = Some(n);
        self.cache = None;
    }

    /// Flip a tool call between shut and open, dropping its memo; a
    /// no-op on any other block.
    pub(super) fn toggle(&mut self) {
        if let BlockKind::ToolCall { open, .. } = &mut self.kind {
            *open = !*open;
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
    /// width-independent — a tool call always opened, so the script is
    /// on the record even while shut on screen.  Routes through the same
    /// rendering path as [`Self::render`] (rail included) with the tool
    /// call forced open.
    pub(super) fn log_lines(&self) -> Vec<Line<'static>> {
        self.render_with(READ_W, true)
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        self.render_with(width, false)
    }

    /// Build the block's body lines (rail-less) then prepend the
    /// data-encoding rail span to the first content row.  `force_open`
    /// reveals a tool call's full script regardless of its toggle — used
    /// only by [`Self::log_lines`] so the on-disk transcript is complete.
    fn render_with(&self, width: u16, force_open: bool) -> Vec<Line<'static>> {
        let mut lines = self.body(width, force_open);
        let kind = self.rail_kind(force_open);
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
        lines
    }

    /// The rail-less body at `width`.
    fn body(&self, width: u16, force_open: bool) -> Vec<Line<'static>> {
        match &self.kind {
            BlockKind::ToolCall {
                tool,
                summary,
                cmd,
                open,
            } => {
                if *open || force_open {
                    line::tool_call_expanded(summary, tool, cmd, width)
                } else {
                    line::tool_call_collapsed(summary, tool, self.result_size, width)
                }
            }
            BlockKind::Markdown(src) => md::render_md(src, width, MD_INDENT),
            BlockKind::Patch { path, hunks } => line::patch(path, hunks),
            BlockKind::Chrome { lines, .. } => lines.clone(),
        }
    }

    /// The rail shape this block wears.  Chrome lifts its [`RailShape`]
    /// discriminant; patches, tool calls, and markdown derive theirs from
    /// the variant.
    fn rail_kind(&self, force_open: bool) -> RailKind {
        match &self.kind {
            BlockKind::ToolCall { open, .. } => RailKind::ToolCall(*open || force_open),
            BlockKind::Markdown(_) => RailKind::Markdown,
            BlockKind::Patch { .. } => RailKind::Patch,
            BlockKind::Chrome { shape, .. } => match shape {
                RailShape::Step => RailKind::Step,
                RailShape::Error => RailKind::Error,
                RailShape::Generic => RailKind::Generic,
            },
        }
    }
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
