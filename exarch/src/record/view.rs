//! The view fold: folds [`Display`] commits and the [`Forensic`] rows a
//! scrollback draws into [`Blocks`] — the memo `tui` and `headless` each keep
//! their own of, stepped by the record and read through [`BlockKind`].
//!
//! [`Block`]'s constructor is private to this module: a printer draws a
//! block, it cannot mint one.  [`Blocks::step`] is one exhaustive match over
//! the outer `Record`, and `Protocol` is skipped by an explicit arm — this
//! fold carries no model-context state, so it has nothing to fold a protocol
//! record into.

use super::{
    BlockId, Cut, Display, EditAuthority, Fold, Forensic, Recorded, Refusal, Role, Seq, Spent,
    TurnRow,
};
use crate::card::{Card, Change, DoneOutcome, Field, FieldVal, Mark, Role as Ink, Span};
use crate::provider::{ProviderError, Usage};
use ral_core::first_order::FOValue;

/// What a [`Block`] carries: the [`Display`] commits verbatim, plus the
/// [`Forensic`] rows this fold draws — errors, notes, nudge marks.
///
/// Field shapes mirror [`Display`] and [`Forensic`] exactly; this fold
/// invents no data, only a home for it.
#[derive(Debug)]
pub enum BlockKind {
    Thinking {
        text: String,
    },
    Prompt {
        text: String,
        turn: Option<u64>,
    },
    Answer {
        text: String,
    },
    ToolCall {
        tool: String,
        cmd: String,
        summary: Option<String>,
        verdict: Option<Verdict>,
    },
    HarnessCall {
        verb: String,
        subject: Option<String>,
        payload: String,
        failed: bool,
    },
    SubagentDone {
        name: String,
        error: Option<String>,
        elapsed_ms: u64,
    },
    Observation {
        value: FOValue,
    },
    Change {
        change: Change,
    },
    Card {
        card: Card,
    },
    Done {
        cmd: String,
        outcome: DoneOutcome,
    },
    Context {
        turns: Vec<TurnRow>,
    },
    Cancelled,
    Error {
        text: String,
    },
    Nudge {
        cause: String,
        spent: Option<Spent>,
    },
    ProviderError {
        error: ProviderError,
    },
    Stalled {
        error: ProviderError,
    },
    SystemNote {
        text: String,
    },
    HarnessResult {
        text: String,
    },
    Turn {
        id: u64,
    },
    Evicted {
        cut: Cut,
        by: EditAuthority,
    },
    Rewound {
        anchor: u64,
    },
}

/// What a result told the call it answers: how much it moved, and whether
/// the run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Verdict {
    pub lines: u32,
    pub failed: bool,
}

/// One committed block of scrollback, named by the [`Seq`] of the record that
/// opened it.
#[derive(Debug)]
pub struct Block {
    seq: Seq,
    kind: BlockKind,
}

impl Block {
    pub fn id(&self) -> BlockId {
        BlockId::new(self.seq)
    }

    pub fn kind(&self) -> &BlockKind {
        &self.kind
    }
}

/// What one [`Blocks::step`] did to the memo — the whole of what a printer
/// needs in order to draw the change rather than the window around it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Delta {
    /// A block was opened at the tail, and is [`Blocks::blocks`]'s last.
    Opened(BlockId),
    /// The named block's lane grew by this record; its text is the memo's.
    Grew(BlockId),
    /// A result was attached to the named call.
    Patched(BlockId),
    /// Every block from the named one on was dropped — `None` when that block
    /// had already left the window — and a block opened at the tail.
    Rewound(Option<BlockId>),
    /// The record moved no block: ambient totals, or a class this fold skips.
    Quiet,
}

/// Past this many resident blocks, the oldest are dropped.
///
/// The one window, for every printer: what leaves the fold leaves the screen,
/// so a printer that mirrors this memo lets a block go exactly as the fold
/// lets it go, and no printer keeps a second trim of its own.
pub const BLOCKS_WINDOW: usize = 1000;

