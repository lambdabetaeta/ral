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

use super::block::{AgentSlot, Block, RailShape, Reveal, append_visual_rows};
use super::fidelity::{self, Fidelity};
use super::group;
use super::line::{grain_run, is_blank, plain, size_bar};
use super::palette::READ_W;
use super::rail::{self, RailKind};
use super::select::plain_slice;
use crate::bus::AgentId;
use crate::bus::card::{Card, Hunk, ObservationKind};
use crate::provider::Usage;
use ratatui::text::Line;
use ratatui::text::Span;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Max scrollback blocks retained in heap per viewport; older blocks are
/// already durable in `user.log`/`events.json` and are evicted oldest-first
/// once the window is exceeded
/// (`decisions/260705_leases-and-budgets`, "Viewport: cap by blocks and
/// rendered rows; evict old blocks before retaining old dead-agent views").
pub(super) const VIEWPORT_MAX_BLOCKS: usize = 500;
/// Max rendered rows retained — an eviction trigger alongside
/// [`VIEWPORT_MAX_BLOCKS`], for the rarer oversized block (a huge diff, a
/// long tool result) that would blow the row budget well before the block
/// count does.
pub(super) const VIEWPORT_MAX_ROWS: usize = 20_000;

/// The three facts kept for a dead sub-agent view once its linger window
/// elapses ([`super::LINGER`]): everything else — blocks, the flatten, the
/// streaming buffers, the pinned register — is dropped. No reload-from-log
/// machinery is built; the log stays readable outside the TUI
/// (`decisions/260705_leases-and-budgets`, "Viewport eviction: a tombstone
/// with the log path is enough").
pub(super) struct Tombstone {
    pub(super) id: AgentId,
    pub(super) error: bool,
    pub(super) log_path: PathBuf,
}

impl Tombstone {
    /// The tombstone's one-line rendering: dim chrome naming the agent, its
    /// final status, and where its full transcript still lives.
    pub(super) fn line(&self) -> Line<'static> {
        let status = if self.error { "error" } else { "done" };
        Line::from(format!(
            "· agent {} {status} — {}",
            self.id,
            self.log_path.display()
        ))
    }
}

pub(super) struct Viewport {
    /// The session's scrollback, oldest block first — each block alongside
    /// its rendered `user.log` row count ([`Entry`]).
    blocks: Vec<Entry>,
    /// Set once this view has been evicted into a tombstone
    /// ([`Self::evict_to_tombstone`]); `blocks` is empty from that point on.
    /// `None` for a live (or still-lingering) view.
    tombstone: Option<Tombstone>,
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

/// One scrollback entry: the block plus its rendered `user.log` row count
/// ([`Viewport::log_block`], [`Viewport::rewrite_log`]), captured where it
/// is already computed. Summed over [`Viewport::blocks`], it is the
/// [`VIEWPORT_MAX_ROWS`] eviction trigger.
struct Entry {
    block: Block,
    rows: usize,
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

/// Where visual row `row` resolves against [`Flat`]'s virtual thinking
/// seat: either a row backed by the committed flatten (indexed after
/// subtracting the seat, if `row` falls past it), or a row inside the seat
/// itself, which has no backing text row of its own.
enum RowSite {
    Committed(usize),
    Seat(usize),
}

/// Walk `open` for the latest paragraph break reached at fence depth
/// zero.  Returns the byte index *after* the `\n\n` so `open.drain(..idx)`
/// peels off the committable prefix; `None` means commit waits — no `\n\n`
/// yet, or every candidate sits inside an open code fence.
///
/// Fence depth toggles on lines whose first non-whitespace token is
/// three-or-more backticks or tildes; nested fences are not a thing in
/// `CommonMark`, so a single bit suffices.
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
            tombstone: None,
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
        for entry in &self.blocks {
            if entry.block.is_step() {
                steps.push(false);
            } else if entry.block.is_tool_call()
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
        self.blocks
            .iter()
            .filter_map(|e| e.block.lines_changed())
            .sum()
    }

    /// Whether the session's last block is an error — the matrix renders
    /// the row's leading cell as `╳` rather than the done/running glyph.
    pub(super) fn last_is_error(&self) -> bool {
        self.blocks.last().is_some_and(|e| e.block.is_error())
    }

