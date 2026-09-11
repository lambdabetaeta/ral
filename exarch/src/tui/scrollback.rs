//! Per-session collapsible scrollback.
//!
//! A [`Scrollback`] *mirrors* the view fold: one [`Block`] — the reader's atom —
//! per incident as it arrived, built from the [`Delta`] each record produces,
//! with the deliberation and work of one burst collapsed into a group as they
//! land.  Nothing is re-derived: the mirror renders those blocks into the
//! screen's visual rows and into the session's `user.log`, the durable
//! counterpart to the one record log.  The whole alt-screen frame is redrawn
//! each tick, so scrollback is ours, not the host terminal's, and every tab
//! keeps its own scroll position.
//!
//! [`Scrollback::fact`] is the sole producer: it steps the fold beside the
//! mirror and acts on what the step reports, and [`Scrollback::transient`]
//! draws whatever is live-only — [`Self::push_chrome`], the chrome door, and
//! [`Self::push_thinking`], which carries the open line the live tail draws.

use super::block::{AgentSlot, Block, BlockKind, Chrome, Detail, Member, Part, seam};
use super::fidelity::Fidelity;
use super::gesture::Cell;
use super::group;
use super::line::is_blank;
use super::palette::READ_W;
use super::row::Row;
use super::select::plain_slice;
use crate::agent::event::{ContextOp, EditAuthority};
use crate::bus::card::{self, Card, Landing, landing, observation_card, observation_from_wire};
use crate::provider::Usage;
use crate::record::{self, BlockId, Blocks, Delta, Seq, Transient};
use ral_core::types::Observed;
use std::fs;
use std::io;
use std::io::{Seek, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Carry a lane's open line across one delta: the text after the last newline
/// is exactly what no record holds yet, since the worker cuts its records at
/// that same newline.  Committed text and open line are complementary by that
/// one rule, so neither side has anything to count.
fn carry(open: &mut String, delta: &str) {
    match delta.rfind('\n') {
        Some(nl) => {
            open.clear();
            open.push_str(&delta[nl + 1..]);
        }
        None => open.push_str(delta),
    }
}

/// A dialable part of one block, as a hover or a click names it.  Minted by
/// [`Scrollback::block_at`] alone, so no caller can invent a row-to-part
/// correspondence of its own.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(super) struct Hit {
    block: usize,
    part: Part,
}

pub(super) struct Scrollback {
    /// This session's scrollback, oldest block first — the mirror of the
    /// fold's own window, and the only thing between the fold and the screen.
    blocks: Vec<Block>,
    /// Set once evicted ([`Self::evict_to_tombstone`]); `blocks` is empty from
    /// that point on, and `false` for a live or still-lingering view.
    tombstoned: bool,
    /// Palette slot stamped onto every rail glyph; root is `0`.
    agent: AgentSlot,
    /// The answer's open line: assistant text past the last newline, which is
    /// past the last [`Display::Answer`](crate::record::Display::Answer)
    /// record the worker has cut.  It is drawn inside the block it will join
    /// ([`Self::live_tail`]).
    answer: String,
    /// The thinking lane's open line — `answer`'s twin on the `∴` lane, drawn
    /// by [`Self::live_tail`] and grown by [`Self::push_thinking`].
    thinking_line: String,
    /// This mirror's own view-fold memo, stepped by every [`Self::fact`].
    /// Its window is the one window: a block leaves the mirror exactly as the
    /// block it was built from leaves the fold.
    fold: Blocks,
    /// Top visible visual row, per-scrollback so each tab keeps its place.
    offset: usize,
    /// Follow the tail.  Cleared by a scroll either way, re-armed at the bottom.
    sticky: bool,
    log_path: PathBuf,
    log: Log,
    /// Total, never absent: the status line always has a state to name.
    state: StateSpan,
    /// Kit-authored *state*: a `key → Card` register drawn as the right-hand
    /// column.  Never logged, and wiped by [`Self::reset`] so a pin is
    /// generation-bounded like the scrollback.  A `Vec`, not a map, so render
    /// order is first-seen insertion order.
    pins: Vec<(String, Card)>,
    /// The rung a group's deliberation is born at — `/thinking`'s standing
    /// datum, which [`Self::set_thinking_level`] moves on every group at once
    /// and every group still to arrive is born at.
    thinking: Detail,
    /// The most recent `ral` script, which an answer's echo signal is read
    /// against — a fact about the stream, accumulated as the calls land.
    last_ral_cmd: Option<String>,
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

/// The tail as it reads right now: the blocks whose own rows still stand, how
/// many rows those put on screen, and the rows the open line draws beneath.
struct Tail {
    blocks: usize,
    rows: usize,
    live: Vec<Row>,
}

/// What one fold block contributes to the mirror, in arrival order.
enum Item {
    /// Something a group may take in.
    Member(Member),
    /// A barrier: its own block, ending whatever group stood at the tail.
    Barrier(BlockKind),
}

/// The session's rendered transcript, `user.log`.
///
/// A block is written once, when the mirror's head trim drops it
/// ([`Self::retire`]), into a prefix that only ever grows — so the file keeps
/// the whole session however long it runs, while the mirror keeps only what
/// the fold's window holds.  The blocks still resident are written past that
/// prefix on demand ([`Self::flush`]) and rewound by the next retirement,
/// which is what lets `/export` read a whole transcript mid-session without
/// the tail being written twice.
///
/// A path that will not open leaves [`Self::file`] `None` and the transcript
/// silently unrecorded, so a log failure never disables the scrollback.
struct Log {
    file: Option<fs::File>,
    /// Bytes of retired blocks; everything past this is provisional.
    durable: u64,
    /// Whether the retired prefix ends blank, so a block joining it collapses
    /// its leading blanks exactly as the screen does.
    prev_blank: bool,
    /// Leading mirror blocks this file already holds: a resumed session's
    /// seeded prefix, which the run that recorded it already wrote.
    seeded: usize,
}

impl Log {
    /// `append` continues a resumed session's transcript; otherwise the file
    /// starts empty, since a fresh session shares none of its history.
    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:scrollback-log] opens the scrollback's rendered-text log; render dump infra, not turn-time data I/O"
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
            seeded: 0,
        }
    }

    /// Render `blocks` at the end of the retired prefix, forced whole — the
    /// script, diff and prose are on the record even while reduced on screen —
    /// returning the length and the seam state that keeping them would leave.
    fn write(&mut self, blocks: &[Block], agent: AgentSlot) -> io::Result<(u64, bool)> {
        let mut prev_blank = self.prev_blank;
        let Some(file) = self.file.as_mut() else {
            return Ok((self.durable, prev_blank));
        };
        file.set_len(self.durable)?;
        file.seek(io::SeekFrom::Start(self.durable))?;
        {
            let mut out = io::BufWriter::new(&mut *file);
            for block in blocks {
                for row in block.log_rows(agent) {
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
            }
            out.flush()?;
        }
        Ok((file.stream_position()?, prev_blank))
    }

    /// Write `blocks` and keep them: they have left the mirror and no later
    /// write may rewind over them.  A transcript that will not write is
    /// abandoned rather than retried, so a failing disk costs one attempt.
    fn retire(&mut self, blocks: &[Block], agent: AgentSlot) {
        match self.write(blocks, agent) {
            Ok((durable, prev_blank)) => {
                self.durable = durable;
                self.prev_blank = prev_blank;
            }
            Err(_) => self.file = None,
        }
    }

    /// Write `blocks` provisionally, so the file reads whole right now.
    ///
    /// # Errors
    /// Returns the write's own error, which `/export` reports.
    fn flush(&mut self, blocks: &[Block], agent: AgentSlot) -> io::Result<()> {
        self.write(blocks, agent).map(|_| ())
    }
}

