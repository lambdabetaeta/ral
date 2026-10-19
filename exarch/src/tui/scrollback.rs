//! Per-session collapsible scrollback.
//!
//! A [`Scrollback`] holds a [`Mirror`] of the view fold — one [`Block`], the
//! reader's atom, per incident as it arrived — and draws it: the visual rows
//! of the screen, the viewport, and the live tail.  The whole alt-screen frame
//! is redrawn each tick, so scrollback is ours, not the host terminal's, and
//! every tab keeps its own scroll position.
//!
//! [`Scrollback::fact`] is the sole producer: it steps the mirror and acts on
//! what the step reports, and `App::transient` draws whatever is live-only —
//! [`Scrollback::push_chrome`], the chrome door, and [`Scrollback::push_token`]
//! and [`Scrollback::push_thinking`], which carry the open lines the live tail
//! draws.
//!
//! The mirror is a cursor over one log, and the channel only its fast path:
//! what the channel cannot vouch for — the log as it stood at birth, a gap —
//! it reads from the log itself ([`Scrollback::catch_up`]), and a record heard
//! twice folds once.

use super::block::{Block, Chrome, Member, seam};
use super::dial::{Detail, Part};
use super::fidelity::Fidelity;
use super::mirror::{Mirror, continues_prose};
use super::palette::{AgentSlot, READ_W};
use super::row::Row;
use super::select::{Cell, plain_slice};
use crate::card::Card;
use crate::provider::Usage;
use crate::record::{self, Cursor, Delta, Emitter as Recorder, Record, Recorded};

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
    /// This session's scrollback: the blocks and the fold they mirror.
    mirror: Mirror,
    /// The answer's open line: assistant text past the last newline, which is
    /// past the last [`Display::Answer`](crate::record::Display::Answer)
    /// record the worker has cut.  It is drawn inside the block it will join
    /// ([`Self::live_tail`]).
    answer: String,
    /// The thinking lane's open line — `answer`'s twin on the `∴` lane, drawn
    /// by [`Self::live_tail`] and grown by [`Self::push_thinking`].
    thinking_line: String,
    /// The log this mirrors, read back whenever the channel cannot vouch for
    /// the next record.
    recorder: Recorder,
    /// How far the mirror has read its log; `None` until it first does.
    cursor: Option<Cursor>,
    /// The chrome a clear, and the first read, open under: a root's session card.
    opening: Option<Card>,
    /// Top visible absolute row, per-scrollback so each tab keeps its place.
    offset: usize,
    /// Blocks drained from the head since this mirror began; every block index
    /// the API hands out counts from the mirror's origin, not from `blocks[0]`.
    head: usize,
    /// The screen rows that left with those blocks; row indices likewise.
    head_rows: usize,
    /// Follow the tail.  Cleared by a scroll either way, re-armed at the bottom.
    sticky: bool,
}

/// A window's place in the buffer, saved and restored around a preview.
#[derive(Clone, Copy)]
pub(super) struct Viewport {
    offset: usize,
    sticky: bool,
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

impl Scrollback {
    /// `thinking` is the standing rung a view is born at — owned by the caller
    /// ([`super::tabs::Tabs`]'s own), so a new view is never told twice, nor
    /// left disagreeing.  The mirror holds nothing until it reads `recorder`.
    pub(super) fn new(
        agent: AgentSlot,
        thinking: Detail,
        recorder: Recorder,
        opening: Option<Card>,
    ) -> Self {
        Self {
            mirror: Mirror::new(agent, thinking),
            answer: String::new(),
            thinking_line: String::new(),
            recorder,
            cursor: None,
            opening,
            offset: 0,
            head: 0,
            head_rows: 0,
            sticky: true,
        }
    }

    /// Mirror a resumed session's history: the records in the log's first
    /// `bytes`.
    pub(super) fn recall(&mut self, bytes: u64) {
        self.read_log(|rec| rec.locus().bytes().start < bytes);
    }

    /// Read the log past this mirror's cursor — all of it when the mirror has
    /// none — exactly as if it had arrived live.  Reports whether a clear
    /// went by.
    pub(super) fn catch_up(&mut self) -> bool {
        self.read_log(|_| true)
    }

