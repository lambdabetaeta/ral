//! The view fold: folds the [`Protocol`] facts a scrollback draws, the
//! [`Display`] records and the [`Forensic`] rows into [`Blocks`] — the memo
//! `tui` and `headless` each keep their own of, stepped by the record and read
//! through [`BlockKind`].
//!
//! [`Block`]'s constructor is private to this module: a printer draws a
//! block, it cannot mint one.  [`Blocks::step`] is one exhaustive match over
//! the outer `Record`, matching by reference: an arm that moves no block
//! clones nothing.

use super::{
    BlockId, Display, EditAuthority, Fold, Forensic, ItemKind, Protocol, Record, Recorded, Refusal,
    Residue, Role, Selection, Seq, Settlement, SpanRow, Spent, Voice,
};
use crate::card::{Card, Change, DoneOutcome, Field, FieldVal, Mark, Role as Ink, Span};
use crate::provider::{ProviderError, Usage};
use ral_core::data::FOValue;
use serde::{Deserialize, Serialize};

/// What a [`Block`] carries: the [`Display`] records verbatim, plus the
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
        outcome: Settlement,
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
        id: Option<u64>,
        title: String,
        elapsed_ms: Option<u64>,
        outcome: DoneOutcome,
    },
    Context {
        rows: Vec<SpanRow>,
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
        residue: Residue,
    },
    Rewound {
        anchor: u64,
    },
    Cleared,
}

/// What a result told the call it answers: how much it moved, and whether
/// the run failed.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct Verdict {
    pub lines: u32,
    pub failed: bool,
}

impl Verdict {
    pub fn of(text: &str, failed: bool) -> Self {
        Self {
            lines: u32::try_from(text.lines().count()).unwrap_or(u32::MAX),
            failed,
        }
    }
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
    /// Every earlier block was dropped; the memo holds only the clear's own
    /// marker, [`BlockKind::Cleared`].
    Cleared,
    /// The record moved no block: ambient totals, or a record that draws nothing.
    Quiet,
}

/// Past this many resident blocks, the oldest are dropped.
///
/// The one window, for every printer: what leaves the fold leaves the screen,
/// so a printer that mirrors this memo lets a block go exactly as the fold
/// lets it go, and no printer keeps a second trim of its own.
pub const BLOCKS_WINDOW: usize = 1000;

