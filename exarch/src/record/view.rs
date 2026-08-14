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
        answer_chars: u32,
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
}

impl Block {
    fn new(seq: Seq, kind: BlockKind) -> Self {
        Self { seq, kind }
    }

    pub fn id(&self) -> super::BlockId {
        super::BlockId::new(self.seq)
    }

    pub fn kind(&self) -> &BlockKind {
        &self.kind
    }
}

/// Cumulative input/output tokens this session has billed, per the forensic
/// usage trail — one of the fidelity inputs this fold admits Forensic for.
#[derive(Default, Clone, Copy)]
struct UsageTotal {
    input: u64,
    output: u64,
}

/// Past this many resident rows, the oldest already-flushed ones are
/// dropped — replacing the old per-printer `SYNC_WINDOW`, so a live memo is
/// bounded once for every printer rather than trimmed per call.  Nothing
/// before [`Blocks::rendered_through`] is ever evicted, so a flush never
/// loses a row it has not yet rendered.
const BLOCKS_WINDOW: usize = 1000;

/// The view fold's memo: every commit and drawn breadcrumb this session has
/// recorded, in log order, windowed once flushed.
///
/// A printer's own window ([`VIEWPORT_MAX_BLOCKS`](crate::tui) and friends)
/// stays a second, purely presentational trim on top of this one.
pub struct Blocks {
    rows: Vec<Block>,
    usage: UsageTotal,
    /// The model in force, from the most recent [`Forensic::ModelChanged`].
    /// A session's *first* model rides `Protocol::SessionStarted`, which is
    /// outside this fold's class — so a session that never switches models
    /// has no entry here.  A printer wanting the opening model too must read
    /// it off the model fold's own memo; this fold does not duplicate it.
    model: Option<(String, String)>,
    /// The highest [`Seq`] a flush has already rendered to `user.log` —
    /// [`Self::render_since`]'s cursor, and the floor eviction never crosses.
    rendered_through: Seq,
}

impl Default for Blocks {
    fn default() -> Self {
        Self {
            rows: Vec::new(),
            usage: UsageTotal::default(),
            model: None,
            rendered_through: Seq::new(0),
        }
    }
}

impl Blocks {
    pub fn rows(&self) -> &[Block] {
        &self.rows
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

    /// Render every row since the last flush and advance
    /// [`Self::rendered_through`] to cover it — what a printer's append-only
    /// `user.log` writes each commit.  Appending this is *equal* to
    /// rendering the whole resident window, which is the regenerability
    /// identity [`Self::render_log`] states as an invariant made testable.
    pub fn render_since(&mut self) -> String {
        let mut out = String::new();
        let mut through = self.rendered_through;
        for block in &self.rows {
            if block.seq > self.rendered_through {
                render_block_text(&mut out, block.kind());
                through = block.seq;
            }
        }
        self.rendered_through = through;
        self.evict();
        out
    }

    /// Drop the oldest resident rows past [`BLOCKS_WINDOW`], never crossing
    /// [`Self::rendered_through`] — a row a flush has not yet rendered is
    /// never lost to eviction.
    fn evict(&mut self) {
        while self.rows.len() > BLOCKS_WINDOW
            && self
                .rows
                .first()
                .is_some_and(|b| b.seq <= self.rendered_through)
        {
            let _ = self.rows.remove(0);
        }
    }

    fn push(&mut self, seq: Seq, kind: BlockKind) {
        self.rows.push(Block::new(seq, kind));
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
        if let Some(BlockKind::ToolCall { result_lines, .. }) = self
            .rows
            .iter_mut()
            .find(|b| b.seq == target)
            .map(Block::kind_mut)
        {
            *result_lines = Some(n);
        }
    }
}

impl Block {
    fn kind_mut(&mut self) -> &mut BlockKind {
        &mut self.kind
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
        Display::Thinking { text, answer_chars } => {
            memo.push(seq, BlockKind::Thinking { text, answer_chars });
        }
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
        Forensic::ModelChanged { model, provider } => memo.model = Some((model, provider)),
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

    fn push(memo: &mut Blocks, seq: u64, text: &str) {
        step_display(memo, Seq::new(seq), Display::Answer { text: text.into() });
    }

    #[test]
    fn render_since_over_a_fresh_memo_equals_render_log() {
        let mut memo = Blocks::default();
        for i in 1..=5u64 {
            push(&mut memo, i, &format!("line {i}"));
        }
        assert_eq!(memo.render_since(), memo.render_log());
    }

    #[test]
    fn a_second_flush_renders_only_the_rows_since_the_first() {
        let mut memo = Blocks::default();
        push(&mut memo, 1, "one");
        let _ = memo.render_since();
        push(&mut memo, 2, "two");
        assert_eq!(memo.render_since(), "two\n");
    }

    #[test]
    fn eviction_never_crosses_the_flush_cursor() {
        let mut memo = Blocks::default();
        let total = BLOCKS_WINDOW + 50;
        for i in 1..=total {
            push(&mut memo, i as u64, &format!("row {i}"));
        }
        assert_eq!(
            memo.rows().len(),
            total,
            "nothing is flushed yet, so nothing may be evicted"
        );
        let rendered = memo.render_since();
        assert_eq!(rendered.lines().count(), total);
        assert_eq!(
            memo.rows().len(),
            BLOCKS_WINDOW,
            "past the window, only rows already flushed evict"
        );
    }
}
