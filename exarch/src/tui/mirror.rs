//! The record-to-[`Block`] half of the scrollback: a [`Mirror`] steps the view
//! fold beside the blocks it draws, with the deliberation and work of one burst
//! collapsed into a group as they land.  Nothing is re-derived: the live
//! scrollback and the `exarch export` renderer both read the blocks it builds.

use super::block::{Block, BlockKind, Chrome, Member};
use super::dial::Detail;
use super::fidelity::Fidelity;
use super::group;
use super::palette::AgentSlot;
use crate::card::{self, Card, Change, Landed, observation_card, observation_spans};
use crate::record::{self, BlockId, Blocks, Delta, Record, Recorded, Refusal, Seq};
use ral_core::data::datum::Datum as _;
use ral_core::shell::runtime::{Observation, Observed};
use std::time::Duration;

pub(super) struct Mirror {
    /// Oldest block first: the mirror of the fold's own window, and the only
    /// thing between the fold and the screen.
    pub(super) blocks: Vec<Block>,
    /// This mirror's own view-fold memo, stepped by every [`Self::try_step`].
    /// Its window is the one window: a block leaves the mirror exactly as the
    /// block it was built from leaves the fold.
    pub(super) fold: Blocks,
    /// Palette slot stamped onto every rail glyph; root is `0`.
    pub(super) agent: AgentSlot,
    /// The rung a group's deliberation is born at — `/thinking`'s standing
    /// datum, which [`Self::set_thinking_level`] moves on every group at once
    /// and every group still to arrive is born at.
    pub(super) thinking: Detail,
    /// The most recent `ral` script, which an answer's echo signal is read
    /// against — a fact about the stream, accumulated as the calls land.
    last_ral_cmd: Option<String>,
}

/// What one fold block contributes to the mirror, in arrival order.
enum Item {
    /// Something a group may take in.
    Member(Member),
    /// A barrier: its own block, ending whatever group stood at the tail.
    Barrier(BlockKind),
    /// A file change: it joins the run of changes standing at the tail, or
    /// opens one.
    Change(Change),
}

impl Mirror {
    pub(super) fn new(agent: AgentSlot, thinking: Detail) -> Self {
        Self {
            blocks: Vec::new(),
            fold: Blocks::default(),
            agent,
            thinking,
            last_ral_cmd: None,
        }
    }

    /// Step the fold beside the mirror and draw what the step reports.  A
    /// record moves one block, so the work here is that block's and not the
    /// window's around it — the difference between a session that costs more
    /// the longer it runs and one that does not.
    ///
    /// The record is handed over only to step that memo: the drawing below
    /// reads [`record::BlockKind`] off the fold, never the record vocabulary,
    /// so a second hand-rolled projection cannot compile.  Returns the blocks
    /// that left the mirror: the head [`Self::trim`] drops and, on a clear,
    /// everything held before it.  A rewind's blocks are gone, not retired.
    pub(super) fn try_step(
        &mut self,
        rec: &Recorded<Record>,
    ) -> Result<(Delta, Vec<Block>), Refusal> {
        let delta = self.fold.try_step(rec)?;
        let mut gone = Vec::new();
        match delta {
            Delta::Opened(_) => self.opened(),
            Delta::Grew(id) => self.grew(id),
            Delta::Patched(id) => self.patched(id),
            Delta::Rewound(cut) => {
                if let Some(at) =
                    cut.and_then(|id| self.blocks.iter().position(|b| b.seq() == Some(id.seq())))
                {
                    self.blocks.truncate(at);
                }
                self.opened();
            }
            Delta::Cleared => {
                gone = std::mem::take(&mut self.blocks);
                self.opened();
            }
            Delta::Quiet => {}
        }
        gone.extend(self.trim());
        Ok((delta, gone))
    }

    /// What the mirror still holds, once the last record is in.
    pub(super) fn finish(self) -> Vec<Block> {
        self.blocks
    }

    /// Append chrome at the tail.  A chrome block is a barrier like any
    /// other, so it ends whatever group the tail held.
    pub(super) fn push_chrome(&mut self, chrome: Chrome) {
        self.blocks.push(Block::chrome(chrome, None));
    }

    /// Set the rung every group's deliberation reads at: the groups on screen
    /// move now, and the ones still to come are born there.
    pub(super) fn set_thinking_level(&mut self, level: Detail) {
        self.thinking = level;
        for block in &mut self.blocks {
            block.set_thinking(level);
        }
    }

