//! Per-session collapsible scrollback.
//!
//! A [`Viewport`] turns one session's content into [`Block`]s, flattens those
//! into the renderer's visual rows, and renders a session's `user.log` — the
//! durable counterpart to the one record log.  The whole alt-screen frame is
//! redrawn each tick, so scrollback is ours, not the host terminal's, and
//! every tab keeps its own scroll position.
//!
//! [`crate::record::Printer`] is the sole producer: [`Self::commit_fact`]
//! steps the fold beside the viewport and re-syncs [`Self::blocks`] wholesale
//! from it, and [`Printer::transient`] draws whatever is live-only —
//! [`Self::push_chrome`], the chrome lane's door, and [`Self::push_thinking`],
//! which carries the open line a reasoning seat draws.

use super::block::{AgentSlot, Block, ChromeKind, Reveal, append_visual_rows};
use super::gesture::Cell;
use super::group;
use super::line::is_blank;
use super::palette::{READ_W, content_w};
use super::rail::{self, RailKind};
use super::row::Row;
use super::select::plain_slice;
use crate::agent::event::{ContextOp, EditAuthority};
use crate::bus::card::{
    self, Card, Landing, ObservationKind, execs_card, greps_card, landing, observation_card,
    observation_from_wire, reads_card,
};
use crate::provider::Usage;
use crate::record::{self, BlockId, Blocks, Fold as _, Printer, Seq, Transient};
use ral_core::types::{Observation, Observed};
use ratatui::text::Line;
use std::collections::HashMap;
use std::fs;
use std::io;
use std::io::{Seek, Write};
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

/// Carry a lane's open line across one delta: the text after the last newline
/// is exactly what no record holds yet, since the worker cuts its records at
/// that same newline.  Block text and open line are complementary by that one
/// rule, so neither side has anything to count.
fn carry(open: &mut String, delta: &str) {
    match delta.rfind('\n') {
        Some(nl) => {
            open.clear();
            open.push_str(&delta[nl + 1..]);
        }
        None => open.push_str(delta),
    }
}