/// Copy a flushed `user.log` to `dest` for `/export` — the caller resolves
/// `dest`, refuses to overwrite, and flushes.  Beside [`Log::open`], so all
/// `user.log` I/O lives in one place.
#[allow(
    clippy::disallowed_methods,
    reason = "[silent:export] copies a flushed user.log to the user-chosen export path; output infra, not turn-time data I/O"
)]
pub(super) fn export_log(src: &Path, dest: &Path) -> io::Result<u64> {
    fs::copy(src, dest)
}

impl Scrollback {
    /// `append` continues a resumed session's `user.log` where the run that
    /// recorded it left off; a fresh session starts the file empty.  The
    /// resumed mirror itself is built by [`Self::seed`], which is what keeps
    /// the continuation from repeating what the file already holds.
    /// `thinking` is the standing rung a view is born at — owned by the caller
    /// ([`super::tabs::Tabs`]'s own), so a new view is never told twice, nor
    /// left disagreeing.
    pub(super) fn new(log_path: PathBuf, agent: AgentSlot, append: bool, thinking: Detail) -> Self {
        Self {
            log: Log::open(&log_path, append),
            blocks: Vec::new(),
            tombstoned: false,
            agent,
            answer: String::new(),
            thinking_line: String::new(),
            fold: Blocks::default(),
            offset: 0,
            sticky: true,
            log_path,
            state: StateSpan::new(crate::bus::AgentState::Ready),
            pins: Vec::new(),
            thinking,
            last_ral_cmd: None,
        }
    }

    /// Build the mirror of a resumed session's replayed fold, which becomes
    /// this printer's own memo.  Every block in it opened once, in order, so
    /// live and resumed scrollback are the same construction; the run that
    /// recorded them already wrote them into `user.log`, so the seeded prefix
    /// is marked as the file's own and the transcript continues rather than
    /// repeats.
    pub(super) fn seed(&mut self, fold: Blocks) {
        self.fold = fold;
        for at in 0..self.fold.blocks().len() {
            self.opened(at);
        }
        self.log.seeded = self.blocks.len();
    }

    pub(super) fn agent(&self) -> AgentSlot {
        self.agent
    }

    /// This session's spend — the matrix's per-agent readout, where
    /// `App::total_usage` is the rule line's sum over all of them.
    pub(super) fn usage(&self) -> Usage {
        self.fold.usage()
    }

    /// Per-turn "had a tool call" flags, oldest first — one bool per turn
    /// block the fold holds, which the matrix renders `●` or `○`.
    pub(super) fn turns(&self) -> Vec<bool> {
        let mut turns: Vec<bool> = Vec::new();
        for block in self.fold.blocks() {
            match block.kind() {
                record::BlockKind::Turn { .. } => turns.push(false),
                record::BlockKind::ToolCall { .. } => {
                    if let Some(last) = turns.last_mut() {
                        *last = true;
                    }
                }
                _ => {}
            }
        }
        turns
    }

    /// Summed changed lines over this session's cards — the matrix's size
    /// readout.  `0` for a read-only agent; prose never inflates it.
    pub(super) fn lines_touched(&self) -> u32 {
        self.blocks.iter().filter_map(Block::lines_changed).sum()
    }

    /// Whether the session's last recorded block is a failure — the matrix
    /// leads a dying row with `╳` rather than `√`.  A cancelled turn is not
    /// one: the work broke off either way, but the human stopped it.
    pub(super) fn last_is_error(&self) -> bool {
        self.fold
            .blocks()
            .iter()
            .rev()
            .find(|block| !silent(block.kind()))
            .is_some_and(|block| {
                matches!(
                    block.kind(),
                    record::BlockKind::Error { .. }
                        | record::BlockKind::ProviderError { .. }
                        | record::BlockKind::Stalled { .. }
                )
            })
    }

    /// `(blocks, rows, bytes)` for the `/resources` fold, read off the render
    /// memos as of the last paint — a read of display state, never a
    /// re-render.
    pub(super) fn probe_figures(&self) -> (u64, u64, u64) {
        let (mut rows, mut bytes) = (0u64, 0u64);
        for row in self.rows() {
            rows += 1;
            bytes += row.bytes() as u64;
        }
        (self.blocks.len() as u64, rows, bytes)
    }