/// The view fold's memo: the last [`BLOCKS_WINDOW`] blocks and drawn
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
    /// The last turn's prompt size — the context gauge's numerator.
    last_input: u64,
    selection: Selection,
    /// The [`Seq`] of the first block this fold ever held, remembered past
    /// eviction — the door [`Self::blocks`] no longer names once the window
    /// has moved off the session's opening block.
    origin: Option<Seq>,
    /// High-water [`Seq`]: a record at or below it has been folded already,
    /// which is what makes a step idempotent.
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

    /// The last turn's prompt size, overwritten (not summed) by each measured usage delta.
    pub fn last_input(&self) -> u64 {
        self.last_input
    }

    /// `(model, provider)` in force — total from the session's first record,
    /// its head bookend, onward.
    pub fn model(&self) -> Option<(&str, &str)> {
        self.selection.model()
    }

    /// The context window the model in force was minted with, if known.
    pub fn context_window(&self) -> Option<u64> {
        self.selection.window()
    }

    /// Fold one witnessed fact in, reporting what it moved.  A record the
    /// memo has already folded moves nothing, so the same log stepped twice
    /// and stepped once are the same memo.
    pub fn step(&mut self, record: &Recorded<Record>) -> Delta {
        let seq = record.locus().seq();
        if self.seen.is_some_and(|folded| seq <= folded) {
            return Delta::Quiet;
        }
        self.seen = Some(seq);
        match record.value() {
            Record::Protocol(p) => self.step_protocol(seq, p),
            // Every display block keeps its whole record.
            Record::Display(d) => self.step_display(seq, d.clone()),
            Record::Forensic(f) => self.step_forensic(seq, f),
        }
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
    /// is what frees the stream producer from having to cut anywhere
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

    /// Attach a verdict to the call it names — a patch record addressed by
    /// `BlockId`.  A target this fold cannot find — evicted, or simply never
    /// resident — is [`Delta::Quiet`] rather than a panic.
    fn attach_verdict(&mut self, call: BlockId, verdict: Verdict) -> Delta {
        let target = call.seq();
        if let Some(block) = self.blocks.iter_mut().find(|b| b.seq == target)
            && let BlockKind::ToolCall { verdict: slot, .. } = &mut block.kind
        {
            *slot = Some(verdict);
            return Delta::Patched(call);
        }
        Delta::Quiet
    }

    fn step_protocol(&mut self, seq: Seq, p: &Protocol) -> Delta {
        match p {
            Protocol::UserPrompt { turn, text, voice } if *voice != Voice::Harness => self.push(
                seq,
                BlockKind::Prompt {
                    text: text.clone(),
                    turn: Some(*turn),
                },
            ),
            Protocol::Steering { text, voice } if *voice != Voice::Harness => self.push(
                seq,
                BlockKind::Prompt {
                    text: text.clone(),
                    turn: None,
                },
            ),
            Protocol::Evicted {
                residue:
                    residue @ Residue {
                        by: EditAuthority::Harness,
                        ..
                    },
            } => self.push(
                seq,
                BlockKind::Evicted {
                    residue: residue.clone(),
                },
            ),
            Protocol::Rewound { anchor } => {
                let from = self.rewind_at(*anchor);
                let cut = from.map(|at| self.blocks[at].id());
                self.blocks.truncate(from.unwrap_or(self.blocks.len()));
                let _ = self.push(seq, BlockKind::Rewound { anchor: *anchor });
                Delta::Rewound(cut)
            }
            Protocol::Cleared => {
                self.blocks.clear();
                self.origin = None;
                self.last_input = 0;
                let _ = self.push(seq, BlockKind::Cleared);
                Delta::Cleared
            }
            Protocol::Evicted {
                residue:
                    Residue {
                        by: EditAuthority::Model,
                        ..
                    },
            }
            | Protocol::UserPrompt { .. }
            | Protocol::Steering { .. }
            | Protocol::System { .. }
            | Protocol::Import { .. }
            | Protocol::Inherited { .. }
            | Protocol::AssistantMessage { .. }
            | Protocol::ToolResults { .. } => Delta::Quiet,
        }
    }

    fn step_display(&mut self, seq: Seq, d: Display) -> Delta {
        match d {
            Display::Thinking { text } => self.push(seq, BlockKind::Thinking { text }),
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
            Display::Verdict { call, verdict } => self.attach_verdict(call, verdict),
            Display::SubagentDone {
                name,
                outcome,
                elapsed_ms,
            } => self.push(
                seq,
                BlockKind::SubagentDone {
                    name,
                    outcome,
                    elapsed_ms,
                },
            ),
            Display::Observation { value } => self.push(seq, BlockKind::Observation { value }),
            Display::Change { change } => self.push(seq, BlockKind::Change { change }),
            Display::Card { card } => self.push(seq, BlockKind::Card { card }),
            Display::Done {
                id,
                title,
                elapsed_ms,
                outcome,
            } => self.push(
                seq,
                BlockKind::Done {
                    id,
                    title,
                    elapsed_ms,
                    outcome,
                },
            ),
            Display::Context { rows } => self.push(seq, BlockKind::Context { rows }),
        }
    }

    fn step_forensic(&mut self, seq: Seq, f: &Forensic) -> Delta {
        match f {
            Forensic::UsageDelta { usage, .. } => {
                if let Some(input) = usage.measured_input() {
                    self.last_input = input;
                }
                self.usage += *usage;
                Delta::Quiet
            }
            Forensic::Cancelled => self.push(seq, BlockKind::Cancelled),
            Forensic::Error { text } => self.push(seq, BlockKind::Error { text: text.clone() }),
            Forensic::Nudge { cause, spent } => self.push(
                seq,
                BlockKind::Nudge {
                    cause: cause.clone(),
                    spent: *spent,
                },
            ),
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
                None => self.push(
                    seq,
                    BlockKind::ProviderError {
                        error: error.clone(),
                    },
                ),
            },
            Forensic::SystemNote { text } => {
                self.push(seq, BlockKind::SystemNote { text: text.clone() })
            }
            Forensic::HarnessResult { text } => {
                self.push(seq, BlockKind::HarnessResult { text: text.clone() })
            }
            Forensic::TurnStarted { turn, .. } => self.push(seq, BlockKind::Turn { id: *turn }),
            // A head bookend opens the session under a model exactly as a
            // switch names one mid-session: all land in the selection and none
            // draws a block.
            Forensic::SessionStarted { .. }
            | Forensic::SessionResumed { .. }
            | Forensic::ModelChanged { .. }
            | Forensic::WindowNarrowed { .. } => {
                self.selection.step(f);
                Delta::Quiet
            }
            // The history informs a resume note; the live register follows the
            // shell boundary and is not restored — so neither is a scrollback
            // block this fold draws.  The tail bookend draws none either.
            // Core's housekeeping is the log's alone.
            Forensic::Pin { .. }
            | Forensic::Reap { .. }
            | Forensic::Prune { .. }
            | Forensic::Unpin { .. }
            | Forensic::SessionEnded
            | Forensic::SessionClosed
            | Forensic::Status { .. }
            | Forensic::Retried { .. } => Delta::Quiet,
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

/// The view fold: [`Fold::step`] over every class of [`Record`].
pub struct View;

impl Fold for View {
    type Memo = Blocks;

    fn step(memo: &mut Blocks, record: Recorded<Record>) -> Result<(), Refusal> {
        let _ = memo.step(&record);
        Ok(())
    }
}

impl Residue {
    /// The line an eviction leaves in scrollback, `tui` and `headless` alike.
    #[must_use]
    pub fn evicted_line(&self) -> String {
        format!("[{} left the context ({})]", self.span, self.by.name())
    }
}

/// A `/context` survey's rows as one [`Mark::Fields`] matrix under a
/// "context" header: the one rendering `tui`, `headless` and synod all draw from.
///
/// The survey is one row per item, a turn or a residue; the card groups them
/// at draw time into a prompt and the turns answering it, since two hundred
/// tool turns under one prompt are one thing the human is reading about. The
/// newest turn is not flagged as un-evictable: no eviction can name it, so
/// flagging it would say nothing.
pub fn context_rows_card(rows: &[SpanRow]) -> Card {
    let fields = context_groups(rows)
        .into_iter()
        .map(|group| {
            let reach = group
                .reach
                .map_or_else(String::new, |reach| format!("{reach} · "));
            Field {
                label: group.head.to_string(),
                value: FieldVal::Inline(vec![
                    Span::plain(group.label),
                    Span::new(Ink::Muted, format!("  {reach}{} KB", group.bytes / 1024)),
                ]),
            }
        })
        .collect();
    let mut marks = vec![Mark::Text {
        spans: vec![Span::new(Ink::Strong, "context")],
    }];
    marks.push(Mark::Fields { rows: fields });
    Card(marks)
}

/// One drawn group: the item it opens on, its opening line, the span of the
/// context it reaches over, and what its rows weigh together.
struct ContextGroup {
    head: super::Span,
    label: String,
    /// `None` for a lone residue, whose head already names all it reaches.
    reach: Option<super::Span>,
    bytes: usize,
}

/// The survey's items grouped as a prompt and the turns answering it: a group
/// opens at every user-role row, so a residue opens its own, labelled by its
/// note. The label is the opening line of whichever row opens the group.
fn context_groups(rows: &[SpanRow]) -> Vec<ContextGroup> {
    let mut drawn: Vec<(&SpanRow, super::Span, usize)> = Vec::new();
    for row in rows {
        match drawn.last_mut() {
            Some((_, reach, bytes)) if !matches!(row.role, Role::User) => {
                *reach = reach.hull(row.span);
                *bytes += row.bytes;
            }
            _ => drawn.push((row, row.span, row.bytes)),
        }
    }
    drawn
        .into_iter()
        .map(|(first, reach, bytes)| ContextGroup {
            head: first.span,
            label: first.label.clone(),
            reach: (first.kind != ItemKind::Residue || reach != first.span).then_some(reach),
            bytes,
        })
        .collect()
}

#[cfg(test)]
mod tests;