    /// [`Self::catch_up`], stopping at the first record `keep` refuses.
    fn read_log(&mut self, keep: impl Fn(&Recorded<Record>) -> bool) -> bool {
        let mut cleared = false;
        let read = self
            .recorder
            .since(self.cursor)
            .and_then(|(from, records)| {
                if self.cursor.is_none() {
                    self.cursor = Some(from);
                    if let Some(card) = self.opening.clone() {
                        self.mirror.push_chrome(Chrome::Opening(card));
                    }
                }
                for rec in records {
                    let rec = rec?;
                    if !keep(&rec) {
                        break;
                    }
                    cleared |= self.step(&rec);
                }
                Ok(())
            });
        if let Err(e) = read {
            self.push_chrome(Chrome::Error(format!(
                "record.jsonl did not read back: {e}; this view may be missing some of it"
            )));
        }
        cleared
    }

    /// The log this view mirrors, for a command the frontend records into it.
    pub(super) fn recorder(&self) -> &Recorder {
        &self.recorder
    }

    pub(super) fn agent(&self) -> AgentSlot {
        self.mirror.agent
    }

    /// This session's spend — the matrix's per-agent readout.
    pub(super) fn usage(&self) -> Usage {
        self.mirror.fold.usage()
    }

    /// The focused gauge's `(last prompt size, context window)`.
    pub(super) fn gauge(&self) -> (u64, Option<u64>) {
        (
            self.mirror.fold.last_input(),
            self.mirror.fold.context_window(),
        )
    }