    /// Enter `state`, restarting the clock and the streamed count.  Re-entering
    /// the state already held is a no-op: a turn that re-drives the same wait
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
        *self = Self::new(log_path, agent, false, self.thinking);
    }

    /// Whether this view has been evicted — the one bit that says its tab has
    /// left the bar, since the two happen together.
    pub(super) fn tombstoned(&self) -> bool {
        self.tombstoned
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
        self.retire(self.blocks.len());
        self.answer = String::new();
        self.thinking_line = String::new();
        self.pins = Vec::new();
    }

    /// Write the provisional tail past the retired prefix and flush, so
    /// `user.log` reads as the whole session right now — what `/export` copies
    /// and what session end leaves behind.  The caller owns the I/O error policy.
    ///
    /// # Errors
    /// Returns the write's own error.
    pub(super) fn flush_log(&mut self) -> io::Result<&Path> {
        let from = self.log.seeded.min(self.blocks.len());
        self.log.flush(&self.blocks[from..], self.agent)?;
        Ok(&self.log_path)
    }

    // ── pinned state (the register) ────────────────────────────────────────
    // The in-place analogue of a scrollback block: a pin writes a keyed slot,
    // touching neither screen nor log — pinned state is ambient, not scrollback.

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

    /// Append chrome at the tail.  A chrome block is a barrier like any
    /// other, so it ends whatever group the tail held.
    pub(super) fn push_chrome(&mut self, chrome: Chrome) {
        self.blocks.push(Block::chrome(chrome, None));
    }

    /// Stream a live thinking delta into [`Self::thinking_line`] — the open
    /// line [`Self::live_tail`] draws, which the delta's own newline retires
    /// as the worker's record of that line lands.
    pub(super) fn push_thinking(&mut self, text: &str) {
        carry(&mut self.thinking_line, text);
    }

    // ── the mirror ────────────────────────────────────────────────────────

    /// Draw the block the fold opened at `at`: tail-merge into the group
    /// standing there, or push what the incident reads as on its own.
    fn opened(&mut self, at: usize) {
        let Some(block) = self.fold.blocks().get(at) else {
            return;
        };
        let seq = block.id().seq();
        let kind = block.kind();
        let ral = match kind {
            record::BlockKind::ToolCall { tool, cmd, .. } if tool == "ral" => Some(cmd.clone()),
            _ => None,
        };
        let prose = matches!(kind, record::BlockKind::Answer { .. });
        let items = self.items(kind, seq);
        if let Some(cmd) = ral {
            self.last_ral_cmd = Some(cmd);
        }
        for item in items {
            self.absorb(seq, item);
        }
        if prose && let Some(tail) = self.blocks.len().checked_sub(1) {
            self.grain(tail);
        }
    }

    /// Grow the block the fold's tail row is drawn in — its prose, or its
    /// group's last stretch of thinking — to the text the memo now holds.
    fn grew(&mut self, id: BlockId) {
        let Some(block) = self.fold.blocks().iter().rev().find(|b| b.id() == id) else {
            return;
        };
        let (text, prose) = match block.kind() {
            record::BlockKind::Answer { text } => (text.clone(), true),
            record::BlockKind::Thinking { text } => (text.clone(), false),
            _ => return,
        };
        let ink = prose.then(|| self.fidelity(&text));
        // The fold grows only its own tail row, whose home is the last block
        // a record authored — past whatever chrome was drawn beside it since.
        let Some(at) = self.blocks.iter().rposition(|b| b.seq().is_some()) else {
            return;
        };
        self.blocks[at].set_text(text);
        if let Some(ink) = ink {
            self.blocks[at].set_fidelity(ink);
            self.grain(at);
        }
    }

    /// Stamp the result magnitude the fold patched onto the call it names.
    fn patched(&mut self, call: BlockId) {
        let Some(n) = self
            .fold
            .blocks()
            .iter()
            .rev()
            .find(|b| b.id() == call)
            .and_then(|b| match b.kind() {
                record::BlockKind::ToolCall { result_lines, .. } => *result_lines,
                _ => None,
            })
        else {
            return;
        };
        let at = call.seq();
        for block in self.blocks.iter_mut().rev() {
            if block.measure(at, n) {
                break;
            }
        }
    }

    /// Weigh the deliberation above the prose block at `at` against it: the
    /// grain's denominator is the mass of the prose that deliberation became,
    /// which the commit precedes and the view therefore measures.
    fn grain(&mut self, at: usize) {
        let Some(chars) = self.blocks[at].prose_chars() else {
            return;
        };
        if let Some(above) = at.checked_sub(1) {
            self.blocks[above].set_answer_chars(chars);
        }
    }

    /// Take `item` into the group it belongs to, or draw it as its own block.
    fn absorb(&mut self, seq: Seq, item: Item) {
        match item {
            // An effect belongs to the call that issued it, and a redirect
            // writes at the seam mid-call: so the walk passes whatever barrier
            // landed since — the `▎ write` card above all — back to the
            // nearest call.  `read a · write b · read c` is one run.
            Item::Member(mut member @ Member::Effect(_)) => {
                for block in self.blocks.iter_mut().rev() {
                    match block.admit(member) {
                        None => return,
                        Some(back) => member = back,
                    }
                }
                self.open(seq, member);
            }
            // Deliberation and work grow the tail alone: a barrier between
            // them is a boundary, not a seam.
            Item::Member(member) => {
                let spare = match self.blocks.last_mut() {
                    Some(tail) => tail.admit(member),
                    None => Some(member),
                };
                if let Some(member) = spare {
                    self.open(seq, member);
                }
            }
            // Consecutive edits to one file are one change: a surfaced diff
            // still standing at the tail grows rather than stacking a second
            // card.  A write is offered no such merge — two writes to one path
            // are two facts.
            Item::Barrier(BlockKind::Card {
                card,
                landing: Landing::Surfaced,
                at,
            }) => {
                let spare = match self.blocks.last_mut() {
                    Some(tail) => tail.merge_diff(card),
                    None => Some(card),
                };
                if let Some(card) = spare {
                    self.blocks.push(Block::new(
                        BlockKind::Card {
                            card,
                            landing: Landing::Surfaced,
                            at,
                        },
                        Some(seq),
                    ));
                }
            }
            Item::Barrier(kind) => self.blocks.push(Block::new(kind, Some(seq))),
        }
    }

    /// The block a member with no group to join reads as: deliberation and
    /// work each open one, an effect that reached the mirror with no call
    /// belongs to none and renders alone, and a turn rule outside a burst is
    /// the boundary it draws.
    fn open(&mut self, seq: Seq, member: Member) {
        let block = match member {
            Member::Thinking(_) | Member::Call(_) => Block::group(member, self.thinking, Some(seq)),
            Member::Effect(what) => {
                let place = landing(&what).expect("an effect member has an effect landing");
                Block::new(BlockKind::card(observation_card(&what), place), Some(seq))
            }
            Member::Turn => Block::chrome(Chrome::Turn, Some(seq)),
        };
        self.blocks.push(block);
    }

    /// Trim the mirror's head to the fold's own window, retiring what goes
    /// into `user.log` — the one bound, and the moment a block becomes
    /// durable.
    ///
    /// Chrome inherits the expiry of the block it was drawn after, having no
    /// commit of its own; chrome drawn before any block — the banner — lives
    /// exactly as long as the fold still holds the session's opening block.
    fn trim(&mut self) {
        let Some(first) = self.fold.blocks().first().map(|b| b.id().seq()) else {
            return;
        };
        let mut expired = self.fold.origin() != Some(first);
        let mut n = 0;
        for block in &self.blocks {
            if let Some(seq) = block.seq() {
                expired = seq < first;
            }
            if !expired {
                break;
            }
            n += 1;
        }
        if n > 0 {
            self.retire(n);
        }
    }

    /// Write the mirror's first `n` blocks into the durable prefix and drop
    /// them.
    fn retire(&mut self, n: usize) {
        let gone: Vec<Block> = self.blocks.drain(..n).collect();
        let seeded = self.log.seeded.min(n);
        self.log.seeded -= seeded;
        self.log.retire(&gone[seeded..], self.agent);
    }

    /// The epistemic signal prose is stamped with: the turn's context
    /// pressure, and how far this text merely restates the script it followed.
    fn fidelity(&self, text: &str) -> Fidelity {
        Fidelity {
            context: self.context_floor(),
            echo: self
                .last_ral_cmd
                .as_deref()
                .map_or(0, |cmd| super::fidelity::echo_delta(text, cmd)),
        }
    }

    /// The turn's degradation floor, graded against the session's billed input.
    /// Both terms are the fold's own: the model in force names the denominator
    /// through the catalog.
    fn context_floor(&self) -> u8 {
        let window = self
            .fold
            .model()
            .and_then(|(model, _)| crate::provider::pricing::context_window(model));
        super::fidelity::context_floor(self.fold.usage().input, window)
    }

    // ── interaction ──────────────────────────────────────────────────────

    /// The block and part owning visual row `row` — valid only against the
    /// most recent [`Self::render_window`].
    pub(super) fn block_at(&self, row: usize) -> Option<Hit> {
        let mut left = row;
        for (block, rows) in self.screen() {
            if left < rows.len() {
                let trimmed = self.blocks[block].rendered().len() - rows.len();
                let part = self.blocks[block].part_at(left + trimmed);
                return Some(Hit { block, part });
            }
            left -= rows.len();
        }
        None
    }

    /// The first visual row of `hit`'s part — the one carrying its rail glyph.
    pub(super) fn block_head(&self, hit: Hit) -> Option<usize> {
        let mut at = 0;
        for (block, rows) in self.screen() {
            if block == hit.block {
                let trimmed = self.blocks[block].rendered().len() - rows.len();
                let start = self.blocks[block]
                    .part_start(hit.part)
                    .saturating_sub(trimmed);
                return (start < rows.len()).then_some(at + start);
            }
            at += rows.len();
        }
        None
    }

    /// Rendered cell width of visual row `row` — its content's extent, not the
    /// pane's, so a gesture binds tight to the text and ignores the dead margin.
    pub(super) fn row_width(&self, row: usize) -> Option<usize> {
        self.rows().nth(row).map(Row::width)
    }

    /// Whether `hit` names a dialable part — a property of its kind, not its
    /// rung, so a click on it claims the gesture even at the ceiling.
    pub(super) fn block_dialable(&self, hit: Hit) -> bool {
        self.blocks
            .get(hit.block)
            .is_some_and(|b| b.dialable(hit.part))
    }

    /// Cycle `hit`'s part a rung on — the click-on-rail affordance.
    pub(super) fn cycle_block(&mut self, hit: Hit) -> bool {
        self.blocks
            .get_mut(hit.block)
            .is_some_and(|b| b.dial(hit.part))
    }

    /// Set the rung every group's deliberation reads at: the groups on screen
    /// move now, and the ones still to come are born there.
    pub(super) fn set_thinking_level(&mut self, level: Detail) {
        self.thinking = level;
        for block in &mut self.blocks {
            block.set_thinking(level);
        }
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
        let slice = |row: usize, a, b| self.rows().nth(row).map(|r| plain_slice(r, a, b));
        if lo.row == hi.row {
            return slice(lo.row, lo.col, hi.col).unwrap_or_default();
        }
        let interior = self
            .rows()
            .skip(lo.row + 1)
            .take(hi.row.saturating_sub(lo.row + 1))
            .map(Row::plain);
        slice(lo.row, lo.col, u16::MAX)
            .into_iter()
            .chain(interior)
            .chain(slice(hi.row, 0, hi.col))
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// The assistant's latest reply as raw markdown — what `/copy` copies.
    /// The fold joins consecutive answer records, so a trailing run of answer
    /// blocks exists only across the kinds that draw nothing; a call, card, or
    /// prompt bounds it, and ending the turn on one leaves the reply empty.
    pub(super) fn latest_reply_md(&self) -> String {
        let mut tail: Vec<&str> = self
            .fold
            .blocks()
            .iter()
            .rev()
            .filter(|block| !silent(block.kind()))
            .map_while(|block| match block.kind() {
                record::BlockKind::Answer { text } => Some(text.as_str()),
                _ => None,
            })
            .collect();
        tail.reverse();
        tail.concat().trim().to_owned()
    }

    // ── rendering ────────────────────────────────────────────────────────

    /// The screen, block by block: each block's memoised rows with the leading
    /// blanks the seam collapses against an already-blank tail dropped.  One
    /// walk, and every row index the frontend hands around is measured against
    /// it.
    fn screen(&self) -> impl Iterator<Item = (usize, &[Row])> {
        self.blocks
            .iter()
            .enumerate()
            .scan(false, |prev_blank, (i, block)| {
                let rows = seam(*prev_blank, block.rendered());
                *prev_blank = rows.last().map_or(*prev_blank, Row::is_blank);
                Some((i, rows))
            })
    }

    fn rows(&self) -> impl Iterator<Item = &Row> {
        self.screen().flat_map(|(_, rows)| rows)
    }

    /// Rows the first `blocks` blocks put on screen, and whether the last of
    /// them is blank — what the live line's seam reads.
    fn upto(&self, blocks: usize) -> (usize, bool) {
        let (mut rows, mut blank) = (0, false);
        for (_, seg) in self.screen().take(blocks) {
            rows += seg.len();
            blank = seg.last().map_or(blank, Row::is_blank);
        }
        (rows, blank)
    }

    /// The tail as it reads right now: the lane's open line rendered *inside*
    /// the block that will absorb it, together with the rows standing above it.
    ///
    /// One rendering path serves the live text and the committed text, because
    /// they are the same block — the open line is simply the part of it no
    /// record covers yet.  So the markdown context the line sits inside (an
    /// open fence, a list) is the block's own, and the record that completes
    /// the line changes the text without changing the picture.
    fn live_tail(&self, width: u16) -> Tail {
        let answering = !self.answer.is_empty();
        let open = if answering {
            &self.answer
        } else {
            &self.thinking_line
        };
        let all = self.blocks.len();
        if open.is_empty() {
            let (rows, _) = self.upto(all);
            return Tail {
                blocks: all,
                rows,
                live: Vec::new(),
            };
        }
        // The block the line continues: the one already drawn at the tail,
        // when it holds this lane.  What is on screen is the authority — this
        // splice and the fold's rule for growing a block must agree, and they
        // do, both being "the run of records of one lane".
        let joins = self.blocks.last().is_some_and(if answering {
            Block::open_prose
        } else {
            Block::open_thinking
        });
        let blocks = all - usize::from(joins);
        let (rows, prev_blank) = self.upto(blocks);
        let fresh = (!joins).then(|| {
            if answering {
                let ink = self
                    .blocks
                    .last()
                    .map_or_else(Fidelity::default, Block::fidelity);
                Block::prose(String::new(), ink, continues_prose(&self.blocks), None)
            } else {
                // No prose has followed the run yet — that is what makes it live.
                Block::group(Member::Thinking(String::new()), self.thinking, None)
            }
        });
        let tail = fresh.as_ref().unwrap_or_else(|| &self.blocks[blocks]);
        // Rendered with the newline the line is about to gain, since markdown
        // reads an unterminated last line differently from a whole one.
        let live = tail.live(width, self.agent, &format!("{open}\n"));
        Tail {
            blocks,
            rows,
            live: seam(prev_blank, &live).to_vec(),
        }
    }

    /// Fill every block's render memo at `width`, so one walk of
    /// [`Self::screen`] sees the whole buffer.
    fn render_blocks(&mut self, width: u16) {
        let agent = self.agent;
        for block in &mut self.blocks {
            block.fill(width, agent);
        }
    }

    /// The visible slice at `width` × `height`.  While `sticky`, `offset` is
    /// pinned to the tail; otherwise it is clamped to `max_off` and `sticky`
    /// re-arms once it reaches the bottom.
    pub(super) fn render_window(&mut self, width: u16, height: usize) -> RenderWindow {
        let content = width.min(READ_W);
        self.render_blocks(content);
        let tail = self.live_tail(content);
        let total = tail.rows + tail.live.len();
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
        let lines: Vec<Row> = self
            .screen()
            .take(tail.blocks)
            .flat_map(|(_, rows)| rows)
            .chain(tail.live.iter())
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
}

/// A fold block that draws nothing: a harness result is already said by its
/// act row, and a nudge is the agent steering itself.  Neither breaks a prose
/// run nor stands between the matrix and the last block it can see.
fn silent(kind: &record::BlockKind) -> bool {
    matches!(
        kind,
        record::BlockKind::Nudge { .. } | record::BlockKind::HarnessResult { .. }
    )
}

/// Whether a prose block appended after `blocks` continues a paragraph run: a
/// block that renders nothing does not break one.
fn continues_prose(blocks: &[Block]) -> bool {
    blocks
        .iter()
        .rev()
        .find(|b| b.renders())
        .is_some_and(Block::is_prose)
}

// ── the record vocabulary, decoded once ─────────────────────────────────────

impl Scrollback {
    /// One fold block, decoded into what it contributes to the mirror — a
    /// card built here once rather than at every frame, an effect as the bare
    /// fact its call will group.  A kind that draws nothing contributes
    /// nothing.
    fn items(&self, kind: &record::BlockKind, seq: Seq) -> Vec<Item> {
        use record::BlockKind as K;
        let chrome = |c: Chrome| vec![Item::Barrier(BlockKind::Chrome(c))];
        let note = |text: &str| chrome(Chrome::Note(text.to_owned()));
        match kind {
            K::Thinking { text } => vec![Item::Member(Member::Thinking(text.clone()))],
            K::Prompt { text } => chrome(Chrome::Prompt(text.clone())),
            K::Answer { text } => vec![Item::Barrier(BlockKind::Prose {
                src: text.clone(),
                fidelity: self.fidelity(text),
                continues: continues_prose(&self.blocks),
            })],
            K::ToolCall {
                cmd,
                summary,
                result_lines,
                ..
            } => match summary {
                Some(summary) => {
                    let mut call =
                        group::Call::open(seq, summary.clone(), cmd.clone(), self.context_floor());
                    if let Some(n) = result_lines {
                        call.measure(*n);
                    }
                    vec![Item::Member(Member::Call(call))]
                }
                None => vec![Item::Barrier(BlockKind::Tool {
                    details: (cmd != crate::shell_eval::tools::ral::INVALID_INPUT)
                        .then(|| cmd.clone()),
                })],
            },
            K::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            } => vec![Item::Barrier(BlockKind::Act {
                verb: verb.clone(),
                subject: subject.clone(),
                payload: payload.clone(),
                failed: *failed,
                at: Detail::Summary,
            })],
            K::SubagentDone {
                name,
                error,
                elapsed_ms,
            } => vec![Item::Barrier(BlockKind::Subagent {
                name: name.clone(),
                error: error.clone(),
                elapsed: Duration::from_millis(*elapsed_ms),
            })],
            K::Observation { value } => observation_items(value.clone()),
            K::Card { card } => vec![surfaced(card.clone())],
            // Not a card: a settled block is announced, not bounded — a line
            // on the rail, exactly as a subagent's answer arrives.  The shape
            // holds however it settled: `╳` is the turn's own failure (a
            // provider error, a stall), never a nonzero exit, which reads as a
            // red status in the row here just as it does on an exec.
            K::Done { outcome } => chrome(Chrome::Settled(card::settled_spans(
                &card::to_card_done(outcome),
            ))),
            K::Notice { notice } => {
                vec![surfaced(card::notice_card(&card::to_card_notice(notice)))]
            }
            K::Context { turns, evicted } => {
                vec![surfaced(card::context_rows_card(turns, *evicted))]
            }
            K::Cancelled => chrome(Chrome::Cancelled),
            K::Error { text } => chrome(Chrome::Error(text.clone())),
            K::Nudge { .. } | K::HarnessResult { .. } => Vec::new(),
            K::ProviderError { error } => chrome(Chrome::ProviderError(error.clone())),
            K::Stalled { error } => chrome(Chrome::Stalled(error.clone())),
            K::SystemNote { text } => note(text),
            K::Turn { .. } => vec![Item::Member(Member::Turn)],
            K::ContextEdited { op, by } => {
                let authority = match by {
                    EditAuthority::Model => "model",
                    EditAuthority::User => "user",
                    EditAuthority::Harness => "harness",
                };
                let text = match op {
                    ContextOp::Evict { through, .. } => {
                        format!("[context evicted through turn {through} ({authority})]")
                    }
                    ContextOp::Drop { exchanges } => {
                        let list = exchanges
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(", ");
                        format!("[context dropped exchange(s) {list} ({authority})]")
                    }
                };
                note(&text)
            }
        }
    }
}

