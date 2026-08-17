//! The view fold: folds [`Display`] commits and the [`Forensic`] rows a
//! scrollback draws into [`Blocks`] — the memo `tui` and `headless` both
//! draw from as printers, never handed a [`Record`] of their own.
//!
//! [`Block`]'s constructor is private to this module: a printer draws a
//! block, it cannot mint one.  `step` is one exhaustive match over the
//! outer `Record`, and `Protocol` is skipped by an explicit arm — this fold
//! carries no model-context state, so it has nothing to fold a protocol
//! record into.

use super::{ContextRow, Display, Fold, Forensic, Recorded, Refusal, Seq};
use crate::agent::event::{ContextOp, EditAuthority, ProviderErrorRecord};
use ral_core::serial::FOValue;

pub use super::{DoneOutcome, NoticeFact};

/// What a [`Block`] carries: the [`Display`] commits verbatim, plus the
/// [`Forensic`] rows this fold draws — errors, notes, nudge marks.
///
/// Field shapes mirror [`Display`] and [`Forensic`] exactly; this fold
/// invents no data, only a home for it.
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
    ObservationGroup {
        values: Vec<FOValue>,
    },
    HarnessCall {
        verb: String,
        subject: Option<String>,
        payload: String,
        failed: bool,
    },
    SubagentDone {
        name: String,
        text: String,
        error: Option<String>,
        elapsed_ms: u64,
    },
    Observation {
        value: FOValue,
    },
    Card {
        marks: serde_json::Value,
    },
    Done {
        outcome: DoneOutcome,
    },
    Notice {
        notice: NoticeFact,
    },
    Context {
        rows: Vec<ContextRow>,
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
    ModelChanged {
        model: String,
        provider: String,
    },
    Step {
        n: u32,
    },
    ContextEdited {
        op: ContextOp,
        by: EditAuthority,
    },
}

/// One committed row of scrollback, named by the [`Seq`] of the record that
/// produced it.
pub struct Block {
    seq: Seq,
    kind: BlockKind,
    rev: u64,
}

impl Block {
    pub fn id(&self) -> super::BlockId {
        super::BlockId::new(self.seq)
    }

    pub fn kind(&self) -> &BlockKind {
        &self.kind
    }

    /// [`Blocks::rev`] as of this row's last change — its opening, the growth
    /// of the run it holds, or a result patched onto it.  A printer that
    /// caches a rendering keeps it exactly while this stays under the
    /// revision it last synced at, which is what lets a sync rebuild only
    /// what the fold has actually moved.
    pub fn rev(&self) -> u64 {
        self.rev
    }
}

/// Cumulative input/output tokens this session has billed, per the forensic
/// usage trail — one of the fidelity inputs this fold admits Forensic for.
#[derive(Default, Clone, Copy)]
struct UsageTotal {
    input: u64,
    output: u64,
}

/// Past this many resident rows, the oldest are dropped — the one window
/// that bounds this fold for every printer, rather than a trim each printer
/// repeats.  A printer that renders incrementally holds its own cursor by
/// [`Seq`] identity, so the memo owes it no unrendered tail: it owes it only
/// the slack this window leaves between two of its syncs.
const BLOCKS_WINDOW: usize = 1000;

/// The view fold's memo: the last [`BLOCKS_WINDOW`] commits and drawn
/// breadcrumbs this session has recorded, in log order.
///
/// A printer's own window ([`VIEWPORT_MAX_BLOCKS`](crate::tui) and friends)
/// stays a second, purely presentational trim on top of this one.
#[derive(Default)]
pub struct Blocks {
    rows: Vec<Block>,
    /// Counts every change this fold has made to a row, so that a row can say
    /// when it last moved ([`Block::rev`]).  Monotone and never reset; `0` is
    /// the revision of a memo nothing has landed in yet, so it stamps no row.
    rev: u64,
    usage: UsageTotal,
    /// The model in force, from the most recent [`Forensic::ModelChanged`].
    /// A session's *first* model rides `Protocol::SessionStarted`, which is
    /// outside this fold's class — so a session that never switches models
    /// has no entry here.  A printer wanting the opening model too must read
    /// it off the model fold's own memo; this fold does not duplicate it.
    model: Option<(String, String)>,
    /// The [`Seq`] of the first row this fold ever held, remembered past
    /// eviction — the door [`Self::rows`] no longer names once the window
    /// has moved off the session's opening row.
    origin: Option<Seq>,
}

impl Blocks {
    pub fn rows(&self) -> &[Block] {
        &self.rows
    }

