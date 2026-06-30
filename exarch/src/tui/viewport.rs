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

use super::block::{AgentSlot, Block, RailShape, Reveal, wrap_line};
use super::fidelity::{self, Fidelity};
use super::group;
use super::line::{
    READ_W, coalesced_queries, deliberation_grain, is_blank, plain, prompt_fence, size_bar,
};
use super::rail::{self, RailKind};
use super::select::plain_slice;
use crate::bus::Hunk;
use crate::card::{Card, ObservationKind};
use crate::provider::Usage;
use ratatui::text::Line;
use ratatui::text::Span;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

pub(super) struct Viewport {
    /// The session's scrollback, oldest block first.
    blocks: Vec<Block>,
    /// This session's agent palette slot, stamped onto every block at
    /// push. Root is `0`; subagents take the next slot at birth.
    agent: AgentSlot,
    /// This session's cumulative token spend, summed from every
    /// `Kind::Usage` event routed to it. Drives the matrix's per-agent
    /// value readout; the global `App::total_usage` stays for `rule_line`.
    usage: Usage,
    /// In-progress assistant text since the last fence-safe paragraph
    /// boundary.  It never streams as prose — [`Self::render_window`]
    /// projects only its growing magnitude as a provisional rail seat
    /// ([`Self::streaming_seat`]) until a fence-safe break or the turn's end
    /// commits it as a [`Block::markdown`].
    open: String,
    /// In-progress reasoning text.  Grows as `Kind::Thinking` events arrive;
    /// rendered as a live deliberation block above the answer stream. Cleared
    /// when the final reasoning commits as its own [`Block::thinking`].
    thinking: String,
    /// Top visible visual row.  Owned per-viewport so each tab keeps its
    /// place; recomputed against the frame height in [`Self::render_window`].
    offset: usize,
    /// Follow the tail: while set, [`Self::render_window`] pins the viewport
    /// to the absolute bottom (the trailing row).  Cleared when the user
    /// scrolls (either direction), re-armed when they scroll back down.
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
    /// The worker's current phase label and its start instant — the live
    /// phase driving the status line's elapsed-wait bar.  Set by
    /// [`Self::set_phase`], cleared by [`Self::clear_phase`] (or restarted by
    /// a superseding `set_phase`).  `None` when the viewport is between
    /// phases, leaving the bar hidden.
    phase: Option<(String, Instant)>,
    /// Kit-authored *state*: an ordered `key → Card` register, the
    /// model-authored dual of the matrix.  Re-pinning a key overwrites its
    /// card in place ([`Self::set_pin`]); `` `unpin `` drops it
    /// ([`Self::drop_pin`]).  Rendered as the reserved right-hand register
    /// column, never logged, and wiped by [`Self::reset`] so a pin is
    /// generation-bounded exactly as [`Self::blocks`] is.  A `Vec` of pairs,
    /// not a map, so render order is first-seen insertion order.
    pins: Vec<(String, Card)>,
}

/// The visible slice of a viewport plus the scroll readout the rule line needs.
pub(super) struct RenderWindow {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) offset: usize,
    /// How far the window has scrolled through the buffer, as a percent in
    /// `0..=100`, or `None` when the whole buffer fits and there is nothing to
    /// scroll.  Computed here beside the `offset` it derives from; the rule
    /// line renders it as a fixed-position magnitude (`⇣ 72%`, `⇣ bot`) in
    /// place of the deleted right-margin scrollbar.
    pub(super) scroll_pct: Option<u16>,
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
    virtual_think_at: usize,
    virtual_think_len: usize,
    virtual_think_widths: Vec<usize>,
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

/// Whether `block` opens its own rail-run rather than continuing a prior
/// prose paragraph's.  A run of consecutive [`Block::markdown_src`] blocks
/// is one response — committed piecewise only because streaming commits
/// each fence-safe paragraph as its own block
/// ([`Viewport::flush_complete_paragraphs`]) — so the rail marks it once,
/// on its head row; a continuation paragraph passes `lead = false`, keeping
/// the gutter but dropping its redundant `·`.  Read off arrival order, like
/// every other projection in the flatten: `prev` is the block immediately
/// preceding `block`, `None` at the buffer's head.
fn opens_rail_run(prev: Option<&Block>, block: &Block) -> bool {
    !(block.markdown_src().is_some() && prev.is_some_and(|p| p.markdown_src().is_some()))
}

/// Open `path` as the session's rendered-text log, truncating any prior
/// content.  Falls back to a discarding sink when the file can't be
/// opened, so a log-path failure never disables the viewport.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:viewport-log] opens the viewport's rendered-text log; render dump infra, not turn-time data I/O"
)]
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

fn extend_visible_lines(
    out: &mut Vec<Line<'static>>,
    window_start: usize,
    window_end: usize,
    segment_start: usize,
    segment: &[Line<'static>],
) {
    let segment_end = segment_start + segment.len();
    if window_start >= segment_end || window_end <= segment_start {
        return;
    }
    let start = window_start.saturating_sub(segment_start);
    let end = window_end.saturating_sub(segment_start).min(segment.len());
    out.extend_from_slice(&segment[start..end]);
}

/// Copy an already-flushed `user.log` at `src` to the user-chosen `dest`
/// for `/export`.  The caller resolves `dest`, refuses to overwrite it, and
/// flushes the log first; this is the doored copy, kept beside [`open_log`]
/// so all `user.log` I/O lives in one place.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:export] copies a flushed user.log to the user-chosen export path; output infra, not turn-time data I/O"
)]
pub(super) fn export_log(src: &Path, dest: &Path) -> io::Result<u64> {
    fs::copy(src, dest)
}

impl Viewport {
    /// Build a viewport that tees its rendered text to `log_path`.
    /// `agent` is this session's palette slot, stamped onto every block.
    pub(super) fn new(log_path: PathBuf, agent: AgentSlot) -> Self {
        Self {
            blocks: Vec::new(),
            agent,
            usage: Usage::default(),
            open: String::new(),
            thinking: String::new(),
            offset: 0,
            sticky: true,
            flat: Flat::default(),
            log: open_log(&log_path),
            log_path,
            log_prev_blank: true,
            phase: None,
            pins: Vec::new(),
        }
    }