fn surfaced(card: Card) -> Item {
    Item::Barrier(BlockKind::card(card, Landing::Surfaced))
}

/// Decode one [`Display::Observation`](crate::record::Display::Observation),
/// through the same [`landing`] the live rail draws from — a rendering, never
/// recorded, built as the fact arrives.
fn observation_items(value: ral_core::serial::FOValue) -> Vec<Item> {
    let Some(obs) = observation_from_wire(value) else {
        return Vec::new();
    };
    observed_items(&obs.what)
}

fn observed_items(what: &Observed) -> Vec<Item> {
    let Some(place) = landing(what) else {
        return Vec::new();
    };
    match place {
        Landing::Effect => vec![Item::Member(Member::Effect(what.clone()))],
        Landing::Write => vec![Item::Barrier(BlockKind::card(
            observation_card(what),
            Landing::Write,
        ))],
        Landing::Surfaced => vec![surfaced(observation_card(what))],
        Landing::Announced => vec![Item::Barrier(BlockKind::Chrome(Chrome::Spawned(
            card::observation_spans(what),
        )))],
    }
}

// ── the sole live producer ──────────────────────────────────────────────────

impl Scrollback {
    /// A transient never authors scrollback: [`Transient::Token`] and
    /// [`Transient::Thinking`] only carry their lane's open line, which the
    /// live tail draws ([`Self::live_tail`], [`Self::push_thinking`]).  Every
    /// line the worker has finished arrives as a record through [`Self::fact`]
    /// instead — a mirror never mints a [`record::Block`] of its own — and
    /// grows the block it belongs to.
    ///
    /// [`Transient::Born`], [`Transient::Died`], and [`Transient::Resources`]
    /// reach no-ops here: they need the tabs a bare `Scrollback` cannot see, so
    /// `App::transient` intercepts and answers them itself, ahead of this call.
    pub(super) fn transient(&mut self, t: &Transient) {
        match t {
            // Prose ends the thinking run, on this side exactly as on the
            // worker's: the run's tail records where the prose begins, so the
            // line it left open is covered by then, and at most one lane is
            // ever open at once.
            Transient::Token(text) => {
                self.note_streamed(text.chars().count());
                self.thinking_line.clear();
                carry(&mut self.answer, text);
            }
            Transient::Thinking(text) => {
                self.note_streamed(text.chars().count());
                self.push_thinking(text);
            }
            Transient::State(state) => self.set_state(*state),
            Transient::Cleared => self.reset(),
            Transient::StopReason(raw) => {
                self.push_chrome(Chrome::StopReason(raw.clone()));
            }
            Transient::Pin { key, card } => self.set_pin(key.clone(), card.clone()),
            Transient::Unpin { key } => self.drop_pin(key),
            Transient::Fault { text } => {
                self.push_chrome(Chrome::Error(text.clone()));
            }
            // The turn's stream is sealed: the worker has recorded every
            // line it means to, tails included, so an open line still
            // standing here stands for text the producer chose not to record
            // — a cancelled deliberation, a whitespace-only tail.  Dropping it is
            // what keeps a seat from outliving the turn it was reading.
            Transient::Boundary => {
                self.answer.clear();
                self.thinking_line.clear();
            }
            Transient::Born { .. } | Transient::Died | Transient::Resources { .. } => {}
        }
    }

