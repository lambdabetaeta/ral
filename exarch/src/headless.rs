//! Headless frontend: a [`Sink`] projecting the bus onto a pair of writers.
//!
//! Headless text and JSON both project the root's deliberate `reply` to `out`
//! once the run finishes; progress, tool calls, cards, failures, and the run
//! summary go to `err`. A conversational host drives [`SettledConversation`]
//! with its own [`Sink`], seeing raw signals rather than rendered text.
//! Recording is not this module's concern: every session writes its own
//! record at the seam, through [`crate::record`].

use crate::SessionInfo;
use crate::agent::spawn::announce;
use crate::agent::{Agent, AgentHandle};
use crate::bus::{AgentOutcome, Closed, FleetBus, Pumping, Sink, pump};
pub use crate::bus::{Job, JobId};
use crate::card::{self, Card, Landed, Mark, Row, observation_card};
use crate::provider::{Provider, Usage};
use crate::record::AgentId;
use crate::record::fault::Remedy;
use crate::record::{self, Blocks, Delta, Record, Recorded, Settlement, Transient, Unavailable};
use crate::shell_eval::user_json;
use ral_core::data::FOValue;
use ral_core::data::datum::Datum as _;
use ral_core::shell::runtime::{Observation, Observed};
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

/// The [`Sink`] behind [`run`], accumulating the run's
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
    /// Counted, not tracked: a `Forensic::TurnStarted { turn }` names the request's own
    /// id, and a cancelled request retaken names it twice.
    turns: u32,
    last_stop: Option<String>,
    /// The root's deliberate `reply`, kept as a value until the consuming
    /// projection chooses ral text or [`user_json`].
    reply: Option<ral_core::data::FOValue>,
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
        self.step(id, rec);
    }

    fn transient(&mut self, id: AgentId, t: &Transient) {
        // Terminal recording failures and channel elisions draw from any source;
        // everything else rides root's own register.
        if id != self.root_id && !matches!(t, Transient::Fault { .. }) {
            return;
        }
        match t {
            Transient::StopReason(raw) => {
                self.last_stop = Some(raw.clone());
                let _ = writeln!(self.err, "[stop: {raw}]");
            }
            // A session's terminal recording failure, announced by its owner,
            // or a channel elision marker.
            Transient::Fault { text } => {
                let _ = writeln!(self.err, "{text}");
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
            | Transient::Died
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
        let delta = fold.step(rec);
        if let (Delta::Opened(_) | Delta::Cleared, Some(block)) = (delta, fold.blocks().last()) {
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
                };
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
                if id == self.root_id {
                    self.turns += 1;
                    let _ = writeln!(self.err, "[turn {turn}]");
                }
            }
            K::Evicted { residue } => {
                let _ = writeln!(self.err, "{}", residue.evicted_line());
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

/// Attend `session` to quiescence in a one-shot headless run from `seed`.
/// Text and JSON both write the root's deliberate reply to stdout once; all
/// progress and operational output goes to stderr.
///
/// # Errors
/// Returns `Err` if no launch prompt was supplied, if recording fails, if the
/// attend worker panics, if the sink's drive fails, or if the run ends without
/// a `reply` (stopped, cancelled, or failed).
pub fn run(
    session: &mut Agent,
    info: &SessionInfo<'_>,
    p: &Provider,
    seed: Option<String>,
    format: OutputFormat,
) -> Result<(), String> {
    let prompt = seed.ok_or("--headless requires a seed prompt: --prompt or --file")?;
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
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
    let mut headless = Headless::new(session.id(), &mut stdout, &mut stderr);
    // A per-exchange bus over the trunk's *own* inbox, so the attend worker and
    // any in-exchange producer share one queue.  It closes when the worker
    // finishes, muting async children on the display — never in the trace.
    let bus = FleetBus::per_exchange();
    // A headless trunk is a returning agent that does not park, so `attend`
    // runs this seeded work and returns once idle.
    session.seed(prompt);
    let root_id = session.id();
    let recorder = session.recorder();
    // An OS signal must reach the fleet's roots for as long as this one attends.
    let _signals = crate::signals::face(&session.fleet);
    let attended = pump(&mut headless, &bus, root_id, &recorder, |emit, _| {
        session.attend(emit)
    });
    // The attend digest drives `is_error`/`error`; a panic arrives as `Ok(None)`.
    // The root's deliberate `reply` stays on its agent.
    headless.reply = session.handle().reply();
    let mut r = match session.check_available() {
        Ok(()) => pumped(attended).and_then(|outcome| match outcome {
            AgentOutcome::Replied => Ok(()),
            AgentOutcome::Stopped(s) => Err(s),
            AgentOutcome::Cancelled => Err("cancelled".to_string()),
            AgentOutcome::Failed(e) => Err(e),
        }),
        // The failure overrides the attended outcome, announced after it.
        Err(failure) => {
            let reason = failure.to_string();
            let announcement = announced(&mut headless, &bus, root_id, &recorder, failure, |_| ());
            failed(std::iter::once(reason).chain(announcement.err()))
        }
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
    let _ = writeln!(headless.err, "Agent log: {}", session.log_dir());
    let _ = writeln!(
        headless.err,
        "[done] {} turns · {:.1}s · {}",
        headless.turns,
        elapsed.as_secs_f64(),
        headless.usage
    );
    r
}

/// A pump's worker value, or why it has none.
fn pumped<T>(ran: io::Result<Option<T>>) -> Result<T, String> {
    match ran {
        Ok(Some(value)) => Ok(value),
        Ok(None) => Err("worker panicked".to_string()),
        Err(error) => Err(error.to_string()),
    }
}

/// Every failure, one per line, or success.
fn failed(failures: impl IntoIterator<Item = String>) -> Result<(), String> {
    let failures: Vec<_> = failures.into_iter().collect();
    if failures.is_empty() {
        Ok(())
    } else {
        Err(failures.join("\n"))
    }
}

/// Announce `failure` in a pump of its own, then `then` under it: the
/// pumped termini conclude after attendance, however it ended.
fn announced<S: Sink, R: Send>(
    sink: &mut S,
    bus: &FleetBus,
    root: AgentId,
    recorder: &record::Emitter,
    failure: Unavailable,
    then: impl Send + FnOnce(&Pumping) -> R,
) -> Result<R, String> {
    pumped(pump(sink, bus, root, recorder, move |emit, pumping| {
        announce(&failure, emit);
        then(pumping)
    }))
}

/// A severed engine ends the conversation, not merely the job.
fn attendance(session: &Agent, outcome: AgentOutcome) -> Result<(), String> {
    match outcome {
        AgentOutcome::Failed(reason) if session.severance().is_some() => Err(reason),
        AgentOutcome::Replied
        | AgentOutcome::Stopped(_)
        | AgentOutcome::Cancelled
        | AgentOutcome::Failed(_) => Ok(()),
    }
}

/// The host-level unit has closed, either at fleet quiescence or after Stop.
pub struct JobEnd {
    pub id: JobId,
    pub stopped: bool,
    pub outcome: Result<(), String>,
}

/// A root-bound conversation whose jobs end at fleet quiescence.
pub struct SettledConversation {
    bus: FleetBus,
    root: Agent,
}
impl SettledConversation {
    /// # Errors
    /// Refuses self-scheduling and reset-resume wakeups.
    pub fn new(root: Agent) -> Result<Self, String> {
        if root.fleet.launch.allow_schedule || root.fleet.launch.resume_on_reset {
            return Err("A settled conversation ends each job only once the fleet quiesces, and an armed self-schedule or reset-resume wakeup may fire again with nothing to wait it out: refused rather than parked past quiescence".into());
        }
        Ok(Self {
            bus: FleetBus::session(),
            root,
        })
    }
    pub fn handle(&self) -> AgentHandle {
        self.root.handle()
    }
    pub fn severance(&self) -> Option<ral_core::carrier::Severed> {
        self.root.severance()
    }
    pub fn run_dir(&self) -> &ral_core::own::OwnDir {
        self.root.run_dir()
    }
    pub fn await_job(&self) -> Option<Job> {
        self.root.await_job()
    }
    pub fn end_fleet(self) {
        self.end_fleet_after(|| {});
    }
    fn end_fleet_after(self, waiting: impl FnOnce()) {
        let census = self.root.fleet.census();
        self.root
            .fleet
            .end(ral_core::process::CancelCause::Cancelled);
        drop(self);
        census.wait_empty_after(waiting);
    }
    /// Attend once; pump through ended helpers' certified teardown.
    ///
    /// # Panics
    /// If the job belongs to another conversation.
    pub fn run<S: Sink>(&mut self, job: Job, sink: &mut S) -> JobEnd {
        let id = job.id();
        assert!(
            job.belongs_to(&self.root.handle().0.mailbox),
            "this job belongs to another conversation; run it on the conversation that opened it"
        );
        let root = &mut self.root;
        let recorder = root.recorder();
        let _signals = crate::signals::face(&root.fleet);
        let ended = pump(sink, &self.bus, root.id(), &recorder, |emit, pumping| {
            let outcome = root.attend(emit);
            let settle = || root.fleet.wait_settled(root.tenant(), pumping);
            let first = settle();
            let (settlement, stopped) = match job.close() {
                Closed::Ran => (first, false),
                // Stop may end helpers after the first census; `_held` closes after the second.
                Closed::Stopped(_held) => (first.and_then(|()| settle()), true),
            };
            (outcome, stopped, settlement)
        });
        let (stopped, attended, settlement) = match pumped(ended) {
            // Judged before any settlement sever, which would read as engine loss.
            Ok((outcome, stopped, settlement)) => (stopped, attendance(root, outcome), settlement),
            Err(why) => (false, Err(why), Ok(())),
        };
        // A recording failure overrides the attended outcome, as at every terminus.
        let (verdict, settlement) = match root.check_available() {
            Ok(()) => (attended, settlement),
            // The helpers it ended settle in a second pump, whose census report
            // supersedes the first.
            Err(failure) => {
                let reason = failure.to_string();
                let (fleet, tenant) = (&root.fleet, root.tenant());
                match announced(
                    sink,
                    &self.bus,
                    root.id(),
                    &recorder,
                    failure,
                    move |pumping| fleet.wait_settled(tenant, pumping),
                ) {
                    Ok(settled) => (Err(reason), settled),
                    Err(why) => (failed([reason, why]), settlement),
                }
            }
        };
        if let Err(why) = &settlement {
            root.engine
                .transport()
                .sever(ral_core::carrier::Severed::Faulted(why.clone()));
        }
        JobEnd {
            id,
            stopped,
            outcome: failed(verdict.err().into_iter().chain(settlement.err())),
        }
    }
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;
