//! Per-session collapsible scrollback.
//!
//! A [`Viewport`] turns one session's content into [`Block`]s, flattens those
//! into the renderer's visual rows, and renders a session's `user.log` — the
//! durable counterpart to the one record log.  The whole alt-screen frame is
//! redrawn each tick, so scrollback is ours, not the host terminal's, and
//! every tab keeps its own scroll position.
//!
//! Two producers feed it, matching `dev/docs/plans/260814_one_seam_one_log.md`:
//! the live bus's `Kind` events (`push_*`, still what production drives today)
//! and [`crate::record::Printer`] (`transient`/`sync`), which draws the one
//! view fold's memo instead.  Both populate the same [`Self::blocks`], so
//! nothing downstream — reflow, the flatten, the gestures — need know which
//! producer is live.  The `push_*` half keeps its own light bookkeeping
//! (`last_call`, `last_ral_cmd`) rather than the tail-walk and resident scan
//! the fold's own commits retire by construction.

use super::block::{AgentSlot, Block, RailShape, Reveal, append_visual_rows};
use super::group;
use super::line::{is_blank, plain, size_bar};
use super::palette::READ_W;
use super::rail::{self, RailKind};
use super::select::plain_slice;
use crate::agent::event::{ContextOp, EditAuthority};
use crate::bus::AgentId;
use crate::bus::card::{
    self, Card, ObservationKind, RailPlace, execs_card, greps_card, observation_card,
    observation_from_wire, rail_place, reads_card,
};
use crate::provider::Usage;
use crate::record::{self, BlockId, Blocks, Printer, Seq, Transient};
use ral_core::types::{Observation, Observed};
use ratatui::text::Line;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Scrollback blocks held in heap; past this they are evicted oldest-first,
/// already durable in the session's record log.
pub(super) const VIEWPORT_MAX_BLOCKS: usize = 500;
/// Rendered-row cap — a second eviction trigger, for the oversized block that
/// blows the row budget long before the block count does.
pub(super) const VIEWPORT_MAX_ROWS: usize = 20_000;

/// The highest [`record::Seq`] the viewport had seen when a chrome row was
/// authored — `None` for a row drawn before any record, the banner.  A chrome
/// row is named by log position, not by arrival, so a rebuild can place it
/// again rather than lose it.
type Anchor = Option<Seq>;

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
    /// Assistant text since the last commit.  It never streams as prose: only
    /// its magnitude shows ([`Self::streaming_seat`]) until the turn's end
    /// commits it as a [`Block::markdown`].  Reasoning has no such seat: each
    /// phase streams into its own live `∴` block from the first delta
    /// ([`Self::push_thinking`]).
    open: String,
    /// Top visible visual row, per-viewport so each tab keeps its place.
    offset: usize,
    /// Follow the tail.  Cleared by a scroll either way, re-armed at the bottom.
    sticky: bool,
    flat: Flat,
    log_path: PathBuf,
    /// Total, never absent: the status line always has a state to name.
    state: StateSpan,
    /// Kit-authored *state*: a `key → Card` register drawn as the right-hand
    /// column.  Never logged, and wiped by [`Self::reset`] so a pin is
    /// generation-bounded like the scrollback.  A `Vec`, not a map, so render
    /// order is first-seen insertion order.
    pins: Vec<(String, Card)>,
    /// Index of the most recently pushed call block — what a landing result
    /// attaches its size to.  A card may land between a call and its result,
    /// so this is an index, not "the last block."  `O(1)`, replacing the
    /// tail-walk `set_result_size` used before the fold could address a
    /// result at its call's own [`BlockId`].
    last_call: Option<usize>,
    /// The most recent `ral` script run, for [`Self::commit_fidelity`]'s echo
    /// signal — an `O(1)` field, replacing a reverse scan of resident blocks.
    last_ral_cmd: Option<String>,
    /// Persisted dial state for blocks a [`Self::sync`] rebuilds fresh from
    /// the fold's memo every call — [`Block`]'s own dial state does not
    /// survive a rebuild, so a printer keeps it here, keyed by the commit
    /// that produced the block.
    reveal: HashMap<BlockId, Reveal>,
    /// The model's context window, for the fidelity a synced [`Block::markdown`]
    /// stamps — set by `App::update_live_model`, since the fold's own memo
    /// carries usage but not the provider's cap.
    context_window: Option<u64>,
    /// The [`record::Seq`] of the fold's rows [`Self::sync`] has already
    /// drained `open` against — `Seq::new(0)` before the first commit.  A
    /// commit is a prefix of the accumulated stream by construction, so each
    /// row past this cursor drains that many bytes off `open`'s front — the
    /// printer-side half of "a printer's `open` is always exactly the
    /// unconsumed suffix."  A cursor by identity rather than position, since
    /// a windowed memo makes an index wrong.
    drained_through: Seq,
    /// Chrome rows [`Self::push_chrome`] has authored, named by the
    /// [`Anchor`] they were drawn at.  [`Self::sync`] stable-merges these
    /// into the folded rows it rebuilds; nothing reads this lane yet, since
    /// production still draws chrome straight into [`Self::blocks`].
    chrome: Vec<(Anchor, RailShape, Vec<Line<'static>>)>,
    /// The highest [`record::Seq`] [`Self::sync`] has last seen — the anchor
    /// a chrome row drawn right now would be named by.
    last_seq: Anchor,
}