    /// Step the fold beside the mirror and draw what the step reports.  A
    /// record moves one block, so the work here is that block's and not the
    /// window's around it — the difference between a session that costs more
    /// the longer it runs and one that does not.
    ///
    /// The record is handed over only to step that memo: the drawing below
    /// reads [`record::BlockKind`] off the fold, never the record vocabulary,
    /// so a second hand-rolled projection cannot compile.
    pub(super) fn fact(&mut self, rec: &record::Recorded<record::Record>) {
        // `Blocks::step` never actually refuses a live commit — no arm returns
        // `Err` — so a refusal here would mean the fold learned a new failure
        // mode this mirror does not yet know to report.
        let delta = self
            .fold
            .step(rec)
            .expect("the view fold never refuses a live commit");
        match delta {
            Delta::Opened(_) => {
                if let Some(at) = self.fold.blocks().len().checked_sub(1) {
                    self.opened(at);
                }
            }
            Delta::Grew(id) => self.grew(id),
            Delta::Patched(id) => self.patched(id),
            Delta::Quiet => {}
        }
        self.trim();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::{Display, Locus, Record, Recorded};

    fn scrollback() -> Scrollback {
        use std::sync::atomic::{AtomicUsize, Ordering};
        static N: AtomicUsize = AtomicUsize::new(0);
        let path = std::env::temp_dir().join(format!(
            "exarch-scrollback-test-{}-{}.log",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed),
        ));
        Scrollback::new(path, AgentSlot(0), false, Detail::Full)
    }

