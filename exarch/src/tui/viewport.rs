//! Per-session collapsible scrollback.
//!
//! [`Viewport`] owns the session's [`Block`] buffer, the in-flight
//! markdown accumulator, its own scroll position, and an append-only tee
//! to the per-session `user.log`.  It turns session-local content events
//! (tokens, boundaries, pre-rendered chrome) into blocks, and flattens
//! those blocks into the visual rows the renderer paints.
//!
//! Scrollback is owned here, not delegated to the host terminal: the
//! whole alt-screen frame is redrawn each tick from
//! [`Viewport::render_window`], and each tab keeps its own
//! [`Viewport::offset`] so switching panes never disturbs another's
//! position.  Every line that lands in a block is also written to
//! `user.log` — a tool call in full, script included — so the on-disk
//! file is the durable counterpart to `events.json`.

use super::block::{Block, wrap_line};
use super::line::{READ_W, is_blank, plain};
use crate::bus::Hunk;
use ratatui::text::Line;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};

pub(super) struct Viewport {
    /// The session's scrollback, oldest block first.
    blocks: Vec<Block>,
    /// In-progress assistant text since the last fence-safe paragraph
    /// boundary; renders nothing until it commits as a [`Block::markdown`].
    open: String,
    /// Top visible visual row.  Owned per-viewport so each tab keeps its
    /// place; recomputed against the frame height in [`Self::render_window`].
    offset: usize,
    /// Follow the tail: while set, [`Self::render_window`] pins `offset`
    /// to the bottom.  Cleared when the user scrolls up, re-armed when
    /// they scroll back down.
    sticky: bool,
    /// Memoised flatten of [`Self::blocks`] into wrapped visual rows.
    flat: Flat,
    /// Append-only tee of every committed line to the session's
    /// `user.log`, flushed as each block lands so the rendered transcript
    /// survives an abnormal exit.
    log: io::BufWriter<Box<dyn io::Write + Send>>,
    log_path: PathBuf,
    /// Whether the last line written to the log was blank, so leading
    /// block blanks collapse against it exactly as they do on screen.
    log_prev_blank: bool,
}

/// The visible slice of a viewport plus the figures the scrollbar needs.
pub(super) struct RenderWindow {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) offset: usize,
    pub(super) total: usize,
}

/// Memoised whole-buffer flatten: every block's lines wrapped to `width`
/// into `rows`, with `row_block[i]` the block each visual row came from.
/// Rebuilt when `dirty` or when asked at a different width.
#[derive(Default)]
struct Flat {
    width: u16,
    rows: Vec<Line<'static>>,
    row_block: Vec<usize>,
    dirty: bool,
}

/// Walk `open` for the latest paragraph break reached at fence depth
/// zero.  Returns the byte index *after* the `\n\n` so `open.drain(..idx)`
/// peels off the committable prefix; `None` means commit waits — no `\n\n`
/// yet, or every candidate sits inside an open code fence.
///
/// Fence depth toggles on lines whose first non-whitespace token is
/// three-or-more backticks or tildes; nested fences are not a thing in
/// CommonMark, so a single bit suffices.
fn safe_paragraph_break(open: &str) -> Option<usize> {
    let bytes = open.as_bytes();
    let mut depth = 0u8;
    let mut last_safe = None;
    let mut i = 0;
    while i < bytes.len() {
        let nl = match bytes[i..].iter().position(|&b| b == b'\n') {
            Some(p) => i + p,
            None => break,
        };
        let t = open[i..nl].trim_start();
        if t.starts_with("```") || t.starts_with("~~~") {
            depth ^= 1;
        }
        if nl + 1 < bytes.len() && bytes[nl + 1] == b'\n' && depth == 0 {
            last_safe = Some(nl + 2);
        }
        i = nl + 1;
    }
    last_safe
}

/// Open `path` as the session's rendered-text log, truncating any prior
/// content.  Falls back to a discarding sink when the file can't be
/// opened, so a log-path failure never disables the viewport.
fn open_log(path: &Path) -> io::BufWriter<Box<dyn io::Write + Send>> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let sink: Box<dyn io::Write + Send> = match fs::File::create(path) {
        Ok(f) => Box::new(f),
        Err(_) => Box::new(io::sink()),
    };
    io::BufWriter::new(sink)
}

impl Viewport {
    /// Build a viewport that tees its rendered text to `log_path`.
    pub(super) fn new(log_path: PathBuf) -> Self {
        Self {
            blocks: Vec::new(),
            open: String::new(),
            offset: 0,
            sticky: true,
            flat: Flat::default(),
            log: open_log(&log_path),
            log_path,
            log_prev_blank: true,
        }
    }

    /// Wipe scrollback, scroll state, and streaming buffer, and truncate
    /// the `user.log` by reopening it.  Used by `/clear` on the root.
    pub(super) fn reset(&mut self) {
        self.blocks.clear();
        self.open.clear();
        self.offset = 0;
        self.sticky = true;
        self.flat = Flat::default();
        self.log = open_log(&self.log_path);
        self.log_prev_blank = true;
    }