/// The view fold's memo: the last [`BLOCKS_WINDOW`] commits and drawn
/// breadcrumbs this session has recorded, in log order.
#[derive(Default)]
#[allow(
    clippy::struct_field_names,
    reason = "the memo is the blocks; the field can only be called that"
)]
pub struct Blocks {
    blocks: Vec<Block>,
    /// Cumulative usage this session has billed, per the forensic usage trail
    /// — one of the fidelity inputs this fold admits Forensic for.
    usage: Usage,
    /// The model in force: the one the session opened under, and then each
    /// [`Forensic::ModelChanged`]'s.
    model: Option<(String, String)>,
    /// The window the provider reported for [`Self::model`], when it did.
    context_window: Option<u64>,
    /// The [`Seq`] of the first block this fold ever held, remembered past
    /// eviction — the door [`Self::blocks`] no longer names once the window
    /// has moved off the session's opening block.
    origin: Option<Seq>,
    /// High-water [`Seq`]: a resumed memo is seeded from the file and then
    /// handed the same head records again when the seam's sink attaches, so a
    /// record at or below this has been folded already.
    seen: Option<Seq>,
}

impl Blocks {
    pub fn blocks(&self) -> &[Block] {
        &self.blocks
    }

    /// The [`Seq`] of the first block this fold ever held — `None` until one
    /// lands.  Unlike `blocks().first()` it survives eviction, so a printer
    /// can ask whether its window still reaches the session's opening block:
    /// the question chrome drawn *before* any block, the startup banner,
    /// hangs on.
    pub fn origin(&self) -> Option<Seq> {
        self.origin
    }

    /// Cumulative usage billed so far, per the forensic usage trail — the
    /// context-floor numerator a printer grades committed prose against.
    pub fn usage(&self) -> Usage {
        self.usage
    }

    /// `(model, provider)` in force — total from the session's first record,
    /// its head bookend, onward.
    pub fn model(&self) -> Option<(&str, &str)> {
        self.model.as_ref().map(|(m, p)| (m.as_str(), p.as_str()))
    }

    /// The context window the model in force was minted with, if known.
    pub fn context_window(&self) -> Option<u64> {
        self.context_window
    }

    /// Fold one witnessed fact in, reporting what it moved.  A record the
    /// memo has already folded moves nothing, so the same log stepped twice
    /// and stepped once are the same memo.
    ///
    /// # Errors
    /// Returns [`Refusal`] when this fold does not recognise the record —
    /// during replay that refuses the session rather than skip it silently.
    pub fn step(&mut self, record: &Recorded<super::Record>) -> Result<Delta, Refusal> {
        let seq = record.locus().seq();
        if self.seen.is_some_and(|folded| seq <= folded) {
            return Ok(Delta::Quiet);
        }
        self.seen = Some(seq);
        Ok(match record.value().clone() {
            super::Record::Protocol(_) => Delta::Quiet,
            super::Record::Display(d) => self.step_display(seq, d),
            super::Record::Forensic(f) => self.step_forensic(seq, f),
        })
    }

    /// Drop the oldest resident blocks past [`BLOCKS_WINDOW`], as each block
    /// lands — the fold's whole bound, and unconditional, since no cursor of
    /// another's lives here to hold the floor down.
    fn evict(&mut self) {
        while self.blocks.len() > BLOCKS_WINDOW {
            let _ = self.blocks.remove(0);
        }
    }

    /// Open a block for `kind` — or, where `kind` continues the lane the last
    /// block already holds, grow that block instead.
    ///
    /// The model's prose and reasoning arrive as many records, one per line,
    /// so a reader sees the text as it is spoken.  A block is the run of
    /// records that meet: any record of another kind — a tool call, a prompt
    /// — ends the run, and the next line of prose opens a fresh block.  This
    /// is what frees the commit producer from having to cut anywhere
    /// meaningful, and it keeps a block's `Seq` the one it opened with, so a
    /// dial set on it survives the growth.
    fn push(&mut self, seq: Seq, kind: BlockKind) -> Delta {
        if let Some(tail) = self.blocks.last_mut() {
            match (&mut tail.kind, &kind) {
                (BlockKind::Answer { text }, BlockKind::Answer { text: more })
                | (BlockKind::Thinking { text }, BlockKind::Thinking { text: more }) => {
                    text.push_str(more);
                    return Delta::Grew(tail.id());
                }
                _ => {}
            }
        }
        let _ = self.origin.get_or_insert(seq);
        self.blocks.push(Block { seq, kind });
        self.evict();
        Delta::Opened(BlockId::new(seq))
    }

