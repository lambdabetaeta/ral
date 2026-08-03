//! Per-session collapsible scrollback.
//!
//! A [`Viewport`] turns one session's content events into [`Block`]s, flattens
//! those into the renderer's visual rows, and tees every committed line to the
//! session's `user.log` — the durable counterpart to `events.json`.  The whole
//! alt-screen frame is redrawn each tick, so scrollback is ours, not the host
//! terminal's, and every tab keeps its own scroll position.

use super::block::{AgentSlot, Block, RailShape, Reveal, append_visual_rows};
use super::fidelity::{self, Fidelity};
use super::group;
use super::line::{self, is_blank, plain, size_bar};
use super::palette::READ_W;
use super::rail::{self, RailKind};
use super::select::plain_slice;
use crate::bus::card::{Card, Hunk, ObservationKind};
use crate::bus::{AgentId, AgentState};
use crate::provider::Usage;
use ratatui::text::Line;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Scrollback blocks held in heap; past this they are evicted oldest-first,
/// already durable in `user.log`/`events.json`.
pub(super) const VIEWPORT_MAX_BLOCKS: usize = 500;
/// Rendered-row cap — a second eviction trigger, for the oversized block that
/// blows the row budget long before the block count does.
pub(super) const VIEWPORT_MAX_ROWS: usize = 20_000;

/// All that outlives a dead sub-agent view once [`super::LINGER`] elapses.
/// Nothing is ever reloaded from the log; it stays readable outside the TUI.
pub(super) struct Tombstone {
    pub(super) id: AgentId,
    pub(super) error: bool,
    pub(super) log_path: PathBuf,
}

impl Tombstone {
    /// The row `Tabs::tombstone_lines` collects for the `/resources` fold.
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
    /// The session's scrollback, oldest block first.
    blocks: Vec<Entry>,
    /// Set once evicted ([`Self::evict_to_tombstone`]); `blocks` is empty from
    /// that point on, and `None` for a live or still-lingering view.
    tombstone: Option<Tombstone>,
    /// Palette slot stamped onto every block at push; root is `0`.
    agent: AgentSlot,
    /// This session's spend — the matrix's per-agent readout, where
    /// `App::total_usage` is the rule line's sum over all of them.
    usage: Usage,
    /// Assistant text since the last fence-safe paragraph break.  It never
    /// streams as prose: only its magnitude shows ([`Self::streaming_seat`])
    /// until a break or the turn's end commits it as a [`Block::markdown`].
    open: String,
    /// In-flight reasoning, likewise shown as magnitude only, cleared when
    /// [`Self::commit_thinking`] lands the authoritative [`Block::thinking`].
    thinking: String,
    /// Top visible visual row, per-viewport so each tab keeps its place.
    offset: usize,
    /// Follow the tail.  Cleared by a scroll either way, re-armed at the bottom.
    sticky: bool,
    flat: Flat,
    /// Tee of every committed line to `user.log`, flushed as each block lands
    /// so the rendered transcript survives an abnormal exit.
    log: io::BufWriter<Box<dyn io::Write + Send>>,
    log_path: PathBuf,
    /// Whether the last logged line was blank, so blanks collapse on disk as
    /// they do on screen.
    log_prev_blank: bool,
    /// Total, never absent: the status line always has a state to name.
    state: StateSpan,
    /// Kit-authored *state*: a `key → Card` register drawn as the right-hand
    /// column.  Never logged, and wiped by [`Self::reset`] so a pin is
    /// generation-bounded like the scrollback.  A `Vec`, not a map, so render
    /// order is first-seen insertion order.
    pins: Vec<(String, Card)>,
}

/// The agent's state, when it was entered, and the model text that has arrived
/// since — the status line's whole datum, so one transition resets all three.
/// The instant anchors the elapsed-wait bar to that transition rather than to
/// the last event of any kind, which is what makes a silent stream legible.
#[derive(Clone, Copy)]
pub(super) struct StateSpan {
    pub(super) state: AgentState,
    since: Instant,
    /// Characters of model text arrived in this state.  A count that stops
    /// growing under a growing [`Self::elapsed`] is a stalled stream.
    pub(super) streamed: usize,
}