    /// This memo's current revision — the watermark a printer syncs at and
    /// then compares each row's own [`Block::rev`] against.
    pub fn rev(&self) -> u64 {
        self.rev
    }

    /// The [`Seq`] of the first row this fold ever held — `None` until one
    /// lands.  Unlike `rows().first()` it survives eviction, so a printer can
    /// ask whether its window still reaches the session's opening row: the
    /// question chrome drawn *before* any row, the startup banner, hangs on.
    pub fn origin(&self) -> Option<Seq> {
        self.origin
    }

    /// Cumulative input tokens billed so far, per the forensic usage trail —
    /// the context-floor numerator a printer grades committed prose against.
    pub fn input_tokens(&self) -> u64 {
        self.usage.input
    }

    pub fn output_tokens(&self) -> u64 {
        self.usage.output
    }

    /// `(model, provider)` of the most recent switch, if any.  See
    /// [`Self::model`]'s field doc for why a session's opening model is not
    /// available here.
    pub fn model(&self) -> Option<(&str, &str)> {
        self.model.as_ref().map(|(m, p)| (m.as_str(), p.as_str()))
    }

    /// A rendering of every resident block, content only — never styling —
    /// the regenerable text a printer's `user.log` is a render of, never a
    /// patch of.  Windowed, like [`Self::rows`]; a full-session render reads
    /// `record.jsonl` through [`super::replay`] instead.
    pub fn render_log(&self) -> String {
        let mut out = String::new();
        for block in &self.rows {
            render_block_text(&mut out, block.kind());
        }
        out
    }

    /// Drop the oldest resident rows past [`BLOCKS_WINDOW`], as each row
    /// lands — the fold's whole bound, and unconditional, since no cursor of
    /// another's lives here to hold the floor down.
    fn evict(&mut self) {
        while self.rows.len() > BLOCKS_WINDOW {
            let _ = self.rows.remove(0);
        }
    }

    /// Open a row for `kind` — or, where `kind` continues the lane the last
    /// row already holds, grow that row instead.
    ///
    /// The model's prose and reasoning arrive as many records, one per line,
    /// so a reader sees the text as it is spoken.  A block is the run of
    /// records that meet: any record of another kind — a tool call, a prompt
    /// — ends the run, and the next line of prose opens a fresh block.  This
    /// is what frees the commit producer from having to cut
    /// anywhere meaningful, and it keeps a block's `Seq` the one it opened
    /// with, so a reveal dial set on it survives the growth.
    fn push(&mut self, seq: Seq, kind: BlockKind) {
        self.rev += 1;
        let rev = self.rev;
        if let Some(row) = self.rows.last_mut() {
            match (&mut row.kind, &kind) {
                (BlockKind::Answer { text }, BlockKind::Answer { text: more })
                | (BlockKind::Thinking { text }, BlockKind::Thinking { text: more }) => {
                    text.push_str(more);
                    row.rev = rev;
                    return;
                }
                _ => {}
            }
        }
        let _ = self.origin.get_or_insert(seq);
        self.rows.push(Block { seq, kind, rev });
        self.evict();
    }

    /// Attach a result's line count to the call it names — a patch record
    /// addressed by `BlockId`, replacing the tail-walk `set_result_size`
    /// used to guess at the same correlation.  A target this fold cannot
    /// find — evicted, or simply never resident — is a no-op rather than a
    /// panic.
    fn attach_result(&mut self, call: super::BlockId, text: &str) {
        let n = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
        let target = call.seq();
        self.rev += 1;
        let rev = self.rev;
        if let Some(row) = self.rows.iter_mut().find(|b| b.seq == target)
            && let BlockKind::ToolCall { result_lines, .. } = &mut row.kind
        {
            *result_lines = Some(n);
            row.rev = rev;
        }
    }
}