    /// This session's agent palette slot.
    pub(super) fn agent(&self) -> AgentSlot {
        self.agent
    }

    /// Fold one turn's token usage into this session's cumulative spend.
    /// Called from the `Kind::Usage` handler alongside `App::total_usage`.
    pub(super) fn add_usage(&mut self, u: Usage) {
        self.usage += u;
    }

    /// This session's cumulative token spend — the matrix's value readout.
    pub(super) fn usage(&self) -> Usage {
        self.usage
    }

    /// Per-step "had a tool call" flags, oldest step first: scan the
    /// blocks, opening a step at each [`Block::is_step`] boundary, and
    /// mark the open step `true` once a tool-call block lands within it.
    /// One bool per step — the matrix renders `●` for `true`, `○` for
    /// `false`.  Empty when no step boundary has landed yet.
    pub(super) fn steps(&self) -> Vec<bool> {
        let mut steps: Vec<bool> = Vec::new();
        for block in &self.blocks {
            if block.is_step() {
                steps.push(false);
            } else if block.is_tool_call()
                && let Some(last) = steps.last_mut()
            {
                *last = true;
            }
        }
        steps
    }

    /// Total lines this session touched: the summed [`Block::lines_changed`]
    /// over its diff blocks.  Drives the matrix's size readout; `0` for a
    /// read-only agent, and prose volume never inflates it.
    pub(super) fn lines_touched(&self) -> u32 {
        self.blocks.iter().filter_map(Block::lines_changed).sum()
    }

    /// Whether the session's last block is an error — the matrix renders
    /// the row's leading cell as `╳` rather than the done/running glyph.
    pub(super) fn last_is_error(&self) -> bool {
        self.blocks.last().is_some_and(Block::is_error)
    }
    /// Begin a new phase, restarting the elapsed-wait clock from now.  A
    /// superseding phase simply replaces the live slot — phases never
    /// overlap, each restarts the bar.
    pub(super) fn set_phase(&mut self, label: String) {
        self.phase = Some((label, Instant::now()));
    }

    /// Clear the live phase, hiding the elapsed-wait bar.  A no-op when no
    /// phase is live, so it is safe to call on every non-`Phase` event.
    pub(super) fn clear_phase(&mut self) {
        self.phase = None;
    }

    /// The live phase label, if one is in progress — the `phase…` text
    /// readout alongside the elapsed-wait bar.
    pub(super) fn phase_label(&self) -> Option<&str> {
        self.phase.as_ref().map(|(label, _)| label.as_str())
    }

    /// Wall-time elapsed in the current phase, if one is live — the
    /// magnitude the elapsed-wait bar encodes. `None` between phases.
    pub(super) fn phase_elapsed(&self) -> Option<Duration> {
        self.phase.as_ref().map(|(_, start)| start.elapsed())
    }

    /// Wipe scrollback, scroll state, and streaming buffer, and truncate
    /// the `user.log` by reopening it.  Used by `/clear` on the root.
    pub(super) fn reset(&mut self) {
        self.blocks.clear();
        self.usage = Usage::default();
        self.open.clear();
        self.thinking.clear();
        self.offset = 0;
        self.sticky = true;
        self.flat = Flat::default();
        self.log = open_log(&self.log_path);
        self.log_prev_blank = true;
        self.phase = None;
        self.pins.clear();
    }

    /// Final flush of the `user.log` at session end; lines are already
    /// written as each block lands.  Caller owns the I/O error policy.
    pub(super) fn flush_log(&mut self) -> io::Result<&Path> {
        self.log.flush()?;
        Ok(&self.log_path)
    }

    // ── content ──────────────────────────────────────────────────────────

    /// Append a tool call as its own collapsible block.  `context` is the
    /// turn's degradation floor, stamped so the coalesced intent line
    /// drains its saturation under context pressure (Move 7).
    pub(super) fn push_tool_call(
        &mut self,
        tool: &'static str,
        summary: String,
        cmd: String,
        context: u8,
    ) {
        self.push_block(Block::tool_call(tool, summary, cmd, context));
    }

    /// Append an async subagent's landed result as its own collapsible
    /// block — collapsed to a one-line header, dialed open to the full
    /// result rendered as markdown.
    pub(super) fn push_subagent(
        &mut self,
        title: String,
        text: String,
        error: Option<String>,
        elapsed: Duration,
        fidelity: Fidelity,
    ) {
        self.push_block(Block::subagent(title, text, error, elapsed, fidelity));
    }

    /// Append a single-file diff block; it re-renders from its hunks at
    /// every width and disclosure level.
    pub(super) fn push_patch(&mut self, path: String, hunks: Vec<Hunk>) {
        self.push_block(Block::patch(path, hunks));
    }

    /// Append a surfaced render document as its own block — a `card` of
    /// Bertin marks (roled text, a measure, a fields matrix, raw ink, or a
    /// richer composite the single-`diff` aggregation path didn't claim).
    /// A surfaced card is the model's own communication, a barrier the
    /// coalescing projection never folds.
    pub(super) fn push_card(&mut self, card: Card) {
        self.push_block(Block::card(card));
    }

    // ── pinned state (the register) ────────────────────────────────────────
    // The in-place analogue of `push_card`: a pin writes a keyed register slot
    // instead of appending a block, and touches neither the flatten nor the
    // log — pinned state is ambient, not scrollback.