pub(super) struct Viewport {
    /// The session's scrollback, oldest block first.
    blocks: Vec<Entry>,
    /// Set once evicted ([`Self::evict_to_tombstone`]); `blocks` is empty from
    /// that point on, and `false` for a live or still-lingering view.
    tombstoned: bool,
    /// Palette slot stamped onto every block at push; root is `0`.
    agent: AgentSlot,
    /// This session's spend — the matrix's per-agent readout, where
    /// `App::total_usage` is the rule line's sum over all of them.
    usage: Usage,
    /// The answer's open line: assistant text past the last newline, which is
    /// past the last [`Display::Answer`](crate::record::Display::Answer)
    /// record the worker has cut.  It seats below the block it will join
    /// ([`Self::streaming_seat`]).
    answer: String,
    /// The reasoning's open line — `answer`'s twin for the step's own `∴`
    /// seat ([`Self::thinking_seat`]), grown by [`Self::push_thinking`].
    reasoning: String,
    /// The fold's own memo, stepped by every [`Self::commit_fact`] and
    /// re-synced from in the same call — the memo P5 moves to live beside the
    /// viewport, fed only through [`crate::record::View::step`].
    fold: Blocks,
    /// Top visible visual row, per-viewport so each tab keeps its place.
    offset: usize,
    /// Follow the tail.  Cleared by a scroll either way, re-armed at the bottom.
    sticky: bool,
    flat: Flat,
    log_path: PathBuf,
    log: Log,
    /// The highest fold [`Seq`] this viewport's own window has evicted — the
    /// floor a [`Self::sync`] starts at, so a row the screen has let go is
    /// never built a second time to be dropped again.
    evicted_through: Option<Seq>,
    /// The fold revision [`Self::sync`] last built at.  A row whose
    /// [`record::Block::rev`] is no greater still renders as it did, so its
    /// block is carried over whole — line memo and all — and only the rows
    /// the fold has since touched are rendered again.
    synced_rev: u64,
    /// Total, never absent: the status line always has a state to name.
    state: StateSpan,
    /// Kit-authored *state*: a `key → Card` register drawn as the right-hand
    /// column.  Never logged, and wiped by [`Self::reset`] so a pin is
    /// generation-bounded like the scrollback.  A `Vec`, not a map, so render
    /// order is first-seen insertion order.
    pins: Vec<(String, Card)>,
    /// Persisted dial state for blocks a [`Self::sync`] rebuilds fresh from
    /// the fold's memo every call — [`Block`]'s own dial state does not
    /// survive a rebuild, so a printer keeps it here, keyed by the commit
    /// that produced the block.
    reveal: HashMap<BlockId, Reveal>,
    /// The rung a thinking trace is born at — `/thinking`'s standing datum,
    /// which [`Self::set_traces_level`] moves and every later sync and live
    /// seat reads, so a trace still to arrive obeys the setting too.  A
    /// per-block dial outlives it: [`Self::reveal`] is consulted after.
    traces: Reveal,
    /// The model's context window, for the fidelity a synced [`Block::markdown`]
    /// stamps — set by `App::update_live_model`, since the fold's own memo
    /// carries usage but not the provider's cap.
    context_window: Option<u64>,
    /// Chrome rows [`Self::push_chrome`] has authored, named by the
    /// [`Anchor`] they were drawn at.  [`Self::sync`] stable-merges these
    /// into the folded rows it rebuilds.
    chrome: Vec<(Anchor, ChromeKind, Vec<Line<'static>>)>,
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
    pub(super) lines: Vec<Row>,
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
    rows: Vec<Row>,
    row_block: Vec<usize>,
    dirty: bool,
}

/// Whether `block` opens its own rail run.  A run of markdown blocks is one
/// response and only its head wears the `·`.
fn opens_rail_run(prev: Option<&Block>, block: &Block) -> bool {
    !(block.markdown_src().is_some() && prev.is_some_and(|p| p.markdown_src().is_some()))
}

/// The session's rendered transcript, `user.log`.
///
/// A block is written once, when it leaves the viewport's window ([`Self::retire`]),
/// into a prefix that only ever grows — so the file keeps the whole session
/// however long it runs, while the viewport keeps only what fits on screen.
/// The blocks still resident are written past that prefix on demand
/// ([`Self::flush`]) and rewound by the next retirement, which is what lets
/// `/export` read a whole transcript mid-session without the tail being
/// written twice.
///
/// A path that will not open leaves [`Self::file`] `None` and the transcript
/// silently unrecorded, so a log failure never disables the viewport.
struct Log {
    file: Option<fs::File>,
    /// Bytes of retired blocks; everything past this is provisional.
    durable: u64,
    /// Whether the retired prefix ends blank, so a block joining it collapses
    /// its leading blanks exactly as the flatten does.
    prev_blank: bool,
    /// Whether the last retired block was prose — what decides whether the
    /// next one opens its own rail run ([`opens_rail_run`]).
    prev_md: bool,
    /// Leading scrollback entries this file already holds: a resumed
    /// session's seeded window, which the run that recorded it already wrote.
    seeded: usize,
}

impl Log {
    /// `append` continues a resumed session's transcript; otherwise the file
    /// starts empty, since a fresh session shares none of its history.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:viewport-log] opens the viewport's rendered-text log; render dump infra, not turn-time data I/O"
    )]
    fn open(path: &Path, append: bool) -> Self {
        if let Some(parent) = path.parent() {
            let _ = fs::create_dir_all(parent);
        }
        let file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(!append)
            .open(path)
            .ok();
        let durable = if append {
            file.as_ref()
                .and_then(|f| f.metadata().ok())
                .map_or(0, |m| m.len())
        } else {
            0
        };
        Self {
            file,
            durable,
            prev_blank: true,
            prev_md: false,
            seeded: 0,
        }
    }

    /// Render `entries` at the end of the retired prefix, returning the length
    /// and the continuation state that keeping them would leave behind.
    fn write(&mut self, entries: &[Entry], agent: AgentSlot) -> io::Result<(u64, bool, bool)> {
        let (mut prev_blank, mut prev_md) = (self.prev_blank, self.prev_md);
        let Some(file) = self.file.as_mut() else {
            return Ok((self.durable, prev_blank, prev_md));
        };
        file.set_len(self.durable)?;
        file.seek(io::SeekFrom::Start(self.durable))?;
        {
            let mut out = io::BufWriter::new(&mut *file);
            for entry in entries {
                let md = entry.block.markdown_src().is_some();
                for row in entry.block.log_lines(agent, !(md && prev_md)) {
                    let line = row.into_line();
                    if is_blank(&line) {
                        if prev_blank {
                            continue;
                        }
                        prev_blank = true;
                    } else {
                        prev_blank = false;
                    }
                    for s in &line.spans {
                        out.write_all(s.content.as_bytes())?;
                    }
                    out.write_all(b"\n")?;
                }
                prev_md = md;
            }
            out.flush()?;
        }
        Ok((file.stream_position()?, prev_blank, prev_md))
    }

    /// Write `entries` and keep them: they have left the window and no later
    /// write may rewind over them.  A transcript that will not write is
    /// abandoned rather than retried, so a failing disk costs one attempt.
    fn retire(&mut self, entries: &[Entry], agent: AgentSlot) {
        match self.write(entries, agent) {
            Ok((durable, prev_blank, prev_md)) => {
                self.durable = durable;
                self.prev_blank = prev_blank;
                self.prev_md = prev_md;
            }
            Err(_) => self.file = None,
        }
    }

    /// Write `entries` provisionally, so the file reads whole right now.
    ///
    /// # Errors
    /// Returns the write's own error, which `/export` reports.
    fn flush(&mut self, entries: &[Entry], agent: AgentSlot) -> io::Result<()> {
        self.write(entries, agent).map(|_| ())
    }
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
    /// `append` continues a resumed session's `user.log` where the run that
    /// recorded it left off; a fresh session starts the file empty.  The
    /// resumed window itself is seeded by [`Self::seed`], which is what keeps
    /// the continuation from repeating what the file already holds.  `traces`
    /// is the standing rung a view is born at — owned by the caller
    /// ([`super::tabs::Tabs`]'s own), so a new view is never told twice, nor
    /// left disagreeing.
    pub(super) fn new(log_path: PathBuf, agent: AgentSlot, append: bool, traces: Reveal) -> Self {
        Self {
            log: Log::open(&log_path, append),
            evicted_through: None,
            synced_rev: 0,
            blocks: Vec::new(),
            tombstoned: false,
            agent,
            usage: Usage::default(),
            answer: String::new(),
            reasoning: String::new(),
            fold: Blocks::default(),
            offset: 0,
            sticky: true,
            flat: Flat::default(),
            log_path,
            state: StateSpan::new(crate::bus::AgentState::Ready),
            pins: Vec::new(),
            reveal: HashMap::new(),
            traces,
            context_window: None,
            chrome: Vec::new(),
            last_seq: None,
        }
    }

    /// Step [`Self::fold`] over one witnessed fact and re-render from it —
    /// the sole way a live commit reaches the screen.  The memo lives beside
    /// the viewport, fed only through [`record::View::step`].
    pub(super) fn commit_fact(&mut self, rec: &record::Recorded<record::Record>) {
        let mut fold = std::mem::take(&mut self.fold);
        // `View::step` never actually refuses a live commit — no arm returns
        // `Err` — so a refusal here would mean the fold learned a new failure
        // mode this printer does not yet know to report.
        record::View::step(&mut fold, rec).expect("the view fold never refuses a live commit");
        self.sync(&fold);
        self.fold = fold;
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
        let bytes: usize = self.flat.rows.iter().map(Row::bytes).sum();
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
        *self = Self::new(log_path, agent, false, self.traces);
        self.context_window = context_window;
    }

    /// Drop this view's heap state once its sub-agent has died and lingered
    /// out.  Idempotent: a second call finds the view already clean.
    ///
    /// The scrollback is retired to `user.log` on the way out — `log_path`
    /// stays put, so that log is readable, and a dead view's blocks are the
    /// last stretch of it nothing else would ever write.
    pub(super) fn evict_to_tombstone(&mut self) {
        if self.tombstoned {
            return;
        }
        self.tombstoned = true;
        let seeded = self.log.seeded.min(self.blocks.len());
        self.log.retire(&self.blocks[seeded..], self.agent);
        self.log.seeded = 0;
        self.blocks = Vec::new();
        self.answer = String::new();
        self.reasoning = String::new();
        self.flat = Flat::default();
        self.pins = Vec::new();
        self.chrome = Vec::new();
    }

    /// Write the resident window past the retired prefix and flush, so
    /// `user.log` reads as the whole session right now — what `/export` copies
    /// and what session end leaves behind.  The caller owns the I/O error policy.
    ///
    /// # Errors
    /// Returns the write's own error.
    pub(super) fn flush_log(&mut self) -> io::Result<&Path> {
        let seeded = self.log.seeded.min(self.blocks.len());
        self.log.flush(&self.blocks[seeded..], self.agent)?;
        Ok(&self.log_path)
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

    /// Append pre-rendered chrome; `shape` lets the rail dispatch on the
    /// sub-kind.  Also records the row into the chrome lane, anchored at the
    /// last `Seq` [`Self::sync`] saw — the door [`Self::sync`]'s stable merge
    /// draws from on every rebuild.
    pub(super) fn push_chrome(&mut self, shape: ChromeKind, lines: Vec<Line<'static>>) {
        self.chrome.push((self.last_seq, shape, lines.clone()));
        self.push_block(Block::chrome(shape, lines));
    }

    /// Stream a live reasoning delta into [`Self::reasoning`] — the open line
    /// [`Self::thinking_seat`] draws, which the delta's own newline retires
    /// as the worker's record of that line lands.
    pub(super) fn push_thinking(&mut self, text: &str) {
        carry(&mut self.reasoning, text);
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
    /// always survives, however oversized.  What leaves the window is retired
    /// to `user.log` on the way out, and the fold row it came from is
    /// remembered so no later sync builds it again.
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
        let mut drop = self.blocks.len() - kept;
        if drop == 0 {
            return;
        }
        // Neither the far half of a fold row that renders as several blocks,
        // nor a chrome row the dropped rows anchored, may be left behind: both
        // would be stranded above a window that can no longer place them, and
        // would leave the transcript by the next sync without ever entering it.
        let last = self.blocks[drop - 1].id;
        let anchored = self.blocks[..drop].iter().any(|e| e.id.is_some());
        while drop + 1 < self.blocks.len() {
            match self.blocks[drop].id {
                Some(id) if Some(id) == last => drop += 1,
                None if anchored => drop += 1,
                _ => break,
            }
        }
        let dropped: Vec<Entry> = self.blocks.drain(..drop).collect();
        if let Some(seq) = dropped.iter().rev().find_map(|e| e.id).map(BlockId::seq) {
            self.evicted_through = Some(seq);
        }
        let seeded = self.log.seeded.min(dropped.len());
        self.log.seeded -= seeded;
        self.log.retire(&dropped[seeded..], self.agent);
        self.flat.dirty = true;
    }

    // ── interaction ──────────────────────────────────────────────────────

    /// The block owning visual row `row` — valid only against the most recent
    /// [`Self::render_window`].
    pub(super) fn block_at(&self, row: usize) -> Option<usize> {
        self.flat.row_block.get(row).copied()
    }

    /// The first visual row of block `idx` — the one carrying its rail glyph.
    pub(super) fn block_head(&self, idx: usize) -> Option<usize> {
        self.flat.row_block.iter().position(|&b| b == idx)
    }

    /// Rendered cell width of visual row `row` — its content's extent, not the
    /// pane's, so a gesture binds tight to the text and ignores the dead margin.
    pub(super) fn row_width(&self, row: usize) -> Option<usize> {
        self.flat.rows.get(row).map(Row::width)
    }

    /// Whether the block at `idx` is dialable — a property of its kind, not its
    /// level, so a click on its glyph claims the gesture even when clamped.
    pub(super) fn block_dialable(&self, idx: usize) -> bool {
        self.blocks.get(idx).is_some_and(|e| e.block.dialable())
    }

    /// Cycle the block at `idx` between L1 and L3 — the click-on-rail affordance.
    pub(super) fn cycle_block(&mut self, idx: usize) -> bool {
        self.mutate_block(idx, Block::cycle)
    }

    /// Set the rung every thinking trace reads at: the traces on screen move
    /// now — through the same seam a click cycles, so the rung is remembered
    /// against a resync — and the ones still to come are born there.
    pub(super) fn set_traces_level(&mut self, level: Reveal) {
        self.traces = level;
        let traces: Vec<usize> = self
            .blocks
            .iter()
            .enumerate()
            .filter(|(_, e)| e.block.is_thinking())
            .map(|(i, _)| i)
            .collect();
        for idx in traces {
            self.mutate_block(idx, |b| b.set_reveal(level));
        }
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
            // A dialed block is carried over by later syncs, so its row count
            // has to move with it or the window would budget against the rung
            // it was opened at.
            entry.rows = Self::estimate_rows(block, self.agent);
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

    /// Plain text a drag selection copies, `lo <= hi` in buffer order; the rail
    /// glyph is stripped automatically.
    pub(super) fn selection_text(&self, lo: Cell, hi: Cell) -> String {
        let slice = |row: usize, a, b| self.flat.rows.get(row).map(|r| plain_slice(r, a, b));
        if lo.row == hi.row {
            return slice(lo.row, lo.col, hi.col).unwrap_or_default();
        }
        let interior = (lo.row + 1..hi.row).filter_map(|row| self.flat.rows.get(row).map(Row::plain));
        slice(lo.row, lo.col, u16::MAX)
            .into_iter()
            .chain(interior)
            .chain(slice(hi.row, 0, hi.col))
            .collect::<Vec<_>>()
            .join("\n")
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

    /// The tail as it reads right now: the lane's open line rendered *inside*
    /// the block that will absorb it, together with the count of flattened
    /// rows standing above it.
    ///
    /// One rendering path serves the live text and the committed text, because
    /// they are the same block — the open line is simply the part of it no
    /// record covers yet.  So the markdown context the line sits inside (an
    /// open fence, a list) is the block's own, and the record that completes
    /// the line changes the text without changing the picture.
    fn live_tail(&self, width: u16) -> (usize, Vec<Row>) {
        let all = self.flat.rows.len();
        let answering = !self.answer.is_empty();
        let open = if answering {
            &self.answer
        } else {
            &self.reasoning
        };
        if open.is_empty() {
            return (all, Vec::new());
        }
        // The block the line continues: the one already drawn at the tail,
        // when it is this lane's.  What is on screen is the authority — this
        // splice and the fold's rule for growing a block must agree, and they
        // do, both being "the run of records of one lane".
        //
        // Rendered with the newline the line is about to gain, since markdown
        // reads an unterminated last line differently from a whole one, and
        // in the ink its own block already carries, so the record that lands
        // changes the text and nothing else.
        let tail = self.blocks.last().map(|e| &e.block);
        let joins = tail.and_then(if answering {
            Block::markdown_src
        } else {
            Block::thinking_src
        });
        let text = format!("{}{open}\n", joins.unwrap_or_default());
        let mut block = if answering {
            let ink = tail.map(Block::fidelity).unwrap_or_default();
            Block::markdown(text, ink)
        } else {
            // No prose has followed the run yet — that is what makes it live.
            let mut trace = Block::thinking(text, 0);
            trace.set_reveal(self.traces);
            trace
        };
        // The absorbed block's own rows come off the flattened tail: it is
        // about to be drawn again, whole.
        let above = self.blocks.len().wrapping_sub(1);
        let keep = match joins {
            Some(_) => self
                .flat
                .row_block
                .iter()
                .rposition(|&b| b != above)
                .map_or(0, |i| i + 1),
            None => all,
        };
        let prev = self
            .blocks
            .len()
            .checked_sub(usize::from(joins.is_some()) + 1)
            .map(|i| &self.blocks[i].block);
        let content_w = width.min(READ_W);
        let lead = opens_rail_run(prev, &block);
        let rows = block.lines(content_w, self.agent, lead).to_vec();
        // A segment's leading blanks collapse against an already-blank tail,
        // exactly as they do in the flatten above.
        let mut first = 0;
        if self.flat.rows[..keep].last().is_some_and(Row::is_blank) {
            while first < rows.len() && rows[first].is_blank() {
                first += 1;
            }
        }
        let mut out: Vec<Row> = Vec::new();
        append_visual_rows(&mut out, &rows[first..], content_w, false, None);
        (keep, out)
    }

    /// The visible slice at `width` × `height`.  While `sticky`, `offset` is
    /// pinned to the tail; otherwise it is clamped to `max_off` and `sticky`
    /// re-arms once it reaches the bottom.
    pub(super) fn render_window(&mut self, width: u16, height: usize) -> RenderWindow {
        self.reflow(width);
        let (committed, tail) = self.live_tail(width);
        let total = committed + tail.len();
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
        let lines: Vec<Row> = self.flat.rows[..committed]
            .iter()
            .chain(&tail)
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
        let mut rows: Vec<Row> = Vec::new();
        let mut row_block: Vec<usize> = Vec::new();
        let mut i = 0;
        while i < self.blocks.len() {
            // `prompt` carries the human turn's rule fence down to
            // `append_visual_rows`, the one place the content width is known.
            // A run is opened by its call and by nothing else: an effect card is
            // buffered until its call has landed ([`super::surface`]), so one
            // reaching the projection with no call behind it belongs to no run
            // and renders alone, through the ordinary path below.
            let (anchor, seg_rows, prompt) = if self.blocks[i].block.is_tool_call() {
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
            if rows.last().is_some_and(Row::is_blank) {
                while first < seg_rows.len() && seg_rows[first].is_blank() {
                    first += 1;
                }
            }
            let added = append_visual_rows(&mut rows, &seg_rows[first..], content_w, prompt, None);
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
    fn render_group(&self, start: usize, end: usize, width: u16) -> Vec<Row> {
        let level = self.blocks[start].block.level();
        let calls = self.group_calls(start, end);
        let lines = group::body(&calls, level, content_w(width).into());
        let open = level >= Reveal::Context;
        let magnitude = group::aggregate_magnitude(&calls);
        let glyph = rail::span(RailKind::ToolCall(open), self.agent, magnitude);
        Row::seat(lines, Some(glyph))
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

/// The mass of the unbroken answer run opening `rows`: a reasoning block's
/// grain denominator, weighing the deliberation against the prose it became.
///
/// The commit cannot carry it — a reasoning run is recorded *before* the
/// prose it precedes — so the view measures it instead, which is also what
/// lets the grain fill as the answer accrues.  Any other kind of row ends
/// the run, so reasoning that led to a tool call rather than to prose is
/// honestly weighed against nothing.
fn answer_run(rows: &[record::Block]) -> u32 {
    let chars: usize = rows
        .iter()
        .map_while(|row| match row.kind() {
            record::BlockKind::Answer { text } => Some(text.chars().count()),
            _ => None,
        })
        .sum();
    u32::try_from(chars).unwrap_or(u32::MAX)
}

// ── `record::Printer`: the sole live producer ───────────────────────────────

impl Printer for Viewport {
    /// A transient never authors scrollback: [`Transient::Token`] and
    /// [`Transient::Thinking`] only carry their lane's open line, which the
    /// seats draw ([`Self::streaming_seat`], [`Self::push_thinking`]).  Every
    /// line the worker has finished arrives as a record through
    /// [`Self::sync`] instead — a printer never mints a [`record::Block`] of
    /// its own — and grows the block it belongs to.
    ///
    /// [`Transient::Born`], [`Transient::Died`], and [`Transient::Resources`]
    /// reach no-ops here: they need the tabs a bare `Viewport` cannot see, so
    /// `App::transient` intercepts and answers them itself, ahead of this call.
    fn transient(&mut self, t: &Transient) {
        match t {
            // Prose ends the reasoning run, on this side exactly as on the
            // worker's: the run's tail records where the prose begins, so the
            // line it left open is covered by then, and at most one lane is
            // ever open at once.
            Transient::Token(text) => {
                self.note_streamed(text.chars().count());
                self.reasoning.clear();
                carry(&mut self.answer, text);
            }
            Transient::Thinking(text) => {
                self.note_streamed(text.chars().count());
                self.push_thinking(text);
            }
            Transient::State(state) => self.set_state(*state),
            Transient::Cleared => self.reset(),
            Transient::StopReason(raw) => {
                self.push_chrome(ChromeKind::Plain, super::line::stop_reason(raw));
            }
            Transient::Pin { key, card } => self.set_pin(key.clone(), card.clone()),
            Transient::Unpin { key } => self.drop_pin(key),
            Transient::Fault { text } => {
                self.push_chrome(ChromeKind::Error, super::line::error(text));
            }
            // The step's stream is sealed: the worker has recorded every
            // line it means to, tails included, so an open line still
            // standing here stands for text the producer chose not to record
            // — a cancelled trace, a whitespace-only tail.  Dropping it is
            // what keeps a seat from outliving the step it was reading.
            Transient::Boundary => {
                self.answer.clear();
                self.reasoning.clear();
            }
            Transient::Born { .. } | Transient::Died | Transient::Resources { .. } => {}
        }
    }

    /// Rebuild from the fold, carrying over every block the fold has not
    /// moved.  A record lands as one changed row, so the work here is the
    /// rendering of that row and not of the window around it — the difference
    /// between a session that costs more the longer it runs and one that does
    /// not.
    fn sync(&mut self, blocks: &Blocks) {
        let all = blocks.rows();
        // Rows this window has already let go stay gone; rebuilding them only
        // to evict them again is the whole cost this sync exists to avoid.
        let rows = &all[all.partition_point(|r| Some(r.id().seq()) <= self.evicted_through)..];
        let mut cache: Vec<Entry> = std::mem::take(&mut self.blocks)
            .into_iter()
            .filter(|e| e.id.is_some())
            .collect();
        let floor = self.rebuild_floor(rows);
        // What the cache still serves: the blocks of `rows[..floor]`.  A row
        // at or past the floor is about to be rendered again, and one below
        // `rows[0]` names a row the fold itself has since let go — which it
        // only ever does to a printer that stopped syncing.
        let low = rows.first().map(|r| r.id().seq());
        let high = rows.get(floor).map(|r| r.id().seq());
        let held = cache.len();
        cache.retain(|e| {
            e.id.is_some_and(|id| {
                low.is_some_and(|l| id.seq() >= l) && high.is_none_or(|h| id.seq() < h)
            })
        });
        // One bound, one place: a block the window can never show again takes
        // its dial memory with it, so no third eviction path can drop one
        // table and keep the other.
        self.reveal
            .retain(|id, _| low.is_some_and(|l| id.seq() >= l));
        let rebuilt = floor < rows.len() || cache.len() < held;

        // The most recent `ral` script an answer's echo signal reads against
        // is a fact about the rows below the floor, which are not being walked.
        let mut last_ral_cmd = rows[..floor].iter().rev().find_map(|r| match r.kind() {
            record::BlockKind::ToolCall { tool, cmd, .. } if tool == "ral" => Some(cmd.as_str()),
            _ => None,
        });
        let mut built = cache;
        for (i, row) in rows[floor..].iter().enumerate() {
            let id = row.id();
            for mut block in self.render_block(
                row.kind(),
                &rows[floor + i + 1..],
                blocks,
                &mut last_ral_cmd,
            ) {
                if block.is_thinking() {
                    block.set_reveal(self.traces);
                }
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
        self.synced_rev = blocks.rev();
        self.blocks = self.merge_chrome(built, blocks.origin());
        self.enforce_window_caps();
        self.flat.dirty |= rebuilt;
    }
}

impl Viewport {
    /// Seed a resumed session's scrollback from the replayed fold.  The run
    /// that recorded these rows already wrote them into `user.log`, so the
    /// seeded window is marked as the file's own and the transcript continues
    /// rather than repeats.
    pub(super) fn seed(&mut self, blocks: &Blocks) {
        self.sync(blocks);
        self.log.seeded = self.blocks.len();
    }

    /// The first row whose rendering no longer stands: one the fold has
    /// touched since [`Self::synced_rev`], whether by opening it, growing the
    /// run it holds, or patching a result onto it.  Everything below is
    /// carried over.
    ///
    /// A reasoning row joins the rebuild whenever the answer run beneath it
    /// has grown, since its grain is a fact about that prose ([`answer_run`])
    /// and not about the row itself.
    fn rebuild_floor(&self, rows: &[record::Block]) -> usize {
        let mut floor = rows
            .iter()
            .position(|r| r.rev() > self.synced_rev)
            .unwrap_or(rows.len());
        if rows
            .get(floor)
            .is_some_and(|r| matches!(r.kind(), record::BlockKind::Answer { .. }))
        {
            while floor > 0 && matches!(rows[floor - 1].kind(), record::BlockKind::Answer { .. }) {
                floor -= 1;
            }
            if floor > 0 && matches!(rows[floor - 1].kind(), record::BlockKind::Thinking { .. }) {
                floor -= 1;
            }
        }
        floor
    }

    /// Stable-merge the chrome lane into `built` by [`Anchor`], arrival
    /// order breaking ties, dropping any chrome whose anchor fell out of the
    /// fold's window along with the row it named — the fix that lets a live
    /// `sync` redraw chrome instead of erasing it.
    fn merge_chrome(&mut self, built: Vec<Entry>, origin: Option<Seq>) -> Vec<Entry> {
        let floor = built.first().and_then(|e| e.id).map(BlockId::seq);
        let before = self.chrome.len();
        self.chrome.retain(|(anchor, ..)| match (anchor, floor) {
            (_, None) => true,
            (Some(a), Some(f)) => *a >= f,
            // Chrome authored before any row — the banner — sits above the
            // session's opening row, so it lives exactly as long as that row
            // is still the window's floor.
            (None, Some(f)) => origin == Some(f),
        });
        self.flat.dirty |= self.chrome.len() != before;

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

    fn chrome_entry(agent: AgentSlot, shape: ChromeKind, lines: Vec<Line<'static>>) -> Entry {
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
    /// echo signal can see the most recent `ral` script without a second
    /// pass; `after` is the rest of the window, which only a `∴` row reads —
    /// its grain is a fact about the prose that follows it ([`answer_run`]).
    fn render_block<'a>(
        &self,
        kind: &'a record::BlockKind,
        after: &[record::Block],
        blocks: &Blocks,
        last_ral_cmd: &mut Option<&'a str>,
    ) -> Vec<Block> {
        use record::BlockKind as K;
        match kind {
            K::Thinking { text } => vec![Block::thinking(text.clone(), answer_run(after))],
            K::Prompt { text } => vec![Block::chrome(
                ChromeKind::Prompt,
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
            // Not a card: a settled block is announced, not bounded — a line
            // on the rail, exactly as a subagent's answer arrives.  The shape
            // holds however it settled: `╳` is the turn's own failure (a
            // provider error, a stall), never a nonzero exit, which reads as a
            // red status in the row here just as it does on an exec.
            K::Done { outcome } => {
                vec![Block::chrome(
                    ChromeKind::Settled,
                    super::line::render_text(&card::settled_spans(&card::to_card_done(outcome))),
                )]
            }
            K::Notice { notice } => {
                vec![Block::card(card::notice_card(&card::to_card_notice(
                    notice,
                )))]
            }
            K::Context { rows } => vec![Block::card(card::context_rows_card(rows))],
            K::Cancelled => vec![Block::chrome(
                ChromeKind::Cancelled,
                super::line::note("cancelled"),
            )],
            K::Error { text } => vec![Block::chrome(ChromeKind::Error, super::line::error(text))],
            // Forensic only: a harness result is already said by its act row,
            // and a nudge is the agent steering itself.
            K::Nudge { .. } | K::HarnessResult { .. } => Vec::new(),
            K::ProviderError { error } => {
                vec![Block::chrome(
                    ChromeKind::Error,
                    super::line::provider_error(error),
                )]
            }
            K::Stalled { error } => {
                vec![Block::chrome(
                    ChromeKind::Error,
                    super::line::stalled(error),
                )]
            }
            K::SystemNote { text } => {
                vec![Block::chrome(ChromeKind::Plain, super::line::note(text))]
            }
            K::ModelChanged { model, provider } => vec![Block::chrome(
                ChromeKind::Plain,
                super::line::note(&format!("model changed: {provider}/{model}")),
            )],
            K::Step { n } => vec![Block::chrome(
                ChromeKind::Step,
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
                vec![Block::chrome(ChromeKind::Plain, super::line::note(&text))]
            }
        }
    }
}

/// Rebuild one [`Display::Observation`](record::Display::Observation)'s block,
/// through the same [`landing`] the live rail draws from — a rendering,
/// never recorded, rebuilt fresh at sync time.
fn render_observation(value: ral_core::serial::FOValue) -> Vec<Block> {
    let Some(obs) = observation_from_wire(value) else {
        return Vec::new();
    };
    render_observed(&obs.what)
}

fn render_observed(what: &Observed) -> Vec<Block> {
    let Some(place) = landing(what) else {
        return Vec::new();
    };
    match place {
        Landing::Grouped(kind) => {
            vec![Block::observation_card(observation_card(what), kind, 1)]
        }
        Landing::Barrier => vec![Block::write_card(observation_card(what))],
        Landing::Standalone => vec![Block::card(observation_card(what))],
        Landing::Announced => vec![Block::chrome(
            ChromeKind::Spawned,
            super::line::render_text(&card::observation_spans(what)),
        )],
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
        Viewport::new(path, AgentSlot(0), false, Reveal::Full)
    }

    fn rail_rows(rows: &[Row], glyph: &str) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter_map(|(i, row)| (row.gutter() == glyph).then_some(i))
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

    /// The open line reads inside the block it will join, and the record that
    /// completes it changes the text without changing the picture — the one
    /// claim that says live and committed are the same rendering.
    #[test]
    fn the_record_that_completes_a_line_does_not_change_the_picture() {
        use crate::record::{Blocks, Display, Fold, Record, Recorded, Seq, Stamp, View};

        let mut vp = viewport();
        let mut memo = Blocks::default();
        let land = |vp: &mut Viewport, memo: &mut Blocks, seq: u64, text: &str| {
            View::step(
                memo,
                &Recorded::new(
                    Stamp::new(Seq::new(seq), 0..0),
                    Record::Display(Display::Answer { text: text.into() }),
                ),
            )
            .expect("a display-only fold never refuses");
            vp.sync(memo);
        };

        // A fence opens and a line of code streams: the fence is recorded, the
        // line is still open.
        vp.transient(&Transient::Token("```ral\nlet x = 1".into()));
        land(&mut vp, &mut memo, 1, "```ral\n");
        let live = format!("{:?}", vp.render_window(READ_W, 24).lines);
        assert!(
            vp.render_window(READ_W, 24)
                .lines
                .last()
                .expect("a row")
                .plain()
                .contains("let x = 1"),
            "the open line reads where its block will hold it"
        );

        // Its record lands and the printer's own newline rule closes it.
        vp.transient(&Transient::Token("\n".into()));
        land(&mut vp, &mut memo, 2, "let x = 1\n");
        assert_eq!(
            format!("{:?}", vp.render_window(READ_W, 24).lines),
            live,
            "the record changed the text and not the picture"
        );
    }

    /// At most one lane is ever open, because prose ends the reasoning run on
    /// the printer's side exactly as it does on the worker's — and the step's
    /// boundary clears whatever is left.
    #[test]
    fn prose_ends_the_open_reasoning_line_and_the_boundary_clears_both() {
        let mut vp = viewport();
        vp.transient(&Transient::Thinking("considering the shape".into()));

        let w = vp.render_window(READ_W, 24);
        let all = w
            .lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("considering the shape"),
            "the reasoning's open line reads on its own rail: {all:?}"
        );
        assert_eq!(
            rail_rows(&w.lines, "∴ ").len(),
            1,
            "and wears the reasoning rail: {all:?}"
        );

        vp.transient(&Transient::Token("First words".into()));
        let w = vp.render_window(READ_W, 24);
        let all = w
            .lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !all.contains("considering the shape") && all.contains("First words"),
            "prose closed the run, whose tail the worker recorded at the same delta: {all:?}"
        );

        vp.transient(&Transient::Boundary);
        let w = vp.render_window(READ_W, 24);
        let all = w
            .lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !all.contains("First words"),
            "no open line outlives the step it was read from: {all:?}"
        );
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
                ChromeKind::Plain,
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

    /// The committed thinking block renders in a sticky viewport too, once a
    /// resync past a full window of chrome carries it in.
    #[test]
    fn committed_thinking_stays_visible_in_sticky_viewport() {
        use crate::record::{Blocks, Display, Fold, Record, Recorded, Seq, Stamp, View};

        let mut vp = viewport();
        vp.push_chrome(ChromeKind::Prompt, vec![Line::from("hello cutie")]);
        // Fill enough chrome to overflow a small window.
        for i in 0..8 {
            vp.push_chrome(
                ChromeKind::Plain,
                vec![
                    Line::from(format!("block {i} line a")),
                    Line::from(format!("block {i} line b")),
                ],
            );
        }
        vp.transient(&Transient::Thinking("considering the shape".into()));
        let live = vp.render_window(READ_W, 8);
        let live_text = live
            .lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n");
        let live_thinking = rail_rows(&live.lines, "∴ ");
        assert!(
            !live_thinking.is_empty(),
            "live thinking has its rail: {live_text:?}"
        );

        let mut memo = Blocks::default();
        let stamp = Stamp::new(Seq::new(1), 0..0);
        View::step(
            &mut memo,
            &Recorded::new(
                stamp,
                Record::Display(Display::Thinking {
                    text: "considering the shape\n".into(),
                }),
            ),
        )
        .expect("a display-only fold never refuses");
        vp.sync(&memo);
        let committed = vp.render_window(READ_W, 8);
        let committed_text = committed
            .lines
            .iter()
            .map(Row::plain)
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
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join(" ")
    }

    /// Past `VIEWPORT_MAX_BLOCKS` the oldest go first, and the newest survives.
    #[test]
    fn window_evicts_oldest_blocks_first_past_the_block_cap() {
        let mut vp = viewport();
        for i in 0..(VIEWPORT_MAX_BLOCKS + 50) {
            vp.push_chrome(ChromeKind::Plain, vec![Line::from(format!("marker {i}"))]);
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
            vp.push_chrome(ChromeKind::Plain, lines);
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

    /// Eviction drops the scrollback and the pinned register, keeping only
    /// the fact that the view is dead; `log_path` is untouched, so the log
    /// stays readable.
    #[test]
    fn evict_to_tombstone_drops_scrollback_and_register() {
        let mut vp = viewport();
        vp.push_chrome(ChromeKind::Plain, vec![Line::from("hello")]);
        vp.set_pin("k".into(), Card(Vec::new()));
        assert!(!vp.blocks.is_empty());

        let log_path = vp.log_path.clone();
        vp.evict_to_tombstone();

        assert_eq!(vp.log_path, log_path);
        assert!(vp.blocks.is_empty(), "the scrollback is dropped");
        assert!(vp.pins().is_empty(), "the pinned register is dropped");
    }

    /// Re-evicting is a harmless no-op: the view is already clean.
    #[test]
    fn evict_to_tombstone_is_idempotent() {
        let mut vp = viewport();
        vp.push_chrome(ChromeKind::Error, vec![Line::from("boom")]);
        vp.evict_to_tombstone();
        assert!(vp.blocks.is_empty());
        vp.evict_to_tombstone();
        assert!(vp.blocks.is_empty());
    }

    /// An act is a barrier: the run scan ends at it, so it renders standalone
    /// under its own `↗` between two separate `▸` runs, never swallowed into a
    /// burst of reads.
    #[test]
    fn an_act_breaks_a_run_of_observations() {
        use crate::record::Display;
        let mut vp = viewport();
        let mut memo = Blocks::default();
        step(
            &mut memo,
            [
                Record::Display(Display::ToolCall {
                    tool: "ral".into(),
                    cmd: "read 'line.rs'".into(),
                    summary: Some("read the renderer".into()),
                }),
                Record::Display(Display::ToolCall {
                    tool: "ral".into(),
                    cmd: "read 'block.rs'".into(),
                    summary: Some("read the block".into()),
                }),
                Record::Display(Display::HarnessCall {
                    verb: "message".into(),
                    subject: Some("hunter".into()),
                    payload: "focus on the renderer first".into(),
                    failed: false,
                }),
                Record::Display(Display::ToolCall {
                    tool: "ral".into(),
                    cmd: "read 'rail.rs'".into(),
                    summary: Some("read the rail".into()),
                }),
            ],
        )
        .expect("a display-only fold never refuses");
        vp.sync(&memo);

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
            w.lines[act[0]].plain(),
            "message    hunter              focus on the renderer first"
        );
    }

    /// An act is not a call-bearing block, so a `ral` result landing after one
    /// walks past it to the call that actually earned the bar.
    #[test]
    fn an_acts_result_stamps_no_bar_and_shields_no_call() {
        use crate::record::{BlockId, Display};
        let mut vp = viewport();
        let mut memo = Blocks::default();
        let call_stamp = Stamp::new(Seq::new(1), 0..0);
        View::step(
            &mut memo,
            &Recorded::new(
                call_stamp.clone(),
                Record::Display(Display::ToolCall {
                    tool: "ral".into(),
                    cmd: "read 'line.rs'".into(),
                    summary: Some("read the renderer".into()),
                }),
            ),
        )
        .expect("a display-only fold never refuses");
        View::step(
            &mut memo,
            &Recorded::new(
                Stamp::new(Seq::new(2), 0..0),
                Record::Display(Display::HarnessCall {
                    verb: "cancel".into(),
                    subject: Some("hunter".into()),
                    payload: String::new(),
                    failed: false,
                }),
            ),
        )
        .expect("a display-only fold never refuses");
        View::step(
            &mut memo,
            &Recorded::new(
                Stamp::new(Seq::new(3), 0..0),
                Record::Display(Display::Result {
                    text: "a line\n".repeat(40),
                    call: BlockId::new(call_stamp.seq()),
                }),
            ),
        )
        .expect("a display-only fold never refuses");
        vp.sync(&memo);

        let w = vp.render_window(READ_W, 40);
        let act = rail_rows(&w.lines, "↗ ");
        assert_eq!(w.lines[act[0]].plain(), "cancel     hunter");
        let run = w.lines[rail_rows(&w.lines, "▸ ")[0]].plain();
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
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(all.contains("hello") && all.contains("hi back"), "{all:?}");
    }

    /// `/thinking`'s two obligations: the traces already synced move, and a
    /// trace that arrives afterwards is born at the rung then in force.
    #[test]
    fn the_standing_trace_rung_reaches_past_and_future_traces() {
        let mut memo = Blocks::default();
        step(
            &mut memo,
            [Record::Display(Display::Thinking {
                text: "weighing the shape".into(),
            })],
        )
        .expect("a display-only fold never refuses");

        let mut vp = viewport();
        vp.sync(&memo);
        vp.set_traces_level(Reveal::Summary);
        assert_eq!(vp.blocks[0].block.level(), Reveal::Summary);

        step(
            &mut memo,
            [
                Record::Display(Display::Answer {
                    text: "the answer".into(),
                }),
                Record::Display(Display::Thinking {
                    text: "and again".into(),
                }),
            ],
        )
        .expect("a display-only fold never refuses");
        vp.sync(&memo);
        let traces: Vec<Reveal> = vp
            .blocks
            .iter()
            .filter(|e| e.block.is_thinking())
            .map(|e| e.block.level())
            .collect();
        assert_eq!(traces, vec![Reveal::Summary, Reveal::Summary]);

        vp.set_traces_level(Reveal::Full);
        let traces: Vec<Reveal> = vp
            .blocks
            .iter()
            .filter(|e| e.block.is_thinking())
            .map(|e| e.block.level())
            .collect();
        assert_eq!(traces, vec![Reveal::Full, Reveal::Full]);
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
        vp.push_chrome(ChromeKind::Plain, vec![Line::from("between")]);

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
            .map(Row::plain)
            .collect::<Vec<_>>();
        let one = rendered.iter().position(|l| l.contains("one")).unwrap();
        let between = rendered.iter().position(|l| l.contains("between")).unwrap();
        let two = rendered.iter().position(|l| l.contains("two")).unwrap();
        assert!(
            one < between && between < two,
            "chrome anchored between two commits must render between them: {rendered:?}"
        );
    }

    // ── the transcript ──────────────────────────────────────────────────────

    /// Lines of `vp`'s `user.log` ending in `marker`, which is how a test asks
    /// whether a block reached the transcript, and how many times.
    fn logged(path: &Path, marker: &str) -> usize {
        fs::read_to_string(path)
            .expect("the transcript reads back")
            .lines()
            .filter(|l| l.ends_with(marker))
            .count()
    }

    /// The window is bounded; the transcript is not.  What scrolls out of
    /// heap is on disk by the time it goes, so a session outliving its own
    /// window still leaves a whole `user.log` behind.
    #[test]
    fn the_transcript_keeps_what_the_window_evicts() {
        let mut vp = viewport();
        for i in 0..(VIEWPORT_MAX_BLOCKS + 50) {
            vp.push_chrome(ChromeKind::Plain, vec![Line::from(format!("marker {i}"))]);
        }
        let path = vp
            .flush_log()
            .expect("the transcript flushes")
            .to_path_buf();
        let text = fs::read_to_string(&path).expect("the transcript reads back");
        let last = format!("marker {}", VIEWPORT_MAX_BLOCKS + 49);
        assert_eq!(logged(&path, "marker 0"), 1, "the evicted head is on disk");
        assert_eq!(logged(&path, &last), 1, "and so is the resident tail");
        assert!(
            text.find("marker 0") < text.find(&last),
            "in the order they were written"
        );
    }

    /// A flush writes the resident window provisionally, so `/export` mid-session
    /// reads a whole transcript; the next one rewinds over it rather than
    /// writing the same blocks twice.
    #[test]
    fn a_second_flush_rewinds_the_provisional_tail() {
        let mut vp = viewport();
        for i in 0..3 {
            vp.push_chrome(ChromeKind::Plain, vec![Line::from(format!("marker {i}"))]);
        }
        let path = vp
            .flush_log()
            .expect("the transcript flushes")
            .to_path_buf();
        let once = fs::read_to_string(&path).expect("the transcript reads back");

        vp.push_chrome(ChromeKind::Plain, vec![Line::from("marker 3")]);
        vp.flush_log().expect("the transcript flushes");
        let twice = fs::read_to_string(&path).expect("the transcript reads back");

        assert!(twice.starts_with(&once), "the first flush is a prefix");
        assert_eq!(logged(&path, "marker 0"), 1, "written once, not twice");
        assert_eq!(logged(&path, "marker 3"), 1, "and the new block joins it");
    }

    /// A resumed session's window was rendered by the run that recorded it, so
    /// the continuation appends to that transcript instead of repeating it.
    #[test]
    fn a_resumed_transcript_continues_rather_than_repeats() {
        let mut memo = Blocks::default();
        let mut seq = 0u64;
        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Prompt { text: "one".into() }),
        );
        let mut first = viewport();
        let path = first.log_path.clone();
        first.sync(&memo);
        first.flush_log().expect("the transcript flushes");
        assert_eq!(logged(&path, "one"), 1);

        let mut resumed = Viewport::new(path.clone(), AgentSlot(0), true, Reveal::Full);
        resumed.seed(&memo);
        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Prompt { text: "two".into() }),
        );
        resumed.sync(&memo);
        resumed.flush_log().expect("the transcript flushes");

        assert_eq!(
            logged(&path, "one"),
            1,
            "the seeded window is not rewritten"
        );
        assert_eq!(logged(&path, "two"), 1, "and the new row joins it");
    }

    // ── incremental sync ────────────────────────────────────────────────────

    /// A record moves one row, so a sync rebuilds one row — which is what
    /// makes the cost of a step independent of how long the session has run.
    #[test]
    fn a_sync_rebuilds_only_the_rows_the_fold_has_moved() {
        let mut memo = Blocks::default();
        let mut seq = 0u64;
        for i in 0..5 {
            advance(
                &mut memo,
                &mut seq,
                Record::Display(Display::Prompt {
                    text: format!("p{i}"),
                }),
            );
        }
        let mut vp = viewport();
        vp.sync(&memo);
        assert_eq!(
            vp.rebuild_floor(memo.rows()),
            5,
            "a sync over a fold nothing has touched rebuilds nothing"
        );

        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Prompt { text: "p5".into() }),
        );
        assert_eq!(
            vp.rebuild_floor(memo.rows()),
            5,
            "one landed record reopens one row"
        );
    }

    /// A result patches the call it names, wherever that call sits — so the
    /// floor follows the fold's revision rather than the tail.
    #[test]
    fn a_result_reopens_the_call_it_patches() {
        let mut memo = Blocks::default();
        let call = Stamp::new(Seq::new(1), 0..0);
        View::step(
            &mut memo,
            &Recorded::new(
                call.clone(),
                Record::Display(Display::ToolCall {
                    tool: "ral".into(),
                    cmd: "read 'x'".into(),
                    summary: Some("look at x".into()),
                }),
            ),
        )
        .expect("a display-only fold never refuses");
        let mut seq = 1u64;
        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Prompt {
                text: "meanwhile".into(),
            }),
        );
        let mut vp = viewport();
        vp.sync(&memo);

        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Result {
                text: "a line\n".repeat(3),
                call: BlockId::new(call.seq()),
            }),
        );
        assert_eq!(
            vp.rebuild_floor(memo.rows()),
            0,
            "the patched call reopens, though rows landed after it"
        );
    }

    /// A reasoning row's grain is a fact about the prose beneath it, so the
    /// answer run growing reopens the row above — the one dependency that
    /// reaches backwards past the floor.
    #[test]
    fn a_growing_answer_run_reopens_the_reasoning_above_it() {
        let mut memo = Blocks::default();
        let mut seq = 0u64;
        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Thinking {
                text: "why\n".into(),
            }),
        );
        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Answer {
                text: "because\n".into(),
            }),
        );
        let mut vp = viewport();
        vp.sync(&memo);

        advance(
            &mut memo,
            &mut seq,
            Record::Display(Display::Answer {
                text: "and also\n".into(),
            }),
        );
        assert_eq!(
            vp.rebuild_floor(memo.rows()),
            0,
            "the reasoning row is weighed against prose that has just grown"
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
        vp.push_chrome(ChromeKind::Plain, vec![Line::from("anchored on first")]);
        assert_eq!(vp.chrome.len(), 1);

        // Pushing past the fold's own window evicts `first`.
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