/// Plain-text content for one block, appended to `out` — the shared body
/// [`Blocks::render_log`] and (once wired) a printer's own richer rendering
/// both start from, kept here so the regenerable projection has exactly one
/// definition.
fn render_block_text(out: &mut String, kind: &BlockKind) {
    let line = match kind {
        BlockKind::Thinking { text, .. } => format!("∴ {text}"),
        BlockKind::Prompt { text } => format!("> {text}"),
        BlockKind::Answer { text }
        | BlockKind::SystemNote { text }
        | BlockKind::HarnessResult { text } => text.clone(),
        BlockKind::ToolCall {
            tool,
            cmd,
            summary,
            result_lines,
        } => {
            let label = summary.as_deref().unwrap_or(cmd);
            match result_lines {
                Some(n) => format!("▸ {tool}: {label} ({n} lines)"),
                None => format!("▸ {tool}: {label}"),
            }
        }
        BlockKind::HarnessCall {
            verb,
            subject,
            payload,
            failed,
        } => {
            let subject = subject.as_deref().unwrap_or_default();
            let mark = if *failed { " (failed)" } else { "" };
            format!("↗ {verb} {subject} {payload}{mark}")
        }
        BlockKind::SubagentDone {
            name,
            text,
            error,
            elapsed_ms,
        } => {
            #[allow(
                clippy::cast_precision_loss,
                reason = "elapsed-ms display precision; far below f64's mantissa"
            )]
            let secs = *elapsed_ms as f64 / 1000.0;
            match error {
                Some(e) => format!("↘ {name} failed in {secs:.1}s — {e}"),
                None => format!("↘ {name} done in {secs:.1}s\n{text}"),
            }
        }
        BlockKind::Observation { value } => format!("· {value:?}"),
        BlockKind::ObservationGroup { values } => format!("· {} items", values.len()),
        BlockKind::Card { marks } => format!("· {marks}"),
        BlockKind::Done { outcome } => match outcome {
            DoneOutcome::Ok => "[done: ok]".to_string(),
            DoneOutcome::Err { message, status } => {
                format!("[done: error {status} — {message}]")
            }
            DoneOutcome::Panic { message } => format!("[done: panic — {message}]"),
        },
        BlockKind::Notice { notice } => match notice {
            NoticeFact::Reap { cmd, cause } => format!("[reap: {cmd} ({cause})]"),
            NoticeFact::Prune { names, .. } => format!("[prune: {}]", names.join(", ")),
        },
        BlockKind::Context { rows } => {
            let lines: Vec<String> = rows
                .iter()
                .map(|row| {
                    format!(
                        "[context: exchange {} {} {}]",
                        row.exchange, row.kind, row.opening
                    )
                })
                .collect();
            lines.join("\n")
        }
        BlockKind::Cancelled => "[cancelled]".to_string(),
        BlockKind::Error { text } => format!("error: {text}"),
        BlockKind::Nudge { used, max, cause } => format!("[nudge {used}/{max}: {cause}]"),
        BlockKind::ProviderError { error } => format!("provider error: {error:?}"),
        BlockKind::Stalled { error } => format!("stream stalled, turn resumes: {error:?}"),
        BlockKind::ModelChanged { model, provider } => {
            format!("[model changed: {provider}/{model}]")
        }
        BlockKind::Step { n } => format!("[step {n}]"),
        BlockKind::ContextEdited { op, by } => {
            let authority = match by {
                EditAuthority::Model => "model",
                EditAuthority::User => "user",
                EditAuthority::Harness => "harness",
            };
            match op {
                ContextOp::Fold {
                    through_exchange, ..
                } => format!("[context folded through exchange {through_exchange} ({authority})]"),
                ContextOp::Drop { exchanges } => {
                    let list = exchanges
                        .iter()
                        .map(u64::to_string)
                        .collect::<Vec<_>>()
                        .join(", ");
                    format!("[context dropped exchange(s) {list} ({authority})]")
                }
            }
        }
    };
    out.push_str(&line);
    out.push('\n');
}

fn step_display(memo: &mut Blocks, seq: Seq, d: Display) {
    match d {
        Display::Thinking { text } => memo.push(seq, BlockKind::Thinking { text }),
        Display::Prompt { text } => memo.push(seq, BlockKind::Prompt { text }),
        Display::Answer { text } => memo.push(seq, BlockKind::Answer { text }),
        Display::ToolCall { tool, cmd, summary } => memo.push(
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
        } => memo.push(
            seq,
            BlockKind::HarnessCall {
                verb,
                subject,
                payload,
                failed,
            },
        ),
        Display::Result { text, call } => memo.attach_result(call, &text),
        Display::ObservationGroup { values } => {
            memo.push(seq, BlockKind::ObservationGroup { values });
        }
        Display::SubagentDone {
            name,
            text,
            error,
            elapsed_ms,
        } => memo.push(
            seq,
            BlockKind::SubagentDone {
                name,
                text,
                error,
                elapsed_ms,
            },
        ),
        Display::Observation { value } => memo.push(seq, BlockKind::Observation { value }),
        Display::Card { marks } => memo.push(seq, BlockKind::Card { marks }),
        Display::Done { outcome } => memo.push(seq, BlockKind::Done { outcome }),
        Display::Notice { notice } => memo.push(seq, BlockKind::Notice { notice }),
        Display::Context { rows } => memo.push(seq, BlockKind::Context { rows }),
        Display::Step { n } => memo.push(seq, BlockKind::Step { n }),
        Display::ContextEdited { op, by } => {
            memo.push(seq, BlockKind::ContextEdited { op, by });
        }
    }
}

