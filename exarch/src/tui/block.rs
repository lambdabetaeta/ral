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

use super::line::{self, READ_W};
use super::md::{self, MD_INDENT};
use crate::bus::Hunk;
use ratatui::text::{Line, Span};
use unicode_width::UnicodeWidthChar;

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
    /// subagent breadcrumb, or a summary-less tool call.
    Chrome(Vec<Line<'static>>),
}

/// A block paired with the lines it last rendered, memoised by width.
pub(super) struct Block {
    kind: BlockKind,
    /// Lines for the current state at [`Self::cache_w`], or `None` when
    /// stale — never rendered, toggled open/shut, or asked at a new
    /// width.
    cache: Option<Vec<Line<'static>>>,
    cache_w: u16,
}

impl Block {
    fn new(kind: BlockKind) -> Self {
        Self {
            kind,
            cache: None,
            cache_w: 0,
        }
    }

    pub(super) fn tool_call(tool: &'static str, summary: String, cmd: String) -> Self {
        Self::new(BlockKind::ToolCall {
            tool,
            summary,
            cmd,
            open: false,
        })
    }
    pub(super) fn markdown(src: String) -> Self {
        Self::new(BlockKind::Markdown(src))
    }
    pub(super) fn patch(path: String, hunks: Vec<Hunk>) -> Self {
        Self::new(BlockKind::Patch { path, hunks })
    }
    pub(super) fn chrome(lines: Vec<Line<'static>>) -> Self {
        Self::new(BlockKind::Chrome(lines))
    }

    /// True for the one block kind a click opens.
    pub(super) fn expandable(&self) -> bool {
        matches!(self.kind, BlockKind::ToolCall { .. })
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
    /// on the record even while shut on screen.
    pub(super) fn log_lines(&self) -> Vec<Line<'static>> {
        match &self.kind {
            BlockKind::ToolCall {
                tool, summary, cmd, ..
            } => line::tool_call_expanded(summary, tool, cmd, READ_W),
            _ => self.render(READ_W),
        }
    }

    fn render(&self, width: u16) -> Vec<Line<'static>> {
        match &self.kind {
            BlockKind::ToolCall {
                tool,
                summary,
                cmd,
                open,
            } => {
                if *open {
                    line::tool_call_expanded(summary, tool, cmd, width)
                } else {
                    line::tool_call_collapsed(summary, tool, width)
                }
            }
            BlockKind::Markdown(src) => md::render_md(src, width, MD_INDENT),
            BlockKind::Patch { path, hunks } => line::patch(path, hunks),
            BlockKind::Chrome(lines) => lines.clone(),
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