    /// The viewport's probe figures for the `/resources` fold:
    /// `(blocks, rows, bytes)` — scrollback blocks, the memoised flatten's
    /// visual rows, and those rows' summed text bytes.  Reads only what the
    /// renderer already keeps (the flatten is as of the last paint), so the
    /// probe is a read of display state, never a re-render.
    pub(super) fn probe_figures(&self) -> (u64, u64, u64) {
        let bytes: usize = self
            .flat
            .rows
            .iter()
            .map(|line| line.spans.iter().map(|s| s.content.len()).sum::<usize>())
            .sum();
        (
            self.blocks.len() as u64,
            self.flat.rows.len() as u64,
            bytes as u64,
        )
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
        *self = Self::new(self.log_path.clone(), self.agent);
    }

    /// Evict this view's heap state into a [`Tombstone`] carrying exactly the
    /// agent id, its final status (read off the last block before it is
    /// dropped, the same signal [`Self::last_is_error`] exposes), and the log
    /// path — the scrollback, flatten, streaming buffers, and pinned register
    /// are already durable in `user.log` and are dropped, never re-read
    /// (`decisions/260705_leases-and-budgets`). A no-op once already a
    /// tombstone.
    pub(super) fn evict_to_tombstone(&mut self, id: AgentId) {
        if self.tombstone.is_some() {
            return;
        }
        self.tombstone = Some(Tombstone {
            id,
            error: self.last_is_error(),
            log_path: self.log_path.clone(),
        });
        self.blocks = Vec::new();
        self.open = String::new();
        self.thinking = String::new();
        self.flat = Flat::default();
        self.pins = Vec::new();
        self.log = io::BufWriter::new(Box::new(io::sink()));
    }