    /// Final flush of the `user.log` at session end; lines are already
    /// written as each block lands.  Caller owns the I/O error policy.
    pub(super) fn flush_log(&mut self) -> io::Result<&Path> {
        self.log.flush()?;
        Ok(&self.log_path)
    }

    // ── content ──────────────────────────────────────────────────────────

    /// Append a tool call as its own collapsible block.
    pub(super) fn push_tool_call(&mut self, tool: &'static str, summary: String, cmd: String) {
        self.push_block(Block::tool_call(tool, summary, cmd));
    }

    /// Append a diff block; it re-wraps with the terminal.
    pub(super) fn push_patch(&mut self, path: String, hunks: Vec<Hunk>) {
        self.push_block(Block::patch(path, hunks));
    }

    /// Append pre-rendered chrome (step header, error, write, task,
    /// meter, banner, subagent breadcrumb, summary-less tool call).
    pub(super) fn push_chrome(&mut self, lines: Vec<Line<'static>>) {
        self.push_block(Block::chrome(lines));
    }

    /// Push streamed assistant text; commit any fence-safe paragraphs.
    pub(super) fn push_token(&mut self, text: &str) {
        self.open.push_str(text);
        self.flush_complete_paragraphs();
    }

    /// End a streaming step: commit whatever remains in `open`.
    pub(super) fn close_boundary(&mut self) {
        self.flush_open();
    }

    /// Commit the longest fence-safe prefix of `open` as one markdown
    /// block.  Committing elsewhere would split a code fence across two
    /// `render_md` calls, so when no safe break exists the buffer keeps
    /// growing until the fence closes or the turn ends.
    pub(super) fn flush_complete_paragraphs(&mut self) {
        let Some(idx) = safe_paragraph_break(&self.open) else {
            return;
        };
        let chunk: String = self.open.drain(..idx).collect();
        if chunk.trim().is_empty() {
            return;
        }
        self.push_block(Block::markdown(chunk));
    }

    /// Commit whatever remains in `open` as a final markdown block.
    /// Called at turn end and `/clear`.
    pub(super) fn flush_open(&mut self) {
        let leftover = std::mem::take(&mut self.open);
        if leftover.trim().is_empty() {
            return;
        }
        self.push_block(Block::markdown(leftover));
    }

    /// Append `block`, tee its log projection, and mark the flatten
    /// stale so the next render rebuilds it.
    fn push_block(&mut self, block: Block) {
        self.log_block(&block);
        self.blocks.push(block);
        self.flat.dirty = true;
    }

    /// Tee a block's full content to `user.log`, collapsing redundant
    /// blank separators against the previous line exactly as the screen
    /// flatten does.
    fn log_block(&mut self, block: &Block) {
        for line in block.log_lines() {
            if is_blank(&line) {
                if self.log_prev_blank {
                    continue;
                }
                self.log_prev_blank = true;
            } else {
                self.log_prev_blank = false;
            }
            for s in &line.spans {
                let _ = self.log.write_all(s.content.as_bytes());
            }
            let _ = self.log.write_all(b"\n");
        }
        let _ = self.log.flush();
    }

    // ── interaction ──────────────────────────────────────────────────────

    /// The block owning visual row `row`, or `None` past the buffer's
    /// end.  Valid against the most recent [`Self::render_window`].
    pub(super) fn block_at(&self, row: usize) -> Option<usize> {
        self.flat.row_block.get(row).copied()
    }

    /// Toggle the block at `idx` if it is expandable, returning whether
    /// it changed — so the caller can tell a real toggle from a click on
    /// inert chrome.
    pub(super) fn toggle_block(&mut self, idx: usize) -> bool {
        let Some(block) = self.blocks.get_mut(idx) else {
            return false;
        };
        if !block.expandable() {
            return false;
        }
        block.toggle();
        self.flat.dirty = true;
        true
    }

    pub(super) fn scroll_up(&mut self, n: usize) {
        self.sticky = false;
        self.offset = self.offset.saturating_sub(n);
    }
    pub(super) fn scroll_down(&mut self, n: usize) {
        self.offset = self.offset.saturating_add(n);
    }

