//! Headless frontend: a [`Sink`] projecting the bus onto a pair of writers.
//!
//! Headless text and JSON both project the root's deliberate `reply` to `out`
//! once the run finishes; progress, tool calls, cards, failures, and the run
//! summary go to `err`. A conversational host takes a different projection
//! through [`converse_on`], which streams assistant tokens, or drives
//! [`converse_sink`] with its own [`Sink`], seeing raw `Signal`s rather than
//! rendered text.
//! Recording is not this module's concern: every session writes its own
//! record at the seam, through [`crate::record`].

use crate::SessionInfo;
use crate::agent::Avatar;
use crate::bus::{AgentOutcome, FleetBus, Sink, pump};
use crate::card::{self, Card, Mark, Row, landing, observation_card};
use crate::provider::{Provider, Usage};
use crate::record::AgentId;
use crate::record::{self, Blocks, Delta, Record, Recorded, Transient};
use crate::shell_eval::user_json;
use ral_core::first_order::FOValue;
use ral_core::first_order::datum::Datum as _;
use ral_core::types::{Observation, Observed};
use std::collections::HashMap;
use std::io::{self, Write};
use std::time::Instant;

/// What the root agent's output looks like on stdout in headless mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Write the agent's final reply as readable ral text. This is the default.
    Text,
    /// Write one JSON result object after the run ends. It contains the reply,
    /// stop reason, turn count, duration, token use and cost.
    Json,
}

/// The three outbound projections have distinct names so a conversational
/// token stream cannot be confused with either headless reply sink.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Projection {
    HeadlessText,
    HeadlessJson,
    Conversation,
}

/// The [`Sink`] behind [`run`] and [`converse_on`], accumulating the run's
/// totals for the closing `[done]` line and the deliberate reply.
pub struct Headless<'a> {
    out: &'a mut (dyn Write + Send),
    err: &'a mut (dyn Write + Send),
    /// Read once from the bus's shared usage meter at the end of the run, so
    /// it counts muted sub-agents too.  A `Forensic::UsageDelta` reaching the
    /// sink is already in that meter and must not be added again.
    usage: Usage,
    root_id: AgentId,
    started: Instant,
    projection: Projection,
    /// Counted, not tracked: a `Display::Turn { id }` names the request's own
    /// id, and a cancelled request retaken names it twice.
    turns: u32,
    last_stop: Option<String>,
    /// The root's deliberate `reply`, kept as a value until the consuming
    /// projection chooses ral text or [`user_json`].
    reply: Option<ral_core::first_order::FOValue>,
    /// `pump` recovers from a worker unwind and returns the worker value as
    /// normal, so without latching the panic here a crashed exchange would
    /// report as a clean, empty success.
    panicked: bool,
    ended_with_newline: bool,
    /// One view-fold memo per source agent — a fact's `Seq` is only unique
    /// within its own session log, so root's commits and a child's may not
    /// share one fold.
    folds: HashMap<AgentId, Blocks>,
}