    /// Attach a result's line count to the call it names — a patch record
    /// addressed by `BlockId`.  A target this fold cannot find — evicted, or
    /// simply never resident — is [`Delta::Quiet`] rather than a panic.
    fn attach_result(&mut self, call: BlockId, text: &str, failed: bool) -> Delta {
        let lines = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
        let target = call.seq();
        if let Some(block) = self.blocks.iter_mut().find(|b| b.seq == target)
            && let BlockKind::ToolCall { verdict, .. } = &mut block.kind
        {
            *verdict = Some(Verdict { lines, failed });
            return Delta::Patched(call);
        }
        Delta::Quiet
    }

    fn step_display(&mut self, seq: Seq, d: Display) -> Delta {
        match d {
            Display::Thinking { text } => self.push(seq, BlockKind::Thinking { text }),
            Display::Prompt { text, turn } => self.push(seq, BlockKind::Prompt { text, turn }),
            Display::Answer { text } => self.push(seq, BlockKind::Answer { text }),
            Display::ToolCall { tool, cmd, summary } => self.push(
                seq,
                BlockKind::ToolCall {
                    tool,
                    cmd,
                    summary,
                    verdict: None,
                },
            ),
            Display::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            } => self.push(
                seq,
                BlockKind::HarnessCall {
                    verb,
                    subject,
                    payload,
                    failed,
                },
            ),
            Display::Result { text, failed, call } => self.attach_result(call, &text, failed),
            Display::SubagentDone {
                name,
                error,
                elapsed_ms,
            } => self.push(
                seq,
                BlockKind::SubagentDone {
                    name,
                    error,
                    elapsed_ms,
                },
            ),
            Display::Observation { value } => self.push(seq, BlockKind::Observation { value }),
            Display::Change { change } => self.push(seq, BlockKind::Change { change }),
            Display::Card { card } => self.push(seq, BlockKind::Card { card }),
            Display::Done { cmd, outcome } => self.push(seq, BlockKind::Done { cmd, outcome }),
            Display::Context { turns } => self.push(seq, BlockKind::Context { turns }),
            Display::Turn { id } => self.push(seq, BlockKind::Turn { id }),
            Display::Evicted { cut, by } => self.push(seq, BlockKind::Evicted { cut, by }),
            Display::Rewound { anchor } => {
                let from = self.rewind_at(anchor);
                let cut = from.map(|at| self.blocks[at].id());
                self.blocks.truncate(from.unwrap_or(self.blocks.len()));
                let _ = self.push(seq, BlockKind::Rewound { anchor });
                Delta::Rewound(cut)
            }
        }
    }

    fn step_forensic(&mut self, seq: Seq, f: Forensic) -> Delta {
        match f {
            Forensic::UsageDelta { usage } => {
                self.usage += usage;
                Delta::Quiet
            }
            Forensic::Cancelled => self.push(seq, BlockKind::Cancelled),
            Forensic::Error { text } => self.push(seq, BlockKind::Error { text }),
            Forensic::Nudge { cause, spent } => self.push(seq, BlockKind::Nudge { cause, spent }),
            // One record, two blocks: a stall wears the chrome that says the
            // exchange survived it, and draws its cause rather than the
            // truncation wrapping it.
            Forensic::ProviderError { error } => match error.stall_cause() {
                Some(cause) => self.push(
                    seq,
                    BlockKind::Stalled {
                        error: cause.clone(),
                    },
                ),
                None => self.push(seq, BlockKind::ProviderError { error }),
            },
            Forensic::SystemNote { text } => self.push(seq, BlockKind::SystemNote { text }),
            Forensic::HarnessResult { text } => self.push(seq, BlockKind::HarnessResult { text }),
            // The history informs a resume note; the live register follows the
            // shell boundary and is not restored — so neither is a scrollback
            // block this fold draws.  The tail bookend and a turn's effort dial
            // draw none either: evidence with a display twin, or with none.
            // Core's housekeeping is the log's alone.
            Forensic::Pin { .. }
            | Forensic::Reap { .. }
            | Forensic::Prune { .. }
            | Forensic::Unpin { .. }
            | Forensic::SessionEnded
            | Forensic::TurnStarted { .. } => Delta::Quiet,
            // A head bookend opens the session under a model exactly as a
            // switch names one mid-session, so all three land in the memo and
            // none of them draws a block.
            Forensic::SessionStarted {
                model,
                label,
                context_window,
                ..
            }
            | Forensic::SessionResumed {
                model,
                label,
                context_window,
                ..
            }
            | Forensic::ModelChanged {
                model,
                label,
                context_window,
                ..
            } => {
                self.model = Some((model, label));
                self.context_window = context_window;
                Delta::Quiet
            }
        }
    }
}

