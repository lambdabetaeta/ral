//! Headless frontend: a [`Sink`] projecting the bus onto a pair of writers.
//!
//! Headless text and JSON both project the root's deliberate `reply` to `out`
//! once the run finishes; progress, tool calls, cards, failures, and the run
//! summary go to `err`.
//! Recording is not this module's concern: every session writes its own
//! record at the seam, through [`crate::record`].

use crate::SessionInfo;
use crate::card::{self, Card, Landed, Mark, Row, observation_card};
use crate::host::Host;
use crate::port::{Signal, Sink, WORKER_PANIC_PREFIX};
use crate::provider::{Provider, Usage};
use crate::record::AgentId;
use crate::record::fault::Remedy;
use crate::record::{
    self, Blocks, ChildStatus, Control, Delta, Record, Recorded, Settlement, Transient,
};
use crate::shell_eval::user_json;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::shell::runtime::{Observation, Observed};
use std::io::{self, Write};
use std::sync::mpsc::RecvTimeoutError;
use std::time::{Duration, Instant};

const POLL: Duration = Duration::from_millis(10);

/// What the root agent's output looks like on stdout in headless mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Write the agent's final reply as readable ral text. This is the default.
    Text,
    /// Write one JSON result object after the run ends. It contains the reply,
    /// stop reason, turn count, duration, token use and cost.
    Json,
}

/// The [`Sink`] behind [`run`], accumulating the run's
/// totals for the closing `[done]` line and the deliberate reply.
pub struct Headless<'a> {
    out: &'a mut (dyn Write + Send),
    err: &'a mut (dyn Write + Send),
    /// Fleet usage is read once from the port, never added by the sink.
    usage: Usage,
    root_id: AgentId,
    started: Instant,
    /// Counted, not tracked: a `Forensic::TurnStarted { turn }` names the request's own
    /// id, and a cancelled request retaken names it twice.
    turns: u32,
    last_stop: Option<String>,
    /// The root's deliberate `reply`, kept as a value until the consuming
    /// projection chooses ral text or [`user_json`].
    reply: Option<ral_core::data::FOValue>,
    panicked: bool,
    ended_with_newline: bool,
    fold: Blocks,
    status: Option<ChildStatus>,
    ended: bool,
    fault: Option<String>,
    terminal_failures: Vec<String>,
}

impl<'a> Headless<'a> {
    fn new(
        root_id: AgentId,
        out: &'a mut (dyn Write + Send),
        err: &'a mut (dyn Write + Send),
    ) -> Self {
        Self {
            out,
            err,
            usage: Usage::default(),
            root_id,
            started: Instant::now(),
            turns: 0,
            last_stop: None,
            reply: None,
            panicked: false,
            ended_with_newline: false,
            fold: Blocks::default(),
            status: None,
            ended: false,
            fault: None,
            terminal_failures: Vec::new(),
        }
    }

    fn finished(&self) -> bool {
        self.status.is_some() || self.ended
    }

    fn verdict(&mut self) -> Result<(), String> {
        if !self.terminal_failures.is_empty() {
            return Err(self.terminal_failures.join("\n"));
        }
        match self.status.take() {
            Some(ChildStatus::Replied(value)) => {
                self.reply = Some(value);
                Ok(())
            }
            Some(ChildStatus::Failed(reason)) => Err(reason),
            Some(ChildStatus::Stopped) => Err("the agent was stopped".into()),
            None => Err(self
                .fault
                .take()
                .unwrap_or_else(|| "the agent ended without a reply".into())),
        }
    }

    /// Write the deliberate reply as ral's existing `VALUE` projection. A
    /// unit reply has no textual projection and therefore writes nothing.
    fn write_reply(&mut self) -> io::Result<bool> {
        let Some(reply) = &self.reply else {
            return Ok(false);
        };
        let Some(text) = crate::shell_eval::ral_value_to_text(reply) else {
            return Ok(false);
        };
        self.out.write_all(text.as_bytes())?;
        self.out.flush()?;
        self.ended_with_newline = text.as_bytes().last() == Some(&b'\n');
        Ok(true)
    }
}