impl<'a> Headless<'a> {
    fn new(
        projection: Projection,
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
            projection,
            turns: 0,
            last_stop: None,
            reply: None,
            panicked: false,
            ended_with_newline: false,
            folds: HashMap::new(),
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
        "total_cost_usd": u.dollars,
        "usage": {
            "input_tokens": u.input,
            "output_tokens": u.output,
            "cache_creation_input_tokens": u.cache_creation,
            "cache_read_input_tokens": u.cache_read,
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
        self.step(id, rec);
    }

    fn transient(&mut self, id: AgentId, t: &Transient) {
        // Every frontend draws a fault whatever its source, since an
        // unwritable log is a fact about the plumbing, not one agent;
        // everything else here rides root's own register.
        if id != self.root_id && !matches!(t, Transient::Fault { .. }) {
            return;
        }
        match t {
            Transient::Token(text) if self.projection == Projection::Conversation => {
                let _ = self.out.write_all(text.as_bytes());
                let _ = self.out.flush();
                if let Some(last) = text.as_bytes().last() {
                    self.ended_with_newline = *last == b'\n';
                }
            }
            Transient::StopReason(raw) => {
                self.last_stop = Some(raw.clone());
                let _ = writeln!(self.err, "[stop: {raw}]");
            }
            // The seam's own diagnostic: drawn by every frontend whatever
            // else it is doing, since it is a fact about the log itself.
            Transient::Fault { text } => {
                let _ = writeln!(self.err, "{text}");
            }
            Transient::Resources { card, .. } | Transient::Limits { card } => {
                for line in card_stderr(card) {
                    let _ = writeln!(self.err, "{line}");
                }
            }
            // A pin overwrites a TUI register; headless has none.  The stream
            // boundary retires a live edge nothing here holds, and the turn
            // it closes is counted off `Display::Turn` instead.
            Transient::Boundary
            | Transient::Token(_)
            | Transient::Thinking(_)
            | Transient::State(_)
            | Transient::Born { .. }
            | Transient::Died
            | Transient::Cleared
            | Transient::Pin { .. }
            | Transient::Unpin { .. } => {}
        }
    }
}

impl Headless<'_> {
    /// Step this source agent's own fold and print what the step opened.
    ///
    /// A block opened is a block to narrate; a block *grown* is prose or
    /// reasoning, the two lanes this projection draws nothing for at all, and
    /// a patched call's line count reaches stderr through no line either.
    fn step(&mut self, id: AgentId, rec: &Recorded<Record>) {
        let mut fold = self.folds.remove(&id).unwrap_or_default();
        let delta = fold
            .step(rec)
            .expect("the view fold never refuses a live Display/Forensic record");
        if let (Delta::Opened(_), Some(block)) = (delta, fold.blocks().last()) {
            self.print_block(id, block.kind());
        }
        let _ = self.folds.insert(id, fold);
    }