fn step_forensic(memo: &mut Blocks, seq: Seq, f: Forensic) {
    match f {
        Forensic::UsageDelta { usage } => {
            memo.usage.input = memo.usage.input.saturating_add(usage.input);
            memo.usage.output = memo.usage.output.saturating_add(usage.output);
        }
        Forensic::Cancelled => memo.push(seq, BlockKind::Cancelled),
        Forensic::Error { text } => memo.push(seq, BlockKind::Error { text }),
        Forensic::Nudge { used, max, cause } => {
            memo.push(seq, BlockKind::Nudge { used, max, cause });
        }
        Forensic::ProviderError { error } => memo.push(seq, BlockKind::ProviderError { error }),
        Forensic::Stalled { error } => memo.push(seq, BlockKind::Stalled { error }),
        Forensic::SystemNote { text } => memo.push(seq, BlockKind::SystemNote { text }),
        Forensic::HarnessResult { text } => memo.push(seq, BlockKind::HarnessResult { text }),
        // The history informs a resume note; the live register follows the
        // shell boundary and is not restored — so neither is a scrollback
        // row this fold draws.
        Forensic::Pin { .. } | Forensic::Unpin { .. } => {}
        Forensic::ModelChanged { model, label, .. } => memo.model = Some((model, label)),
    }
}

/// The view fold: [`Fold::step`] over [`Display`] and [`Forensic`], skipping
/// [`super::Protocol`] by an explicit arm rather than a wildcard.
pub struct View;

impl Fold for View {
    type Memo = Blocks;

    fn step(memo: &mut Blocks, record: &Recorded<super::Record>) -> Result<(), Refusal> {
        let seq = record.stamp().seq();
        match record.value().clone() {
            super::Record::Protocol(_) => {}
            super::Record::Display(d) => step_display(memo, seq, d),
            super::Record::Forensic(f) => step_forensic(memo, seq, f),
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A row that never joins the one before it, so a test about flushing or
    /// eviction counts rows rather than the lanes that grow.
    fn push(memo: &mut Blocks, seq: u64, text: &str) {
        step_forensic(
            memo,
            Seq::new(seq),
            Forensic::SystemNote { text: text.into() },
        );
    }

    /// One lane, many records, one block: consecutive prose grows the row it
    /// opened, and a record of another kind ends the run.
    #[test]
    fn consecutive_records_of_one_lane_grow_a_single_block() {
        let mut memo = Blocks::default();
        for (seq, text) in [(1, "first line\n"), (2, "second line\n")] {
            step_display(
                &mut memo,
                Seq::new(seq),
                Display::Answer { text: text.into() },
            );
        }
        match memo.rows() {
            [row] => {
                let BlockKind::Answer { text } = row.kind() else {
                    panic!("expected one answer block")
                };
                assert_eq!(text, "first line\nsecond line\n", "the block holds the run");
            }
            other => panic!("expected one row, got {} rows", other.len()),
        }

        step_display(
            &mut memo,
            Seq::new(3),
            Display::ToolCall {
                tool: "ral".into(),
                cmd: "ls".into(),
                summary: None,
            },
        );
        step_display(
            &mut memo,
            Seq::new(4),
            Display::Answer {
                text: "after the call\n".into(),
            },
        );
        assert_eq!(
            memo.rows().len(),
            3,
            "the tool call ends the run, so the prose after it opens its own block"
        );
    }

    #[test]
    fn the_window_bounds_the_fold_as_rows_land() {
        let mut memo = Blocks::default();
        let total = BLOCKS_WINDOW + 50;
        for i in 1..=total {
            push(&mut memo, i as u64, &format!("row {i}"));
        }
        assert_eq!(
            memo.rows().len(),
            BLOCKS_WINDOW,
            "the window binds while rows land, with no flush to wait on"
        );
        assert_eq!(
            memo.rows().first().map(|b| b.seq),
            Some(Seq::new(51)),
            "the oldest rows go first"
        );
        assert_eq!(
            memo.origin(),
            Some(Seq::new(1)),
            "the session's opening row is remembered past its eviction"
        );
    }
}