    fn rail_rows(rows: &[Row], glyph: &str) -> Vec<usize> {
        rows.iter()
            .enumerate()
            .filter_map(|(i, row)| (row.gutter() == glyph).then_some(i))
            .collect()
    }

    /// A running counter's worth of facts, so a test can interleave chrome
    /// between commits without two calls colliding on the same `Seq`.
    struct Stream(u64);

    impl Stream {
        fn new() -> Self {
            Self(0)
        }

        fn land(&mut self, sb: &mut Scrollback, record: Record) -> Seq {
            self.0 += 1;
            let seq = Seq::new(self.0);
            sb.fact(&Recorded::new(Locus::placeholder(seq), record));
            seq
        }

        fn say(&mut self, sb: &mut Scrollback, text: &str) {
            let _ = self.land(sb, Record::Display(Display::Answer { text: text.into() }));
        }

        fn call(&mut self, sb: &mut Scrollback, intent: &str, cmd: &str) -> Seq {
            self.land(
                sb,
                Record::Display(Display::ToolCall {
                    tool: "ral".into(),
                    cmd: cmd.into(),
                    summary: Some(intent.into()),
                }),
            )
        }
    }

    fn text(sb: &mut Scrollback) -> String {
        sb.render_window(READ_W, 60)
            .lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Re-pinning overwrites a slot in place, `drop_pin` removes one, `reset`
    /// wipes the lot — the same generation discipline that bounds scrollback.
    #[test]
    fn pins_overwrite_in_place_and_keep_insertion_order() {
        use crate::bus::card::Mark;
        let raw = |b: &[u8]| Card(vec![Mark::Raw { bytes: b.to_vec() }]);
        let keys = |sb: &Scrollback| sb.pins().iter().map(|(k, _)| k.clone()).collect::<Vec<_>>();
        let mut sb = scrollback();
        sb.set_pin("tasks".into(), raw(b"v1"));
        sb.set_pin("build".into(), raw(b"ok"));
        assert_eq!(keys(&sb), ["tasks", "build"]);

        sb.set_pin("tasks".into(), raw(b"v2"));
        assert_eq!(keys(&sb), ["tasks", "build"]);
        assert!(matches!(&sb.pins()[0].1.0[..], [Mark::Raw { bytes }] if bytes == b"v2"));

        sb.drop_pin("tasks");
        assert_eq!(keys(&sb), ["build"]);
        sb.reset();
        assert!(sb.pins().is_empty(), "reset wipes the register");
    }

    /// The open line reads inside the block it will join, and the record that
    /// completes it changes the text without changing the picture — the one
    /// claim that says live and committed are the same rendering.
    #[test]
    fn the_record_that_completes_a_line_does_not_change_the_picture() {
        let mut sb = scrollback();
        let mut log = Stream::new();

        // A fence opens and a line of code streams: the fence is recorded, the
        // line is still open.
        sb.transient(&Transient::Token("```ral\nlet x = 1".into()));
        log.say(&mut sb, "```ral\n");
        let live = format!("{:?}", sb.render_window(READ_W, 24).lines);
        assert!(
            sb.render_window(READ_W, 24)
                .lines
                .last()
                .expect("a row")
                .plain()
                .contains("let x = 1"),
            "the open line reads where its block will hold it"
        );

        // Its record lands and the printer's own newline rule closes it.
        sb.transient(&Transient::Token("\n".into()));
        log.say(&mut sb, "let x = 1\n");
        assert_eq!(
            format!("{:?}", sb.render_window(READ_W, 24).lines),
            live,
            "the record changed the text and not the picture"
        );
    }

    /// At most one lane is ever open, because prose ends the thinking run on
    /// the printer's side exactly as it does on the worker's — and the turn's
    /// boundary clears whatever is left.
    #[test]
    fn prose_ends_the_open_thinking_line_and_the_boundary_clears_both() {
        let mut sb = scrollback();
        sb.transient(&Transient::Thinking("considering the shape".into()));

        let all = text(&mut sb);
        assert!(
            all.contains("considering the shape"),
            "the thinking lane's open line reads on its own rail: {all:?}"
        );
        assert_eq!(
            rail_rows(&sb.render_window(READ_W, 60).lines, "∴ ").len(),
            1,
            "and wears the thinking rail: {all:?}"
        );

        sb.transient(&Transient::Token("First words".into()));
        let all = text(&mut sb);
        assert!(
            !all.contains("considering the shape") && all.contains("First words"),
            "prose closed the run, whose tail the worker recorded at the same delta: {all:?}"
        );

        sb.transient(&Transient::Boundary);
        let all = text(&mut sb);
        assert!(
            !all.contains("First words"),
            "no open line outlives the turn it was read from: {all:?}"
        );
    }

    /// `scroll_down` clears `sticky`, so `render_window` takes the clamping
    /// branch — scrolling down at the bottom must not over-scroll past
    /// `max_off` and blank the rows below.
    #[test]
    fn scroll_down_while_sticky_clamps_to_max_off() {
        let mut sb = scrollback();
        for i in 0..10 {
            sb.push_chrome(Chrome::Prompt(format!(
                "block {i} line a\nblock {i} line b\nblock {i} line c"
            )));
        }
        let height = 10;
        let w0 = sb.render_window(READ_W, height);
        assert!(sb.sticky, "a fresh scrollback follows the tail");
        let max_off = w0.offset;
        sb.scroll_down(5);
        let w1 = sb.render_window(READ_W, height);
        assert_eq!(
            w1.offset, max_off,
            "scroll_down while sticky stays at max_off, not past it"
        );
        assert_eq!(
            w1.lines.len(),
            height,
            "the window fills every row — no blank space below the tail"
        );
        assert!(sb.sticky, "re-armed at the bottom after the clamp");
    }

    /// The committed deliberation renders in a sticky scrollback too, once its
    /// record lands past a full window of chrome.
    #[test]
    fn committed_thinking_stays_visible_in_sticky_scrollback() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        sb.push_chrome(Chrome::Prompt("hello cutie".into()));
        for i in 0..8 {
            sb.push_chrome(Chrome::Prompt(format!(
                "block {i} line a\nblock {i} line b"
            )));
        }
        sb.transient(&Transient::Thinking("considering the shape".into()));
        let live = sb.render_window(READ_W, 8);
        assert!(
            !rail_rows(&live.lines, "∴ ").is_empty(),
            "live thinking has its rail"
        );

        let _ = log.land(
            &mut sb,
            Record::Display(Display::Thinking {
                text: "considering the shape\n".into(),
            }),
        );
        let committed = sb.render_window(READ_W, 8);
        assert!(
            !rail_rows(&committed.lines, "∴ ").is_empty(),
            "committed thinking stays visible in a sticky scrollback: {:?}",
            committed.lines.iter().map(Row::plain).collect::<Vec<_>>()
        );
    }