impl StateSpan {
    pub(super) fn new(state: AgentState) -> Self {
        Self {
            state,
            since: Instant::now(),
            streamed: 0,
        }
    }

    /// Time in state.
    pub(super) fn elapsed(self) -> Duration {
        self.since.elapsed()
    }
}

pub(super) struct RenderWindow {
    pub(super) lines: Vec<Line<'static>>,
    pub(super) offset: usize,
    /// Progress through the buffer in `0..=100`, or `None` when it all fits.
    /// The rule line shows it in place of a right-margin scrollbar.
    pub(super) scroll_pct: Option<u16>,
}

/// A scrollback block beside its `user.log` row count, captured where that
/// count is already computed; summed, it is the [`VIEWPORT_MAX_ROWS`] trigger.
struct Entry {
    block: Block,
    rows: usize,
}

/// Memoised whole-buffer flatten: block lines wrapped to `width`, `row_block[i]`
/// naming the block row `i` came from, and the `virtual_think_*` record of the
/// live thinking seat — rows the render splices in that back no text.
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

/// Where a visual row lands relative to the spliced thinking seat: in the
/// committed flatten (re-indexed if it falls past the seat), or in the seat.
enum RowSite {
    Committed(usize),
    Seat(usize),
}

/// The byte index just past the last `\n\n` at fence depth zero, so
/// `open.drain(..idx)` peels off the committable prefix; `None` means every
/// candidate sits inside an open fence.  `CommonMark` has no nested fences, so
/// one bit of depth suffices.
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

/// Whether `block` opens its own rail run.  Streaming commits each fence-safe
/// paragraph separately ([`Viewport::flush_complete_paragraphs`]), so a run of
/// markdown blocks is one response and only its head wears the `·`.
fn opens_rail_run(prev: Option<&Block>, block: &Block) -> bool {
    !(block.markdown_src().is_some() && prev.is_some_and(|p| p.markdown_src().is_some()))
}

/// Open the session's rendered-text log, truncating any prior content.  Falls
/// back to a discarding sink, so a log-path failure never disables the viewport.
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

/// Copy a flushed `user.log` to `dest` for `/export` — the caller resolves
/// `dest`, refuses to overwrite, and flushes.  Beside [`open_log`], so all
/// `user.log` I/O lives in one place.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:export] copies a flushed user.log to the user-chosen export path; output infra, not turn-time data I/O"
)]
pub(super) fn export_log(src: &Path, dest: &Path) -> io::Result<u64> {
    fs::copy(src, dest)
}