    /// Plain text of the rows `lo..=hi` (rail stripped), the projection a
    /// drag-selection copies.
    pub(super) fn selection_text(&self, lo: usize, hi: usize) -> String {
        let hi = hi.min(self.flat.rows.len().saturating_sub(1));
        if self.flat.rows.is_empty() || lo > hi {
            return String::new();
        }
        self.flat.rows[lo..=hi]
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Plain text of the whole buffer, the projection `Ctrl+Y` yanks.
    pub(super) fn yank_text(&self) -> String {
        self.flat
            .rows
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    // ── rendering ────────────────────────────────────────────────────────

    /// The visible slice at `width` × `height`, after re-flattening if
    /// stale and resolving the scroll position (pinned to the tail while
    /// sticky, clamped otherwise — and re-armed to sticky once it reaches
    /// the bottom).
    pub(super) fn render_window(&mut self, width: u16, height: usize) -> RenderWindow {
        self.reflow(width);
        let total = self.flat.rows.len();
        let max_off = total.saturating_sub(height);
        if self.sticky {
            self.offset = max_off;
        } else {
            self.offset = self.offset.min(max_off);
            self.sticky = self.offset >= max_off;
        }
        let end = (self.offset + height).min(total);
        RenderWindow {
            lines: self.flat.rows[self.offset..end].to_vec(),
            offset: self.offset,
            total,
        }
    }

    /// Rebuild [`Self::flat`] when stale or asked at a new width: every
    /// block's lines, wrapped to the readable width, with each block's
    /// leading blank collapsed against an already-blank tail so a step
    /// separator before leading-blank chrome reads as one gap.
    fn reflow(&mut self, width: u16) {
        if !self.flat.dirty && self.flat.width == width {
            return;
        }
        let content_w = width.min(READ_W);
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut row_block: Vec<usize> = Vec::new();
        for (i, block) in self.blocks.iter_mut().enumerate() {
            let lines = block.lines(content_w);
            let mut first = 0;
            if rows.last().is_some_and(is_blank) {
                while first < lines.len() && is_blank(&lines[first]) {
                    first += 1;
                }
            }
            for line in &lines[first..] {
                for vrow in wrap_line(line, content_w as usize) {
                    rows.push(vrow);
                    row_block.push(i);
                }
            }
        }
        self.flat = Flat {
            width,
            rows,
            row_block,
            dirty: false,
        };
    }

    /// Whole buffer flattened to plain-text rows at `width`, rail glyphs
    /// retained — the inspection hook the rendering tests assert on.
    #[cfg(test)]
    pub(in crate::tui) fn flatten_text(&mut self, width: u16) -> Vec<String> {
        self.reflow(width);
        self.flat
            .rows
            .iter()
            .map(|l| l.spans.iter().map(|s| s.content.as_ref()).collect())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use ratatui::text::Span;

    fn fresh() -> Viewport {
        Viewport::new(PathBuf::from("/dev/null"))
    }

    /// A step-boundary blank followed by leading-blank chrome collapses
    /// to one separator, not two: each chrome builder prepends a
    /// `Line::default()`, and so does a step, so two would otherwise
    /// stack into a visible double gap.
    #[test]
    fn step_then_chrome_collapses_to_single_blank() {
        let mut vp = fresh();
        vp.push_chrome(vec![Line::default(), Line::from(Span::raw("header1"))]);
        vp.push_chrome(vec![Line::default()]);
        vp.push_chrome(vec![Line::default(), Line::from(Span::raw("header2"))]);
        assert_eq!(vp.flatten_text(READ_W), vec!["", "header1", "", "header2"]);
    }

    /// `\n\n` inside a fence is ignored, but a later boundary outside the
    /// (now closed) fence is taken — fence-state must persist across
    /// candidates, not reset at each `\n\n`.
    #[test]
    fn break_skips_in_fence_then_takes_after() {
        let s = "intro\n\n```\nx\n\ny\n```\n\nfinal";
        let idx = safe_paragraph_break(s).expect("post-fence boundary");
        assert_eq!(&s[..idx], "intro\n\n```\nx\n\ny\n```\n\n");
    }

    /// A tool call shows only its summary shut, and reveals the full
    /// script when toggled open — the click-to-expand contract.
    #[test]
    fn tool_call_expands_on_toggle() {
        let mut vp = fresh();
        vp.push_tool_call(
            "ral",
            "build the parser".into(),
            "cargo build\nral test".into(),
        );
        let shut = vp.flatten_text(READ_W);
        assert!(shut.iter().any(|t| t.contains("build the parser")));
        assert!(!shut.iter().any(|t| t.contains("cargo build")));
        assert!(vp.toggle_block(0), "a tool call is expandable");
        let open = vp.flatten_text(READ_W);
        assert!(open.iter().any(|t| t.contains("cargo build")));
        assert!(open.iter().any(|t| t.contains("ral test")));
    }

    /// `Ctrl+Y` yanks the rail-stripped text of what is on screen — the
    /// summary survives, the disclosure glyph does not.
    #[test]
    fn yank_strips_the_rail_glyph() {
        let mut vp = fresh();
        vp.push_tool_call("ral", "do a thing".into(), "script".into());
        let _ = vp.render_window(READ_W, 10);
        let text = vp.yank_text();
        assert!(text.contains("do a thing"));
        assert!(!text.contains('▸'));
    }

    /// The `user.log` carries a tool call's full script even while it is
    /// collapsed on screen — the on-disk transcript is the complete
    /// record, independent of what is revealed.
    #[test]
    fn log_keeps_the_script_while_collapsed() {
        let tmp = std::env::temp_dir().join(format!("exarch-vp-log-{}", std::process::id()));
        let mut vp = Viewport::new(tmp.clone());
        vp.push_tool_call("ral", "short summary".into(), "the full script line".into());
        vp.flush_log().expect("flush");
        let logged = std::fs::read_to_string(&tmp).unwrap_or_default();
        let _ = std::fs::remove_file(&tmp);
        assert!(logged.contains("short summary"));
        assert!(logged.contains("the full script line"));
    }
}