    /// Per-turn "had a tool call" flags, oldest first: one bool per turn
    /// block the fold holds, which the matrix renders `●` or `○`.
    pub(super) fn turns(&self) -> Vec<bool> {
        let mut turns: Vec<bool> = Vec::new();
        for block in self.mirror.fold.blocks() {
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
        self.mirror
            .blocks
            .iter()
            .filter_map(Block::lines_changed)
            .sum()
    }

    /// Whether the session's last recorded block is a failure — the matrix
    /// leads a dying row with `╳` rather than `√`.  A cancelled turn is not
    /// one: the work broke off either way, but the human stopped it.
    pub(super) fn last_is_error(&self) -> bool {
        self.mirror
            .fold
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
        (self.mirror.blocks.len() as u64, rows, bytes)
    }

    /// Append chrome at the tail.  A chrome block is a barrier like any
    /// other, so it ends whatever group the tail held.
    pub(super) fn push_chrome(&mut self, chrome: Chrome) {
        self.mirror.push_chrome(chrome);
    }

    /// Stream a live thinking delta into [`Self::thinking_line`] — the open
    /// line [`Self::live_tail`] draws, which the delta's own newline retires
    /// as the worker's record of that line lands.
    pub(super) fn push_thinking(&mut self, text: &str) {
        carry(&mut self.thinking_line, text);
    }

    // ── interaction ──────────────────────────────────────────────────────

    /// The block and part owning absolute row `row` — valid only against the
    /// most recent [`Self::render_window`].
    pub(super) fn block_at(&self, row: usize) -> Option<Hit> {
        let mut left = row.checked_sub(self.head_rows)?;
        for (block, rows) in self.screen() {
            if left < rows.len() {
                let trimmed = self.mirror.blocks[block].rendered().len() - rows.len();
                let part = self.mirror.blocks[block].part_at(left + trimmed);
                return Some(Hit {
                    block: self.head + block,
                    part,
                });
            }
            left -= rows.len();
        }
        None
    }

    /// The first absolute row of `hit`'s part — the one carrying its rail glyph.
    pub(super) fn block_head(&self, hit: Hit) -> Option<usize> {
        let want = hit.block.checked_sub(self.head)?;
        let mut at = self.head_rows;
        for (block, rows) in self.screen() {
            if block == want {
                let trimmed = self.mirror.blocks[block].rendered().len() - rows.len();
                let start = self.mirror.blocks[block]
                    .part_start(hit.part)
                    .saturating_sub(trimmed);
                return (start < rows.len()).then_some(at + start);
            }
            at += rows.len();
        }
        None
    }

    /// Rendered cell width of absolute row `row` — its content's extent, not the
    /// pane's, so a gesture binds tight to the text and ignores the dead margin.
    pub(super) fn row_width(&self, row: usize) -> Option<usize> {
        self.rows()
            .nth(row.checked_sub(self.head_rows)?)
            .map(Row::width)
    }

    /// Whether `hit` names a dialable part — a property of its kind, not its
    /// rung, so a click on it claims the gesture even at the ceiling.
    pub(super) fn block_dialable(&self, hit: Hit) -> bool {
        hit.block
            .checked_sub(self.head)
            .and_then(|i| self.mirror.blocks.get(i))
            .is_some_and(|b| b.dialable(hit.part))
    }

    /// Cycle `hit`'s part a rung on — the click-on-rail affordance.
    pub(super) fn cycle_block(&mut self, hit: Hit) -> bool {
        hit.block
            .checked_sub(self.head)
            .and_then(|i| self.mirror.blocks.get_mut(i))
            .is_some_and(|b| b.cycle(hit.part))
    }

    /// Set the rung every group's deliberation reads at: the groups on screen
    /// move now, and the ones still to come are born there.
    pub(super) fn set_thinking_level(&mut self, level: Detail) {
        self.mirror.set_thinking_level(level);
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

    /// Where the window stands, to put it back after a preview moved it.
    pub(super) fn viewport(&self) -> Viewport {
        Viewport {
            offset: self.offset,
            sticky: self.sticky,
        }
    }

    pub(super) fn set_viewport(&mut self, viewport: Viewport) {
        self.offset = viewport.offset;
        self.sticky = viewport.sticky;
    }

    /// Scroll so absolute row `row` is the top one; the next render clamps.
    pub(super) fn scroll_to(&mut self, row: usize) {
        self.sticky = false;
        self.offset = row;
    }

    // ── rewind ───────────────────────────────────────────────────────────

    /// The turn-opening prompts on screen, oldest first, with their turns.
    pub(super) fn prompts(&self) -> Vec<(u64, String)> {
        self.mirror
            .fold
            .prompts()
            .map(|(turn, text)| (turn, text.to_owned()))
            .collect()
    }

    /// The first absolute row `/rewind anchor` would cut — valid against the
    /// most recent [`Self::render_window`].  Chrome drawn before the cut
    /// block stays, as the mirror's trim would keep it.
    pub(super) fn cut_row(&self, anchor: u64) -> Option<usize> {
        let cut = self.mirror.fold.rewind_point(anchor)?.seq();
        let mut at = self.head_rows;
        for (block, rows) in self.screen() {
            if self.mirror.blocks[block]
                .seq()
                .is_some_and(|seq| seq >= cut)
            {
                return Some(at);
            }
            at += rows.len();
        }
        None
    }

    /// Plain text a drag selection copies, `lo <= hi` in buffer order; the rail
    /// glyph is stripped automatically.
    pub(super) fn selection_text(&self, lo: Cell, hi: Cell) -> String {
        let slice = |row: usize, a, b| {
            let rel = row.checked_sub(self.head_rows)?;
            self.rows().nth(rel).map(|r| plain_slice(r, a, b))
        };
        let rel = |row: usize| row.saturating_sub(self.head_rows);
        if lo.row == hi.row {
            return slice(lo.row, lo.col, hi.col).unwrap_or_default();
        }
        let interior = self
            .rows()
            .skip(rel(lo.row + 1))
            .take(rel(hi.row).saturating_sub(rel(lo.row + 1)))
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
            .mirror
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

    /// The screen, block by block: [`screen`] over this mirror's blocks.  One
    /// walk, and every row index the frontend hands around is measured
    /// against it.
    fn screen(&self) -> impl Iterator<Item = (usize, &[Row])> {
        screen(&self.mirror.blocks)
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
        let all = self.mirror.blocks.len();
        if open.is_empty() {
            let (rows, _) = self.upto(all);
            return Tail {
                blocks: all,
                rows,
                live: Vec::new(),
            };
        }
        // The block the line continues: the one at the tail, when the line's
        // record would grow it.  This splice and the fold's rule for growing a
        // block must agree, or the commit moves the picture.
        let joins = self.mirror.blocks.last().is_some_and(if answering {
            Block::takes_prose
        } else {
            Block::takes_thinking
        });
        let blocks = all - usize::from(joins);
        let (rows, prev_blank) = self.upto(blocks);
        let fresh = (!joins).then(|| {
            if answering {
                let ink = self
                    .mirror
                    .blocks
                    .last()
                    .map_or_else(Fidelity::default, Block::fidelity);
                Block::prose(
                    String::new(),
                    ink,
                    continues_prose(&self.mirror.blocks),
                    None,
                )
            } else {
                // No prose has followed the run yet — that is what makes it live.
                Block::group(Member::Thinking(String::new()), self.mirror.thinking, None)
            }
        });
        let tail = fresh
            .as_ref()
            .unwrap_or_else(|| &self.mirror.blocks[blocks]);
        // Rendered with the newline the line is about to gain, since markdown
        // reads an unterminated last line differently from a whole one.
        let live = tail.live(width, self.mirror.agent, &format!("{open}\n"));
        Tail {
            blocks,
            rows,
            live: seam(prev_blank, &live).to_vec(),
        }
    }

    /// Fill every block's render memo at `width`, so one walk of
    /// [`Self::screen`] sees the whole buffer.
    fn render_blocks(&mut self, width: u16) {
        let agent = self.mirror.agent;
        for block in &mut self.mirror.blocks {
            block.fill(width, agent);
        }
    }

    /// Preview the cut a third down the viewport, clamped at either end, and
    /// return its row.  Re-pins the offset every frame, so the wheel is inert.
    pub(super) fn render_cut_window(
        &mut self,
        width: u16,
        height: usize,
        anchor: u64,
    ) -> (RenderWindow, Option<usize>) {
        self.render_blocks(width.min(READ_W));
        let cut = self.cut_row(anchor);
        if let Some(row) = cut {
            self.scroll_to(row.saturating_sub(height / 3));
        }
        (self.render_window(width, height), cut)
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
        let rel = if self.sticky {
            max_off
        } else {
            let rel = self.offset.saturating_sub(self.head_rows).min(max_off);
            self.sticky = rel >= max_off;
            rel
        };
        self.offset = self.head_rows + rel;
        #[allow(
            clippy::cast_possible_truncation,
            reason = "scroll percentage already clamped to 0..=100"
        )]
        let scroll_pct = (max_off > 0).then(|| (rel * 100 / max_off).min(100) as u16);
        let lines: Vec<Row> = self
            .screen()
            .take(tail.blocks)
            .flat_map(|(_, rows)| rows)
            .chain(tail.live.iter())
            .skip(rel)
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

/// Each block's memoised rows with the leading blanks the seam collapses
/// against an already-blank tail dropped.
fn screen<'a>(
    blocks: impl IntoIterator<Item = &'a Block>,
) -> impl Iterator<Item = (usize, &'a [Row])> {
    blocks
        .into_iter()
        .enumerate()
        .scan(false, |prev_blank, (i, block)| {
            let rows = seam(*prev_blank, block.rendered());
            *prev_blank = rows.last().map_or(*prev_blank, Row::is_blank);
            Some((i, rows))
        })
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

// ── the sole live producer ──────────────────────────────────────────────────

impl Scrollback {
    /// Prose ends the thinking run, on this side exactly as on the
    /// worker's: the run's tail records where the prose begins, so the
    /// line it left open is covered by then, and at most one lane is
    /// ever open at once.
    pub(super) fn push_token(&mut self, text: &str) {
        self.thinking_line.clear();
        carry(&mut self.answer, text);
    }

    /// The turn's stream is sealed: the worker has recorded every
    /// line it means to, tails included, so an open line still
    /// standing here stands for text the producer chose not to record
    /// — a cancelled deliberation, a whitespace-only tail.  Dropping it is
    /// what keeps an open line from outliving its turn.
    pub(super) fn close_lanes(&mut self) {
        self.answer.clear();
        self.thinking_line.clear();
    }

    /// Mirror one fact heard live, first reading the log when it is not the
    /// next one this mirror holds: none yet, or a gap.  A record heard twice
    /// folds once.  Reports whether the mirror started over — a `/clear`.
    pub(super) fn fact(&mut self, rec: &Recorded<Record>) -> bool {
        match self
            .cursor
            .map(|cursor| rec.locus().seq().get().cmp(&cursor.next()))
        {
            Some(std::cmp::Ordering::Less) => false,
            Some(std::cmp::Ordering::Equal) => self.step(rec),
            _ => self.catch_up(),
        }
    }

    /// Step the mirror and account for what left it: the screen rows and
    /// blocks it retired, and — on a clear — the open lines and scroll state,
    /// under the opening again.  Reports whether the record was a clear.
    fn step(&mut self, rec: &Recorded<Record>) -> bool {
        self.cursor = self.cursor.map(|cursor| cursor.past(rec.locus()));
        let (delta, gone) = self.mirror.step(rec);
        let all: usize = screen(gone.iter().chain(&self.mirror.blocks))
            .map(|(_, rows)| rows.len())
            .sum();
        // What left the screen, not the gone blocks' own rows: the new head's
        // leading blank no longer collapses against them.
        self.head_rows += all - self.rows().count();
        self.head += gone.len();
        let cleared = delta == Delta::Cleared;
        if cleared {
            self.answer.clear();
            self.thinking_line.clear();
            self.sticky = true;
            if let Some(card) = self.opening.clone() {
                self.mirror
                    .blocks
                    .insert(0, Block::chrome(Chrome::Opening(card), None));
            }
        }
        cleared
    }
}

#[cfg(test)]
mod tests;
