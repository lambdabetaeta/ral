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

use super::block::{AgentSlot, Block, RailShape, Reveal, append_visual_rows};
use super::group;
use super::line::{is_blank, plain};
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
use crate::record::{self, BlockId, Blocks, Fold as _, Printer, Seq, Transient};
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
    /// that point on, and `None` for a live or still-lingering view.
    tombstone: Option<Tombstone>,
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
    /// The model's context window, for the fidelity a synced [`Block::markdown`]
    /// stamps — set by `App::update_live_model`, since the fold's own memo
    /// carries usage but not the provider's cap.
    context_window: Option<u64>,
    /// Chrome rows [`Self::push_chrome`] has authored, named by the
    /// [`Anchor`] they were drawn at.  [`Self::sync`] stable-merges these
    /// into the folded rows it rebuilds.
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
        self.answer = String::new();
        self.reasoning = String::new();
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
    pub(super) fn push_chrome(&mut self, shape: RailShape, lines: Vec<Line<'static>>) {
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

    /// The tail as it reads right now: the lane's open line rendered *inside*
    /// the block that will absorb it, together with the count of flattened
    /// rows standing above it.
    ///
    /// One rendering path serves the live text and the committed text, because
    /// they are the same block — the open line is simply the part of it no
    /// record covers yet.  So the markdown context the line sits inside (an
    /// open fence, a list) is the block's own, and the record that completes
    /// the line changes the text without changing the picture.
    fn live_tail(&self, width: u16) -> (usize, Vec<Line<'static>>) {
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
            Block::thinking(text, 0)
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
        let lines = block.lines(content_w, self.agent, lead).to_vec();
        // A segment's leading blanks collapse against an already-blank tail,
        // exactly as they do in the flatten above.
        let mut first = 0;
        if self.flat.rows[..keep].last().is_some_and(is_blank) {
            while first < lines.len() && is_blank(&lines[first]) {
                first += 1;
            }
        }
        let mut out: Vec<Line<'static>> = Vec::new();
        append_visual_rows(&mut out, &lines[first..], content_w, false, None);
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
        let lines: Vec<Line<'static>> = self.flat.rows[..committed]
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
                self.push_chrome(RailShape::Plain, super::line::stop_reason(raw));
            }
            Transient::Pin { key, card } => self.set_pin(key.clone(), card.clone()),
            Transient::Unpin { key } => self.drop_pin(key),
            Transient::Fault { text } => {
                self.push_chrome(RailShape::Error, super::line::error(text));
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

    fn sync(&mut self, blocks: &Blocks) {
        let rows = blocks.rows();
        let mut built: Vec<Entry> = Vec::with_capacity(rows.len());
        let mut last_ral_cmd: Option<&str> = None;
        for (i, row) in rows.iter().enumerate() {
            let id = row.id();
            for mut block in
                self.render_block(row.kind(), &rows[i + 1..], blocks, &mut last_ral_cmd)
            {
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
        self.blocks = self.merge_chrome(built, blocks.origin());
        self.enforce_window_caps();
        self.flat.dirty = true;
    }
}

impl Viewport {
    /// Stable-merge the chrome lane into `built` by [`Anchor`], arrival
    /// order breaking ties, dropping any chrome whose anchor fell out of the
    /// fold's window along with the row it named — the fix that lets a live
    /// `sync` redraw chrome instead of erasing it.
    fn merge_chrome(&mut self, built: Vec<Entry>, origin: Option<Seq>) -> Vec<Entry> {
        let floor = built.first().and_then(|e| e.id).map(BlockId::seq);
        self.chrome.retain(|(anchor, ..)| match (anchor, floor) {
            (_, None) => true,
            (Some(a), Some(f)) => *a >= f,
            // Chrome authored before any row — the banner — sits above the
            // session's opening row, so it lives exactly as long as that row
            // is still the window's floor.
            (None, Some(f)) => origin == Some(f),
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
            // Not a card: a settled block is announced, not bounded — a line
            // on the rail, exactly as a subagent's answer arrives.  The shape
            // holds however it settled: `╳` is the turn's own failure (a
            // provider error, a stall), never a nonzero exit, which reads as a
            // red status in the row here just as it does on an exec.
            K::Done { outcome } => {
                vec![Block::chrome(
                    RailShape::Settled,
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
            plain(vp.render_window(READ_W, 24).lines.last().expect("a row")).contains("let x = 1"),
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
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
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
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(
            !all.contains("considering the shape") && all.contains("First words"),
            "prose closed the run, whose tail the worker recorded at the same delta: {all:?}"
        );

        vp.transient(&Transient::Boundary);
        let w = vp.render_window(READ_W, 24);
        let all = w.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
        assert!(
            !all.contains("First words"),
            "no open line outlives the step it was read from: {all:?}"
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

    /// The committed thinking block renders in a sticky viewport too, once a
    /// resync past a full window of chrome carries it in.
    #[test]
    fn committed_thinking_stays_visible_in_sticky_viewport() {
        use crate::record::{Blocks, Display, Fold, Record, Recorded, Seq, Stamp, View};

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
        vp.transient(&Transient::Thinking("considering the shape".into()));
        let live = vp.render_window(READ_W, 8);
        let live_text = live.lines.iter().map(plain).collect::<Vec<_>>().join("\n");
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
            plain(&w.lines[act[0]]),
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
