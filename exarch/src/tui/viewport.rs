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

use super::block::{AgentSlot, Block, RailShape, wrap_line};
use super::fidelity::{self, Fidelity};
use super::group;
use super::line::{READ_W, is_blank, plain, prompt_fence, size_bar};
use super::rail::{self, RailKind};
use crate::bus::Hunk;
use crate::card::Card;
use crate::provider::Usage;
use ratatui::text::Line;
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
    /// Top visible visual row.  Owned per-viewport so each tab keeps its
    /// place; recomputed against the frame height in [`Self::render_window`].
    offset: usize,
    /// Follow the tail: while set, [`Self::render_window`] pins the trailing
    /// segment's head row in place (see [`TailAnchor`]).  Cleared when the
    /// user scrolls up, re-armed when they scroll back down.
    sticky: bool,
    /// The live tail-follow anchor while `sticky`, re-seeded whenever the
    /// trailing segment, width, or its disclosure level changes; `None` when
    /// not following the tail.
    tail_anchor: Option<TailAnchor>,
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
}

/// The tail-follow anchor: the trailing live segment's identity and the
/// greatest height it has reached.  While following the tail, the segment's
/// head row is pinned at the highest screen line it has occupied, so the
/// segment grows downward (and clips at the bottom edge once taller than the
/// window) while the committed transcript above it never recedes — a burst
/// of streaming tool calls coalesces into one trailing group whose height
/// oscillates as each result lands, and pinning its head keeps that churn
/// from shoving the whole transcript up and down.
///
/// `peak` is the height in visual rows, so it is keyed on `width` and the
/// segment's disclosure `level` alongside its anchor `block`: a reflow at a
/// new width or a dial of the live group re-measures from scratch rather
/// than holding a stale peak open as a gap.
struct TailAnchor {
    block: usize,
    width: u16,
    level: u8,
    peak: usize,
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
            offset: 0,
            sticky: true,
            tail_anchor: None,
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
        self.offset = 0;
        self.sticky = true;
        self.tail_anchor = None;
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

    /// Append a structural I/O effect card.  `write` marks a write redirect
    /// — a barrier that ends the current ral block, like a diff; reads,
    /// greps, and execs (`write == false`) are observations the projection
    /// folds under their call.
    pub(super) fn push_io_card(&mut self, card: Card, write: bool) {
        self.push_block(Block::io_card(card, write));
    }

    /// Append pre-rendered chrome (step header, error, banner, subagent
    /// breadcrumb, summary-less tool call).  `shape` lets the rail dispatch
    /// on the chrome sub-kind.
    pub(super) fn push_chrome(&mut self, shape: RailShape, lines: Vec<Line<'static>>) {
        self.push_block(Block::chrome(shape, lines));
    }

    /// Attach a tool result's magnitude — `text.lines().count()` — to the
    /// most-recent [`Block::is_tool_call`] block, searched backward from
    /// the tail since `Patch` / `Wrote` side effects may land between a
    /// call and its result.  Marks the flatten stale so the collapsed
    /// header re-renders with its size-bar.  A no-op when no tool call
    /// precedes the result (e.g. `fff`, whose call is summary-less chrome).
    pub(super) fn set_result_size(&mut self, text: &str) {
        let n = text.lines().count() as u32;
        if let Some(block) = self.blocks.iter_mut().rev().find(|b| b.is_tool_call()) {
            block.set_result_size(n);
            self.flat.dirty = true;
        }
    }

    /// Push streamed assistant text; commit any fence-safe paragraphs at
    /// the turn's `context_floor` (the degradation seed).
    pub(super) fn push_token(&mut self, text: &str, context_floor: u8) {
        self.open.push_str(text);
        self.flush_complete_paragraphs(context_floor);
    }

