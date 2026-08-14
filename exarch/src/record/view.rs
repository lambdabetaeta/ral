//! The view fold: folds [`Display`] commits and the [`Forensic`] rows a
//! scrollback draws into [`Blocks`] — the memo `tui` and `headless` both
//! draw from as printers, never handed a [`Record`] of their own.
//!
//! [`Block`]'s constructor is private to this module: a printer draws a
//! block, it cannot mint one.  `step` is one exhaustive match over the
//! outer `Record`, and `Protocol` is skipped by an explicit arm — this fold
//! carries no model-context state, so it has nothing to fold a protocol
//! record into.

use super::{
    ContextRow, Display, Fold, Forensic, Recorded, Refusal, Seq,
};
use crate::agent::event::ProviderErrorRecord;
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
    ToolCall {
        tool: String,
        cmd: String,
        summary: Option<String>,
        /// Attached after the fact by [`Blocks::attach_result`] — the one
        /// correlation this fold cannot do by id (see the module's own note).
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
}

/// One committed row of scrollback, named by the [`Seq`] of the record that
/// produced it.
///
/// The fold keys on `Seq` rather than [`super::BlockId`]: `BlockId` wraps a
/// `Seq` but exposes no way to read it back out or compare two for equality,
/// so it cannot serve as this fold's own correlation key — only as the
/// opaque handle [`Self::id`] hands to a caller that already has one to
/// compare against structurally (a patch record carrying its own `BlockId`,
/// once one exists — see the module's note on `Display::Result`).
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

/// The view fold's memo: every commit and drawn breadcrumb this session has
/// recorded, in log order.
///
/// Never evicts — a printer's own window
/// ([`VIEWPORT_MAX_BLOCKS`](crate::tui) and friends) is presentation, not
/// this fold's business, so a rendering of every block this memo ever
/// admitted is what makes `user.log` regenerable rather than patched.
#[derive(Default)]
pub struct Blocks {
    rows: Vec<Block>,
    usage: UsageTotal,
    /// The model in force, from the most recent [`Forensic::ModelChanged`].
    /// A session's *first* model rides `Protocol::SessionStarted`, which is
    /// outside this fold's class — so a session that never switches models
    /// has no entry here.  A printer wanting the opening model too must read
    /// it off the model fold's own memo; this fold does not duplicate it.
    model: Option<(String, String)>,
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

    /// A rendering of every committed block, content only — never styling —
    /// the regenerable text a printer's `user.log` is a render of, never a
    /// patch of.  Prose paragraphs are not among the [`Display`] commits
    /// this fold can fold (see the module's own note), so this render omits
    /// them until that commit exists.
    pub fn render_log(&self) -> String {
        let mut out = String::new();
        for block in &self.rows {
            render_block_text(&mut out, block.kind());
        }
        out
    }

    fn push(&mut self, seq: Seq, kind: BlockKind) {
        self.rows.push(Block::new(seq, kind));
    }

    /// Attach a result's line count to the nearest preceding tool call.
    ///
    /// This is the tail-walk the plan's own text asks to retire in favour of
    /// a patch record naming its call by `BlockId` — retired here only in
    /// spelling, not in mechanism: `Display::Result` carries no target id to
    /// address instead, so the walk survives.  See the module's own note.
    fn attach_result(&mut self, text: &str) {
        let n = u32::try_from(text.lines().count()).unwrap_or(u32::MAX);
        if let Some(BlockKind::ToolCall { result_lines, .. }) = self
            .rows
            .iter_mut()
            .rev()
            .map(Block::kind_mut)
            .find(|k| matches!(k, BlockKind::ToolCall { .. }))
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
        BlockKind::SystemNote { text } | BlockKind::HarnessResult { text } => text.clone(),
        BlockKind::ModelChanged { model, provider } => {
            format!("[model changed: {provider}/{model}]")
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
        // No id of its own to address instead — see `Blocks::attach_result`.
        Display::Result { text } => memo.attach_result(&text),
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