    /// Overwrite (or insert, keeping first-seen order) the register slot `key`.
    pub(super) fn set_pin(&mut self, key: String, card: Card) {
        match self.pins.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => *slot = card,
            None => self.pins.push((key, card)),
        }
    }

    /// Drop the register slot `key`, if present.
    pub(super) fn drop_pin(&mut self, key: &str) {
        self.pins.retain(|(k, _)| k != key);
    }

    /// The pinned register slots in stable insertion order — the register
    /// column's content for the focused session.
    pub(super) fn pins(&self) -> &[(String, Card)] {
        &self.pins
    }

    /// Append a foldable observation card — a read, grep, or exec the projection
    /// folds under its call, carrying the `count` it represents for the run's
    /// census.  `kind` and `count` come straight off the buffered effects the
    /// host grouped into this card.  Writes use [`Self::push_write_card`].
    pub(super) fn push_observation_card(&mut self, card: Card, kind: ObservationKind, count: u32) {
        self.push_block(Block::observation_card(card, kind, count));
    }

    /// Append a write card — a barrier that ends the current ral block, like a
    /// diff, never folded into a run.  The card carries the `write <path>
    /// <outcome>` heading and a preview of what was written.
    pub(super) fn push_write_card(&mut self, card: Card) {
        self.push_block(Block::write_card(card));
    }

    /// Append pre-rendered chrome (step header, error, banner, subagent
    /// breadcrumb).  `shape` lets the rail dispatch on the chrome sub-kind.
    pub(super) fn push_chrome(&mut self, shape: RailShape, lines: Vec<Line<'static>>) {
        self.push_block(Block::chrome(shape, lines));
    }

    /// Append a summary-less query call.  Adjacent same-tool queries coalesce
    /// on screen into one `tool : q1, q2, …` line ([`Self::render_query_run`]);
    /// `query` is `None` for an invisible parse-failure boundary.
    pub(super) fn push_query(&mut self, tool: &'static str, query: Option<String>) {
        self.push_block(Block::query(tool, query));
    }

    /// Attach a tool result's magnitude — `text.lines().count()` — to the
    /// call it belongs to: the most-recent call-bearing block, searched
    /// backward from the tail since `Patch` / `Wrote` side effects may land
    /// between a call and its result.  The search halts at the first
    /// [`Block::is_call`] — a dialable tool call *or* a summary-less query —
    /// so a query's result stops there and never reaches past it to clobber an
    /// earlier dialable call's size bar.  Only a dialable call carries a size
    /// bar, so landing on a query (which has none) is a no-op that still halts.
    /// Marks the flatten stale so the collapsed header re-renders.
    pub(super) fn set_result_size(&mut self, text: &str) {
        let n = text.lines().count() as u32;
        if let Some(block) = self.blocks.iter_mut().rev().find(|b| b.is_call())
            && block.is_tool_call()
        {
            block.set_result_size(n);
            self.flat.dirty = true;
        }
    }

    /// Commit the turn's reasoning as its own dialable block.  If answer
    /// paragraphs already committed, insert the thinking block before that
    /// trailing markdown run so `∴` and the answer's `·` stay separate and
    /// ordered as deliberation then conclusion.  `answer_chars` is the whole
    /// turn's answer mass, the deliberation grain's denominator.
    pub(super) fn commit_thinking(&mut self, text: String, answer_chars: u32) {
        let preserve_scrollback = !self.sticky
            && self.flat.virtual_think_len > 0
            && self.offset <= self.flat.virtual_think_at + self.flat.virtual_think_len;
        self.thinking.clear();
        self.insert_thinking(text, answer_chars);
        if preserve_scrollback {
            // The live header is about to become a real collapsed block. If
            // the viewport was looking at the scrollback it had pushed down,
            // do not immediately tail-follow and yank those rows back up; let
            // the next render clamp only if the buffer truly no longer has
            // enough rows to hold this offset.
            self.sticky = false;
        }
    }

    /// Append a live reasoning chunk from the model's thinking phase.
    /// Grows the provisional thinking buffer; `thinking_seat` renders it above
    /// the streaming answer seat until `commit_thinking` supersedes it with a
    /// real block.
    pub(super) fn push_thinking(&mut self, text: &str) {
        self.thinking.push_str(text);
        self.flat.dirty = true;
    }

    /// Push streamed assistant text; commit any fence-safe paragraphs at
    /// the turn's `context_floor` (the degradation seed).
    pub(super) fn push_token(&mut self, text: &str, context_floor: u8) {
        self.open.push_str(text);
        self.flush_complete_paragraphs(context_floor);
    }

    /// End a streaming step: commit whatever remains in `open`.
    pub(super) fn close_boundary(&mut self, context_floor: u8) {
        if !self.thinking.trim().is_empty() {
            let text = std::mem::take(&mut self.thinking);
            let answer_chars = self.current_answer_chars();
            self.insert_thinking(text, answer_chars);
        }
        self.flush_open(context_floor);
    }

    /// Commit the longest fence-safe prefix of `open` as one markdown
    /// block.  Committing elsewhere would split a code fence across two
    /// `render_md` calls, so when no safe break exists the buffer keeps
    /// growing until the fence closes or the turn ends.
    pub(super) fn flush_complete_paragraphs(&mut self, context_floor: u8) {
        let Some(idx) = safe_paragraph_break(&self.open) else {
            return;
        };
        let chunk: String = self.open.drain(..idx).collect();
        if chunk.trim().is_empty() {
            return;
        }
        let fidelity = self.commit_fidelity(&chunk, context_floor);
        self.push_block(Block::markdown(chunk, fidelity));
    }

    /// Commit whatever remains in `open` as a final markdown block.
    /// Called at turn end and `/clear`.
    pub(super) fn flush_open(&mut self, context_floor: u8) {
        let leftover = std::mem::take(&mut self.open);
        if leftover.trim().is_empty() {
            return;
        }
        let fidelity = self.commit_fidelity(&leftover, context_floor);
        self.push_block(Block::markdown(leftover, fidelity));
    }

    /// The fidelity to stamp on a committing markdown block: the turn-level
    /// `context_floor` (passed by `App`) plus a per-block echo delta — the
    /// trigram overlap of `text` with the most-recent `ral` script in this
    /// session, when the latest tool call was `ral`.  Context is the floor
    /// every paragraph inherits; echo is the per-paragraph modifier.
    fn commit_fidelity(&self, text: &str, context_floor: u8) -> Fidelity {
        let echo = self
            .blocks
            .iter()
            .rev()
            .find(|b| b.is_tool_call())
            .and_then(Block::ral_cmd)
            .map_or(0, |cmd| fidelity::echo_delta(text, cmd));
        Fidelity {
            context: context_floor,
            echo,
        }
    }

    /// Append `block`, tee its log projection, and mark the flatten
    /// stale so the next render rebuilds it.
    fn push_block(&mut self, block: Block) {
        self.log_block(&block);
        self.blocks.push(block);
        self.flat.dirty = true;
    }

    fn current_answer_chars(&self) -> u32 {
        let committed = self
            .blocks
            .iter()
            .rev()
            .map_while(Block::markdown_src)
            .fold(0u32, |n, text| {
                n.saturating_add(text.chars().count() as u32)
            });
        committed.saturating_add(self.open.chars().count() as u32)
    }

    fn insert_thinking(&mut self, text: String, answer_chars: u32) {
        if text.trim().is_empty() {
            return;
        }
        let block = Block::thinking(text, answer_chars);
        let start = self
            .blocks
            .iter()
            .rposition(|b| !b.is_markdown())
            .map_or(0, |i| i + 1);
        if start < self.blocks.len() && self.blocks[start].is_markdown() {
            self.blocks.insert(start, block);
            self.rewrite_log();
            self.flat.dirty = true;
        } else {
            self.push_block(block);
        }
    }

    /// Tee a block's full content to `user.log`, collapsing redundant
    /// blank separators against the previous line exactly as the screen
    /// flatten does.
    fn log_block(&mut self, block: &Block) {
        let lead = opens_rail_run(self.blocks.last(), block);
        let lines = block.log_lines(self.agent, lead);
        self.write_log_lines(lines);
    }

    fn rewrite_log(&mut self) {
        let entries = self
            .blocks
            .iter()
            .enumerate()
            .map(|(i, block)| {
                let lead = opens_rail_run(i.checked_sub(1).map(|j| &self.blocks[j]), block);
                block.log_lines(self.agent, lead)
            })
            .collect::<Vec<_>>();
        self.log = open_log(&self.log_path);
        self.log_prev_blank = true;
        for lines in entries {
            self.write_log_lines(lines);
        }
    }

    fn write_log_lines(&mut self, lines: Vec<Line<'static>>) {
        for line in lines {
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
        let think_at = self.flat.virtual_think_at;
        let think_len = self.flat.virtual_think_len;
        if think_len == 0 || row < think_at {
            return self.flat.row_block.get(row).copied();
        }
        if row < think_at + think_len {
            return None;
        }
        self.flat.row_block.get(row - think_len).copied()
    }

    /// Rendered cell width of visual row `row` — its content's extent, not
    /// the pane's — so a gesture can be bound tight to the text and ignore
    /// the dead margin past where the line ends.  `None` past the buffer.
    pub(super) fn row_width(&self, row: usize) -> Option<usize> {
        let think_at = self.flat.virtual_think_at;
        let think_len = self.flat.virtual_think_len;
        if think_len == 0 || row < think_at {
            return self.flat.rows.get(row).map(Line::width);
        }
        if row < think_at + think_len {
            return self.flat.virtual_think_widths.get(row - think_at).copied();
        }
        self.flat.rows.get(row - think_len).map(Line::width)
    }

    /// Whether the block at `idx` is dialable — a stable property of its
    /// kind, independent of its current level, so a wheel resting on its
    /// glyph can claim the gesture even when the level is already clamped.
    pub(super) fn block_dialable(&self, idx: usize) -> bool {
        self.blocks.get(idx).is_some_and(Block::dialable)
    }

    /// Dial the block at `idx` by `delta` if it is dialable, returning
    /// whether it changed — so the caller can tell a real dial from a
    /// gesture on inert chrome or a clamped level.
    pub(super) fn dial_block(&mut self, idx: usize, delta: i8) -> bool {
        self.mutate_block(idx, |b| b.dial(delta))
    }

    /// Cycle the block at `idx` between L1 and L3 — the click-on-rail
    /// affordance — returning whether it changed.
    pub(super) fn cycle_block(&mut self, idx: usize) -> bool {
        self.mutate_block(idx, Block::cycle)
    }

    /// Apply `f` to the dialable block at `idx`, marking the flatten stale
    /// when its memo actually dropped, and report whether it changed.
    fn mutate_block(&mut self, idx: usize, f: impl FnOnce(&mut Block)) -> bool {
        let Some(block) = self.blocks.get_mut(idx) else {
            return false;
        };
        if !block.dialable() {
            return false;
        }
        let before = block.level();
        f(block);
        let changed = block.level() != before;
        if changed {
            self.flat.dirty = true;
        }
        changed
    }

    pub(super) fn scroll_up(&mut self, n: usize) {
        self.sticky = false;
        self.offset = self.offset.saturating_sub(n);
    }
    pub(super) fn scroll_down(&mut self, n: usize) {
        self.sticky = false;
        self.offset = self.offset.saturating_add(n);
    }

    /// Plain text the drag-selection copies.  `lo` and `hi` are each
    /// `(row, col)` where `col` is a cell-column within the text area
    /// (0 = left edge); the rail glyph is stripped automatically.
    pub(super) fn selection_text(&self, lo: (usize, u16), hi: (usize, u16)) -> String {
        let (lo_row, lo_col) = lo;
        let (hi_row, hi_col) = hi;
        let last = self.flat.rows.len().saturating_sub(1);
        if self.flat.rows.is_empty() || lo_row > last || hi_row > last {
            return String::new();
        }
        if lo_row == hi_row {
            let (a, b) = if lo_col <= hi_col {
                (lo_col, hi_col)
            } else {
                (hi_col, lo_col)
            };
            return plain_slice(&self.flat.rows[lo_row], a, b);
        }
        let (first_row, first_col) = if lo_row < hi_row {
            (lo_row, lo_col)
        } else {
            (hi_row, hi_col)
        };
        let (last_row, last_col) = if lo_row < hi_row {
            (hi_row, hi_col)
        } else {
            (lo_row, lo_col)
        };
        let mut parts: Vec<String> = Vec::new();
        // First row: from first_col to end.
        parts.push(plain_slice(&self.flat.rows[first_row], first_col, u16::MAX));
        // Middle rows: full lines.
        for row in (first_row + 1)..last_row {
            parts.push(plain(&self.flat.rows[row]));
        }
        // Last row: from start to last_col.
        parts.push(plain_slice(&self.flat.rows[last_row], 0, last_col));
        parts.join("\n")
    }

    /// The assistant's latest reply as raw markdown: the trailing
    /// contiguous run of [`Block::markdown_src`] prose blocks, concatenated
    /// in order.  Each fence-safe paragraph commits as its own block, so
    /// the run reassembles the multi-paragraph answer verbatim; a tool
    /// call, card, or chrome block bounds it.  Edges trimmed; empty when
    /// the last block is not prose (the turn ended on a diff or a card).
    /// What `/copy` puts on the clipboard.
    pub(super) fn latest_reply_md(&self) -> String {
        let mut tail: Vec<&str> = self
            .blocks
            .iter()
            .rev()
            .map_while(Block::markdown_src)
            .collect();
        tail.reverse();
        tail.concat().trim().to_owned()
    }

    // ── rendering ────────────────────────────────────────────────────────

    /// The provisional seat for in-flight reasoning: `RailKind::Thinking` (∴)
    /// with a live size bar and deliberation grain, but no prose. The final
    /// committed thinking block is dialable, so the trace stays available
    /// without a tall live seat that snaps shut at the boundary.
    fn thinking_seat(&self) -> Vec<Line<'static>> {
        if self.thinking.trim().is_empty() {
            return vec![];
        }
        let think_chars = self.thinking.chars().count() as u32;
        let think_lines = self.thinking.lines().count() as u32;
        vec![
            Line::default(),
            Line::from(vec![
                rail::span(RailKind::Thinking, self.agent, Some(think_lines)),
                Span::raw(" "),
                deliberation_grain(think_chars, 0),
                Span::raw(" "),
                size_bar(think_lines),
            ]),
        ]
    }

    fn trailing_markdown_start(&self) -> Option<usize> {
        let start = self
            .blocks
            .iter()
            .rposition(|b| !b.is_markdown())
            .map_or(0, |i| i + 1);
        self.blocks
            .get(start)
            .is_some_and(Block::is_markdown)
            .then_some(start)
    }

    fn provisional_thinking_row(&self) -> usize {
        let Some(start) = self.trailing_markdown_start() else {
            return self.flat.rows.len();
        };
        self.flat
            .row_block
            .iter()
            .position(|&block| block >= start)
            .unwrap_or(self.flat.rows.len())
    }

    /// The provisional rail seat for the in-flight response: a single row
    /// projecting only the *magnitude* of the streamed-but-uncommitted
    /// [`Self::open`] buffer — the markdown rail shape (`·`), lightened by its
    /// line count, then a [`size_bar`] of the same — and never its text.
    /// `None` between turns, when `open` is empty.  Drawn as the trailing row
    /// by [`Self::render_window`] and superseded the instant
    /// [`Self::flush_open`] commits the real [`Block::markdown`] at the
    /// boundary: the growing edge reads as accruing volume while the settled
    /// transcript above it stays a finished image.
    fn streaming_seat(&self) -> Option<Line<'static>> {
        if self.open.trim().is_empty() {
            return None;
        }
        let magnitude = self.open.lines().count() as u32;
        Some(Line::from(vec![
            rail::span(RailKind::Markdown, self.agent, Some(magnitude)),
            size_bar(magnitude),
        ]))
    }

    /// The visible slice at `width` × `height`, after re-flattening if
    /// stale and resolving the scroll position: head-anchored to the trailing
    /// segment while sticky ([`Self::tail_anchored_offset`]), clamped
    /// otherwise — and re-armed to sticky once it reaches the bottom.
    pub(super) fn render_window(&mut self, width: u16, height: usize) -> RenderWindow {
        self.reflow(width);
        // The provisional thinking seat (when the model is reasoning) renders
        // before the trailing markdown answer run.  That lets answer
        // paragraphs keep committing live without visually jumping ahead of
        // the deliberation they follow.
        let think = self.thinking_seat();
        let seat = self.streaming_seat();
        let committed = self.flat.rows.len();
        let think_at = if think.is_empty() {
            committed
        } else {
            self.provisional_thinking_row()
        };
        self.flat.virtual_think_at = think_at;
        self.flat.virtual_think_len = think.len();
        self.flat.virtual_think_widths = think.iter().map(Line::width).collect();
        let total = committed + think.len() + seat.is_some() as usize;
        let max_off = total.saturating_sub(height);
        if self.sticky {
            self.offset = max_off;
        } else {
            self.offset = self.offset.min(max_off);
            self.sticky = self.offset >= max_off;
        }
        let end = (self.offset + height).min(total);
        // Scroll position as a percentage of the scrollable range: `0%` at the
        // top, `100%` once `offset` reaches `max_off` (the tail).  `None` when
        // the whole buffer fits, so the rule line shows no readout.  `offset`
        // is clamped to `max_off` so it stays within the valid scroll range.
        let scroll_pct =
            (max_off > 0).then(|| (self.offset.min(max_off) * 100 / max_off).min(100) as u16);
        let mut lines = Vec::new();
        extend_visible_lines(
            &mut lines,
            self.offset,
            end,
            0,
            &self.flat.rows[..think_at.min(committed)],
        );
        extend_visible_lines(&mut lines, self.offset, end, think_at, &think);
        extend_visible_lines(
            &mut lines,
            self.offset,
            end,
            think_at + think.len(),
            &self.flat.rows[think_at.min(committed)..],
        );
        if let Some(s) = seat
            && end > committed + think.len()
            && self.offset <= committed + think.len()
        {
            lines.push(s);
        }
        RenderWindow {
            lines,
            offset: self.offset,
            scroll_pct,
        }
    }

    /// Rebuild [`Self::flat`] when stale or asked at a new width.
    ///
    /// The flatten is the **coalescing projection**: an observation run
    /// ([`Block::observation`] — a call and its reads/greps/execs, bridged
    /// across the interior step boundaries between consecutive calls,
    /// [`Self::observation_run_end`]) folds into one dialable ral block
    /// ([`super::group`]); a run of adjacent same-tool summary-less queries
    /// ([`Block::is_query`], bridged the same way) folds into one flat
    /// `tool : …` line ([`Self::render_query_run`]); every genuine barrier — a
    /// diff, a write, a surfaced card, markdown, a subagent result, or chrome —
    /// renders as its own block exactly as before, save a step boundary interior
    /// to a run, which is folded away.  The projection reads what arrival order
    /// already adjoins; nothing about how blocks are pushed, logged, or
    /// aggregated changes.
    /// Each visual row maps to its source block index — a group's rows to
    /// its anchor call — so the dial, click, and copy paths address whole
    /// projected blocks.  Each segment's leading blank collapses against an
    /// already-blank tail so a step separator before leading-blank chrome
    /// reads as one gap.
    fn reflow(&mut self, width: u16) {
        if !self.flat.dirty && self.flat.width == width {
            return;
        }
        let content_w = width.min(READ_W);
        let agent = self.agent;
        let mut rows: Vec<Line<'static>> = Vec::new();
        let mut row_block: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < self.blocks.len() {
            // `prompt` is true only for the human turn — the flatten paints its
            // full-width rule fence here, where the content width is known, so
            // the rule spans the reading column as the turn's opening seam.
            let (anchor, lines, prompt) = if self.blocks[i].observation() {
                let end = self.observation_run_end(i);
                let anchor = self.group_anchor(i, end);
                let segment = (anchor, self.render_group(i, end, anchor, content_w), false);
                i = end;
                segment
            } else if self.blocks[i].is_query() {
                // A run of adjacent same-tool query calls coalesces into one
                // `tool : …` line.  It anchors on its first block — inert, since
                // a query never dials — and bridges interior step boundaries.
                let tool = self.blocks[i].query_tool();
                let end = self.run_end(i, |b| b.query_tool() == tool);
                let segment = (i, self.render_query_run(i, end, content_w), false);
                i = end;
                segment
            } else {
                let prompt = self.blocks[i].is_prompt();
                let lead =
                    opens_rail_run(i.checked_sub(1).map(|j| &self.blocks[j]), &self.blocks[i]);
                let segment = (
                    i,
                    self.blocks[i].lines(content_w, agent, lead).to_vec(),
                    prompt,
                );
                i += 1;
                segment
            };
            let mut first = 0;
            if rows.last().is_some_and(is_blank) {
                while first < lines.len() && is_blank(&lines[first]) {
                    first += 1;
                }
            }
            // A prompt opens with its fence: a full-width rule just above its
            // first text row (the `❖` rides the rail on that row).
            let mut fenced = false;
            for line in &lines[first..] {
                for vrow in wrap_line(line, content_w as usize) {
                    if prompt && !fenced && !is_blank(&vrow) {
                        rows.push(prompt_fence(content_w));
                        row_block.push(anchor);
                        fenced = true;
                    }
                    rows.push(vrow);
                    row_block.push(anchor);
                }
            }
        }
        self.flat = Flat {
            width,
            rows,
            row_block,
            dirty: false,
            virtual_think_at: 0,
            virtual_think_len: 0,
            virtual_think_widths: Vec::new(),
        };
    }

    /// The end (exclusive) of the maximal observation run starting at
    /// `start` — the span of [`Block::observation`] blocks the projection
    /// coalesces into one ral block.  [`Self::run_end`] over the observation
    /// predicate.
    fn observation_run_end(&self, start: usize) -> usize {
        self.run_end(start, Block::observation)
    }

    /// The end (exclusive) of the maximal run of `in_run` blocks starting at
    /// `start`, **bridged across the step boundaries interior to it** — the one
    /// genuinely shared piece between the ral observation group and the `fff`
    /// query coalesce.  Each call is its own provider round-trip, so a
    /// [`Block::is_step`] chrome (`Kind::Step`) lands between consecutive calls;
    /// left a barrier it would cut every burst back to a single call.  A step
    /// boundary is provider bookkeeping, not content: when it falls *between*
    /// run members it is subsumed (and never rendered); a step at the run's tail
    /// is also subsumed — the step carries no content and the run's own rail
    /// already marks its edge.
    fn run_end(&self, start: usize, in_run: impl Fn(&Block) -> bool) -> usize {
        // `end` advances past every run member and any step, so a trailing
        // step is folded into the run rather than rendered as its own block.
        let mut end = start;
        let mut i = start;
        while i < self.blocks.len() {
            // A run member or a step both fold into the run; anything else ends it.
            if in_run(&self.blocks[i]) || self.blocks[i].is_step() {
                i += 1;
                end = i;
            } else {
                break;
            }
        }
        end
    }

    /// Render the observation run `start..end` as one coalesced ral block:
    /// build a [`group::Call`] per tool call (its effects being the
    /// observation cards that follow it in the run), render the body at the
    /// run's disclosure level, and prepend the data-encoding rail — the
    /// disclosure triangle, the agent hue, the run's aggregate magnitude —
    /// to the first content row.  The level lives on the run's `anchor` call
    /// ([`Self::group_anchor`]); a run is opened by a call, so it has one.
    fn render_group(
        &self,
        start: usize,
        end: usize,
        anchor: usize,
        width: u16,
    ) -> Vec<Line<'static>> {
        let level = self.blocks[anchor].level();
        let calls = self.group_calls(start, end);
        let mut lines = group::body(&calls, level, width as usize);
        let open = level >= Reveal::Context;
        let magnitude = group::aggregate_magnitude(&calls);
        let rail = rail::span(RailKind::ToolCall(open), self.agent, magnitude);
        let idx = lines.iter().position(|l| !is_blank(l)).unwrap_or(0);
        if let Some(line) = lines.get_mut(idx) {
            line.spans.insert(0, rail);
        }
        lines
    }

    /// The anchor block of an observation run — its first tool call, whose
    /// [`Block::level`] is the run's disclosure level.  Falls back to the
    /// run's first block when (defensively) no call leads it.
    fn group_anchor(&self, start: usize, end: usize) -> usize {
        (start..end)
            .find(|&i| self.blocks[i].is_tool_call())
            .unwrap_or(start)
    }

    /// Build the run's calls in arrival order: each tool call opens a
    /// [`group::Call`]; the observation cards that follow it (until the next
    /// call) are its effects — both their rendered rows and their census
    /// [`group::Tally`], folded by `|>` kind.
    fn group_calls(&self, start: usize, end: usize) -> Vec<group::Call> {
        let mut calls: Vec<group::Call> = Vec::new();
        let mut effects: Vec<Line<'static>> = Vec::new();
        let mut tally = group::Tally::default();
        let mut pending: Option<group::CallParts<'_>> = None;
        for block in &self.blocks[start..end] {
            if let Some(parts) = block.call_view() {
                if let Some(prev) = pending.take() {
                    calls.push(group::Call::new(
                        prev,
                        std::mem::take(&mut tally),
                        std::mem::take(&mut effects),
                    ));
                }
                pending = Some(parts);
            } else {
                effects.extend(block.effect_lines());
                if let Some((kind, count)) = block.io_tally() {
                    tally.add(kind, count);
                }
            }
        }
        if let Some(prev) = pending {
            calls.push(group::Call::new(prev, tally, effects));
        }
        calls
    }

    /// Render a coalesced query run `start..end` as one `tool : q1, q2, …` line
    /// ([`coalesced_queries`]), seating the shut `▸` rail on its head row
    /// exactly as [`Self::render_group`] does.  Placeholder queries
    /// ([`Block::query_text`] `None`) drop out; an all-placeholder run renders
    /// nothing — the invisible boundary.  The run's blocks share a tool by
    /// construction ([`Self::run_end`]'s predicate), so any member names the head.
    fn render_query_run(&self, start: usize, end: usize, width: u16) -> Vec<Line<'static>> {
        let queries: Vec<&str> = self.blocks[start..end]
            .iter()
            .filter_map(Block::query_text)
            .collect();
        if queries.is_empty() {
            return Vec::new();
        }
        let tool = self.blocks[start..end]
            .iter()
            .find_map(Block::query_tool)
            .unwrap_or_default();
        let mut lines = coalesced_queries(tool, &queries, width);
        let rail = rail::span(RailKind::ToolCall(false), self.agent, None);
        let idx = lines.iter().position(|l| !is_blank(l)).unwrap_or(0);
        if let Some(line) = lines.get_mut(idx) {
            line.spans.insert(0, rail);
        }
        lines
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Viewport {
        let path = std::env::temp_dir().join("exarch-streaming-seat-test.log");
        Viewport::new(path, AgentSlot(0))
    }

    fn rail_rows(lines: &[Line<'static>], glyph: &str) -> Vec<usize> {
        lines
            .iter()
            .enumerate()
            .filter_map(|(i, line)| {
                (line.spans.first().map(|s| s.content.as_ref()) == Some(glyph)).then_some(i)
            })
            .collect()
    }

    /// The register is keyed state: a repeated key overwrites its slot in
    /// place (no new slot, order preserved), `drop_pin` removes just that
    /// slot, and `reset` wipes the whole register — the generation discipline
    /// that bounds it to a session exactly as it bounds the scrollback.
    #[test]
    fn pins_overwrite_in_place_and_keep_insertion_order() {
        use crate::card::Mark;
        let raw = |b: &[u8]| Card(vec![Mark::Raw { bytes: b.to_vec() }]);
        let keys = |vp: &Viewport| vp.pins().iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        let mut vp = viewport();
        vp.set_pin("tasks".into(), raw(b"v1"));
        vp.set_pin("build".into(), raw(b"ok"));
        assert_eq!(keys(&vp), ["tasks", "build"]);

        // Re-pinning a key overwrites in place: no new slot, order unchanged,
        // the card replaced.
        vp.set_pin("tasks".into(), raw(b"v2"));
        assert_eq!(keys(&vp), ["tasks", "build"]);
        assert!(matches!(&vp.pins()[0].1.0[..], [Mark::Raw { bytes }] if bytes == b"v2"));

        // Drop removes just the named slot; reset wipes the register.
        vp.drop_pin("tasks");
        assert_eq!(keys(&vp), ["build"]);
        vp.reset();
        assert!(vp.pins().is_empty(), "reset wipes the register");
    }

    /// While a response streams, the uncommitted `open` buffer renders as a
    /// single trailing seat row — the markdown rail glyph and a size-bar of
    /// its line count — and never its text.  The prose appears only when the
    /// boundary commits it as a block, at which point the seat gives way.
    #[test]
    fn streaming_renders_a_magnitude_seat_not_text() {
        let mut vp = viewport();
        // A fenced script with no fence-safe paragraph break: nothing commits,
        // so the whole chunk stays in `open`.
        vp.push_token("```ral\nlet x = 1\nlet y = 2\n", 0);
        assert!(vp.open.contains("let x = 1"));

        let w = vp.render_window(READ_W, 24);
        let seat_line = w.lines.last().expect("a seat row while streaming");
        // The leading span is the markdown rail glyph (`plain` would strip it).
        assert_eq!(
            seat_line.spans.first().map(|s| s.content.as_ref()),
            Some("· "),
            "seat wears the markdown rail glyph",
        );
        let seat = plain(seat_line);
        assert!(seat.contains('█'), "seat shows a filled size-bar: {seat:?}");
        assert!(
            !seat.contains("let x = 1"),
            "seat withholds the streamed text: {seat:?}"
        );

        // The boundary commits the buffer: the seat gives way to the rendered
        // prose block, and `open` is drained.
        vp.close_boundary(0);
        assert!(vp.open.is_empty());
        let w = vp.render_window(READ_W, 24);
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(
            all.contains("let x = 1"),
            "committed prose now renders: {all:?}"
        );
        assert!(
            !plain(w.lines.last().expect("committed rows")).contains('░'),
            "no provisional seat remains after commit: {all:?}"
        );
    }

    #[test]
    fn live_thinking_header_precedes_streamed_markdown_without_showing_trace() {
        let mut vp = viewport();
        vp.push_thinking("considering the shape\n");
        vp.push_token("First paragraph.\n\nSecond paragraph still streaming", 0);

        let w = vp.render_window(READ_W, 24);
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(
            all.contains("First paragraph."),
            "fence-safe markdown still commits while thinking is live: {all:?}"
        );
        assert!(
            !all.contains("considering the shape"),
            "live thinking hides the trace prose until it commits: {all:?}"
        );

        let thinking = rail_rows(&w.lines, "∴ ");
        let markdown = rail_rows(&w.lines, "· ");
        assert!(!thinking.is_empty(), "live thinking has its own rail");
        assert!(
            !markdown.is_empty(),
            "committed markdown keeps its answer rail"
        );
        assert!(
            thinking[0] < markdown[0],
            "thinking renders before the answer rail: {all:?}"
        );
    }

    #[test]
    fn live_thinking_header_keeps_height_when_committed() {
        let mut vp = viewport();
        for i in 0..8 {
            vp.push_chrome(
                RailShape::Plain,
                vec![
                    Line::from(format!("block {i} line a")),
                    Line::from(format!("block {i} line b")),
                ],
            );
        }
        vp.push_thinking("hidden trace\nline two\n");

        let height = 8;
        let live = vp.render_window(READ_W, height);
        let live_offset = live.offset;
        let live_text = live.lines.iter().map(plain).collect::<Vec<_>>();

        vp.commit_thinking("hidden trace\nline two\n".into(), 0);
        let committed = vp.render_window(READ_W, height);
        let committed_text = committed.lines.iter().map(plain).collect::<Vec<_>>();

        assert_eq!(
            committed.offset, live_offset,
            "committing the hidden trace keeps the scrollback offset stable"
        );
        assert_eq!(
            committed_text.len(),
            live_text.len(),
            "the live header and collapsed block occupy the same row budget"
        );
    }

    #[test]
    fn final_thinking_is_separate_from_the_answer_run() {
        let mut vp = viewport();
        vp.push_thinking("draft trace\n");
        vp.push_token("First paragraph.\n\nSecond paragraph.", 0);
        vp.commit_thinking("final trace\nline two".into(), vp.current_answer_chars());
        vp.close_boundary(0);

        assert_eq!(
            vp.latest_reply_md(),
            "First paragraph.\n\nSecond paragraph."
        );

        let w = vp.render_window(READ_W, 24);
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        let thinking = rail_rows(&w.lines, "∴ ");
        let markdown = rail_rows(&w.lines, "· ");
        assert!(!thinking.is_empty(), "final thinking is a real rail block");
        assert!(
            !markdown.is_empty(),
            "answer remains a separate markdown block"
        );
        assert!(
            thinking[0] < markdown[0],
            "thinking block stays before the answer block: {all:?}"
        );

        let idx = vp
            .block_at(w.offset + thinking[0])
            .expect("thinking rail row maps to its block");
        assert!(vp.cycle_block(idx), "thinking block is dialable");
        let w = vp.render_window(READ_W, 24);
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(
            all.contains("final trace"),
            "dialing the committed thinking block reveals the trace: {all:?}"
        );
    }

    /// A committed prompt opens with a full-width rule fence (its boundary
    /// mark) and carries no background band — background belongs to code now.
    #[test]
    fn prompt_opens_with_a_rule_fence_no_band() {
        let mut vp = viewport();
        vp.push_chrome(
            RailShape::Prompt,
            vec![Line::default(), Line::from("hello cutie")],
        );
        let w = vp.render_window(READ_W, 24);
        let fence = w.lines.iter().any(|l| {
            let t: String = l.spans.iter().map(|s| s.content.as_ref()).collect();
            !t.is_empty() && t.chars().all(|c| c == '─')
        });
        assert!(fence, "prompt opens with a full-width rule");
        for l in &w.lines {
            for s in &l.spans {
                assert!(s.style.bg.is_none(), "no background band on a prompt");
            }
        }
    }

    /// Between turns the buffer is empty, so there is no seat: the window is
    /// exactly the committed flatten.
    #[test]
    fn no_seat_between_turns() {
        let mut vp = viewport();
        vp.push_token("```ral\nlet x = 1\n", 0);
        vp.close_boundary(0);
        let w = vp.render_window(READ_W, 24);
        assert!(
            !plain(w.lines.last().expect("committed rows")).contains('░'),
            "an idle viewport shows no streaming seat"
        );
    }

    /// Scrolling down while sticky must not over-scroll past `max_off`.
    /// Before the fix, `scroll_down` left `sticky` set, so
    /// [`Self::tail_anchored_offset`]—whose `.max(self.offset)` floor is meant
    /// to keep the view from receding as content grows—instead let the offset
    /// creep up to the tail segment head row, blanking the lower rows.
    /// Clearing `sticky` on every user scroll routes through the non-sticky
    /// clamp in [`Self::render_window`], which bounds `offset` to `max_off`.
    #[test]
    fn scroll_down_while_sticky_clamps_to_max_off() {
        let mut vp = viewport();
        // Enough chrome blocks to overflow a 10-row window.
        for i in 0..10 {
            vp.push_chrome(
                RailShape::Plain,
                vec![
                    Line::from(format!("block {i} line a")),
                    Line::from(format!("block {i} line b")),
                    Line::from(format!("block {i} line c")),
                ],
            );
        }
        let height = 10;
        // First render establishes sticky at the bottom.
        let w0 = vp.render_window(READ_W, height);
        assert!(vp.sticky, "a fresh viewport follows the tail");
        let max_off = w0.offset;
        // Scrolling down while sticky should be a no-op (already at the
        // bottom), not an over-scroll blanking rows below.
        vp.scroll_down(5);
        let w1 = vp.render_window(READ_W, height);
        assert_eq!(
            w1.offset, max_off,
            "scroll_down while sticky stays at max_off, not past it"
        );
        assert_eq!(
            w1.lines.len(),
            height,
            "the window fills every row — no blank space below the tail"
        );
        assert!(vp.sticky, "re-armed at the bottom after the clamp");
    }
}