impl Blocks {
    /// The prompts resident in the window that opened a turn, oldest first,
    /// each with the turn it opened — what `/rewind` offers.
    pub fn prompts(&self) -> impl Iterator<Item = (u64, &str)> {
        self.blocks.iter().filter_map(|b| {
            let BlockKind::Prompt {
                text,
                turn: Some(turn),
            } = &b.kind
            else {
                return None;
            };
            Some((*turn, text.as_str()))
        })
    }

    /// The block `/rewind anchor` cuts from: the first that *is* a turn at
    /// or past the anchor — a prompt opening one, or a request's breadcrumb.
    /// The one cut rule, so a preview of the cut and the cut agree.
    pub fn rewind_point(&self, anchor: u64) -> Option<BlockId> {
        self.rewind_at(anchor).map(|at| self.blocks[at].id())
    }

    fn rewind_at(&self, anchor: u64) -> Option<usize> {
        self.blocks.iter().position(|b| {
            matches!(
                b.kind,
                BlockKind::Prompt { turn: Some(t), .. } | BlockKind::Turn { id: t } if t >= anchor
            )
        })
    }
}

/// The view fold: [`Fold::step`] over [`Display`] and [`Forensic`], skipping
/// [`super::Protocol`] by an explicit arm rather than a wildcard.
pub struct View;

impl Fold for View {
    type Memo = Blocks;

    fn step(memo: &mut Blocks, record: &Recorded<super::Record>) -> Result<(), Refusal> {
        memo.step(record).map(drop)
    }
}

/// A `/context` survey's rows as one [`Mark::Fields`] matrix under a
/// "context" header: the one rendering `tui`, `headless` and synod all draw from.
///
/// The survey is one row per turn; the card groups them at draw time into a
/// prompt and the turns answering it, since two hundred tool turns under one
/// prompt are one thing the human is reading about. No live marker: the
/// newest turn is the one an eviction structurally cannot name, so saying so
/// twice would say nothing.
pub fn context_rows_card(rows: &[TurnRow]) -> Card {
    let fields = context_groups(rows)
        .into_iter()
        .map(|group| Field {
            label: format!("turn {}", group.prompt),
            value: FieldVal::Inline(vec![
                Span::plain(group.label),
                Span::new(
                    Ink::Muted,
                    format!("  {} · {} KB", group.turns, group.bytes / 1024),
                ),
            ]),
        })
        .collect();
    let mut marks = vec![Mark::Text {
        spans: vec![Span::new(Ink::Strong, "context")],
    }];
    marks.push(Mark::Fields { rows: fields });
    Card(marks)
}

/// One drawn group: the prompt it opens on, its opening line, the turns of it
/// still in the context, and what they weigh together.
struct ContextGroup {
    prompt: u64,
    label: String,
    /// `turns 13–15`, or `turn 13` where one stands alone.
    turns: String,
    bytes: usize,
}

/// The survey's turns grouped as a prompt and the turns answering it: a group
/// opens at every user row, so rows before the first one — an ancestor's
/// turns, or an answer whose prompt a cut took — form a group of their own.
/// The label is the opening line of whichever row opens the group.
fn context_groups(rows: &[TurnRow]) -> Vec<ContextGroup> {
    let mut drawn: Vec<(&TurnRow, u64, usize)> = Vec::new();
    for row in rows {
        match drawn.last_mut() {
            Some((_, last, bytes)) if !matches!(row.role, Role::User) => {
                *last = row.id;
                *bytes += row.bytes;
            }
            _ => drawn.push((row, row.id, row.bytes)),
        }
    }
    drawn
        .into_iter()
        .map(|(first, last, bytes)| ContextGroup {
            prompt: first.id,
            label: first.label.clone(),
            turns: if first.id == last {
                format!("turn {last}")
            } else {
                format!("turns {}–{last}", first.id)
            },
            bytes,
        })
        .collect()
}

#[cfg(test)]
mod tests;