/// Condense a surfaced [`Card`]'s mark tree to stderr lines.  This is only a
/// rendering; the trace keeps the structural fact each card was composed from.
fn card_stderr(card: &Card) -> Vec<String> {
    let mut out = Vec::new();
    for mark in card.marks() {
        match mark {
            Mark::Text { spans } => {
                let text: String = spans.iter().map(|s| s.text.as_str()).collect();
                out.extend(text.lines().map(|l| format!("  {l}")));
            }
            Mark::Measure(m) => out.push(format!("[{}: {}]", m.label, m.readout.plain())),
            Mark::Fields { rows } => {
                for f in rows {
                    out.push(format!("  {}: {}", f.label, f.value.plain()));
                }
            }
            Mark::Diff { path, hunks } => {
                out.push(format!("[diff: {path}]"));
                for h in hunks {
                    for row in &h.rows {
                        let text = row.text();
                        out.push(match row {
                            Row::Context(_) => format!("    {text}"),
                            Row::Del(_) => format!("  - {text}"),
                            Row::Add(_) => format!("  + {text}"),
                        });
                    }
                }
            }
            Mark::Raw { bytes } => {
                let text = String::from_utf8_lossy(bytes);
                out.extend(text.lines().map(|l| format!("  {l}")));
            }
        }
    }
    out
}

/// The `--output-format json` result object on stdout.  Field names mirror
/// Claude Code's headless result so a harness can parse either agent
/// identically; `result` diverges in *type* — `string | object | array | null`,
/// so a structured reply is not double-encoded into a JSON string.
fn result_json(h: &Headless, r: &Result<(), String>, elapsed: std::time::Duration) -> String {
    use serde_json::json;
    let u = &h.usage;
    #[allow(
        clippy::cast_possible_truncation,
        reason = "elapsed-ms fits u64 for any real run"
    )]
    let duration_ms = elapsed.as_millis() as u64;
    let mut obj = json!({
        "type": "result",
        "is_error": r.is_err() || h.panicked,
        "result": h.reply.as_ref().map(user_json),
        "stop_reason": h.last_stop.clone().unwrap_or_else(|| {
            // Neither fallback may default to "completed": a recovered panic
            // leaves no StopReason, and an error with none means the run ended
            // before producing a `reply`.  The `error` field carries the detail.
            if h.panicked {
                "panicked".into()
            } else if r.is_err() {
                "no_reply".into()
            } else {
                "completed".into()
            }
        }),
        "num_turns": h.turns,
        "duration_ms": duration_ms,
        "total_cost_usd": (u.unreported == 0).then_some(u.dollars),
        "usage": {
            "input_tokens": u.input,
            "output_tokens": u.output,
            "cache_creation_input_tokens": u.cache_creation,
            "cache_read_input_tokens": u.cache_read,
            "unreported_turns": u.unreported,
        },
    });
    if let Err(e) = r
        && let Some(map) = obj.as_object_mut()
    {
        map.insert("error".into(), json!(e));
    }
    serde_json::to_string(&obj)
        .unwrap_or_else(|_| String::from(r#"{"type":"result","is_error":true}"#))
}

impl Sink for Headless<'_> {
    fn fact(&mut self, id: AgentId, rec: &Recorded<Record>) {
        if id != self.root_id {
            return;
        }
        self.step(rec);
        if let Record::Control(Control::Status { status }) = rec.value() {
            self.status = Some(status.clone());
        }
    }

    fn transient(&mut self, id: AgentId, t: &Transient) {
        if id != self.root_id
            && !matches!(
                t,
                Transient::Fault { .. } | Transient::Elided { .. } | Transient::Died { .. }
            )
        {
            return;
        }
        match t {
            Transient::StopReason(raw) => {
                self.last_stop = Some(raw.clone());
                let _ = writeln!(self.err, "[stop: {raw}]");
            }
            Transient::Fault { text } => {
                let _ = writeln!(self.err, "{text}");
                self.fault = Some(text.clone());
            }
            Transient::Elided { text } => {
                let _ = writeln!(self.err, "{text}");
            }
            Transient::Died { failures } => {
                self.panicked |= failures
                    .iter()
                    .any(|failure| failure.starts_with(WORKER_PANIC_PREFIX));
                if id == self.root_id {
                    self.terminal_failures.clone_from(failures);
                    self.ended = true;
                }
            }
            Transient::Resources { card, .. } => {
                for line in card_stderr(card) {
                    let _ = writeln!(self.err, "{line}");
                }
            }
            // A pin overwrites a TUI register; headless has none.  The stream
            // boundary retires a live edge nothing here holds, and the turn
            // it closes is counted off `Forensic::TurnStarted` instead.
            Transient::Boundary
            | Transient::Token(_)
            | Transient::Thinking(_)
            | Transient::State(_)
            | Transient::Pin { .. }
            | Transient::Unpin { .. } => {}
        }
    }
}