    /// Eviction drops the scrollback and the pinned register, keeping only
    /// the fact that the view is dead; `log_path` is untouched, so the log
    /// stays readable.
    #[test]
    fn evict_to_tombstone_drops_scrollback_and_register() {
        let mut sb = scrollback();
        sb.push_chrome(Chrome::Note("hello".into()));
        sb.set_pin("k".into(), Card(Vec::new()));
        assert!(!sb.blocks.is_empty());

        let log_path = sb.log_path.clone();
        sb.evict_to_tombstone();

        assert_eq!(sb.log_path, log_path);
        assert!(sb.blocks.is_empty(), "the scrollback is dropped");
        assert!(sb.pins().is_empty(), "the pinned register is dropped");
    }

    /// Re-evicting is a harmless no-op: the view is already clean.
    #[test]
    fn evict_to_tombstone_is_idempotent() {
        let mut sb = scrollback();
        sb.push_chrome(Chrome::Error("boom".into()));
        sb.evict_to_tombstone();
        assert!(sb.blocks.is_empty());
        sb.evict_to_tombstone();
        assert!(sb.blocks.is_empty());
    }

    /// An act is a barrier: it ends the group before it, so it renders
    /// standalone under its own `↗` between two separate `▸` runs, never
    /// swallowed into a burst of reads.
    #[test]
    fn an_act_breaks_a_run_of_observations() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        let _ = log.call(&mut sb, "read the renderer", "read 'line.rs'");
        let _ = log.call(&mut sb, "read the block", "read 'block.rs'");
        let _ = log.land(
            &mut sb,
            Record::Display(Display::HarnessCall {
                verb: "message".into(),
                subject: Some("hunter".into()),
                payload: "focus on the renderer first".into(),
                failed: false,
            }),
        );
        let _ = log.call(&mut sb, "read the rail", "read 'rail.rs'");

