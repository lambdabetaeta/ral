//! The view fold: folds [`Display`] commits and the [`Forensic`] rows a
//! scrollback draws into [`Blocks`] — the memo `tui` and `headless` each keep
//! their own of, stepped by the record and read through [`BlockKind`].
//!
//! [`Block`]'s constructor is private to this module: a printer draws a
//! block, it cannot mint one.  [`Blocks::step`] is one exhaustive match over
//! the outer `Record`, and `Protocol` is skipped by an explicit arm — this
//! fold carries no model-context state, so it has nothing to fold a protocol
//! record into.

use super::{BlockId, Display, Fold, Forensic, Recorded, Refusal, Seq, TurnRow};
use crate::agent::event::{ContextOp, EditAuthority, ProviderErrorRecord};
use crate::bus::card::Card;
use crate::provider::Usage;
use ral_core::serial::FOValue;

pub use super::{DoneOutcome, NoticeFact};

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
    },
    Answer {
        text: String,
    },
    ToolCall {
        tool: String,
        cmd: String,
        summary: Option<String>,
        result_lines: Option<u32>,
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
    Card {
        card: Card,
    },
    Done {
        outcome: DoneOutcome,
    },
    Notice {
        notice: NoticeFact,
    },
    Context {
        turns: Vec<TurnRow>,
        evicted: usize,
    },
    Cancelled,
    Error {
        text: String,
    },
    Nudge {
        used: u32,
        max: u32,
        cause: String,
    },
    ProviderError {
        error: ProviderErrorRecord,
    },
    Stalled {
        error: ProviderErrorRecord,
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
    ContextEdited {
        op: ContextOp,
        by: EditAuthority,
    },
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
    fn attach_result(&mut self, call: BlockId, text: &str) -> Delta {
        let n = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
        let target = call.seq();
        if let Some(block) = self.blocks.iter_mut().find(|b| b.seq == target)
            && let BlockKind::ToolCall { result_lines, .. } = &mut block.kind
        {
            *result_lines = Some(n);
            return Delta::Patched(call);
        }
        Delta::Quiet
    }

    fn step_display(&mut self, seq: Seq, d: Display) -> Delta {
        match d {
            Display::Thinking { text } => self.push(seq, BlockKind::Thinking { text }),
            Display::Prompt { text } => self.push(seq, BlockKind::Prompt { text }),
            Display::Answer { text } => self.push(seq, BlockKind::Answer { text }),
            Display::ToolCall { tool, cmd, summary } => self.push(
                seq,
                BlockKind::ToolCall {
                    tool,
                    cmd,
                    summary,
                    result_lines: None,
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
            Display::Result { text, call } => self.attach_result(call, &text),
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
            Display::Card { card } => self.push(seq, BlockKind::Card { card }),
            Display::Done { outcome } => self.push(seq, BlockKind::Done { outcome }),
            Display::Notice { notice } => self.push(seq, BlockKind::Notice { notice }),
            Display::Context { turns, evicted } => {
                self.push(seq, BlockKind::Context { turns, evicted })
            }
            Display::Turn { id } => self.push(seq, BlockKind::Turn { id }),
            Display::ContextEdited { op, by } => {
                self.push(seq, BlockKind::ContextEdited { op, by })
            }
        }
    }

    fn step_forensic(&mut self, seq: Seq, f: Forensic) -> Delta {
        match f {
            Forensic::UsageDelta { usage } => {
                self.usage += Usage::from(&usage);
                Delta::Quiet
            }
            Forensic::Cancelled => self.push(seq, BlockKind::Cancelled),
            Forensic::Error { text } => self.push(seq, BlockKind::Error { text }),
            Forensic::Nudge { used, max, cause } => {
                self.push(seq, BlockKind::Nudge { used, max, cause })
            }
            Forensic::ProviderError { error } => self.push(seq, BlockKind::ProviderError { error }),
            Forensic::Stalled { error } => self.push(seq, BlockKind::Stalled { error }),
            Forensic::SystemNote { text } => self.push(seq, BlockKind::SystemNote { text }),
            Forensic::HarnessResult { text } => self.push(seq, BlockKind::HarnessResult { text }),
            // The history informs a resume note; the live register follows the
            // shell boundary and is not restored — so neither is a scrollback
            // block this fold draws.  The tail bookend and a turn's effort dial
            // draw none either: evidence with a display twin, or with none.
            Forensic::Pin { .. }
            | Forensic::Unpin { .. }
            | Forensic::SessionEnded
            | Forensic::TurnStarted { .. } => Delta::Quiet,
            // A head bookend opens the session under a model exactly as a
            // switch names one mid-session, so all three land in the memo and
            // none of them draws a block.
            Forensic::SessionStarted { model, label, .. }
            | Forensic::SessionResumed { model, label, .. }
            | Forensic::ModelChanged { model, label, .. } => {
                self.model = Some((model, label));
                Delta::Quiet
            }
        }
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

#[cfg(test)]
mod tests {
    use super::BlockId;
    use super::*;

    /// A block that never joins the one before it, so a test about the window
    /// counts blocks rather than the lanes that grow.
    fn push(memo: &mut Blocks, seq: u64, text: &str) -> Delta {
        memo.step_forensic(Seq::new(seq), Forensic::SystemNote { text: text.into() })
    }

    /// One lane, many records, one block: consecutive prose grows the block it
    /// opened, and a record of another kind ends the run.
    #[test]
    fn consecutive_records_of_one_lane_grow_a_single_block() {
        let mut memo = Blocks::default();
        let opened = memo.step_display(
            Seq::new(1),
            Display::Answer {
                text: "first line\n".into(),
            },
        );
        assert_eq!(opened, Delta::Opened(BlockId::new(Seq::new(1))));
        let grew = memo.step_display(
            Seq::new(2),
            Display::Answer {
                text: "second line\n".into(),
            },
        );
        assert_eq!(
            grew,
            Delta::Grew(BlockId::new(Seq::new(1))),
            "the second record grows the block the first opened"
        );
        match memo.blocks() {
            [block] => {
                let BlockKind::Answer { text } = block.kind() else {
                    panic!("expected one answer block")
                };
                assert_eq!(text, "first line\nsecond line\n", "the block holds the run");
            }
            other => panic!("expected one block, got {}", other.len()),
        }

        let _ = memo.step_display(
            Seq::new(3),
            Display::ToolCall {
                tool: "ral".into(),
                cmd: "ls".into(),
                summary: None,
            },
        );
        let _ = memo.step_display(
            Seq::new(4),
            Display::Answer {
                text: "after the call\n".into(),
            },
        );
        assert_eq!(
            memo.blocks().len(),
            3,
            "the tool call ends the run, so the prose after it opens its own block"
        );
    }

    /// A result addresses its call wherever that call sits, and a patch whose
    /// target the window has let go moves nothing.
    #[test]
    fn a_result_patches_the_call_it_names() {
        let mut memo = Blocks::default();
        let call = BlockId::new(Seq::new(1));
        let _ = memo.step_display(
            Seq::new(1),
            Display::ToolCall {
                tool: "ral".into(),
                cmd: "read 'x'".into(),
                summary: Some("look at x".into()),
            },
        );
        let _ = memo.step_display(Seq::new(2), Display::Prompt { text: "on".into() });
        assert_eq!(
            memo.step_display(
                Seq::new(3),
                Display::Result {
                    text: "a\nb\n".into(),
                    call,
                },
            ),
            Delta::Patched(call)
        );
        assert_eq!(
            memo.step_display(
                Seq::new(4),
                Display::Result {
                    text: "a\n".into(),
                    call: BlockId::new(Seq::new(99)),
                },
            ),
            Delta::Quiet,
            "a patch naming no resident call moves nothing"
        );
    }

    #[test]
    fn the_window_bounds_the_fold_as_blocks_land() {
        let mut memo = Blocks::default();
        let total = BLOCKS_WINDOW + 50;
        for i in 1..=total {
            let _ = push(&mut memo, i as u64, &format!("block {i}"));
        }
        assert_eq!(
            memo.blocks().len(),
            BLOCKS_WINDOW,
            "the window binds while blocks land, with no flush to wait on"
        );
        assert_eq!(
            memo.blocks().first().map(|b| b.seq),
            Some(Seq::new(51)),
            "the oldest blocks go first"
        );
        assert_eq!(
            memo.origin(),
            Some(Seq::new(1)),
            "the session's opening block is remembered past its eviction"
        );
    }
}