    /// End a streaming step: commit whatever remains in `open`.
    pub(super) fn close_boundary(&mut self, context_floor: u8) {
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

    /// Tee a block's full content to `user.log`, collapsing redundant
    /// blank separators against the previous line exactly as the screen
    /// flatten does.
    fn log_block(&mut self, block: &Block) {
        let lead = opens_rail_run(self.blocks.last(), block);
        for line in block.log_lines(self.agent, lead) {
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
        // The provisional streaming seat (when a response is in flight) is the
        // trailing row, one past the committed flatten: a fixed-height mark
        // that grows in place, so it joins the scroll arithmetic as one extra
        // row rather than a block churning the memoised flatten each token.
        let seat = self.streaming_seat();
        let committed = self.flat.rows.len();
        let total = committed + seat.is_some() as usize;
        let max_off = total.saturating_sub(height);
        if self.sticky {
            self.offset = self.tail_anchored_offset(width, height, total);
        } else {
            self.offset = self.offset.min(max_off);
            self.sticky = self.offset >= max_off;
            self.tail_anchor = None;
        }
        let end = (self.offset + height).min(total);
        // Scroll position as a percentage of the scrollable range: `0%` at the
        // top, `100%` once `offset` reaches `max_off` (the tail).  `None` when
        // the whole buffer fits, so the rule line shows no readout.  `offset`
        // is clamped to `max_off` because a head-anchored shrunken tail group
        // can run it past the bottom (a gap opens below).
        let scroll_pct = (max_off > 0)
            .then(|| (self.offset.min(max_off) * 100 / max_off).min(100) as u16);
        // Committed rows fill the window up to the seat's row (`committed`);
        // the seat itself lands only once the window reaches past them — i.e.
        // the tail is in view.
        let mut lines = self.flat.rows[self.offset.min(committed)..end.min(committed)].to_vec();
        if let Some(seat) = seat
            && end > committed
        {
            lines.push(seat);
        }
        RenderWindow {
            lines,
            offset: self.offset,
            scroll_pct,
        }
    }

    /// The tail-following offset that pins the trailing segment's head row at
    /// the highest screen line it has reached.  The head row is fixed while a
    /// group streams — committed content above it does not move, and fresh
    /// observations join the same coalesced run rather than push it down — so
    /// holding the peak height keeps a shrink (a call landing before its
    /// result) from receding the transcript; the segment instead opens a
    /// transient gap below, and once taller than the window pins its head at
    /// the top and clips its tail at the bottom edge.
    fn tail_anchored_offset(&mut self, width: u16, height: usize, total: usize) -> usize {
        let Some(&block) = self.flat.row_block.last() else {
            self.tail_anchor = None;
            return 0;
        };
        let head_row = self.tail_segment_start(block);
        let group_height = total - head_row;
        let level = self.blocks.get(block).map_or(1, Block::level);
        let peak = match &mut self.tail_anchor {
            Some(a) if a.block == block && a.width == width && a.level == level => {
                a.peak = a.peak.max(group_height);
                a.peak
            }
            _ => {
                self.tail_anchor = Some(TailAnchor {
                    block,
                    width,
                    level,
                    peak: group_height,
                });
                group_height
            }
        };
        // Pin the head so the segment, at its peak height, just fills to the
        // bottom; a shorter current height leaves that much space empty below.
        let space_below = height.saturating_sub(peak);
        head_row.saturating_sub(space_below)
    }

    /// The first visual-row index of the trailing segment — the run of rows
    /// at the buffer's end sharing `block` as their anchor.  Row anchors are
    /// non-decreasing, so the segment is the final equal-valued run.
    fn tail_segment_start(&self, block: usize) -> usize {
        self.flat
            .row_block
            .iter()
            .rposition(|&b| b != block)
            .map_or(0, |p| p + 1)
    }

    /// Rebuild [`Self::flat`] when stale or asked at a new width.
    ///
    /// The flatten is the **coalescing projection**: an observation run
    /// ([`Block::observation`] — a call and its reads/greps/execs, bridged
    /// across the interior step boundaries between consecutive calls,
    /// [`Self::observation_run_end`]) folds into one dialable ral block
    /// ([`super::group`]); every genuine barrier — a diff, a write, a
    /// surfaced card, markdown, a subagent result, or chrome — renders as its
    /// own block exactly as before, save a step boundary interior to a run,
    /// which is folded away.  The projection reads what arrival order already
    /// adjoins; nothing about how blocks are pushed, logged, or aggregated
    /// changes.
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
            } else {
                let prompt = self.blocks[i].is_prompt();
                let lead = opens_rail_run(i.checked_sub(1).map(|j| &self.blocks[j]), &self.blocks[i]);
                let segment = (i, self.blocks[i].lines(content_w, agent, lead).to_vec(), prompt);
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
        };
    }