impl Headless<'_> {
    /// Step the root's fold and print what the step opened.
    ///
    /// A block opened is a block to narrate; a block *grown* is prose or
    /// reasoning, the two lanes this projection draws nothing for at all, and
    /// a patched call's line count reaches stderr through no line either.
    fn step(&mut self, rec: &Recorded<Record>) {
        let mut fold = std::mem::take(&mut self.fold);
        let delta = fold.step(rec);
        if let (Delta::Opened(_) | Delta::Cleared, Some(block)) = (delta, fold.blocks().last()) {
            self.print_block(block.kind());
        }
        self.fold = fold;
    }

    /// One fold block, projected onto stderr as a rendering chosen fresh
    /// each time from the block kinds this frontend's own memo holds.
    #[allow(clippy::match_same_arms)]
    fn print_block(&mut self, kind: &record::BlockKind) {
        use record::BlockKind as K;
        match kind {
            K::ToolCall {
                tool, cmd, summary, ..
            } => {
                let _ = writeln!(self.err, "[tool: {tool}]");
                if let Some(s) = summary {
                    for line in s.lines() {
                        let _ = writeln!(self.err, "  {line}");
                    }
                }
                for line in cmd.lines() {
                    let _ = writeln!(self.err, "  {line}");
                }
            }
            // A stderr line has no rail hue and no column budget, so an act
            // wears the same `[tool: …]` shape a call does.
            K::HarnessCall {
                verb,
                subject,
                payload,
                ..
            } => {
                let _ = writeln!(self.err, "[tool: {verb}]");
                if let Some(s) = subject {
                    let _ = writeln!(self.err, "  {s}");
                }
                for line in payload.lines() {
                    let _ = writeln!(self.err, "  {line}");
                }
            }
            // Sub-agent tokens never reach `out`, so this breadcrumb is the
            // only live sign of a child; its own log dir holds the rest.
            K::SubagentDone {
                name,
                outcome,
                elapsed_ms,
            } => {
                let took =
                    crate::card::elapsed_phrase(std::time::Duration::from_millis(*elapsed_ms));
                let _ = match outcome {
                    Settlement::Replied => {
                        writeln!(self.err, "[agent {name} replied after {took}]")
                    }
                    Settlement::Failed { reason } => {
                        writeln!(self.err, "[agent {name} failed after {took}: {reason}]")
                    }
                    Settlement::Stopped => {
                        writeln!(self.err, "[agent {name} stopped after {took}]")
                    }
                };
            }
            K::Error { text } => {
                if text.starts_with(WORKER_PANIC_PREFIX) {
                    self.panicked = true;
                }
                let _ = writeln!(self.err, "error: {text}");
            }
            K::SystemNote { text } => {
                let _ = writeln!(self.err, "{text}");
            }
            K::ProviderError { error } => self.print_readout(&record::fault::Readout::fatal(error)),
            K::Stalled { error } => self.print_readout(&record::fault::Readout::stall(error)),
            K::Observation { value } => self.print_observation(value),
            K::Change { change } => self.print_card(&card::change_card(change)),
            K::Card { card } => self.print_card(card),
            K::Done {
                id,
                title,
                elapsed_ms,
                outcome,
            } => {
                let elapsed = elapsed_ms.map(std::time::Duration::from_millis);
                let _ = writeln!(
                    self.err,
                    "{}",
                    card::settled_text(*id, title, elapsed, outcome)
                );
            }
            K::Context { rows } => {
                self.print_card(&record::context_rows_card(rows));
            }
            K::Turn { id: turn } => {
                self.turns += 1;
                let _ = writeln!(self.err, "[turn {turn}]");
            }
            K::Evicted { eviction } => {
                let _ = writeln!(self.err, "{}", eviction.evicted_line());
            }
            K::Rewound { anchor } => {
                let _ = writeln!(self.err, "[rewound to turn {anchor}]");
            }
            K::Cleared => {
                let _ = writeln!(self.err, "[cleared]");
            }
            // Interactive-only, pure presentation, or — the nudge — the agent
            // steering itself, which stays forensic and never addresses the
            // caller.
            K::Thinking { .. }
            | K::Prompt { .. }
            | K::Answer { .. }
            | K::Cancelled
            | K::HarnessResult { .. }
            | K::Nudge { .. } => {}
        }
    }

    fn print_card(&mut self, card: &Card) {
        for line in card_stderr(card) {
            let _ = writeln!(self.err, "{line}");
        }
    }

    /// A provider failure's [`record::fault::Readout`] as plain stderr lines,
    /// following [`card_stderr`]'s `Mark::Fields` convention (`  label: value`).
    /// A multi-line value (`prettify` can produce one) keeps its two-space
    /// indent on continuation lines, so it never reads as a new field.
    fn print_readout(&mut self, readout: &record::fault::Readout) {
        let _ = writeln!(self.err, "error: {}", readout.headline);
        for f in &readout.fields {
            let value = match &f.datum {
                record::fault::Datum::Text(text) => text.clone(),
                record::fault::Datum::Seconds(secs) => crate::clock::hms(*secs),
            };
            let mut lines = value.split('\n');
            let first = lines.next().unwrap_or("");
            let _ = writeln!(self.err, "  {}: {first}", f.label);
            // Deeper than a label line: a `key: value` continuation from a
            // pretty-printed body must not read as the next field.
            for line in lines {
                let _ = writeln!(self.err, "    {line}");
            }
        }
        for remedy in &readout.remedies {
            let hint = match remedy {
                Remedy::Evict => continue,
                Remedy::Reroute => {
                    "rerun with --model to pick a model or endpoint with a larger window"
                }
                Remedy::RaiseMaxTokens => {
                    "rerun with --max-tokens N, or split the turn into smaller writes"
                }
                Remedy::SignIn => "run `exarch login <service>`",
            };
            let _ = writeln!(self.err, "  hint: {hint}");
        }
    }

    fn print_observation(&mut self, value: &FOValue) {
        let Ok(obs) = Observation::decode(value) else {
            return;
        };
        self.print_observed(&obs.what);
    }

    fn print_observed(&mut self, what: &Observed) {
        let Some(landed) = Landed::of(what) else {
            return;
        };
        let card = observation_card(&landed);
        self.print_card(&card);
    }
}