    /// Draw the block the fold just opened: tail-merge into the group
    /// standing there, or push what the incident reads as on its own.
    fn opened(&mut self) {
        let Some(block) = self.fold.blocks().last() else {
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

    /// Settle the call the fold patched with the verdict its result earned.
    fn patched(&mut self, call: BlockId) {
        let Some(verdict) = self
            .fold
            .blocks()
            .iter()
            .rev()
            .find(|b| b.id() == call)
            .and_then(|b| match b.kind() {
                record::BlockKind::ToolCall { verdict, .. } => *verdict,
                _ => None,
            })
        else {
            return;
        };
        let at = call.seq();
        for block in self.blocks.iter_mut().rev() {
            if block.settle(at, verdict) {
                break;
            }
        }
    }

    /// Weigh the deliberation above the prose block at `at` against it: the
    /// grain's denominator is the mass of the prose that deliberation became,
    /// which the record precedes and the view therefore measures.
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
            // landed since — the `▎` run of changes above all — back to the
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
            // What the work between two calls wrote reads as one patch: a
            // change joins the run standing at the tail, which effects pass
            // over on their way back to their call.
            Item::Change(change) => {
                let spare = match self.blocks.last_mut() {
                    Some(tail) => tail.admit_change(change),
                    None => Some(change),
                };
                if let Some(change) = spare {
                    let kind = BlockKind::Changes {
                        changes: vec![change],
                    };
                    self.blocks.push(Block::new(kind, Some(seq)));
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
            Member::Effect(e) => Block::new(
                BlockKind::Card {
                    card: observation_card(&Landed::Effect(e)),
                    framed: false,
                },
                Some(seq),
            ),
            Member::Turn => Block::chrome(Chrome::Turn, Some(seq)),
        };
        self.blocks.push(block);
    }

    /// Drop the mirror's head to the fold's own window — the one bound — and
    /// hand back what went.
    ///
    /// Chrome inherits the expiry of the block it was drawn after, having no
    /// record of its own; chrome drawn before any block — the banner — lives
    /// exactly as long as the fold still holds the session's opening block.
    fn trim(&mut self) -> Vec<Block> {
        let Some(first) = self.fold.blocks().first().map(|b| b.id().seq()) else {
            return Vec::new();
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
        self.blocks.drain(..n).collect()
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

    /// The turn's degradation floor, graded against the last prompt's size.
    /// Both terms are the fold's own: the model in force carries its window.
    fn context_floor(&self) -> u8 {
        super::fidelity::context_floor(self.fold.last_input(), self.fold.context_window())
    }

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
            K::Prompt { text, .. } => chrome(Chrome::Prompt(text.clone())),
            K::Answer { text } => vec![Item::Barrier(BlockKind::Prose {
                src: text.clone(),
                fidelity: self.fidelity(text),
                continues: continues_prose(&self.blocks),
            })],
            K::ToolCall {
                cmd,
                summary,
                verdict,
                ..
            } => match summary {
                Some(summary) => {
                    let mut call =
                        group::Call::open(seq, summary.clone(), cmd.clone(), self.context_floor());
                    if let Some(verdict) = verdict {
                        call.settle(*verdict);
                    }
                    vec![Item::Member(Member::Call(call))]
                }
                None => vec![Item::Barrier(BlockKind::Tool {
                    details: (cmd != crate::record::INVALID_INPUT).then(|| cmd.clone()),
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
            })],
            K::SubagentDone {
                name,
                outcome,
                elapsed_ms,
            } => chrome(Chrome::Subagent {
                name: name.clone(),
                outcome: outcome.clone(),
                elapsed: Duration::from_millis(*elapsed_ms),
            }),
            K::Observation { value } => observation_items(value),
            K::Change { change } => vec![Item::Change(change.clone())],
            K::Card { card } => vec![surfaced(card.clone())],
            // Not a card: a settled block is announced, not bounded — a line
            // on the rail, exactly as a subagent's answer arrives.  The shape
            // holds however it settled: `╳` is the turn's own failure (a
            // provider error, a stall), never a nonzero exit, which reads as a
            // red status in the row here just as it does on an exec.
            K::Done {
                id,
                title,
                elapsed_ms,
                outcome,
            } => chrome(Chrome::Settled(card::settled_spans(
                *id,
                title,
                elapsed_ms.map(Duration::from_millis),
                outcome,
            ))),
            K::Context { rows } => {
                vec![surfaced(record::context_rows_card(rows))]
            }
            K::Cancelled => chrome(Chrome::Cancelled),
            K::Error { text } => chrome(Chrome::Error(text.clone())),
            K::Nudge { .. } | K::HarnessResult { .. } => Vec::new(),
            K::ProviderError { error } => chrome(Chrome::ProviderError(error.clone())),
            K::Stalled { error } => chrome(Chrome::Stalled(error.clone())),
            K::SystemNote { text } => note(text),
            K::Turn { .. } => vec![Item::Member(Member::Turn)],
            K::Evicted { eviction } => note(&eviction.evicted_line()),
            K::Rewound { anchor } => note(&format!("[rewound to turn {anchor}]")),
            K::Cleared => note("[cleared]"),
        }
    }
}

/// Whether a prose block appended after `blocks` continues a paragraph run: a
/// block that renders nothing does not break one.
pub(super) fn continues_prose(blocks: &[Block]) -> bool {
    blocks
        .iter()
        .rev()
        .find(|b| b.renders())
        .is_some_and(Block::is_prose)
}

fn surfaced(card: Card) -> Item {
    Item::Barrier(BlockKind::Card { card, framed: true })
}

/// Decode one [`Display::Observation`](crate::record::Display::Observation),
/// through the same [`Landed::of`] the live rail draws from — a rendering, never
/// recorded, built as the fact arrives.
fn observation_items(value: &ral_core::data::FOValue) -> Vec<Item> {
    let Ok(obs) = Observation::decode(value) else {
        return Vec::new();
    };
    observed_items(&obs.what)
}

fn observed_items(what: &Observed) -> Vec<Item> {
    match Landed::of(what) {
        None => Vec::new(),
        Some(Landed::Effect(e)) => vec![Item::Member(Member::Effect(e))],
        Some(l @ Landed::Surfaced(_)) => vec![surfaced(observation_card(&l))],
        Some(l @ Landed::Announced(_)) => vec![Item::Barrier(BlockKind::Chrome(Chrome::Spawned(
            observation_spans(&l),
        )))],
    }
}