        let w = sb.render_window(READ_W, 40);
        let act = rail_rows(&w.lines, "↗ ");
        assert_eq!(act.len(), 1, "the act renders exactly one rail row");
        let runs = rail_rows(&w.lines, "▸ ");
        assert_eq!(
            runs.len(),
            2,
            "the act splits the reads into two groups, not one"
        );
        assert!(
            runs[0] < act[0] && act[0] < runs[1],
            "the act holds its arrival position between the two runs"
        );
        // Its own row is three columns, not an intent line under a run's head.
        assert_eq!(
            w.lines[act[0]].plain(),
            "message      hunter              focus on the renderer first"
        );
    }

    /// A result addresses the call it names, so a call shielded by a later act
    /// still earns the bar its own result measured.
    #[test]
    fn a_result_stamps_the_call_it_names() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        let call = log.call(&mut sb, "read the renderer", "read 'line.rs'");
        let _ = log.land(
            &mut sb,
            Record::Display(Display::HarnessCall {
                verb: "cancel".into(),
                subject: Some("hunter".into()),
                payload: String::new(),
                failed: false,
            }),
        );
        let _ = log.land(
            &mut sb,
            Record::Display(Display::Result {
                text: "a line\n".repeat(40),
                call: BlockId::new(call),
            }),
        );

        let w = sb.render_window(READ_W, 40);
        let act = rail_rows(&w.lines, "↗ ");
        assert_eq!(w.lines[act[0]].plain(), "cancel       hunter");
        let run = w.lines[rail_rows(&w.lines, "▸ ")[0]].plain();
        assert!(
            run.ends_with(crate::tui::line::spark_glyph(Some(40))),
            "the `ral` call keeps the magnitude it earned: {run:?}"
        );
    }

    /// The printer never mints a block itself: the mirror only ever draws what
    /// the fold has reported.
    #[test]
    fn the_mirror_draws_what_the_fold_reports() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        let _ = log.land(
            &mut sb,
            Record::Display(Display::Prompt {
                text: "hello".into(),
            }),
        );
        log.say(&mut sb, "hi back");
        let all = text(&mut sb);
        assert!(all.contains("hello") && all.contains("hi back"), "{all:?}");
    }

    /// Deliberation and the work it ordered read as one group — one `∴` head
    /// over one `▸` run — and prose closes it, so the answer after opens its
    /// own block.
    #[test]
    fn deliberation_and_its_work_read_as_one_group() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        let _ = log.land(
            &mut sb,
            Record::Display(Display::Thinking {
                text: "weighing the shape\n".into(),
            }),
        );
        let _ = log.call(&mut sb, "read the renderer", "read 'line.rs'");
        let _ = log.land(&mut sb, Record::Display(Display::Turn { id: 1 }));
        let _ = log.call(&mut sb, "read the block", "read 'block.rs'");
        assert_eq!(sb.blocks.len(), 1, "one group holds the whole burst");

        log.say(&mut sb, "and so\n");
        assert_eq!(sb.blocks.len(), 2, "prose closes the group");
        let w = sb.render_window(READ_W, 60);
        let (thinking, run) = (rail_rows(&w.lines, "∴ "), rail_rows(&w.lines, "▸ "));
        assert_eq!(thinking.len(), 1, "one deliberation head");
        assert_eq!(run.len(), 1, "over one run of work");
        assert!(thinking[0] < run[0], "the deliberation is hoisted above it");
    }

    /// Two dials on one group: the parts move apart, and each row answers to
    /// the part it belongs to.
    #[test]
    fn a_group_carries_a_dial_per_part() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        let _ = log.land(
            &mut sb,
            Record::Display(Display::Thinking {
                text: "weighing the shape\n".into(),
            }),
        );
        let _ = log.call(&mut sb, "read the renderer", "read 'line.rs'");
        let _ = sb.render_window(READ_W, 60);

        let hit = |part| Hit { block: 0, part };
        assert!(sb.block_dialable(hit(Part::Thinking)));
        assert!(sb.block_dialable(hit(Part::Run)));
        let head = |part| sb.block_head(hit(part)).expect("both parts are on screen");
        assert!(
            head(Part::Thinking) < head(Part::Run),
            "the two heads are distinct rows"
        );
        assert_eq!(
            sb.block_at(head(Part::Run)).map(|h| h.part),
            Some(Part::Run),
            "a row in the run answers to the run's dial"
        );
        assert!(sb.cycle_block(hit(Part::Run)), "the run takes the dial");
    }

    /// `/thinking`'s two obligations: the groups already drawn move, and a
    /// group that arrives afterwards is born at the rung then in force.
    #[test]
    fn the_standing_thinking_rung_reaches_past_and_future_groups() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        let think = |log: &mut Stream, sb: &mut Scrollback, text: &str| {
            let _ = log.land(sb, Record::Display(Display::Thinking { text: text.into() }));
        };
        think(&mut log, &mut sb, "weighing the shape\n");
        sb.set_thinking_level(Detail::Summary);
        log.say(&mut sb, "the answer\n");
        think(&mut log, &mut sb, "and again\n");

        let w = sb.render_window(READ_W, 60);
        let all = w
            .lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            !all.contains("weighing the shape") && !all.contains("and again"),
            "both deliberations read as their headers alone: {all:?}"
        );
        sb.set_thinking_level(Detail::Full);
        let w = sb.render_window(READ_W, 60);
        let all = w
            .lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>()
            .join("\n");
        assert!(
            all.contains("weighing the shape") && all.contains("and again"),
            "and both open again together: {all:?}"
        );
    }

    /// Chrome drawn between two commits keeps its place: it is appended where
    /// it was drawn, and no later record moves it.
    #[test]
    fn chrome_holds_its_place_among_the_commits() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        let _ = log.land(
            &mut sb,
            Record::Display(Display::Prompt { text: "one".into() }),
        );
        sb.push_chrome(Chrome::Note("between".into()));
        let _ = log.land(
            &mut sb,
            Record::Display(Display::Prompt { text: "two".into() }),
        );

        let rendered = sb
            .render_window(READ_W, 60)
            .lines
            .iter()
            .map(Row::plain)
            .collect::<Vec<_>>();
        let at = |needle: &str| {
            rendered
                .iter()
                .position(|l| l.contains(needle))
                .expect("drawn")
        };
        assert!(
            at("one") < at("between") && at("between") < at("two"),
            "chrome drawn between two commits renders between them: {rendered:?}"
        );
    }

    // ── the transcript ──────────────────────────────────────────────────────

    /// Lines of `sb`'s `user.log` ending in `marker`, which is how a test asks
    /// whether a block reached the transcript, and how many times.
    fn logged(path: &Path, marker: &str) -> usize {
        fs::read_to_string(path)
            .expect("the transcript reads back")
            .lines()
            .filter(|l| l.ends_with(marker))
            .count()
    }

    /// The mirror is bounded; the transcript is not.  What the fold's window
    /// drops is on disk by the time it goes, so a session outliving that
    /// window still leaves a whole `user.log` behind.
    #[test]
    fn the_transcript_keeps_what_the_window_drops() {
        let mut sb = scrollback();
        let mut log = Stream::new();
        let total = record::BLOCKS_WINDOW + 50;
        for i in 0..total {
            let _ = log.land(
                &mut sb,
                Record::Forensic(crate::record::Forensic::SystemNote {
                    text: format!("marker {i}"),
                }),
            );
        }
        assert_eq!(
            sb.blocks.len(),
            record::BLOCKS_WINDOW,
            "the mirror is bounded by the fold's own window"
        );
        let path = sb
            .flush_log()
            .expect("the transcript flushes")
            .to_path_buf();
        let text = fs::read_to_string(&path).expect("the transcript reads back");
        let last = format!("marker {}", total - 1);
        assert_eq!(logged(&path, "marker 0"), 1, "the dropped head is on disk");
        assert_eq!(logged(&path, &last), 1, "and so is the resident tail");
        assert!(
            text.find("marker 0") < text.find(&last),
            "in the order they were written"
        );
    }

    /// A flush writes the resident mirror provisionally, so `/export`
    /// mid-session reads a whole transcript; the next one rewinds over it
    /// rather than writing the same blocks twice.
    #[test]
    fn a_second_flush_rewinds_the_provisional_tail() {
        let mut sb = scrollback();
        for i in 0..3 {
            sb.push_chrome(Chrome::Note(format!("marker {i}")));
        }
        let path = sb
            .flush_log()
            .expect("the transcript flushes")
            .to_path_buf();
        let once = fs::read_to_string(&path).expect("the transcript reads back");

        sb.push_chrome(Chrome::Note("marker 3".into()));
        sb.flush_log().expect("the transcript flushes");
        let twice = fs::read_to_string(&path).expect("the transcript reads back");

        assert!(twice.starts_with(&once), "the first flush is a prefix");
        assert_eq!(logged(&path, "marker 0"), 1, "written once, not twice");
        assert_eq!(logged(&path, "marker 3"), 1, "and the new block joins it");
    }

    /// A resumed session's mirror was rendered by the run that recorded it, so
    /// the continuation appends to that transcript instead of repeating it.
    #[test]
    fn a_resumed_transcript_continues_rather_than_repeats() {
        let mut first = scrollback();
        let mut log = Stream::new();
        let path = first.log_path.clone();
        let _ = log.land(
            &mut first,
            Record::Display(Display::Prompt { text: "one".into() }),
        );
        first.flush_log().expect("the transcript flushes");
        assert_eq!(logged(&path, "one"), 1);

        let replayed = std::mem::take(&mut first.fold);
        let mut resumed = Scrollback::new(path.clone(), AgentSlot(0), true, Detail::Full);
        resumed.seed(replayed);
        let _ = log.land(
            &mut resumed,
            Record::Display(Display::Prompt { text: "two".into() }),
        );
        resumed.flush_log().expect("the transcript flushes");

        assert_eq!(
            logged(&path, "one"),
            1,
            "the seeded prefix is not rewritten"
        );
        assert_eq!(logged(&path, "two"), 1, "and the new block joins it");
    }
}