    /// The end (exclusive) of the maximal observation run starting at
    /// `start` — the span of [`Block::observation`] blocks the projection
    /// coalesces into one ral block, **bridged across the step boundaries
    /// interior to it**.  Each `ral` call is its own provider round-trip, so
    /// a [`Block::is_step`] chrome (`Kind::Step`) lands between consecutive
    /// calls; left a barrier it would cut every burst back to a single call.
    /// A step boundary is provider bookkeeping, not content: when it falls
    /// *between* observations it is subsumed into the run (and never
    /// rendered); a step at the run's tail — before genuine content
    /// (markdown, a diff, a surfaced card, other chrome) or at the buffer's
    /// end — is left out, so it still renders as the boundary it is.
    fn observation_run_end(&self, start: usize) -> usize {
        // `end` advances only past an observation, so a trailing step (whose
        // following observation has not arrived) stays outside the run; an
        // interior step is folded in once a later observation commits `end`
        // past it.
        let mut end = start;
        let mut i = start;
        while i < self.blocks.len() {
            if self.blocks[i].observation() {
                i += 1;
                end = i;
            } else if self.blocks[i].is_step() {
                i += 1;
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
        let level = self.blocks[anchor].level().max(1);
        let calls = self.group_calls(start, end);
        let mut lines = group::body(&calls, level, width as usize);
        let open = level >= 2;
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
    /// call) are its effects.
    fn group_calls(&self, start: usize, end: usize) -> Vec<group::Call> {
        let mut calls: Vec<group::Call> = Vec::new();
        let mut effects: Vec<Line<'static>> = Vec::new();
        let mut pending: Option<group::CallParts<'_>> = None;
        for block in &self.blocks[start..end] {
            if let Some(parts) = block.call_view() {
                if let Some(prev) = pending.take() {
                    calls.push(group::Call::new(prev, std::mem::take(&mut effects)));
                }
                pending = Some(parts);
            } else {
                effects.extend(block.effect_lines());
            }
        }
        if let Some(prev) = pending {
            calls.push(group::Call::new(prev, effects));
        }
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Viewport {
        let path = std::env::temp_dir().join("exarch-streaming-seat-test.log");
        Viewport::new(path, AgentSlot(0))
    }

    /// The register is keyed state: a repeated key overwrites its slot in
    /// place (no new slot, order preserved), `drop_pin` removes just that
    /// slot, and `reset` wipes the whole register — the generation discipline
    /// that bounds it to a session exactly as it bounds the scrollback.
    #[test]
    fn pins_overwrite_in_place_and_keep_insertion_order() {
        use crate::card::Mark;
        let raw = |b: &[u8]| Card(vec![Mark::Raw { bytes: b.to_vec() }]);
        let keys = |vp: &Viewport| {
            vp.pins()
                .iter()
                .map(|(k, _)| k.clone())
                .collect::<Vec<_>>()
        };
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
        assert!(all.contains("let x = 1"), "committed prose now renders: {all:?}");
        assert!(
            !plain(w.lines.last().expect("committed rows")).contains('░'),
            "no provisional seat remains after commit: {all:?}"
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
}