impl Viewport {
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
            state: StateSpan::new(AgentState::Ready),
            pins: Vec::new(),
        }
    }

    pub(super) fn agent(&self) -> AgentSlot {
        self.agent
    }

    /// Fold one turn's spend in; the same event also feeds `App::total_usage`.
    pub(super) fn add_usage(&mut self, u: Usage) {
        self.usage += u;
    }

    pub(super) fn usage(&self) -> Usage {
        self.usage
    }

    /// Per-step "had a tool call" flags, oldest first — one bool per
    /// [`Block::is_step`] boundary, which the matrix renders `●` or `○`.
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

    /// Summed [`Block::lines_changed`] over this session's diffs — the matrix's
    /// size readout.  `0` for a read-only agent; prose never inflates it.
    pub(super) fn lines_touched(&self) -> u32 {
        self.blocks
            .iter()
            .filter_map(|e| e.block.lines_changed())
            .sum()
    }

    /// Whether the session's last block is an error — the matrix leads a dying
    /// row with `╳` rather than `√`.
    pub(super) fn last_is_error(&self) -> bool {
        self.blocks.last().is_some_and(|e| e.block.is_error())
    }

    /// `(blocks, rows, bytes)` for the `/resources` fold, read off the flatten
    /// as of the last paint — a read of display state, never a re-render.
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
    /// Enter `state`, restarting the clock and the streamed count.  Re-entering
    /// the state already held is a no-op: a step that re-drives the same wait
    /// must not reset the clock measuring how long that wait has run.
    pub(super) fn set_state(&mut self, state: AgentState) {
        if self.state.state != state {
            self.state = StateSpan::new(state);
        }
    }

    /// Count `chars` of arriving model text against the current state — what
    /// separates a stream that is delivering from one that has gone silent.
    pub(super) fn note_streamed(&mut self, chars: usize) {
        self.state.streamed = self.state.streamed.saturating_add(chars);
    }

    pub(super) fn state(&self) -> StateSpan {
        self.state
    }

    /// Wipe scrollback, scroll state, and streaming buffers, truncating
    /// `user.log` by reopening it.  `/clear` on the root.
    pub(super) fn reset(&mut self) {
        *self = Self::new(self.log_path.clone(), self.agent);
    }

    /// Drop this view's heap state for a [`Tombstone`], reading the final status
    /// off the last block before that block goes.  Idempotent: by a second call
    /// the view is clean, so re-reading the status would record a lie.
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

    pub(super) fn tombstone(&self) -> Option<&Tombstone> {
        self.tombstone.as_ref()
    }

    /// Final flush at session end; the caller owns the I/O error policy.
    pub(super) fn flush_log(&mut self) -> io::Result<&Path> {
        self.log.flush()?;
        Ok(&self.log_path)
    }

    // ── content ──────────────────────────────────────────────────────────

    /// Append a tool call as its own collapsible block.  `context` is the turn's
    /// degradation floor, so the intent line drains under context pressure.
    pub(super) fn push_tool_call(
        &mut self,
        tool: &'static str,
        summary: String,
        cmd: String,
        context: u8,
    ) {
        self.push_block(Block::tool_call(tool, summary, cmd, context));
    }

    /// Append a harness act — a barrier the coalescing projection never folds
    /// into a `ral` run, since an act changes the world rather than observing it.
    pub(super) fn push_act(
        &mut self,
        verb: &'static str,
        subject: Option<String>,
        payload: String,
        failed: bool,
    ) {
        self.push_block(Block::act(verb, subject, payload, failed));
    }

    /// Append an async subagent's landed result, collapsed to a one-line header.
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

    /// Append a single-file diff; it re-renders from its hunks at every width
    /// and disclosure level.
    pub(super) fn push_patch(&mut self, path: String, hunks: Vec<Hunk>) {
        self.push_block(Block::patch(path, hunks));
    }

    /// Append a surfaced render document — the model's own communication, so
    /// the coalescing projection never folds it.
    pub(super) fn push_card(&mut self, card: Card) {
        self.push_block(Block::card(card));
    }

    // ── pinned state (the register) ────────────────────────────────────────
    // The in-place analogue of `push_card`: a pin writes a keyed slot, touching
    // neither flatten nor log — pinned state is ambient, not scrollback.

    /// Overwrite the register slot `key`, or append it, keeping first-seen order.
    pub(super) fn set_pin(&mut self, key: String, card: Card) {
        match self.pins.iter_mut().find(|(k, _)| *k == key) {
            Some((_, slot)) => *slot = card,
            None => self.pins.push((key, card)),
        }
    }

    pub(super) fn drop_pin(&mut self, key: &str) {
        self.pins.retain(|(k, _)| k != key);
    }

    pub(super) fn pins(&self) -> &[(String, Card)] {
        &self.pins
    }

    /// Append a foldable observation card — a read, grep or exec the projection
    /// folds under its call, `count` being its weight in the run's census.
    pub(super) fn push_observation_card(&mut self, card: Card, kind: ObservationKind, count: u32) {
        self.push_block(Block::observation_card(card, kind, count));
    }

    /// Append a write card — a barrier ending the current ral run, like a diff.
    pub(super) fn push_write_card(&mut self, card: Card) {
        self.push_block(Block::write_card(card));
    }

    /// Append pre-rendered chrome; `shape` lets the rail dispatch on the sub-kind.
    pub(super) fn push_chrome(&mut self, shape: RailShape, lines: Vec<Line<'static>>) {
        self.push_block(Block::chrome(shape, lines));
    }

    /// Append a summary-less tool call, shown standalone as a `▸` rail block.
    /// `detail` is `None` for an invisible parse-failure boundary.
    pub(super) fn push_plain_call(&mut self, detail: Option<String>) {
        self.push_block(Block::plain_call(detail));
    }

    /// Attach a tool result's line count to the nearest call behind the tail —
    /// a card may land between a call and its result.  The walk halts at the
    /// first [`Block::is_call`], plain or dialable, so a plain call's result
    /// cannot reach past it to an earlier call's size bar.
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

    /// Commit a reasoning phase into the turn's coalescing `∴` block
    /// ([`Self::thinking_target`]), superseding its provisional deltas, or into
    /// a fresh block before the trailing markdown run.  `answer_chars` is the
    /// say-side of the deliberation ratio.
    pub(super) fn commit_thinking(&mut self, text: String, answer_chars: u32) {
        let preserve_scrollback = self.looking_at_pushed_thinking();
        self.thinking.clear();
        self.upsert_thinking(text, answer_chars);
        if preserve_scrollback {
            self.sticky = false;
        }
    }

    /// Whether the view is parked on scrollback the live thinking seat pushed
    /// down; turning that seat into a real block must then not re-arm
    /// tail-follow and yank those rows back up under the reader.
    fn looking_at_pushed_thinking(&self) -> bool {
        !self.sticky
            && self.flat.virtual_think_len > 0
            && self.offset <= self.flat.virtual_think_at + self.flat.virtual_think_len
    }

    /// Append a live reasoning delta: into the coalescing `∴` block's
    /// provisional buffer when the turn has one ([`Self::thinking_target`]), so
    /// its magnitude ticks in place and nothing moves, else into the seat.
    pub(super) fn push_thinking(&mut self, text: &str) {
        if let Some(idx) = self.thinking_target() {
            self.blocks[idx].block.push_provisional_thinking(text);
        } else {
            self.thinking.push_str(text);
        }
        self.flat.dirty = true;
    }

    /// The block the turn's reasoning coalesces into: the latest `∴` block with
    /// no prose or prompt after it.  Tool calls do not break the run — only
    /// having spoken to the reader, or a new human turn, seeds a fresh block.
    fn thinking_target(&self) -> Option<usize> {
        let idx = self.blocks.iter().rposition(|e| e.block.is_thinking())?;
        let unbroken = !self.blocks[idx + 1..]
            .iter()
            .any(|e| e.block.is_markdown() || e.block.is_prompt());
        unbroken.then_some(idx)
    }

    /// Push streamed text, committing any fence-safe paragraph at `context_floor`.
    pub(super) fn push_token(&mut self, text: &str, context_floor: u8) {
        self.open.push_str(text);
        self.flush_complete_paragraphs(context_floor);
    }

    /// End a streaming step: commit the pending reasoning, then `open`.
    pub(super) fn close_boundary(&mut self, context_floor: u8) {
        if !self.thinking.trim().is_empty() {
            let text = std::mem::take(&mut self.thinking);
            let answer_chars = self.current_answer_chars();
            self.commit_thinking(text, answer_chars);
        }
        self.flush_open(context_floor);
    }
    /// Commit the longest fence-safe prefix of `open` as one markdown block.
    /// Breaking elsewhere would split a code fence across two `render_md` calls,
    /// so with no safe break the buffer grows until the fence or the turn closes.
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

    /// Commit what remains in `open`, at turn end or on `/clear`.
    pub(super) fn flush_open(&mut self, context_floor: u8) {
        let leftover = std::mem::take(&mut self.open);
        if leftover.trim().is_empty() {
            return;
        }
        let fidelity = self.commit_fidelity(&leftover, context_floor);
        self.push_block(Block::markdown(leftover, fidelity));
    }

    /// The fidelity to stamp on a committing markdown block: `context_floor` is
    /// the turn-level floor every paragraph inherits, the echo delta its
    /// per-paragraph modifier against the most recent `ral` script.
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

    fn push_block(&mut self, block: Block) {
        let rows = self.log_block(&block);
        self.blocks.push(Entry { block, rows });
        self.flat.dirty = true;
        self.enforce_window_caps();
    }

    /// Evict oldest-first once either cap is crossed: one walk from the tail for
    /// the longest suffix satisfying both, then one `drain`.  The newest block
    /// always survives, however oversized.
    fn enforce_window_caps(&mut self) {
        let mut kept = 0usize;
        let mut rows = 0usize;
        for entry in self.blocks.iter().rev() {
            if kept == VIEWPORT_MAX_BLOCKS || (kept > 0 && rows + entry.rows > VIEWPORT_MAX_ROWS) {
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
        match self.trailing_markdown_start() {
            Some(at) => {
                self.blocks.insert(
                    at,
                    Entry {
                        block: Block::thinking(text, answer_chars),
                        rows: 0,
                    },
                );
                self.rewrite_log();
                self.flat.dirty = true;
            }
            None => self.push_block(Block::thinking(text, answer_chars)),
        }
    }

    /// Tee a block to `user.log`, collapsing blanks exactly as the screen
    /// flatten does, and return its line count — the row cap reuses that rather
    /// than paying a second pass over the block.
    fn log_block(&mut self, block: &Block) -> usize {
        let lead = opens_rail_run(self.blocks.last().map(|e| &e.block), block);
        let lines = block.log_lines(self.agent, lead);
        let n = lines.len();
        self.write_log_lines(lines);
        n
    }

    /// Rebuild the whole log — a thinking block mutated in place or inserted
    /// mid-vector cannot be appended.  Refreshes each entry's row count in the
    /// same pass and re-enforces the caps, which an in-place append can breach
    /// without changing the block count.
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

    /// The one statement of the seat-splice arithmetic; [`Self::block_at`],
    /// [`Self::flat_row`], and [`Self::row_width`] are one-liners over it.
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

    /// The block owning visual row `row` — valid only against the most recent
    /// [`Self::render_window`], which is what fixed the seat's position.
    pub(super) fn block_at(&self, row: usize) -> Option<usize> {
        match self.row_site(row) {
            RowSite::Committed(r) => self.flat.row_block.get(r).copied(),
            RowSite::Seat(_) => None,
        }
    }

    /// Visual row to index in `flat.rows`, or `None` inside the thinking seat
    /// (no backing text) or past the end.
    fn flat_row(&self, row: usize) -> Option<usize> {
        match self.row_site(row) {
            RowSite::Committed(r) => (r < self.flat.rows.len()).then_some(r),
            RowSite::Seat(_) => None,
        }
    }

    /// Rendered cell width of visual row `row` — its content's extent, not the
    /// pane's, so a gesture binds tight to the text and ignores the dead margin.
    pub(super) fn row_width(&self, row: usize) -> Option<usize> {
        match self.row_site(row) {
            RowSite::Committed(r) => self.flat.rows.get(r).map(Line::width),
            RowSite::Seat(s) => self.flat.virtual_think_widths.get(s).copied(),
        }
    }

    /// Whether the block at `idx` is dialable — a property of its kind, not its
    /// level, so a wheel on its glyph claims the gesture even when clamped.
    pub(super) fn block_dialable(&self, idx: usize) -> bool {
        self.blocks.get(idx).is_some_and(|e| e.block.dialable())
    }

    /// Dial the block at `idx`, reporting whether it changed — so the caller
    /// can tell a real dial from a gesture on inert chrome or a clamped level.
    pub(super) fn dial_block(&mut self, idx: usize, delta: i8) -> bool {
        self.mutate_block(idx, |b| b.dial(delta))
    }

    /// Cycle the block at `idx` between L1 and L3 — the click-on-rail affordance.
    pub(super) fn cycle_block(&mut self, idx: usize) -> bool {
        self.mutate_block(idx, Block::cycle)
    }

    /// Apply `f` to the dialable block at `idx`, staling the flatten if the
    /// level moved.
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

    /// Plain text a drag selection copies.  `col` is a cell-column within the
    /// text area (0 = left edge); the rail glyph is stripped automatically.
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
        if let Some(r) = self.flat_row(first_row) {
            parts.push(plain_slice(&self.flat.rows[r], first_col, u16::MAX));
        }
        for row in (first_row + 1)..last_row {
            if let Some(r) = self.flat_row(row) {
                parts.push(plain(&self.flat.rows[r]));
            }
        }
        if let Some(r) = self.flat_row(last_row) {
            parts.push(plain_slice(&self.flat.rows[r], 0, last_col));
        }
        parts.join("\n")
    }

    /// The assistant's latest reply as raw markdown — what `/copy` copies.  Each
    /// paragraph committed separately, so the trailing prose run reassembles the
    /// answer; a call, card, or chrome block bounds it, and ending the turn on
    /// one leaves the reply empty.
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
    /// block's collapsed header row for row so committing shifts nothing: a
    /// blank, then the deliberation grain beside a [`size_bar`] — no prose.
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
        line::thinking_header(think_chars, think_lines, answer_chars)
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

    /// The trailing seat for the in-flight response: one row projecting only the
    /// *magnitude* of [`Self::open`], never its text, so the growing edge reads
    /// as accruing volume while the transcript above stays a finished image.
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

    /// The visible slice at `width` × `height`.  While `sticky`, `offset` is
    /// pinned to the tail; otherwise it is clamped to `max_off` and `sticky`
    /// re-arms once it reaches the bottom.
    pub(super) fn render_window(&mut self, width: u16, height: usize) -> RenderWindow {
        self.reflow(width);
        // The thinking seat renders *before* the trailing answer run, so
        // paragraphs keep committing live without jumping ahead of the
        // deliberation they follow.
        let mut think = self.thinking_seat();
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
        // Following a thinking seat or a non-markdown block, the streaming seat
        // opens a fresh run, so it wears the blank separator a committing lead
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
    /// The flatten is the coalescing projection: an observation run — a call and
    /// its reads, greps and execs ([`Self::observation_run_end`]) — folds into
    /// one dialable ral block ([`super::group`]), while every genuine barrier
    /// keeps its own.  Each visual row maps to its source block, a group's rows
    /// to its anchor call, so dial, click and copy address whole blocks.
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
            // `prompt` carries the human turn's rule fence down to
            // `append_visual_rows`, the one place the content width is known.
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
            // A segment's leading blanks collapse against an already-blank tail,
            // so a step separator before leading-blank chrome reads as one gap.
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

    /// End (exclusive) of the maximal [`Block::observation`] run at `start`,
    /// bridged across step boundaries.  Each call is its own provider
    /// round-trip, so a [`Block::is_step`] chrome lands between consecutive
    /// calls; left a barrier it would cut every burst back to one call.
    fn observation_run_end(&self, start: usize) -> usize {
        let mut end = start;
        let mut i = start;
        while i < self.blocks.len() {
            let block = &self.blocks[i].block;
            if block.observation() || block.is_step() {
                i += 1;
                end = i;
            } else {
                break;
            }
        }
        end
    }

    /// Render the observation run `start..end` as one coalesced ral block: a
    /// [`group::Call`] per tool call, the body at the `anchor` call's disclosure
    /// level, and the rail — triangle, hue, aggregate magnitude — on row one.
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

    /// A run's anchor: its first tool call, whose [`Block::level`] is the run's
    /// disclosure level.  Falls back to the head if, defensively, none leads it.
    fn group_anchor(&self, start: usize, end: usize) -> usize {
        (start..end)
            .find(|&i| self.blocks[i].block.is_tool_call())
            .unwrap_or(start)
    }

    /// The run's calls in arrival order: each tool call opens a [`group::Call`],
    /// and the observation cards up to the next call are its effects — rendered
    /// rows plus a [`group::Tally`] folded by `|>` kind.
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

    /// Re-pinning overwrites a slot in place, `drop_pin` removes one, `reset`
    /// wipes the lot — the same generation discipline that bounds scrollback.
    #[test]
    fn pins_overwrite_in_place_and_keep_insertion_order() {
        use crate::bus::card::Mark;
        let raw = |b: &[u8]| Card(vec![Mark::Raw { bytes: b.to_vec() }]);
        let keys = |vp: &Viewport| vp.pins().iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        let mut vp = viewport();
        vp.set_pin("tasks".into(), raw(b"v1"));
        vp.set_pin("build".into(), raw(b"ok"));
        assert_eq!(keys(&vp), ["tasks", "build"]);

        vp.set_pin("tasks".into(), raw(b"v2"));
        assert_eq!(keys(&vp), ["tasks", "build"]);
        assert!(matches!(&vp.pins()[0].1.0[..], [Mark::Raw { bytes }] if bytes == b"v2"));

        vp.drop_pin("tasks");
        assert_eq!(keys(&vp), ["build"]);
        vp.reset();
        assert!(vp.pins().is_empty(), "reset wipes the register");
    }

    /// A streaming response renders as one trailing magnitude row and never its
    /// text; the prose appears only when the boundary commits it.
    #[test]
    fn streaming_renders_a_magnitude_seat_not_text() {
        let mut vp = viewport();
        // An unclosed fence has no fence-safe break, so nothing commits.
        vp.push_token("```ral\nlet x = 1\nlet y = 2\n", 0);
        assert!(vp.open.contains("let x = 1"));

        let w = vp.render_window(READ_W, 24);
        let seat_line = w.lines.last().expect("a seat row while streaming");
        // Read off the spans, since `plain` would strip the rail glyph.
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

    /// A prompt opens with a rule fence and no band — background belongs to code.
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

    /// `scroll_down` clears `sticky`, so `render_window` takes the clamping
    /// branch — scrolling down at the bottom must not over-scroll past
    /// `max_off` and blank the rows below.
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
        let w0 = vp.render_window(READ_W, height);
        assert!(vp.sticky, "a fresh viewport follows the tail");
        let max_off = w0.offset;
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

    /// The committed thinking block must land where the seat rendered, before
    /// the trailing markdown run — not after the last prompt, where a sticky
    /// viewport would scroll straight past it.
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
        let live = vp.render_window(READ_W, 8);
        let live_text = live.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        let live_thinking = rail_rows(&live.lines, "∴ ");
        assert!(
            !live_thinking.is_empty(),
            "live thinking has its rail: {live_text:?}"
        );
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

    // ── viewport window caps and tombstones ────────────────────────────────

    /// A block's log rendering as one string, so a test can read back which
    /// marker survived without hard-coding the render shape.
    fn block_text(b: &Block) -> String {
        b.log_lines(AgentSlot(0), true)
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Past `VIEWPORT_MAX_BLOCKS` the oldest go first, and the newest survives.
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

    /// The row cap evicts oldest-first on its own, without the block-count cap.
    #[test]
    fn window_evicts_oldest_blocks_first_past_the_row_cap() {
        let mut vp = viewport();
        // 30,000 raw lines against a 20,000-row cap, from only five blocks —
        // nowhere near `VIEWPORT_MAX_BLOCKS`.
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

    /// A tombstone carries the id, the status, and the log path; the rest goes.
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

        let rendered = plain(&t.line());
        assert!(rendered.contains("42"), "{rendered:?}");
        assert!(rendered.contains("done"), "{rendered:?}");
        assert!(
            rendered.contains(&log_path.display().to_string()),
            "{rendered:?}"
        );
    }

    /// The status is read off the last block before that block is dropped.
    #[test]
    fn evict_to_tombstone_reads_error_status_off_the_last_block() {
        let mut vp = viewport();
        vp.push_chrome(RailShape::Error, vec![Line::from("boom")]);
        assert!(vp.last_is_error());
        vp.evict_to_tombstone(7);
        assert!(vp.tombstone().unwrap().error);
    }

    /// Re-evicting is a no-op: the view is clean by then, so re-reading its
    /// status would overwrite the first tombstone's with a lie.
    #[test]
    fn evict_to_tombstone_is_idempotent() {
        let mut vp = viewport();
        vp.push_chrome(RailShape::Error, vec![Line::from("boom")]);
        vp.evict_to_tombstone(1);
        assert!(vp.tombstone().unwrap().error);
        vp.evict_to_tombstone(999);
        assert_eq!(vp.tombstone().unwrap().id, 1, "the id is not overwritten");
        assert!(
            vp.tombstone().unwrap().error,
            "the status is not overwritten"
        );
    }

    /// An act is a barrier: the run scan ends at it, so it renders standalone
    /// under its own `↗` between two separate `▸` runs, never swallowed into a
    /// burst of reads.
    #[test]
    fn an_act_breaks_a_run_of_observations() {
        let mut vp = viewport();
        vp.push_tool_call(
            "ral",
            "read the renderer".into(),
            "read 'line.rs'".into(),
            0,
        );
        vp.push_tool_call("ral", "read the block".into(), "read 'block.rs'".into(), 0);
        vp.push_act(
            "message",
            Some("hunter".into()),
            "focus on the renderer first".into(),
            false,
        );
        vp.push_tool_call("ral", "read the rail".into(), "read 'rail.rs'".into(), 0);

        let w = vp.render_window(READ_W, 40);
        let act = rail_rows(&w.lines, "↗ ");
        assert_eq!(act.len(), 1, "the act renders exactly one rail row");
        let runs = rail_rows(&w.lines, "▸ ");
        assert_eq!(
            runs.len(),
            2,
            "the act splits the reads into two coalesced runs, not one"
        );
        assert!(
            runs[0] < act[0] && act[0] < runs[1],
            "the act holds its arrival position between the two runs"
        );
        // Its own row is three columns, not an intent line under a run's head.
        assert_eq!(
            plain(&w.lines[act[0]]),
            "message    hunter              focus on the renderer first"
        );
    }

    /// An act is not a call-bearing block, so a `ral` result landing after one
    /// walks past it to the call that actually earned the bar.
    #[test]
    fn an_acts_result_stamps_no_bar_and_shields_no_call() {
        let mut vp = viewport();
        vp.push_tool_call(
            "ral",
            "read the renderer".into(),
            "read 'line.rs'".into(),
            0,
        );
        vp.push_act("cancel", Some("hunter".into()), String::new(), false);
        vp.set_result_size(&"a line\n".repeat(40));

        let w = vp.render_window(READ_W, 40);
        let act = rail_rows(&w.lines, "↗ ");
        assert_eq!(plain(&w.lines[act[0]]), "cancel     hunter");
        let run = plain(&w.lines[rail_rows(&w.lines, "▸ ")[0]]);
        assert!(
            run.ends_with(crate::tui::line::spark_glyph(Some(40))),
            "the `ral` call keeps the magnitude it earned: {run:?}"
        );
        assert!(
            !run.ends_with(crate::tui::line::spark_glyph(None)),
            "and it is not the resultless call's shortest bar: {run:?}"
        );
    }
}