    /// One fold block, projected onto stderr as a rendering chosen fresh
    /// each time from the block kinds this frontend's own memo holds.
    #[allow(clippy::match_same_arms)]
    fn print_block(&mut self, id: AgentId, kind: &record::BlockKind) {
        use record::BlockKind as K;
        match kind {
            K::ToolCall {
                tool, cmd, summary, ..
            } if id == self.root_id => {
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
            } if id == self.root_id => {
                let _ = writeln!(self.err, "[tool: {verb}]");
                if let Some(s) = subject {
                    let _ = writeln!(self.err, "  {s}");
                }
                for line in payload.lines() {
                    let _ = writeln!(self.err, "  {line}");
                }
            }
            K::ToolCall { .. } | K::HarnessCall { .. } => {}
            // Sub-agent tokens never reach `out`, so this breadcrumb is the
            // only live sign of a child; its own log dir holds the rest.
            K::SubagentDone {
                name,
                error,
                elapsed_ms,
                ..
            } => {
                let took =
                    crate::bus::elapsed_phrase(std::time::Duration::from_millis(*elapsed_ms));
                match error {
                    Some(reason) => {
                        let _ = writeln!(self.err, "[agent {name} failed after {took}: {reason}]");
                    }
                    None => {
                        let _ = writeln!(self.err, "[agent {name} finished after {took}]");
                    }
                }
            }
            K::Error { text } => {
                if text.starts_with(crate::bus::WORKER_PANIC_PREFIX) {
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
            K::Done { cmd, outcome } => {
                let _ = writeln!(self.err, "{}", card::settled_text(cmd, outcome));
            }
            K::Context { turns } => {
                self.print_card(&card::context_rows_card(turns));
            }
            K::Turn { id: turn } => {
                if id == self.root_id {
                    self.turns += 1;
                    let _ = writeln!(self.err, "[turn {turn}]");
                }
            }
            K::Evicted { cut, by } => {
                let runs = crate::record::model::runs(&cut.turns);
                let _ = writeln!(self.err, "[turns {runs} left the context ({})]", by.name());
            }
            K::Rewound { anchor } => {
                let _ = writeln!(self.err, "[rewound to turn {anchor}]");
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
    }

    fn print_observation(&mut self, value: &FOValue) {
        let Ok(obs) = Observation::decode(value) else {
            return;
        };
        self.print_observed(&obs.what);
    }

    fn print_observed(&mut self, what: &Observed) {
        if landing(what).is_none() {
            return;
        }
        let card = observation_card(what);
        self.print_card(&card);
    }
}

/// Attend `session` to quiescence in a one-shot headless run from `seed`.
/// Text and JSON both write the root's deliberate reply to stdout once; all
/// progress and operational output goes to stderr.
///
/// # Errors
/// Returns `Err` if no launch prompt was supplied, if the attend worker
/// panics, if the sink's drive fails, or if the run ends without a `reply`
/// (stopped, cancelled, or failed).
pub fn run(
    session: &mut Avatar,
    info: &SessionInfo<'_>,
    p: &Provider,
    seed: Option<String>,
    format: OutputFormat,
) -> Result<(), String> {
    let prompt = seed.ok_or("--headless requires a seed prompt: --prompt or --file")?;
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    // The account id, not a label: a label is relative to a set this banner
    // does not have, and the id is what `--provider` takes to reproduce the run.
    let _ = writeln!(
        stderr,
        "exarch: provider={} model={} base={}",
        p.account().id,
        p.model(),
        info.base
    );
    let projection = match format {
        OutputFormat::Text => Projection::HeadlessText,
        OutputFormat::Json => Projection::HeadlessJson,
    };
    let mut headless = Headless::new(projection, session.agent.id, &mut stdout, &mut stderr);
    // A per-exchange bus over the trunk's *own* inbox, so the attend worker and
    // any in-exchange producer share one queue.  It closes when the worker
    // finishes, muting async children on the display — never in the trace.
    let bus = FleetBus::per_exchange(&session.inbox());
    // A headless trunk is a returning agent that does not park, so `attend`
    // runs this seeded work and returns once idle.
    session.seed(prompt);
    let root_id = session.agent.id;
    let recorder = session.recorder();
    // This is the process's trunk — a fact of the launch, not of any position
    // in the tree — so it is what an OS signal must reach, for as long as it
    // attends.
    let _signals = crate::signals::face(session.agent.reach());
    let outcome = pump(&mut headless, &bus, root_id, &recorder, |emit| {
        session.attend(emit)
    });
    // The attend digest: an outcome driving `is_error`/`error`, and the root's
    // `reply`, deposited on its agent.  A panic arrives as `Ok(None)`, already
    // latched by the sink.
    headless.reply = session.agent.reply();
    let mut r: Result<(), String> = match outcome {
        Ok(Some(agent_outcome)) => match agent_outcome {
            AgentOutcome::Replied => Ok(()),
            AgentOutcome::Stopped(s) => Err(s),
            AgentOutcome::Cancelled => Err("cancelled".to_string()),
            AgentOutcome::Failed(e) => Err(e),
        },
        Ok(None) => Err("worker panicked".to_string()),
        Err(e) => Err(e.to_string()),
    };
    // Summed at the emit seam, so it includes async children muted on this
    // per-exchange bus whose usage never reached the sink.
    headless.usage = bus.usage_total();
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
    let _ = writeln!(headless.err, "Agent log: {}", session.log_dir().display());
    let _ = writeln!(
        headless.err,
        "[done] {} turns · {:.1}s · {}",
        headless.turns,
        elapsed.as_secs_f64(),
        headless.usage
    );
    r
}

/// One exchange of a converse session, over the process's own stdout/stderr.
///
/// [`converse_on`] carries the contract.
///
/// # Errors
/// Returns `Err` if the attend worker panics or the sink's drive fails.
pub fn converse(session: &mut Avatar, message: String) -> Result<(), String> {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    converse_on(session, message, &mut stdout, &mut stderr)
}

/// One exchange of a converse session: assistant tokens stream to `out`,
/// everything else to `err`.
///
/// This remains conversational — unlike headless text, it does not wait for
/// or print a deliberate root reply.
///
/// `session` must have been built with
/// [`RootConfig::interactive`](crate::agent::RootConfig) set: a conversing
/// trunk withholds `reply` and parks between messages rather than returning
/// once, so there is no result to report and a digest saying "no reply" is not
/// an error.  Call it once per user message on the *same* session, so each
/// exchange sees what came before.
///
/// # Errors
/// Returns `Err` if the attend worker panics or the sink's drive fails.
pub fn converse_on(
    session: &mut Avatar,
    message: String,
    out: &mut (dyn Write + Send),
    err: &mut (dyn Write + Send),
) -> Result<(), String> {
    let mut sink = Headless::new(Projection::Conversation, session.agent.id, out, err);
    let outcome = converse_sink(session, message, &mut sink);
    if outcome.is_ok() && !sink.ended_with_newline {
        let _ = writeln!(sink.out);
    }
    outcome
}

/// One exchange of a converse session, projected onto any [`Sink`].
///
/// The structured twin of [`converse_on`], which carries the contract: here the
/// caller supplies the sink and so sees raw `Signal`s rather than rendered
/// text.
///
/// # Errors
/// Returns `Err` if the attend worker panics or the sink's drive fails.
pub fn converse_sink<S: Sink>(
    session: &mut Avatar,
    message: String,
    sink: &mut S,
) -> Result<(), String> {
    // Per-exchange: this call's channel closes when the exchange parks, so
    // draining can never block on a message the caller has not sent yet.
    let bus = FleetBus::per_exchange(&session.inbox());
    session.seed(message);
    let root_id = session.agent.id;
    let recorder = session.recorder();
    let outcome = pump(sink, &bus, root_id, &recorder, |emit| {
        session.attend_backlog(emit)
    });
    exchange_ending(session, outcome)
}

/// A converse exchange's verdict. A severed session is over, not an exchange
/// that went badly: its `Err` is the sentence `EngineLost` settled it with.
fn exchange_ending(
    session: &Avatar,
    outcome: io::Result<Option<AgentOutcome>>,
) -> Result<(), String> {
    match outcome {
        Ok(Some(AgentOutcome::Failed(lost))) if session.severance().is_some() => Err(lost),
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("worker panicked".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

/// One exchange that ends only at fleet quiescence.
///
/// The trunk parked, no busy children, and their results drained and
/// announced — the driver `synod` (and any embedder wanting the same
/// guarantee) runs instead of [`converse_sink`].
///
/// Two differences from [`converse_sink`]: the bus gives a spawned child a
/// live emitter rather than a muted one
/// ([`FleetBus::per_exchange_live`](crate::bus::FleetBus::per_exchange_live)),
/// and it runs the blocking `attend` loop, not `attend_backlog`, since Law B
/// means the exchange itself must wait out whatever the fleet is still doing
/// — which an unattended conversing trunk's park does on its own, holding for
/// live children and quiescing once they settle.  `converse_sink` stays
/// exactly as it is for exarch's own headless mode.
///
/// # Errors
/// Refuses at once, before touching the bus or seeding `message`, if
/// `session` holds `allow_schedule` or `resume_on_reset`: an armed wakeup may
/// park past [`ParkMode::UntilCancelled`](crate::bus::ParkMode), a wait this
/// driver's policy does not cover and Law B forbids waiting out regardless. Otherwise
/// returns `Err` if the attend worker panics or the sink's drive fails.
pub fn converse_settled<S: Sink>(
    session: &mut Avatar,
    message: String,
    sink: &mut S,
) -> Result<(), String> {
    if session.fleet.launch.allow_schedule || session.fleet.launch.resume_on_reset {
        return Err(
            "converse_settled ends an exchange only once the fleet quiesces, and an armed \
             self-schedule or reset-resume wakeup may fire again with nothing to wait it out: \
             refused rather than parked past quiescence"
                .to_string(),
        );
    }
    let bus = FleetBus::per_exchange_live(&session.inbox());
    session.seed(message);
    let root_id = session.agent.id;
    let recorder = session.recorder();
    // The embedder's trunk, for the extent of the exchange it drives: an OS
    // signal reaching this process must reach it.
    let _signals = crate::signals::face(session.agent.reach());
    let outcome = pump(sink, &bus, root_id, &recorder, |emit| session.attend(emit));
    exchange_ending(session, outcome)
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[test] test fs scaffolding for a throwaway run dir"
)]
mod tests;