/// The agent's state, when it was entered, and the model text that has arrived
/// since — the status line's whole datum, so one transition resets all three.
/// The instant anchors the elapsed-wait bar to that transition rather than to
/// the last event of any kind, which is what makes a silent stream legible.
#[derive(Clone, Copy)]
pub(super) struct StateSpan {
    pub(super) state: crate::bus::AgentState,
    since: Instant,
    /// Characters of model text arrived in this state.  A count that stops
    /// growing under a growing [`Self::elapsed`] is a stalled stream.
    pub(super) streamed: usize,
}

impl StateSpan {
    pub(super) fn new(state: crate::bus::AgentState) -> Self {
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
/// count is already computed, and the [`BlockId`] a [`Viewport::sync`] built
/// it from — `None` for a block the live `push_*` half authored, which has no
/// commit of its own to be named by.
struct Entry {
    block: Block,
    rows: usize,
    id: Option<BlockId>,
}

/// Memoised whole-buffer flatten: block lines wrapped to `width`, with
/// `row_block[i]` naming the block row `i` came from.
#[derive(Default)]
struct Flat {
    width: u16,
    rows: Vec<Line<'static>>,
    row_block: Vec<usize>,
    dirty: bool,
}

/// Whether `block` opens its own rail run.  A run of markdown blocks is one
/// response and only its head wears the `·`.
fn opens_rail_run(prev: Option<&Block>, block: &Block) -> bool {
    !(block.markdown_src().is_some() && prev.is_some_and(|p| p.markdown_src().is_some()))
}

/// Open the session's rendered-text log, always fresh: `user.log` is a
/// regenerable render of the fold's commits, never a file patched in place,
/// so there is no append mode left to preserve a resumed prefix into. Falls
/// back to a discarding sink, so a log-path failure never disables the
/// viewport.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:viewport-log] opens the viewport's rendered-text log; render dump infra, not turn-time data I/O"
)]
fn open_log(path: &Path) -> io::BufWriter<Box<dyn io::Write + Send>> {
    if let Some(parent) = path.parent() {
        let _ = fs::create_dir_all(parent);
    }
    let sink: Box<dyn io::Write + Send> = match std::fs::OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(path)
    {
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
    /// `append` is accepted for signature compatibility with `Tabs::new`'s
    /// resume plumbing, but no longer changes how `user.log` opens: a
    /// regenerated render has no "preserved prefix" to protect, and seeding a
    /// resumed session's scrollback is `dev/docs/plans/260814_one_seam_one_log.md`'s
    /// own step 7 (parcel P6), not yet wired.
    pub(super) fn new(log_path: PathBuf, agent: AgentSlot, _append: bool) -> Self {
        Self {
            blocks: Vec::new(),
            tombstone: None,
            agent,
            usage: Usage::default(),
            open: String::new(),
            offset: 0,
            sticky: true,
            flat: Flat::default(),
            log_path,
            state: StateSpan::new(crate::bus::AgentState::Ready),
            pins: Vec::new(),
            last_call: None,
            last_ral_cmd: None,
            reveal: HashMap::new(),
            context_window: None,
            drained_through: Seq::new(0),
            chrome: Vec::new(),
            last_seq: None,
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

    /// The model's context window, read by [`Self::sync`]'s fidelity
    /// recomputation.  Set once per focus or model change, beside
    /// `App::update_live_model`.
    pub(super) fn set_context_window(&mut self, window: Option<u64>) {
        self.context_window = window;
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
    pub(super) fn set_state(&mut self, state: crate::bus::AgentState) {
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
        let log_path = self.log_path.clone();
        let agent = self.agent;
        let context_window = self.context_window;
        *self = Self::new(log_path, agent, false);
        self.context_window = context_window;
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
        self.flat = Flat::default();
        self.pins = Vec::new();
        self.chrome = Vec::new();
    }

    pub(super) fn tombstone(&self) -> Option<&Tombstone> {
        self.tombstone.as_ref()
    }

    /// Final flush at session end: regenerate `user.log` from the resident
    /// blocks and flush it.  The caller owns the I/O error policy.
    pub(super) fn flush_log(&self) -> io::Result<&Path> {
        self.regenerate_log()?;
        Ok(&self.log_path)
    }

    /// Rewrite `user.log` whole from the resident blocks — the regenerable
    /// render `dev/docs/plans/260814_one_seam_one_log.md` asks for, never a
    /// file patched incrementally.  This is also the bug fix step 6 names: the
    /// old incremental tee truncated to a resumed prefix and then, past
    /// eviction, silently deleted this session's own evicted transcript from
    /// disk. A render of whatever is resident right now has no such tail to lose.
    fn regenerate_log(&self) -> io::Result<()> {
        let mut log = open_log(&self.log_path);
        let mut prev_blank = true;
        for (i, entry) in self.blocks.iter().enumerate() {
            let lead = opens_rail_run(
                i.checked_sub(1).map(|j| &self.blocks[j].block),
                &entry.block,
            );
            for line in entry.block.log_lines(self.agent, lead) {
                if is_blank(&line) {
                    if prev_blank {
                        continue;
                    }
                    prev_blank = true;
                } else {
                    prev_blank = false;
                }
                for s in &line.spans {
                    log.write_all(s.content.as_bytes())?;
                }
                log.write_all(b"\n")?;
            }
        }
        log.flush()
    }

    // ── content (the live `push_*` half; still what production drives) ─────

    /// Append a tool call as its own collapsible block.  `context` is the turn's
    /// degradation floor, so the intent line drains under context pressure.
    pub(super) fn push_tool_call(
        &mut self,
        tool: &'static str,
        summary: String,
        cmd: String,
        context: u8,
    ) {
        if tool == "ral" {
            self.last_ral_cmd = Some(cmd.clone());
        }
        self.push_block(Block::tool_call(tool, summary, cmd, context));
        self.last_call = Some(self.blocks.len() - 1);
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
        fidelity: super::fidelity::Fidelity,
    ) {
        self.push_block(Block::subagent(name, text, error, elapsed, fidelity));
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

    /// Append pre-rendered chrome; `shape` lets the rail dispatch on the
    /// sub-kind.  Also records the row into the chrome lane, anchored at the
    /// last `Seq` [`Self::sync`] saw — the door [`Self::sync`]'s stable merge
    /// draws from once the fold, not `push_*`, drives the live screen.
    pub(super) fn push_chrome(&mut self, shape: RailShape, lines: Vec<Line<'static>>) {
        self.chrome.push((self.last_seq, shape, lines.clone()));
        self.push_block(Block::chrome(shape, lines));
    }

    /// Append a summary-less tool call, shown standalone as a `▸` rail block.
    /// `detail` is `None` for an invisible parse-failure boundary.
    pub(super) fn push_plain_call(&mut self, detail: Option<String>) {
        self.push_block(Block::plain_call(detail));
        self.last_call = Some(self.blocks.len() - 1);
    }

    /// Attach a tool result's line count to the most recently pushed call —
    /// `O(1)` off [`Self::last_call`], a card may still land between a call and
    /// its result without breaking the correlation.
    pub(super) fn set_result_size(&mut self, text: &str) {
        #[allow(
            clippy::cast_possible_truncation,
            reason = "content count; u32 headroom far exceeds any in-memory transcript"
        )]
        let n = text.lines().count() as u32;
        if let Some(entry) = self.last_call.and_then(|i| self.blocks.get_mut(i))
            && entry.block.is_tool_call()
        {
            entry.block.set_result_size(n);
            self.flat.dirty = true;
        }
    }

    /// Land a phase's authoritative reasoning: into its live `∴` block
    /// ([`Self::live_thinking`]), superseding the streamed deltas, or — when
    /// nothing streamed — into a fresh block before the trailing markdown run.
    /// `answer_chars` is the say-side of the deliberation ratio.
    pub(super) fn commit_thinking(&mut self, text: String, answer_chars: u32) {
        if text.trim().is_empty() {
            return;
        }
        if let Some(idx) = self.live_thinking() {
            self.blocks[idx].block.commit_thinking(text, answer_chars);
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
                        id: None,
                    },
                );
                self.flat.dirty = true;
            }
            None => self.push_block(Block::thinking(text, answer_chars)),
        }
    }

    /// Stream a live reasoning delta into the phase's `∴` block, seating one at
    /// the first delta.  The block arrives open, its trace growing as the
    /// deltas land; a dial shuts it to its header.
    pub(super) fn push_thinking(&mut self, text: &str) {
        if let Some(idx) = self.live_thinking() {
            self.blocks[idx].block.push_provisional_thinking(text);
            self.flat.dirty = true;
        } else {
            self.push_block(Block::thinking_live(text.to_string()));
        }
    }

    /// The still-streaming phase's block.  Each phase seats its own, so this is
    /// simply the newest with no authoritative text yet — at most one exists.
    fn live_thinking(&self) -> Option<usize> {
        self.blocks.iter().rposition(|e| e.block.is_live_thinking())
    }

    /// Buffer streamed text.  Chopping the delta stream into fence-safe
    /// paragraphs is the commit producer's job now (`record::commit`'s
    /// chopper); the live `push_*` half only ever sees the whole answer land
    /// at once, on [`Self::close_boundary`], so `open` grows unchopped until then.
    pub(super) fn push_token(&mut self, text: &str) {
        self.open.push_str(text);
    }

    /// End a streaming step: seal a still-live reasoning phase — its deltas
    /// stand as the text when no commit arrived — then commit `open`.
    pub(super) fn close_boundary(&mut self, context_floor: u8) {
        if let Some(idx) = self.live_thinking() {
            if !self.blocks[idx].block.seal_thinking() {
                self.blocks.remove(idx);
            }
            self.flat.dirty = true;
        }
        self.flush_open(context_floor);
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
    /// modifier against the most recent `ral` script — an `O(1)` field, not a
    /// scan of resident blocks.
    fn commit_fidelity(&self, text: &str, context_floor: u8) -> super::fidelity::Fidelity {
        let echo = self
            .last_ral_cmd
            .as_deref()
            .map_or(0, |cmd| super::fidelity::echo_delta(text, cmd));
        super::fidelity::Fidelity {
            context: context_floor,
            echo,
        }
    }

    fn push_block(&mut self, block: Block) {
        let rows = Self::estimate_rows(&block, self.agent);
        self.blocks.push(Entry {
            block,
            rows,
            id: None,
        });
        self.flat.dirty = true;
        self.enforce_window_caps();
    }

    /// The row count [`Self::enforce_window_caps`] budgets against — a
    /// fixed-width render, an estimate rather than the screen's own width, so
    /// pushing a block never depends on the terminal's current size.
    fn estimate_rows(block: &Block, agent: AgentSlot) -> usize {
        block.log_lines(agent, true).len()
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
            self.last_call = self.last_call.and_then(|i| i.checked_sub(drop));
            self.flat.dirty = true;
        }
    }

    // ── interaction ──────────────────────────────────────────────────────

    /// The block owning visual row `row` — valid only against the most recent
    /// [`Self::render_window`].
    pub(super) fn block_at(&self, row: usize) -> Option<usize> {
        self.flat.row_block.get(row).copied()
    }

    /// Visual row to index in `flat.rows`, or `None` past the end.
    fn flat_row(&self, row: usize) -> Option<usize> {
        (row < self.flat.rows.len()).then_some(row)
    }

    /// Rendered cell width of visual row `row` — its content's extent, not the
    /// pane's, so a gesture binds tight to the text and ignores the dead margin.
    pub(super) fn row_width(&self, row: usize) -> Option<usize> {
        self.flat.rows.get(row).map(Line::width)
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
    /// level moved, and — for a block a fold commit named — remembering the
    /// new rung in [`Self::reveal`] so the next [`Self::sync`] restores it.
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
            if let Some(id) = entry.id {
                self.reveal.insert(id, block.level());
            }
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
        let mut seat: Vec<Line<'static>> = self.streaming_seat().into_iter().collect();
        let committed = self.flat.rows.len();
        // Following a non-markdown block, the streaming seat opens a fresh run,
        // so it wears the blank separator a committing lead paragraph would.
        if !seat.is_empty() && self.trailing_markdown_start().is_none() && committed > 0 {
            seat.insert(0, Line::default());
        }
        let total = committed + seat.len();
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
        let lines: Vec<Line<'static>> = self
            .flat
            .rows
            .iter()
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
            // A run is opened by its call and by nothing else: an effect card is
            // buffered until its call has landed ([`super::surface`]), so one
            // reaching the projection with no call behind it belongs to no run
            // and renders alone, through the ordinary path below.
            let (anchor, lines, prompt) = if self.blocks[i].block.is_tool_call() {
                let end = self.observation_run_end(i);
                let segment = (i, self.render_group(i, end, content_w), false);
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
    /// [`group::Call`] per tool call, the body at the run's disclosure level —
    /// its opening call's — and the rail — triangle, hue, aggregate magnitude —
    /// on row one.  A run opens with a call, so its body is never empty.
    fn render_group(&self, start: usize, end: usize, width: u16) -> Vec<Line<'static>> {
        let level = self.blocks[start].block.level();
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

    /// The run's calls in arrival order: each tool call opens a [`group::Call`],
    /// and the observation cards up to the next call are its effects — rendered
    /// rows plus a [`group::Tally`] folded by `|>` kind.  A run opens with its
    /// call, so no effect precedes one and none is dropped.
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

// ── `record::Printer`: the view fold's second consumer ─────────────────────
//
// `transient` and `sync` are additive to the `push_*` half above, not yet the
// live path a running session drives — `dev/docs/plans/260814_one_seam_one_log.md`'s
// R1 owns the seam wiring that would hand a real `Blocks` value to `sync`.
// Both are exercised directly in this module's tests, per the parcel's own
// suggested strategy: drive `record::View::step` over synthetic records and
// assert on the rebuilt scrollback, no live producer required.

impl Printer for Viewport {
    /// A transient never authors scrollback: [`Transient::Token`] and
    /// [`Transient::Thinking`] only grow the raw streams the seat and the
    /// live `∴` block draw their *magnitude* from ([`Self::streaming_seat`],
    /// [`Self::push_thinking`]).  The committed text those deltas become
    /// arrives, chopped, through [`Self::sync`] instead — a printer never
    /// mints a [`record::Block`] of its own — and [`Self::sync`] drains the
    /// matching prefix back off `open` once it sees the commit land.
    fn transient(&mut self, t: &Transient) {
        match t {
            Transient::Token(text) => {
                self.note_streamed(text.chars().count());
                self.open.push_str(text);
            }
            Transient::Thinking(text) => {
                self.note_streamed(text.chars().count());
                self.push_thinking(text);
            }
            Transient::State(state) => self.set_state(*state),
            Transient::Cleared => self.reset(),
            // The producer's own tail flush lands as a commit `sync` will
            // see; nothing to do here but wait for it.
            // The register and the chrome lane both gain a live producer in a
            // later wave; for now these publish with nothing yet drawing them.
            Transient::Boundary
            | Transient::Born { .. }
            | Transient::Died
            | Transient::StopReason(_)
            | Transient::Resources { .. }
            | Transient::Pin { .. }
            | Transient::Unpin { .. }
            | Transient::Fault { .. } => {}
        }
    }

    fn sync(&mut self, blocks: &Blocks) {
        let rows = blocks.rows();
        // `open` is always exactly the unconsumed suffix of the raw stream:
        // drain it against every `Answer` commit not yet accounted for,
        // named by `Seq` rather than position, since the memo is windowed.
        for row in rows.iter().filter(|r| r.id().seq() > self.drained_through) {
            if let record::BlockKind::Answer { text } = row.kind() {
                let n = text.len().min(self.open.len());
                let _ = self.open.drain(..n);
            }
        }
        if let Some(last) = rows.last() {
            self.drained_through = last.id().seq();
        }

        let mut built: Vec<Entry> = Vec::with_capacity(rows.len());
        let mut last_ral_cmd: Option<&str> = None;
        for row in rows {
            let id = row.id();
            for mut block in self.render_block(row.kind(), blocks, &mut last_ral_cmd) {
                if let Some(level) = self.reveal.get(&id) {
                    block.set_reveal(*level);
                }
                let rows = Self::estimate_rows(&block, self.agent);
                built.push(Entry {
                    block,
                    rows,
                    id: Some(id),
                });
            }
        }
        self.last_seq = rows.last().map(|r| r.id().seq());
        self.blocks = self.merge_chrome(built);
        self.last_call = self.blocks.iter().rposition(|e| e.block.is_call());
        self.flat.dirty = true;
    }
}

impl Viewport {
    /// Stable-merge the chrome lane into `built` by [`Anchor`], arrival
    /// order breaking ties, dropping any chrome whose anchor fell out of the
    /// fold's window along with the row it named — the fix that lets a live
    /// `sync` redraw chrome instead of erasing it.
    fn merge_chrome(&mut self, built: Vec<Entry>) -> Vec<Entry> {
        let floor = built.first().and_then(|e| e.id).map(BlockId::seq);
        self.chrome.retain(|(anchor, ..)| match (anchor, floor) {
            (_, None) => true,
            (Some(a), Some(f)) => *a >= f,
            (None, Some(f)) => f.get() <= 1,
        });

        let mut merged = Vec::with_capacity(built.len() + self.chrome.len());
        let mut chrome = self.chrome.iter();
        let mut next = chrome.next();
        while let Some((None, shape, lines)) = next {
            merged.push(Self::chrome_entry(self.agent, *shape, lines.clone()));
            next = chrome.next();
        }
        for entry in built {
            let seq = entry.id.map(BlockId::seq);
            merged.push(entry);
            while let Some((anchor, shape, lines)) = next
                && *anchor == seq
            {
                merged.push(Self::chrome_entry(self.agent, *shape, lines.clone()));
                next = chrome.next();
            }
        }
        while let Some((_, shape, lines)) = next {
            merged.push(Self::chrome_entry(self.agent, *shape, lines.clone()));
            next = chrome.next();
        }
        merged
    }

    fn chrome_entry(agent: AgentSlot, shape: RailShape, lines: Vec<Line<'static>>) -> Entry {
        let block = Block::chrome(shape, lines);
        let rows = Self::estimate_rows(&block, agent);
        Entry {
            block,
            rows,
            id: None,
        }
    }
}

impl Viewport {
    /// One fold commit, rendered into zero, one, or several [`Block`]s — an
    /// [`record::BlockKind::ObservationGroup`] explodes into one card per
    /// bucket, everything else is exactly one block.  `last_ral_cmd` threads
    /// through the caller's window scan, so an [`record::BlockKind::Answer`]'s
    /// echo signal can see the most recent `ral` script without a second pass.
    fn render_block<'a>(
        &self,
        kind: &'a record::BlockKind,
        blocks: &Blocks,
        last_ral_cmd: &mut Option<&'a str>,
    ) -> Vec<Block> {
        use record::BlockKind as K;
        match kind {
            K::Thinking { text, answer_chars } => {
                vec![Block::thinking(text.clone(), *answer_chars)]
            }
            K::Prompt { text } => vec![Block::chrome(
                RailShape::Prompt,
                super::line::user_prompt(text),
            )],
            K::Answer { text } => {
                let echo = last_ral_cmd.map_or(0, |cmd| super::fidelity::echo_delta(text, cmd));
                let fidelity = super::fidelity::Fidelity {
                    context: super::fidelity::context_floor(
                        blocks.input_tokens(),
                        self.context_window,
                    ),
                    echo,
                };
                vec![Block::markdown(text.clone(), fidelity)]
            }
            K::ToolCall {
                tool,
                cmd,
                summary,
                result_lines,
            } => {
                if tool == "ral" {
                    *last_ral_cmd = Some(cmd.as_str());
                }
                let context =
                    super::fidelity::context_floor(blocks.input_tokens(), self.context_window);
                let mut block = match summary {
                    Some(s) => Block::tool_call(tool.clone(), s.clone(), cmd.clone(), context),
                    None => Block::plain_call(
                        (cmd != crate::shell_eval::tools::ral::INVALID_INPUT).then(|| cmd.clone()),
                    ),
                };
                if let Some(n) = result_lines {
                    block.set_result_size(*n);
                }
                vec![block]
            }
            K::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            } => vec![Block::act(
                verb.clone(),
                subject.clone(),
                payload.clone(),
                *failed,
            )],
            K::SubagentDone {
                name,
                text,
                error,
                elapsed_ms,
            } => {
                let fidelity = super::fidelity::Fidelity {
                    context: super::fidelity::context_floor(
                        blocks.input_tokens(),
                        self.context_window,
                    ),
                    echo: 0,
                };
                vec![Block::subagent(
                    name.clone(),
                    text.clone(),
                    error.clone(),
                    Duration::from_millis(*elapsed_ms),
                    fidelity,
                )]
            }
            K::Observation { value } => render_observation(value.clone()),
            K::ObservationGroup { values } => render_observation_group(values),
            K::Card { marks } => match serde_json::from_value::<Card>(marks.clone()) {
                Ok(card) => vec![Block::card(card)],
                Err(_) => Vec::new(),
            },
            K::Done { outcome } => {
                vec![Block::card(card::done_card(&card::to_card_done(outcome)))]
            }
            K::Notice { notice } => {
                vec![Block::card(card::notice_card(&card::to_card_notice(
                    notice,
                )))]
            }
            K::Context { rows } => vec![Block::card(card::context_rows_card(rows))],
            K::Cancelled => vec![Block::chrome(
                RailShape::Plain,
                super::line::note("cancelled"),
            )],
            K::Error { text } => vec![Block::chrome(RailShape::Error, super::line::error(text))],
            K::Nudge { used, max, cause } => vec![Block::chrome(
                RailShape::Plain,
                super::line::note(&format!("nudge {used}/{max}: {cause}")),
            )],
            K::ProviderError { error } => {
                vec![Block::chrome(
                    RailShape::Error,
                    super::line::provider_error(error),
                )]
            }
            K::Stalled { error } => {
                vec![Block::chrome(RailShape::Error, super::line::stalled(error))]
            }
            K::SystemNote { text } => {
                vec![Block::chrome(RailShape::Plain, super::line::note(text))]
            }
            K::HarnessResult { .. } => Vec::new(),
            K::ModelChanged { model, provider } => vec![Block::chrome(
                RailShape::Plain,
                super::line::note(&format!("model changed: {provider}/{model}")),
            )],
            K::Step { n } => vec![Block::chrome(
                RailShape::Step,
                super::line::step(*n as usize),
            )],
            K::ContextEdited { op, by } => {
                let authority = match by {
                    EditAuthority::Model => "model",
                    EditAuthority::User => "user",
                    EditAuthority::Harness => "harness",
                };
                let text = match op {
                    ContextOp::Fold {
                        through_exchange, ..
                    } => format!(
                        "[context folded through exchange {through_exchange} ({authority})]"
                    ),
                    ContextOp::Drop { exchanges } => {
                        let list = exchanges
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("[context dropped exchange(s) {list} ({authority})]")
                    }
                };
                vec![Block::chrome(RailShape::Plain, super::line::note(&text))]
            }
        }
    }
}

/// Rebuild one [`Display::Observation`](record::Display::Observation)'s card,
/// through the same [`observation_card`]/[`rail_place`] the live rail draws
/// from — a rendering, never recorded, rebuilt fresh at sync time.
fn render_observation(value: ral_core::serial::FOValue) -> Vec<Block> {
    let Some(obs) = observation_from_wire(value) else {
        return Vec::new();
    };
    render_observed(&obs.what)
}

fn render_observed(what: &Observed) -> Vec<Block> {
    let Some(place) = rail_place(what) else {
        return Vec::new();
    };
    let card = observation_card(what);
    match place {
        RailPlace::Grouped(kind) => vec![Block::observation_card(card, kind, 1)],
        RailPlace::Barrier => vec![Block::write_card(card)],
        RailPlace::Standalone => vec![Block::card(card)],
    }
}

/// Rebuild a [`Display::ObservationGroup`](record::Display::ObservationGroup)'s
/// members, decoded and re-bucketed exactly as `record/commit.rs`'s buffer
/// grouped them at record time — reads, execs, and greps comma-joined under
/// [`reads_card`]/[`execs_card`]/[`greps_card`], each write its own barrier.
fn render_observation_group(values: &[ral_core::serial::FOValue]) -> Vec<Block> {
    let mut reads: Vec<String> = Vec::new();
    let mut execs: Vec<Observed> = Vec::new();
    let mut greps: Vec<Observed> = Vec::new();
    let mut out: Vec<Block> = Vec::new();
    for value in values {
        let Some(Observation { what, .. }) = observation_from_wire(value.clone()) else {
            continue;
        };
        match what {
            Observed::Read { path } => reads.push(path),
            Observed::Command {
                origin:
                    ral_core::types::CommandOrigin::External | ral_core::types::CommandOrigin::Detached,
                ..
            } => execs.push(what),
            Observed::Grep { .. } => greps.push(what),
            other => out.extend(render_observed(&other)),
        }
    }
    #[allow(
        clippy::cast_possible_truncation,
        reason = "one flush's observation count; u32 headroom far exceeds any real burst"
    )]
    {
        if let Some(card) = reads_card(&reads) {
            out.insert(
                0,
                Block::observation_card(card, ObservationKind::Read, reads.len() as u32),
            );
        }
        if let Some(card) = execs_card(&execs) {
            out.insert(
                0,
                Block::observation_card(card, ObservationKind::Exec, execs.len() as u32),
            );
        }
        if let Some(card) = greps_card(&greps) {
            out.insert(
                0,
                Block::observation_card(card, ObservationKind::Grep, greps.len() as u32),
            );
        }
    }
    out
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
        Viewport::new(path, AgentSlot(0), false)
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
        vp.push_token("```ral\nlet x = 1\nlet y = 2\n");
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
    fn live_thinking_streams_its_trace_before_the_answer() {
        let mut vp = viewport();
        vp.push_thinking("considering the shape\n");
        vp.push_token("First paragraph.\n\nSecond paragraph still streaming");

        let w = vp.render_window(READ_W, 24);
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(
            all.contains("considering the shape"),
            "a live trace streams in the open: {all:?}"
        );
        assert!(
            !all.contains("First paragraph."),
            "the answer stays buffered until the boundary commits it now: {all:?}"
        );

        let thinking = rail_rows(&w.lines, "∴ ");
        assert!(!thinking.is_empty(), "live thinking has its own rail");

        vp.close_boundary(0);
        let w = vp.render_window(READ_W, 24);
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(
            all.contains("First paragraph.") && all.contains("Second paragraph"),
            "the whole answer lands as one block at the boundary: {all:?}"
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
        vp.push_token("First paragraph.\n\nSecond paragraph.");
        let live = vp.render_window(READ_W, 8);
        let live_text = live.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        let live_thinking = rail_rows(&live.lines, "∴ ");
        assert!(
            !live_thinking.is_empty(),
            "live thinking has its rail: {live_text:?}"
        );
        vp.commit_thinking("considering the shape\n".into(), 30);
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

    /// A write ends a run; it does not cut one in half.  Its call's effects
    /// reach the projection ahead of it, so the run folds them whatever its
    /// rung — and at the census, where a run is only its tally, the read is
    /// counted rather than silently dropped.
    #[test]
    fn a_write_closes_a_run_and_the_census_counts_what_it_closed() {
        use crate::bus::card::{Mark, ObservationKind, Span, reads_card};
        let mut vp = viewport();
        vp.push_tool_call(
            "ral",
            "write and read back".into(),
            "'hi' |> 'f'; read 'g'".into(),
            0,
        );
        vp.push_observation_card(
            reads_card(&["g".to_string()]).expect("a read card"),
            ObservationKind::Read,
            1,
        );
        vp.push_write_card(Card(vec![Mark::Text {
            spans: vec![Span::plain("write f committed")],
        }]));

        let rows = |vp: &mut Viewport| -> Vec<String> {
            vp.render_window(READ_W, 40)
                .lines
                .iter()
                .map(plain)
                .collect()
        };
        let all = rows(&mut vp).join("\n");
        assert!(
            all.contains("read g"),
            "the call's read is folded under it, not orphaned: {all:?}"
        );

        let w = vp.render_window(READ_W, 40);
        let run = rail_rows(&w.lines, "▸ ");
        let barrier = rail_rows(&w.lines, "▎ ");
        assert_eq!(run.len(), 1, "one run, opened by the one call");
        assert_eq!(barrier.len(), 1, "the write wears its own `▎`");
        assert!(
            run[0] < barrier[0],
            "and closes the run rather than splitting it"
        );

        // Dialled to its floor the run is one line — and that line must account
        // for the read, which is the whole point of a tally.
        assert!(vp.dial_block(0, -1), "a run dials down to its census");
        let census = rows(&mut vp)
            .into_iter()
            .find(|r| r.contains("Ran "))
            .expect("a census line");
        assert!(
            census.contains("Ran 1 script, read 1 file."),
            "the census counts the read the write closed over: {census:?}"
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

    // ── `record::Printer` ───────────────────────────────────────────────────

    use crate::record::{Display, Fold, Record, Recorded, Refusal, Seq, Stamp, View};

    fn step(memo: &mut Blocks, records: impl IntoIterator<Item = Record>) -> Result<(), Refusal> {
        for (i, r) in records.into_iter().enumerate() {
            #[allow(clippy::cast_possible_truncation, reason = "test record count")]
            let stamp = Stamp::new(Seq::new(i as u64), 0..0);
            View::step(memo, &Recorded::new(stamp, r))?;
        }
        Ok(())
    }

    /// The printer never mints a [`record::Block`] itself: [`Printer::sync`]
    /// only ever rebuilds from what the fold already committed.
    #[test]
    fn sync_rebuilds_scrollback_from_the_fold_alone() {
        let mut memo = Blocks::default();
        step(
            &mut memo,
            [
                Record::Display(Display::Prompt {
                    text: "hello".into(),
                }),
                Record::Display(Display::Answer {
                    text: "hi back".into(),
                }),
            ],
        )
        .expect("a display-only fold never refuses");

        let mut vp = viewport();
        vp.sync(&memo);
        let all = vp
            .render_window(READ_W, 40)
            .lines
            .iter()
            .map(plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("hello") && all.contains("hi back"), "{all:?}");
    }

    /// A dial applied after one `sync` survives the next — the whole point of
    /// keeping reveal state in a side table keyed by `BlockId` rather than
    /// inside the rebuilt `Block`.
    #[test]
    fn dial_state_survives_a_resync() {
        let mut memo = Blocks::default();
        step(
            &mut memo,
            [Record::Display(Display::ToolCall {
                tool: "ral".into(),
                cmd: "read 'x'".into(),
                summary: Some("look at x".into()),
            })],
        )
        .expect("a display-only fold never refuses");

        let mut vp = viewport();
        vp.sync(&memo);
        assert!(vp.dial_block(0, 1), "a tool call dials open");
        let opened = vp.blocks[0].block.level();

        vp.sync(&memo);
        assert_eq!(
            vp.blocks[0].block.level(),
            opened,
            "the rebuilt block keeps the dial a prior sync set"
        );
    }

    /// A running counter's worth of `View::step`, so a test can interleave
    /// `push_chrome` between commits without two calls to [`step`] colliding
    /// on the same `Seq`.
    fn advance(memo: &mut Blocks, seq: &mut u64, record: Record) {
        *seq += 1;
        let stamp = Stamp::new(Seq::new(*seq), 0..0);
        View::step(memo, &Recorded::new(stamp, record)).expect("a display-only fold never refuses");
    }

    /// A chrome row drawn between two commits keeps its place across a
    /// `sync` that rebuilds the folded lane from scratch — the whole point
    /// of naming it by [`Anchor`] rather than by where `push_chrome` happened
    /// to land it in `blocks`.
    #[test]
    fn chrome_holds_its_place_across_a_resync() {
        let mut memo = Blocks::default();
        let mut seq = 0u64;
        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Prompt { text: "one".into() }),
        );

        let mut vp = viewport();
        vp.sync(&memo);
        vp.push_chrome(RailShape::Plain, vec![Line::from("between")]);

        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Prompt { text: "two".into() }),
        );
        vp.sync(&memo);

        let rendered = vp
            .render_window(READ_W, 40)
            .lines
            .iter()
            .map(plain)
            .collect::<Vec<_>>();
        let one = rendered.iter().position(|l| l.contains("one")).unwrap();
        let between = rendered.iter().position(|l| l.contains("between")).unwrap();
        let two = rendered.iter().position(|l| l.contains("two")).unwrap();
        assert!(
            one < between && between < two,
            "chrome anchored between two commits must render between them: {rendered:?}"
        );
    }

    /// Chrome anchored at a commit the fold has since evicted is dropped
    /// along with it, rather than piling up forever in the chrome lane.
    #[test]
    fn chrome_falls_out_of_the_window_with_its_anchor() {
        let mut memo = Blocks::default();
        let mut seq = 0u64;
        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Prompt {
                text: "first".into(),
            }),
        );

        let mut vp = viewport();
        vp.sync(&memo);
        vp.push_chrome(RailShape::Plain, vec![Line::from("anchored on first")]);
        assert_eq!(vp.chrome.len(), 1);

        // Flushing and pushing past the fold's own window evicts `first`.
        let _ = memo.render_since();
        for i in 0..1100 {
            advance(
                &mut memo,
                &mut seq,
                Record::Display(Display::Prompt {
                    text: format!("filler {i}"),
                }),
            );
        }
        vp.sync(&memo);

        assert!(
            vp.chrome.is_empty(),
            "chrome anchored on an evicted row must be dropped too"
        );
    }
}