    /// The tombstone, once evicted — `None` for a live (or still-lingering)
    /// view.  `.tombstone().is_some()` is the "was this view evicted?" query.
    pub(super) fn tombstone(&self) -> Option<&Tombstone> {
        self.tombstone.as_ref()
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
        name: String,
        text: String,
        error: Option<String>,
        elapsed: Duration,
        fidelity: Fidelity,
    ) {
        self.push_block(Block::subagent(name, text, error, elapsed, fidelity));
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

    /// Append a summary-less tool call, shown standalone as a `▸` rail block.
    /// `detail` is `None` for an invisible parse-failure boundary.
    pub(super) fn push_plain_call(&mut self, tool: &'static str, detail: Option<String>) {
        self.push_block(Block::plain_call(tool, detail));
    }

    /// Attach a tool result's magnitude — `text.lines().count()` — to the
    /// call it belongs to: the most-recent call-bearing block, searched
    /// backward from the tail since `Patch` / `Wrote` side effects may land
    /// between a call and its result.  The search halts at the first
    /// [`Block::is_call`] — a dialable tool call *or* a plain tool call —
    /// so a plain call's result stops there and never reaches past it to clobber an
    /// earlier dialable call's size bar.  Only a dialable call carries a size
    /// bar, so landing on a plain call (which has none) is a no-op that still halts.
    /// Marks the flatten stale so the collapsed header re-renders.
    pub(super) fn set_result_size(&mut self, text: &str) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "content count; u32 headroom far exceeds any in-memory transcript"
        )]
        let n = text.lines().count() as u32;
        if let Some(entry) = self.blocks.iter_mut().rev().find(|e| e.block.is_call())
            && entry.block.is_tool_call()
        {
            entry.block.set_result_size(n);
            self.flat.dirty = true;
        }
    }

    /// Commit a reasoning phase.  If the turn has a coalescing `∴` block
    /// ([`Self::thinking_target`] — only prose or a prompt breaks the run,
    /// tool calls do not), append to it: its provisional deltas are
    /// superseded by this authoritative text and its header ticks in place.
    /// Otherwise insert a new thinking block before the trailing markdown
    /// run.
    /// `answer_chars` is the current turn's answer mass — the deliberation
    /// grain's say-side, so the committed `∴` block's think/say ratio
    /// reflects how dearly the answer was bought.
    pub(super) fn commit_thinking(&mut self, text: String, answer_chars: u32) {
        let preserve_scrollback = self.looking_at_pushed_thinking();
        self.thinking.clear();
        self.upsert_thinking(text, answer_chars);
        if preserve_scrollback {
            self.sticky = false;
        }
    }

    /// Whether the view is parked on scrollback the live thinking seat had
    /// pushed down.  When it is, turning that seat into a real collapsed block
    /// must not re-arm tail-follow and yank those rows back up under the
    /// reader; the next render clamps only if the buffer truly can no longer
    /// hold the offset.
    fn looking_at_pushed_thinking(&self) -> bool {
        !self.sticky
            && self.flat.virtual_think_len > 0
            && self.offset <= self.flat.virtual_think_at + self.flat.virtual_think_len
    }

    /// Append a live reasoning chunk from the model's thinking phase.  When
    /// the turn already has a coalescing `∴` block ([`Self::thinking_target`])
    /// the delta streams into that block's provisional buffer — its magnitude
    /// ticks in place, nothing appears or moves.  Otherwise it grows the
    /// provisional thinking buffer; `thinking_seat` renders it above the
    /// streaming answer seat until `commit_thinking` supersedes it with a
    /// real block.
    pub(super) fn push_thinking(&mut self, text: &str) {
        if let Some(idx) = self.thinking_target() {
            self.blocks[idx].block.push_provisional_thinking(text);
        } else {
            self.thinking.push_str(text);
        }
        self.flat.dirty = true;
    }

    /// The block index the turn's reasoning coalesces into: the most recent
    /// `∴` block with no prose or prompt after it.  Tool calls and other
    /// chrome do not break the run — only an answer paragraph (the reader has
    /// been spoken to since) or a new human turn seeds a fresh block.
    fn thinking_target(&self) -> Option<usize> {
        let idx = self.blocks.iter().rposition(|e| e.block.is_thinking())?;
        let unbroken = !self.blocks[idx + 1..]
            .iter()
            .any(|e| e.block.is_markdown() || e.block.is_prompt());
        unbroken.then_some(idx)
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
            let preserve_scrollback = self.looking_at_pushed_thinking();
            let text = std::mem::take(&mut self.thinking);
            let answer_chars = self.current_answer_chars();
            self.upsert_thinking(text, answer_chars);
            if preserve_scrollback {
                self.sticky = false;
            }
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
            .find(|e| e.block.is_tool_call())
            .and_then(|e| e.block.ral_cmd())
            .map_or(0, |cmd| fidelity::echo_delta(text, cmd));
        Fidelity {
            context: context_floor,
            echo,
        }
    }

    /// Append `block`, tee its log projection, mark the flatten stale so the
    /// next render rebuilds it, and enforce the window caps.
    fn push_block(&mut self, block: Block) {
        let rows = self.log_block(&block);
        self.blocks.push(Entry { block, rows });
        self.flat.dirty = true;
        self.enforce_window_caps();
    }

    /// Evict oldest-first once either window cap is crossed — the dropped
    /// blocks are already durable in `user.log`/`events.json` and are never
    /// re-read from heap (`decisions/260705_leases-and-budgets`).  Walked
    /// once from the tail to find the longest suffix satisfying both caps,
    /// then the rest is dropped in a single `drain`.
    fn enforce_window_caps(&mut self) {
        let mut kept = 0usize;
        let mut rows = 0usize;
        for entry in self.blocks.iter().rev() {
            if kept == VIEWPORT_MAX_BLOCKS || rows + entry.rows > VIEWPORT_MAX_ROWS {
                break;
            }
            kept += 1;
            rows += entry.rows;
        }
        let drop = self.blocks.len() - kept;
        if drop > 0 {
            self.blocks.drain(..drop);
            self.flat.dirty = true;
        }
    }

    fn current_answer_chars(&self) -> u32 {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "committed markdown char count; feeds deliberation ratio, u32 headroom"
        )]
        let committed = self
            .blocks
            .iter()
            .rev()
            .map_while(|e| e.block.markdown_src())
            .fold(0u32, |n, text| {
                n.saturating_add(text.chars().count() as u32)
            });
        #[allow(
            clippy::cast_possible_truncation,
            reason = "committed markdown char count; feeds deliberation ratio, u32 headroom"
        )]
        let open_chars = self.open.chars().count() as u32;
        committed.saturating_add(open_chars)
    }

    fn upsert_thinking(&mut self, text: String, answer_chars: u32) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(idx) = self.thinking_target() {
            self.blocks[idx].block.append_thinking(&text, answer_chars);
            self.rewrite_log();
            self.flat.dirty = true;
            return;
        }
        // No existing thinking block for this turn: insert before the trailing
        // markdown run, or push to the end if none.
        let insert_at = self
            .blocks
            .iter()
            .rposition(|e| !e.block.is_markdown() && !e.block.is_thinking())
            .map_or(0, |i| i + 1);
        let block = Block::thinking(text, answer_chars);
        if insert_at < self.blocks.len() && self.blocks[insert_at].block.is_markdown() {
            self.blocks.insert(insert_at, Entry { block, rows: 0 });
            self.rewrite_log();
            self.flat.dirty = true;
        } else {
            self.push_block(block);
        }
    }

    /// Tee a block's full content to `user.log`, collapsing redundant
    /// blank separators against the previous line exactly as the screen
    /// flatten does. Returns its line count — the row-cap estimate
    /// ([`Self::push_block`]) reuses this rather than paying for a second
    /// pass over the block.
    fn log_block(&mut self, block: &Block) -> usize {
        let lead = opens_rail_run(self.blocks.last().map(|e| &e.block), block);
        let lines = block.log_lines(self.agent, lead);
        let n = lines.len();
        self.write_log_lines(lines);
        n
    }

    /// Rebuild the whole log from `self.blocks` — used when an existing
    /// block was mutated or a new one inserted mid-vector (a thinking-block
    /// append or insert), rather than appended. Also refreshes each entry's
    /// row count from the same pass (no second walk) and re-enforces the
    /// window caps, since an in-place append can grow a block past the row
    /// budget without changing [`Self::blocks`]'s length.
    fn rewrite_log(&mut self) {
        let rendered = self
            .blocks
            .iter()
            .enumerate()
            .map(|(i, entry)| {
                let lead = opens_rail_run(
                    i.checked_sub(1).map(|j| &self.blocks[j].block),
                    &entry.block,
                );
                entry.block.log_lines(self.agent, lead)
            })
            .collect::<Vec<_>>();
        for (entry, lines) in self.blocks.iter_mut().zip(&rendered) {
            entry.rows = lines.len();
        }
        self.log = open_log(&self.log_path);
        self.log_prev_blank = true;
        for lines in rendered {
            self.write_log_lines(lines);
        }
        self.enforce_window_caps();
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

    /// Resolve visual row `row` against the virtual thinking seat: a row
    /// backed by the committed flatten, or a row inside the seat itself.
    /// The one statement of the seat-splice arithmetic; [`Self::block_at`],
    /// [`Self::flat_row`], and [`Self::row_width`] are each one-liners over it.
    fn row_site(&self, row: usize) -> RowSite {
        let think_at = self.flat.virtual_think_at;
        let think_len = self.flat.virtual_think_len;
        if think_len == 0 || row < think_at {
            RowSite::Committed(row)
        } else if row < think_at + think_len {
            RowSite::Seat(row - think_at)
        } else {
            RowSite::Committed(row - think_len)
        }
    }

    /// The block owning visual row `row`, or `None` past the buffer's
    /// end.  Valid against the most recent [`Self::render_window`].
    pub(super) fn block_at(&self, row: usize) -> Option<usize> {
        match self.row_site(row) {
            RowSite::Committed(r) => self.flat.row_block.get(r).copied(),
            RowSite::Seat(_) => None,
        }
    }

    /// Map a visual row back to its index in the flattened row buffer, or
    /// `None` when it lands in the virtual thinking seat (which has no backing
    /// text row) or past the buffer's end.  Mirrors [`Self::block_at`] so the
    /// mouse layer's absolute virtual rows resolve to the right text.
    fn flat_row(&self, row: usize) -> Option<usize> {
        match self.row_site(row) {
            RowSite::Committed(r) => (r < self.flat.rows.len()).then_some(r),
            RowSite::Seat(_) => None,
        }
    }

    /// Rendered cell width of visual row `row` — its content's extent, not
    /// the pane's — so a gesture can be bound tight to the text and ignore
    /// the dead margin past where the line ends.  `None` past the buffer.
    pub(super) fn row_width(&self, row: usize) -> Option<usize> {
        match self.row_site(row) {
            RowSite::Committed(r) => self.flat.rows.get(r).map(Line::width),
            RowSite::Seat(s) => self.flat.virtual_think_widths.get(s).copied(),
        }
    }

    /// Whether the block at `idx` is dialable — a stable property of its
    /// kind, independent of its current level, so a wheel resting on its
    /// glyph can claim the gesture even when the level is already clamped.
    pub(super) fn block_dialable(&self, idx: usize) -> bool {
        self.blocks.get(idx).is_some_and(|e| e.block.dialable())
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
        let Some(entry) = self.blocks.get_mut(idx) else {
            return false;
        };
        let block = &mut entry.block;
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
    /// Scroll by `delta` rows, negative for up.
    pub(super) fn scroll_by(&mut self, delta: isize) {
        if delta < 0 {
            self.scroll_up(delta.unsigned_abs());
        } else {
            self.scroll_down(delta.unsigned_abs());
        }
    }

    /// Plain text the drag-selection copies.  `lo` and `hi` are each
    /// `(row, col)` where `col` is a cell-column within the text area
    /// (0 = left edge); the rail glyph is stripped automatically.
    pub(super) fn selection_text(&self, lo: (usize, u16), hi: (usize, u16)) -> String {
        let (lo_row, lo_col) = lo;
        let (hi_row, hi_col) = hi;
        if lo_row == hi_row {
            let (a, b) = if lo_col <= hi_col {
                (lo_col, hi_col)
            } else {
                (hi_col, lo_col)
            };
            return match self.flat_row(lo_row) {
                Some(r) => plain_slice(&self.flat.rows[r], a, b),
                None => String::new(),
            };
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
        if let Some(r) = self.flat_row(first_row) {
            parts.push(plain_slice(&self.flat.rows[r], first_col, u16::MAX));
        }
        // Middle rows: full lines.
        for row in (first_row + 1)..last_row {
            if let Some(r) = self.flat_row(row) {
                parts.push(plain(&self.flat.rows[r]));
            }
        }
        // Last row: from start to last_col.
        if let Some(r) = self.flat_row(last_row) {
            parts.push(plain_slice(&self.flat.rows[r], 0, last_col));
        }
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
            .map_while(|e| e.block.markdown_src())
            .collect();
        tail.reverse();
        tail.concat().trim().to_owned()
    }

    // ── rendering ────────────────────────────────────────────────────────

    /// The provisional seat for in-flight reasoning, matching the committed
    /// block's collapsed (L1) header: a blank separator, then the deliberation
    /// grain beside a `size_bar` — no prose.  The caller seats the `∴` rail glyph
    /// on the first content row (matching `render_with`).
    fn thinking_seat(&self) -> Vec<Line<'static>> {
        if self.thinking.trim().is_empty() {
            return vec![];
        }
        #[allow(
            clippy::cast_possible_truncation,
            reason = "think-block char/line count; u32 headroom far exceeds any in-memory transcript"
        )]
        let think_chars = self.thinking.chars().count() as u32;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "think-block char/line count; u32 headroom far exceeds any in-memory transcript"
        )]
        let think_lines = self.thinking.lines().count() as u32;
        let answer_chars = self.current_answer_chars();
        vec![
            Line::default(),
            Line::from(vec![
                grain_run(think_chars, answer_chars),
                Span::raw(" "),
                size_bar(think_lines),
            ]),
        ]
    }

    fn trailing_markdown_start(&self) -> Option<usize> {
        let start = self
            .blocks
            .iter()
            .rposition(|e| !e.block.is_markdown() && !e.block.is_thinking())
            .map_or(0, |i| i + 1);
        self.blocks
            .get(start)
            .is_some_and(|e| e.block.is_markdown())
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
        #[allow(
            clippy::cast_possible_truncation,
            reason = "content count; u32 headroom far exceeds any in-memory transcript"
        )]
        let magnitude = self.open.lines().count() as u32;
        Some(Line::from(vec![
            rail::span(RailKind::Markdown, self.agent, Some(magnitude)),
            size_bar(magnitude),
        ]))
    }

    /// The visible slice at `width` × `height`, after re-flattening if
    /// stale and resolving the scroll position: while `sticky`, `offset` is
    /// pinned to the tail (`max_off`); otherwise the stored `offset` is
    /// clamped to `max_off` and `sticky` re-arms once it reaches the bottom.
    pub(super) fn render_window(&mut self, width: u16, height: usize) -> RenderWindow {
        self.reflow(width);
        // The provisional thinking seat (when the model is reasoning) renders
        // before the trailing markdown answer run.  That lets answer
        // paragraphs keep committing live without visually jumping ahead of
        // the deliberation they follow.
        let mut think = self.thinking_seat();
        // Seat the rail on the thinking rows, matching committed-block rendering.
        if !think.is_empty() {
            #[allow(
                clippy::cast_possible_truncation,
                reason = "think-block char/line count; u32 headroom far exceeds any in-memory transcript"
            )]
            let think_lines = self.thinking.lines().count() as u32;
            if let Some(idx) = think.iter().position(|l| !is_blank(l)) {
                think[idx].spans.insert(
                    0,
                    rail::span(RailKind::Thinking, self.agent, Some(think_lines)),
                );
            }
        }
        let mut seat: Vec<Line<'static>> = self.streaming_seat().into_iter().collect();
        let committed = self.flat.rows.len();
        // The seat continues the trailing answer run when there is one; when
        // it instead follows a thinking seat or a non-markdown block, it opens
        // a fresh run and wears the same blank separator a committing lead
        // paragraph would.
        if !seat.is_empty()
            && self.trailing_markdown_start().is_none()
            && committed + think.len() > 0
        {
            seat.insert(0, Line::default());
        }
        let think_at = if think.is_empty() {
            committed
        } else {
            self.provisional_thinking_row()
        };
        self.flat.virtual_think_at = think_at;
        self.flat.virtual_think_len = think.len();
        self.flat.virtual_think_widths = think.iter().map(Line::width).collect();
        let total = committed + think.len() + seat.len();
        let max_off = total.saturating_sub(height);
        if self.sticky {
            self.offset = max_off;
        } else {
            self.offset = self.offset.min(max_off);
            self.sticky = self.offset >= max_off;
        }
        // Scroll position as a percentage of the scrollable range: `0%` at the
        // top, `100%` once `offset` reaches `max_off` (the tail).  `None` when
        // the whole buffer fits, so the rule line shows no readout.  `offset`
        // is clamped to `max_off` so it stays within the valid scroll range.
        #[allow(
            clippy::cast_possible_truncation,
            reason = "scroll percentage already clamped to 0..=100"
        )]
        let scroll_pct =
            (max_off > 0).then(|| (self.offset.min(max_off) * 100 / max_off).min(100) as u16);
        let split = think_at.min(committed);
        let lines: Vec<Line<'static>> = self.flat.rows[..split]
            .iter()
            .chain(&think)
            .chain(&self.flat.rows[split..])
            .chain(&seat)
            .skip(self.offset)
            .take(height)
            .cloned()
            .collect();
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
    /// ([`super::group`]); every genuine barrier — a diff, a write, a
    /// surfaced card, markdown, a subagent result, or chrome — renders as
    /// its own block exactly as before, save a step boundary interior to a
    /// run, which is folded away.  The projection reads what arrival order
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
            let (anchor, lines, prompt) = if self.blocks[i].block.observation() {
                let end = self.observation_run_end(i);
                let anchor = self.group_anchor(i, end);
                let segment = (anchor, self.render_group(i, end, anchor, content_w), false);
                i = end;
                segment
            } else {
                let prompt = self.blocks[i].block.is_prompt();
                let lead = opens_rail_run(
                    i.checked_sub(1).map(|j| &self.blocks[j].block),
                    &self.blocks[i].block,
                );
                let segment = (
                    i,
                    self.blocks[i].block.lines(content_w, agent, lead).to_vec(),
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
            let added = append_visual_rows(&mut rows, &lines[first..], content_w, prompt, None);
            for _ in 0..added {
                row_block.push(anchor);
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
    /// coalesces into one ral block, **bridged across the step boundaries
    /// interior to it**.  Each call is its own provider round-trip, so a
    /// [`Block::is_step`] chrome (`Kind::Step`) lands between consecutive
    /// calls; left a barrier it would cut every burst back to a single call.
    /// A step boundary is provider bookkeeping, not content: when it falls
    /// *between* run members it is subsumed (and never rendered); a step at
    /// the run's tail is also subsumed — the step carries no content and the
    /// run's own rail already marks its edge.
    fn observation_run_end(&self, start: usize) -> usize {
        // `end` advances past every run member and any step, so a trailing
        // step is folded into the run rather than rendered as its own block.
        let mut end = start;
        let mut i = start;
        while i < self.blocks.len() {
            let block = &self.blocks[i].block;
            // A run member or a step both fold into the run; anything else ends it.
            if block.observation() || block.is_step() {
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
        let level = self.blocks[anchor].block.level();
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
            .find(|&i| self.blocks[i].block.is_tool_call())
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
        for entry in &self.blocks[start..end] {
            if let Some(parts) = entry.block.call_view() {
                if let Some(prev) = pending.take() {
                    calls.push(group::Call::new(
                        prev,
                        std::mem::take(&mut tally),
                        std::mem::take(&mut effects),
                    ));
                }
                pending = Some(parts);
            } else {
                effects.extend(entry.block.effect_lines());
                if let Some((kind, count)) = entry.block.io_tally() {
                    tally.add(kind, count);
                }
            }
        }
        if let Some(prev) = pending {
            calls.push(group::Call::new(prev, tally, effects));
        }
        calls
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn viewport() -> Viewport {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "exarch-viewport-test-{}-{}.log",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
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
        use crate::bus::card::Mark;
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
        let live_text_len = live.lines.iter().map(plain).count();

        vp.commit_thinking("hidden trace\nline two\n".into(), 0);
        let committed = vp.render_window(READ_W, height);
        let committed_text_len = committed.lines.iter().map(plain).count();

        assert_eq!(
            committed.offset, live_offset,
            "committing the hidden trace keeps the scrollback offset stable"
        );
        assert_eq!(
            committed_text_len, live_text_len,
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
    /// [`Self::render_window`] kept pinning `offset` to the tail instead of
    /// honouring the user's position, blanking the lower rows.  Clearing
    /// `sticky` on every user scroll routes through the non-sticky clamp,
    /// which bounds `offset` to `max_off`.
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

    /// A committed thinking block must stay visible in a sticky viewport.
    /// The provisional seat renders near the bottom (before the trailing
    /// markdown run); the committed block must land at the same position,
    /// not jump to after the last prompt where a sticky viewport would
    /// scroll past it.
    #[test]
    fn committed_thinking_stays_visible_in_sticky_viewport() {
        let mut vp = viewport();
        vp.push_chrome(RailShape::Prompt, vec![Line::from("hello cutie")]);
        // Fill enough chrome to overflow a small window.
        for i in 0..8 {
            vp.push_chrome(
                RailShape::Plain,
                vec![
                    Line::from(format!("block {i} line a")),
                    Line::from(format!("block {i} line b")),
                ],
            );
        }
        vp.push_thinking("considering the shape\n");
        vp.push_token("First paragraph.\n\nSecond paragraph.", 0);
        // While thinking is live, the provisional seat is visible.
        let live = vp.render_window(READ_W, 8);
        let live_text = live.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        let live_thinking = rail_rows(&live.lines, "∴ ");
        assert!(
            !live_thinking.is_empty(),
            "live thinking has its rail: {live_text:?}"
        );
        // Commit the thinking — the real block should land where the
        // provisional seat was, not jump above the visible window.
        vp.commit_thinking("considering the shape\n".into(), vp.current_answer_chars());
        vp.close_boundary(0);
        let committed = vp.render_window(READ_W, 8);
        let committed_text = committed
            .lines
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join("\n");
        let committed_thinking = rail_rows(&committed.lines, "∴ ");
        assert!(
            !committed_thinking.is_empty(),
            "committed thinking stays visible in sticky viewport: {committed_text:?}"
        );
    }

    // ── viewport window caps and tombstones (7b) ───────────────────────────

    /// Each pushed block's log rendering, joined into one string — lets a
    /// test read back which marker survived without hard-coding the render
    /// shape.
    fn block_text(b: &Block) -> String {
        b.log_lines(AgentSlot(0), true)
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Pushing past `VIEWPORT_MAX_BLOCKS` evicts the oldest blocks first: the
    /// count never exceeds the cap, and the newest survives.
    #[test]
    fn window_evicts_oldest_blocks_first_past_the_block_cap() {
        let mut vp = viewport();
        for i in 0..(VIEWPORT_MAX_BLOCKS + 50) {
            vp.push_chrome(RailShape::Plain, vec![Line::from(format!("marker {i}"))]);
        }
        assert_eq!(
            vp.blocks.len(),
            VIEWPORT_MAX_BLOCKS,
            "capped at the block limit"
        );
        assert!(
            block_text(&vp.blocks[0].block).contains("marker 50"),
            "oldest-first eviction: the 51st pushed block is now the oldest survivor: {}",
            block_text(&vp.blocks[0].block)
        );
        assert!(
            block_text(&vp.blocks.last().unwrap().block)
                .contains(&format!("marker {}", VIEWPORT_MAX_BLOCKS + 49)),
            "the newest block always survives"
        );
    }

    /// A run of oversized blocks blows the row budget well before the block
    /// count does; the row cap evicts oldest-first on its own, independent of
    /// the block-count cap.
    #[test]
    fn window_evicts_oldest_blocks_first_past_the_row_cap() {
        let mut vp = viewport();
        // Five ~6,000-line blocks: 30,000 raw lines against a 20,000-row cap,
        // with only 5 blocks pushed — nowhere near `VIEWPORT_MAX_BLOCKS`.
        for block in 0..5 {
            let lines = (0..6000)
                .map(|i| Line::from(format!("b{block} line {i}")))
                .collect();
            vp.push_chrome(RailShape::Plain, lines);
        }
        assert!(
            vp.blocks.len() < 5,
            "the row cap evicts at least the oldest block on its own: {} remain",
            vp.blocks.len()
        );
        assert!(
            vp.blocks.iter().map(|e| e.rows).sum::<usize>() <= VIEWPORT_MAX_ROWS,
            "resident rows stay under the cap"
        );
        assert!(
            !block_text(&vp.blocks[0].block).contains("b0 line"),
            "the oldest block (b0) was evicted, not a newer one"
        );
        assert!(
            block_text(&vp.blocks.last().unwrap().block).contains("b4 line"),
            "the newest block (b4) survives"
        );
    }

    /// Eviction into a tombstone carries exactly the agent id, the final
    /// status, and the log path — everything else (blocks, pins) is dropped.
    #[test]
    fn evict_to_tombstone_keeps_exactly_the_three_facts() {
        let mut vp = viewport();
        vp.push_chrome(RailShape::Plain, vec![Line::from("hello")]);
        vp.set_pin("k".into(), Card(Vec::new()));
        assert!(vp.tombstone().is_none());

        let log_path = vp.log_path.clone();
        vp.evict_to_tombstone(42);

        assert!(vp.tombstone().is_some());
        let t = vp.tombstone().expect("tombstoned");
        assert_eq!(t.id, 42);
        assert!(!t.error);
        assert_eq!(t.log_path, log_path);
        assert!(vp.blocks.is_empty(), "the scrollback is dropped");
        assert!(vp.pins().is_empty(), "the pinned register is dropped");

        // Its one-line rendering names the agent, the status, and the path.
        let rendered = plain(&t.line());
        assert!(rendered.contains("42"), "{rendered:?}");
        assert!(rendered.contains("done"), "{rendered:?}");
        assert!(
            rendered.contains(&log_path.display().to_string()),
            "{rendered:?}"
        );
    }

    /// The tombstone's status is read off the view's last block before it is
    /// dropped: an error-terminated session tombstones as an error.
    #[test]
    fn evict_to_tombstone_reads_error_status_off_the_last_block() {
        let mut vp = viewport();
        vp.push_chrome(RailShape::Error, vec![Line::from("boom")]);
        assert!(vp.last_is_error());
        vp.evict_to_tombstone(7);
        assert!(vp.tombstone().unwrap().error);
    }

    /// Re-evicting an already-tombstoned view is a no-op — the first
    /// tombstone's facts (and its status) are not overwritten by whatever
    /// state (or lack of it) exists at the second call.
    #[test]
    fn evict_to_tombstone_is_idempotent() {
        let mut vp = viewport();
        vp.push_chrome(RailShape::Error, vec![Line::from("boom")]);
        vp.evict_to_tombstone(1);
        assert!(vp.tombstone().unwrap().error);
        // A second call (e.g. a defensive re-tick) must not reset the id or
        // status even though the view is now clean (no error block).
        vp.evict_to_tombstone(999);
        assert_eq!(vp.tombstone().unwrap().id, 1, "the id is not overwritten");
        assert!(
            vp.tombstone().unwrap().error,
            "the status is not overwritten"
        );
    }
}