/// Read the root's report in a one-shot headless run from `seed`.
/// Text and JSON both write the root's deliberate reply to stdout once; all
/// progress and operational output goes to stderr.
///
/// # Errors
/// Returns `Err` if no launch prompt was supplied, the agent ends without a
/// reply, or writing the reply fails.
pub fn run(
    host: &Host,
    info: &SessionInfo<'_>,
    p: &Provider,
    seed: Option<String>,
    format: OutputFormat,
) -> Result<(), String> {
    run_to(
        host,
        info,
        p,
        seed,
        format,
        (&mut io::stdout(), &mut io::stderr()),
    )
}

fn run_to(
    host: &Host,
    info: &SessionInfo<'_>,
    p: &Provider,
    seed: Option<String>,
    format: OutputFormat,
    (stdout, stderr): (&mut (dyn Write + Send), &mut (dyn Write + Send)),
) -> Result<(), String> {
    let prompt = seed.ok_or("--headless requires a seed prompt: --prompt or --file")?;
    // The account id, not a label: a label is relative to a set this banner
    // does not have, and the id is what `--provider` takes to reproduce the run;
    // `via=` prints what `--via` takes.
    let via = p.via().map(|via| format!(" via={via}")).unwrap_or_default();
    let _ = writeln!(
        stderr,
        "exarch: provider={} model={}{via} base={}",
        p.account().id,
        p.model(),
        info.base
    );
    let root = host.children()[0];
    let log_dir = match host.feed().recv() {
        Ok(Signal::Born(id, born)) if id == root => born.log_dir,
        _ => return Err("the first signal of a run was not its root's birth".into()),
    };
    let port = host.port();
    let mut headless = Headless::new(root, stdout, stderr);
    let refused = port.submit(root, prompt).is_err();
    // A refused submit awaits the end, which reports the real fault.
    let done = |h: &Headless<'_>| if refused { h.ended } else { h.finished() };
    while !done(&headless) {
        match host.feed().recv_timeout(POLL) {
            Ok(sig) => headless.accept(sig),
            Err(RecvTimeoutError::Timeout) => {}
            Err(RecvTimeoutError::Disconnected) => break,
        }
    }
    if refused
        && headless.status.is_none()
        && headless.fault.is_none()
        && headless.terminal_failures.is_empty()
    {
        headless.fault = Some("the agent ended before it could take the prompt".into());
    }
    let mut r = headless.verdict();
    headless.usage = port.usage();
    let elapsed = headless.started.elapsed();
    // A write failure here (e.g. a closed pipe) must not report as the clean
    // success `r` would otherwise carry — text and JSON both project the same
    // reply the run produced, whether or not the run itself succeeded.
    let write_err = match format {
        OutputFormat::Json => {
            let result = result_json(&headless, &r, elapsed);
            writeln!(headless.out, "{result}").err()
        }
        OutputFormat::Text => match headless.write_reply() {
            Ok(true) if !headless.ended_with_newline => {
                // So the next shell prompt lands at column 1.
                writeln!(headless.out).err()
            }
            Ok(_) => None,
            Err(e) => Some(e),
        },
    };
    if let Some(e) = write_err
        && r.is_ok()
    {
        r = Err(format!("writing reply: {e}"));
    }
    let _ = writeln!(headless.err, "Agent log: {log_dir}");
    let _ = writeln!(
        headless.err,
        "[done] {} turns · {:.1}s · {}",
        headless.turns,
        elapsed.as_secs_f64(),
        headless.usage
    );
    r
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
