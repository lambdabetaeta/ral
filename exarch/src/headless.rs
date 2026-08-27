//! Headless frontend: a [`Sink`] projecting the bus onto a pair of writers.
//!
//! Headless text and JSON both project the root's deliberate `reply` to `out`
//! once the run finishes; progress, tool calls, cards, failures, and the run
//! summary go to `err`. A conversational host takes a different projection
//! through [`converse_on`], which streams assistant tokens, or drives
//! [`converse_sink`] with its own [`Sink`], seeing raw `Signal`s rather than
//! rendered text.
//! Recording is not this module's concern: every session writes its own
//! record at the seam, through [`crate::agent::event`].

use crate::agent::Avatar;
use crate::agent::event::{ContextOp, EditAuthority};
use crate::bus::card::{
    self, Card, Mark, Row, execs_card, greps_card, observation_card, observation_from_wire,
    rail_place, reads_card,
};
use crate::bus::{AgentId, AgentOutcome, BusReceiver, FleetBus, Signal, Sink, pump};
use crate::provider::{Provider, Usage};
use crate::record::{self, Blocks, Fold, Printer, Transient, View};
use crate::shell_eval::user_json;
use crate::tui::SessionInfo;
use ral_core::serial::FOValue;
use ral_core::types::{CommandOrigin, Observation, Observed};
use std::collections::HashMap;
use std::io::{self, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc::{RecvTimeoutError, TryRecvError};
use std::time::{Duration, Instant};

/// What the root agent's output looks like on stdout in headless mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    /// Stdout carries one result object at the end, not assistant narration.
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
    /// Counted, not tracked: the per-segment `Display::Step { n }` index
    /// restarts each time the root resumes after an async-spawned block.
    steps: u32,
    last_stop: Option<String>,
    /// The root's deliberate `reply`, kept as a value until the consuming
    /// projection chooses ral text or [`user_json`].
    reply: Option<ral_core::serial::FOValue>,
    /// `pump` recovers from a worker unwind and returns the worker value as
    /// normal, so without latching the panic here a crashed exchange would
    /// report as a clean, empty success.
    panicked: bool,
    ended_with_newline: bool,
    /// The [`record::Seq`] of [`record::Blocks::rows`] this printer has
    /// already turned into stderr progress lines, per source agent — a
    /// fact's `Seq` is only unique within its own session log, so root's and
    /// a child's commits each need their own cursor. A cursor by identity
    /// rather than position, since a windowed memo makes an index wrong.
    synced_through: HashMap<AgentId, record::Seq>,
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
            steps: 0,
            last_stop: None,
            reply: None,
            panicked: false,
            ended_with_newline: false,
            synced_through: HashMap::new(),
        }
    }

    /// Write the deliberate reply as ral's existing `VALUE` projection. A
    /// unit reply has no textual projection and therefore writes nothing.
    fn write_reply(&mut self) -> bool {
        let Some(reply) = &self.reply else {
            return false;
        };
        let Some(text) =
            crate::shell_eval::ral_value_to_text(&ral_core::Value::from(reply.clone()))
        else {
            return false;
        };
        let _ = self.out.write_all(text.as_bytes());
        let _ = self.out.flush();
        self.ended_with_newline = text.as_bytes().last() == Some(&b'\n');
        true
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
            Mark::Measure(m) => {
                let bound = m.max.map(|mx| format!("/{mx}")).unwrap_or_default();
                let unit = m.unit.as_deref().unwrap_or("");
                out.push(format!("[{}: {}{bound}{unit}]", m.label, m.value));
            }
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
        "num_steps": h.steps,
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
    /// Drain the bus fully, dispatching every `Signal`, then block for the
    /// next arrival exactly as the default [`Sink::drive`] does.  Headless
    /// overrides rather than shares that default because it folds each source
    /// agent's facts into its own [`Blocks`] memo, which the default's
    /// stateless `accept` has nowhere to keep.
    fn drive(&mut self, rx: &BusReceiver, done: &AtomicBool) -> io::Result<()> {
        /// How often a blocked drain wakes to re-check `done` — mirrors
        /// `bus::sink::DRAIN_POLL`, kept as its own constant rather than a
        /// shared one so this override stays self-contained.
        const POLL: Duration = Duration::from_millis(10);
        let mut blocks: HashMap<AgentId, Blocks> = HashMap::new();
        loop {
            loop {
                match rx.try_recv() {
                    Ok(sig) => self.absorb(sig, &mut blocks),
                    Err(TryRecvError::Empty) => break,
                    Err(TryRecvError::Disconnected) => return Ok(()),
                }
            }
            if done.load(Ordering::Acquire) {
                return Ok(());
            }
            match rx.recv_timeout(POLL) {
                Ok(sig) => self.absorb(sig, &mut blocks),
                Err(RecvTimeoutError::Timeout) => {}
                Err(RecvTimeoutError::Disconnected) => return Ok(()),
            }
        }
    }
}

impl Headless<'_> {
    /// One `Signal`, dispatched to whichever half carries it.
    ///
    /// A fact is folded into its source agent's own [`Blocks`] — each agent's
    /// `Seq` numbers its own log, so root's and a child's commits never share
    /// one memo — and drawn fresh from there.
    fn absorb(&mut self, sig: Signal, blocks: &mut HashMap<AgentId, Blocks>) {
        match sig {
            Signal::Fact(id, rec) => {
                let entry = blocks.entry(id).or_default();
                View::step(entry, &rec)
                    .expect("the view fold never refuses a live Display/Forensic record");
                self.sync_agent(id, entry);
            }
            Signal::Transient(id, t) => {
                // Every printer draws a fault whatever its source, since an
                // unwritable log is a fact about the plumbing, not one agent;
                // everything else here rides root's own register.
                if id == self.root_id || matches!(t, Transient::Fault { .. }) {
                    Printer::transient(self, &t);
                }
            }
        }
    }

    /// Print every row `blocks` has committed since this source agent's own
    /// cursor, then advance it.
    fn sync_agent(&mut self, id: AgentId, blocks: &Blocks) {
        let since = self
            .synced_through
            .get(&id)
            .copied()
            .unwrap_or_else(|| record::Seq::new(0));
        for row in blocks.rows().iter().filter(|r| r.id().seq() > since) {
            self.print_row(id, row.kind());
        }
        if let Some(last) = blocks.rows().last() {
            self.synced_through.insert(id, last.id().seq());
        }
    }
}

impl Printer for Headless<'_> {
    fn transient(&mut self, t: &Transient) {
        match t {
            Transient::Token(text) if self.projection == Projection::Conversation => {
                let _ = self.out.write_all(text.as_bytes());
                let _ = self.out.flush();
                if let Some(last) = text.as_bytes().last() {
                    self.ended_with_newline = *last == b'\n';
                }
            }
            Transient::Boundary => self.steps += 1,
            Transient::StopReason(raw) => {
                self.last_stop = Some(raw.clone());
                let _ = writeln!(self.err, "[stop: {raw}]");
            }
            // The seam's own diagnostic: drawn by every printer whatever
            // else it is doing, since it is a fact about the log itself.
            Transient::Fault { text } => {
                let _ = writeln!(self.err, "{text}");
            }
            Transient::Resources { card, .. } => {
                for line in card_stderr(card) {
                    let _ = writeln!(self.err, "{line}");
                }
            }
            // A pin overwrites a TUI register; headless has none.
            Transient::Token(_)
            | Transient::Thinking(_)
            | Transient::State(_)
            | Transient::Born { .. }
            | Transient::Died
            | Transient::Cleared
            | Transient::Pin { .. }
            | Transient::Unpin { .. } => {}
        }
    }

    fn sync(&mut self, blocks: &Blocks) {
        let root_id = self.root_id;
        self.sync_agent(root_id, blocks);
    }
}

impl Headless<'_> {
    /// One fold commit, projected onto stderr as a rendering chosen fresh
    /// each time from whichever record vocabulary reached this printer.
    #[allow(clippy::match_same_arms)]
    fn print_row(&mut self, id: AgentId, kind: &record::BlockKind) {
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
                #[allow(
                    clippy::cast_precision_loss,
                    reason = "elapsed-ms display precision; far below f64's mantissa"
                )]
                let secs = *elapsed_ms as f64 / 1000.0;
                match error {
                    Some(reason) => {
                        let _ =
                            writeln!(self.err, "[agent: {name} failed in {secs:.1}s — {reason}]");
                    }
                    None => {
                        let _ = writeln!(self.err, "[agent: {name} done in {secs:.1}s]");
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
            K::Nudge { used, max, cause } => {
                if id == self.root_id {
                    let _ = writeln!(self.err, "[nudge {used}/{max}: {cause}]");
                }
            }
            K::ProviderError { error } => {
                let _ = writeln!(self.err, "provider error: {error:?}");
            }
            K::Stalled { error } => {
                let _ = writeln!(self.err, "stream stalled, turn resumes: {error:?}");
            }
            K::Observation { value } => self.print_observation(value.clone()),
            K::ObservationGroup { values } => self.print_observation_group(values),
            K::Card { marks } => {
                if let Ok(card) = serde_json::from_value::<Card>(marks.clone()) {
                    self.print_card(&card);
                }
            }
            K::Done { outcome } => {
                let _ = writeln!(
                    self.err,
                    "{}",
                    card::settled_text(&card::to_card_done(outcome))
                );
            }
            K::Notice { notice } => {
                self.print_card(&card::notice_card(&card::to_card_notice(notice)));
            }
            K::Context { rows } => {
                self.print_card(&card::context_rows_card(rows));
            }
            K::Step { n } => {
                if id == self.root_id {
                    self.steps += 1;
                    let _ = writeln!(self.err, "[step {n}]");
                }
            }
            K::ContextEdited { op, by } => {
                let authority = match by {
                    EditAuthority::Model => "model",
                    EditAuthority::User => "user",
                    EditAuthority::Harness => "harness",
                };
                match op {
                    ContextOp::Fold {
                        through_exchange, ..
                    } => {
                        let _ = writeln!(
                            self.err,
                            "[context folded through exchange {through_exchange} ({authority})]"
                        );
                    }
                    ContextOp::Drop { exchanges } => {
                        let list = exchanges
                            .iter()
                            .map(u64::to_string)
                            .collect::<Vec<_>>()
                            .join(", ");
                        let _ = writeln!(
                            self.err,
                            "[context dropped exchange(s) {list} ({authority})]"
                        );
                    }
                }
            }
            // Interactive-only, or pure presentation: nothing is lost by
            // leaving them undrawn here.
            K::Thinking { .. }
            | K::Prompt { .. }
            | K::Answer { .. }
            | K::Cancelled
            | K::HarnessResult { .. }
            | K::ModelChanged { .. } => {}
        }
    }

    fn print_card(&mut self, card: &Card) {
        for line in card_stderr(card) {
            let _ = writeln!(self.err, "{line}");
        }
    }

    fn print_observation(&mut self, value: FOValue) {
        let Some(obs) = observation_from_wire(value) else {
            return;
        };
        self.print_observed(&obs.what);
    }

    fn print_observed(&mut self, what: &Observed) {
        if rail_place(what).is_none() {
            return;
        }
        let card = observation_card(what);
        self.print_card(&card);
    }

    /// Decode and re-bucket exactly as `record/commit.rs`'s buffer grouped
    /// these at record time: reads, execs, and greps comma-joined, a write or
    /// a standalone denial printed on its own.
    fn print_observation_group(&mut self, values: &[FOValue]) {
        let mut reads: Vec<String> = Vec::new();
        let mut execs: Vec<Observed> = Vec::new();
        let mut greps: Vec<Observed> = Vec::new();
        for value in values {
            let Some(Observation { what, .. }) = observation_from_wire(value.clone()) else {
                continue;
            };
            match what {
                Observed::Read { path } => reads.push(path),
                Observed::Command {
                    origin: CommandOrigin::External | CommandOrigin::Detached,
                    ..
                } => execs.push(what),
                Observed::Grep { .. } => greps.push(what),
                other => self.print_observed(&other),
            }
        }
        if let Some(card) = reads_card(&reads) {
            self.print_card(&card);
        }
        if let Some(card) = execs_card(&execs) {
            self.print_card(&card);
        }
        if let Some(card) = greps_card(&greps) {
            self.print_card(&card);
        }
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
    let prompt = seed
        .ok_or("--headless requires a seed prompt: --prompt, --file, or trailing words after --")?;
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
    // No slash commands here, so `Control` is a no-op; declared out here so its
    // `&mut` borrow outlives the closure `pump` runs on its scoped thread.
    let mut control = crate::agent::NoControl;
    let root_id = session.agent.id;
    let recorder = session.recorder();
    // This is the process's trunk — a fact of the launch, not of any position
    // in the tree — so its token is what an OS signal must reach, for as long
    // as it attends.
    let _slot = crate::agent::cancel::publish(session.cancel_token());
    let outcome = pump(&mut headless, &bus, root_id, &recorder, |emit| {
        session.attend(&mut control, emit)
    });
    // The attend digest: an outcome driving `is_error`/`error`, and the root's
    // `reply`.  A panic arrives as `Ok(None)`, already latched by the sink.
    let r: Result<(), String> = match outcome {
        Ok(Some((agent_outcome, payload))) => {
            headless.reply = payload;
            match agent_outcome {
                AgentOutcome::Replied => Ok(()),
                AgentOutcome::Stopped(s) => Err(s),
                AgentOutcome::Cancelled => Err("cancelled".to_string()),
                AgentOutcome::Failed(e) => Err(e),
            }
        }
        Ok(None) => Err("worker panicked".to_string()),
        Err(e) => Err(e.to_string()),
    };
    // Summed at the emit seam, so it includes async children muted on this
    // per-exchange bus whose usage never reached the sink.
    headless.usage = bus.usage_total();
    let elapsed = headless.started.elapsed();
    match format {
        OutputFormat::Json => {
            let result = result_json(&headless, &r, elapsed);
            let _ = writeln!(headless.out, "{result}");
        }
        OutputFormat::Text => {
            let wrote_reply = r.is_ok() && headless.write_reply();
            if wrote_reply && !headless.ended_with_newline {
                // So the next shell prompt lands at column 1.
                let _ = writeln!(headless.out);
            }
        }
    }
    let _ = writeln!(headless.err, "Agent log: {}", session.log_dir().display());
    let _ = writeln!(
        headless.err,
        "[done] {} steps · {:.1}s · {}",
        headless.steps,
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
    match outcome {
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
/// Three differences from [`converse_sink`]: the bus gives a spawned child a
/// live emitter rather than a muted one
/// ([`FleetBus::per_exchange_live`](crate::bus::FleetBus::per_exchange_live));
/// the loop runs under [`crate::agent::quiesce_when_childless`] rather than
/// the identity policy [`Avatar::attend`](crate::agent::Avatar::attend) uses, so
/// a conversing trunk holds on a live fleet instead of parking blind to it;
/// and it runs the blocking `attend` loop, not `attend_backlog`, since Law B
/// means the exchange itself must wait out whatever the fleet is still doing.
/// `converse_sink` stays exactly as it is for exarch's own headless mode.
///
/// # Errors
/// Refuses at once, before touching the bus or seeding `message`, if
/// `session` holds `allow_schedule`: an armed self-schedule may park past
/// [`ParkMode::UntilCancelled`](crate::bus::ParkMode), a wait this driver's
/// policy does not cover and Law B forbids waiting out regardless. Otherwise
/// returns `Err` if the attend worker panics or the sink's drive fails.
pub fn converse_settled<S: Sink>(
    session: &mut Avatar,
    message: String,
    sink: &mut S,
) -> Result<(), String> {
    if session.agent.allow_schedule {
        return Err(
            "converse_settled ends an exchange only once the fleet quiesces, and an armed \
             self-schedule may fire again with nothing to wait it out — refused rather than \
             parked past quiescence"
                .to_string(),
        );
    }
    let bus = FleetBus::per_exchange_live(&session.inbox());
    session.seed(message);
    let root_id = session.agent.id;
    let recorder = session.recorder();
    // The embedder's trunk, for the extent of the exchange it drives: an OS
    // signal reaching this process must find that token published.
    let _slot = crate::agent::cancel::publish(session.cancel_token());
    let outcome = pump(sink, &bus, root_id, &recorder, |emit| {
        session.attend_with(
            &mut crate::agent::NoControl,
            emit,
            crate::agent::quiesce_when_childless,
        )
    });
    match outcome {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("worker panicked".to_string()),
        Err(e) => Err(e.to_string()),
    }
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs scaffolding for a throwaway run dir"
)]
mod tests {
    use super::*;
    use crate::agent::{RecordedAccount, RootConfig, RootSeat, SPAWN_FUEL};
    use crate::bus::{AgentResult, AgentState, Post};
    use crate::provider::scripted::{Reply, Script};
    use crate::record::{Display, Record};
    use crate::shell_eval::tools::agent::{AsyncSpawn, spawn_async};
    use std::sync::Arc;

    /// A fresh conversing trunk over a scripted provider, in a throwaway run
    /// dir beside its scratch — both go when the trunk does.
    fn converse_trunk(tag: &str, script: Script) -> Avatar {
        let scratch = Arc::new(
            crate::bootstrap::Scratch::for_test(crate::bootstrap::EXARCH, tag)
                .expect("scratch dir"),
        );
        let dir = scratch.test_sibling("run").expect("temp run dir");
        Avatar::root(
            RootConfig {
                system: "system".into(),
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir,
                resume: None,
                no_logs: false,
                run_lock: None,
                model: "test-model".into(),
                account: RecordedAccount::for_test("test"),
                allow_schedule: false,
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: 0,
                egress: crate::egress::Egress::for_test(),
                dial: None,
            },
            RootSeat::Identity {
                scratch,
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
            },
            Arc::new(Provider::scripted("test-model", script)),
        )
        .expect("root trunk")
    }

    /// A conversation, not a sequence of one-shot runs: the second exchange's
    /// rendered context must carry the first's user text and reply.
    #[test]
    fn converse_carries_context_from_one_exchange_into_the_next() {
        let mut session = converse_trunk(
            "context",
            Script::new()
                .then(Reply::text("hello back"))
                .then(Reply::text("and again")),
        );
        converse(&mut session, "first message".into())
            .expect("a conversing trunk never fails for want of a reply");
        converse(&mut session, "second message".into())
            .expect("the second exchange runs on the same, unbroken session");

        let rendered = format!("{:?}", session.rendered_messages());
        assert!(rendered.contains("first message"), "{rendered}");
        assert!(rendered.contains("hello back"), "{rendered}");
        assert!(rendered.contains("second message"), "{rendered}");
    }

    /// The conversing trunk parks rather than returning: no payload rises, and
    /// the digest never wears `Replied`, the tag only a deliberate `reply` earns.
    #[test]
    fn the_interactive_trunk_parks_rather_than_replying() {
        let mut session = converse_trunk("parks", Script::new().then(Reply::text("hi there")));
        session.seed("hello".into());
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.agent.id);
        let (outcome, payload) = session.attend_backlog(&emit);
        assert!(
            payload.is_none(),
            "a conversing trunk carries no reply payload"
        );
        assert!(
            !matches!(outcome, AgentOutcome::Replied),
            "a conversing trunk never completes by returning a value: {outcome:?}"
        );
    }

    /// `pump` absorbs the unwind and returns normally, so `run` builds its
    /// result from `Ok(())`; only the recorded `Forensic::Error` whose text
    /// carries [`crate::bus::WORKER_PANIC_PREFIX`] betrays the panic, latched
    /// in `print_row`'s `K::Error` arm.
    #[test]
    fn recovered_worker_panic_reports_error_not_success() {
        use crate::record::{Forensic, Record, Recorded, Seq, Stamp};
        let root: AgentId = 1;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
        let mut blocks = HashMap::new();
        h.absorb(
            Signal::Fact(
                root,
                Recorded::new(
                    Stamp::new(Seq::new(1), 0..0),
                    Record::Forensic(Forensic::Error {
                        text: format!("{}boom", crate::bus::WORKER_PANIC_PREFIX),
                    }),
                ),
            ),
            &mut blocks,
        );
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert_eq!(v["is_error"], serde_json::json!(true), "{out}");
        assert_eq!(v["stop_reason"], serde_json::json!("panicked"), "{out}");
    }

    /// A `Display::Step` fact through the real live path — `absorb`, which
    /// `drive` calls in production — folded into blocks and drawn from there.
    /// `n`'s index restarts each time the root resumes after an async-spawned
    /// block, so storing it would report only the last segment; a child's own
    /// steps never count toward root's.
    #[test]
    fn num_steps_counts_root_steps_across_segments() {
        use crate::record::{Display, Record, Recorded, Seq, Stamp};
        let root: AgentId = 1;
        let sub: AgentId = 2;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
        let mut blocks = HashMap::new();
        let mut seq = 0u64;
        let mut step_started = |id: AgentId, n: u32| {
            seq += 1;
            Signal::Fact(
                id,
                Recorded::new(
                    Stamp::new(Seq::new(seq), 0..0),
                    Record::Display(Display::Step { n }),
                ),
            )
        };
        // Two segments whose indices reset (1..3, then 1..2).
        for n in [1, 2, 3, 1, 2] {
            h.absorb(step_started(root, n), &mut blocks);
        }
        h.absorb(step_started(sub, 1), &mut blocks);
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert_eq!(v["num_steps"], serde_json::json!(5), "{out}");
    }

    /// A `Display::Card` fact reaches stderr only through the view fold
    /// `absorb` drives.
    #[test]
    fn a_card_fact_reaches_stderr_through_the_view_fold() {
        use crate::record::{Display, Record, Recorded, Seq, Stamp};
        let root: AgentId = 1;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(Projection::HeadlessText, root, &mut sink_out, &mut sink_err);
        let mut blocks = HashMap::new();
        let marks = serde_json::to_value(Card(vec![Mark::Raw {
            bytes: b"a rendered surface".to_vec(),
        }]))
        .expect("a Card always serialises");
        h.absorb(
            Signal::Fact(
                root,
                Recorded::new(
                    Stamp::new(Seq::new(1), 0..0),
                    Record::Display(Display::Card { marks }),
                ),
            ),
            &mut blocks,
        );
        let err = String::from_utf8_lossy(&sink_err);
        assert!(
            err.contains("a rendered surface"),
            "the fact's card must reach stderr: {err:?}"
        );
    }

    /// A seam fault reaches stderr from any source agent, root or a child —
    /// it names a plumbing failure, not a fact about one agent's session.
    #[test]
    fn a_transient_fault_reaches_stderr_from_any_agent() {
        let root: AgentId = 1;
        let sub: AgentId = 2;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(Projection::HeadlessText, root, &mut sink_out, &mut sink_err);
        let mut blocks = HashMap::new();
        h.absorb(
            Signal::Transient(
                sub,
                Transient::Fault {
                    text: "disk is full".into(),
                },
            ),
            &mut blocks,
        );
        let err = String::from_utf8_lossy(&sink_err);
        assert!(err.contains("disk is full"), "{err:?}");
    }

    /// An object stays an object: stringifying it would double-encode the
    /// structure, leaving the harness to parse JSON out of a JSON string.
    #[test]
    fn structured_result_is_faithful_not_stringified() {
        let root: AgentId = 1;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
        h.reply = Some(ral_core::serial::FOValue::Map {
            entries: vec![(
                "files".into(),
                ral_core::serial::FOValue::List {
                    items: vec![
                        ral_core::serial::FOValue::String {
                            value: "a.rs".into(),
                        },
                        ral_core::serial::FOValue::String {
                            value: "b.rs".into(),
                        },
                    ],
                },
            )],
        });
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert!(
            v["result"].is_object(),
            "a structured reply stays structured, not stringified: {out}"
        );
        assert_eq!(v["result"]["files"][0], serde_json::json!("a.rs"), "{out}");
        assert_eq!(v["is_error"], serde_json::json!(false), "{out}");
    }

    /// A root that finished without calling `reply` fails honestly: null
    /// `result`, `is_error`, and `no_reply` rather than the flattering
    /// "completed".
    #[test]
    fn no_reply_root_is_error_with_null_result() {
        let root: AgentId = 1;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
        let out = result_json(
            &h,
            &Err("ended without calling `reply`".to_string()),
            std::time::Duration::ZERO,
        );
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert_eq!(v["is_error"], serde_json::json!(true), "{out}");
        assert_eq!(v["result"], serde_json::Value::Null, "{out}");
        assert_eq!(v["stop_reason"], serde_json::json!("no_reply"), "{out}");
    }

    /// Through the real `FOValue -> user_json` projection, not just
    /// `result_json`'s JSON-in JSON-out shape: no quoting inside quoting.
    #[test]
    fn user_json_projected_result_keeps_a_string_reply_raw() {
        let root: AgentId = 1;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
        let payload = ral_core::serial::FOValue::String {
            value: "plain text reply".into(),
        };
        h.reply = Some(payload);
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert_eq!(v["result"], serde_json::json!("plain text reply"), "{out}");
    }

    /// The structured half of the same projection: ordinary JSON, not
    /// `FOValue`'s internally-tagged transport encoding.
    #[test]
    fn user_json_projected_result_keeps_structure_ordinary_json() {
        let root: AgentId = 1;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(Projection::HeadlessJson, root, &mut sink_out, &mut sink_err);
        let payload = ral_core::serial::FOValue::Map {
            entries: vec![(
                "files".to_string(),
                ral_core::serial::FOValue::List {
                    items: vec![
                        ral_core::serial::FOValue::String {
                            value: "a.rs".into(),
                        },
                        ral_core::serial::FOValue::String {
                            value: "b.rs".into(),
                        },
                    ],
                },
            )],
        };
        h.reply = Some(payload);
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert!(v["result"].is_object(), "{out}");
        assert_eq!(v["result"]["files"][0], serde_json::json!("a.rs"), "{out}");
    }

    /// A seed of whitespace is no seed: [`crate::cli::load_seed`] collapses it,
    /// and `run` refuses before the provider is touched, naming all three ways
    /// to supply one — the only thing a scripted caller has to go on.
    #[test]
    fn a_blank_seed_is_no_seed_and_the_refusal_names_every_flag() {
        use clap::Parser;
        let cli = crate::cli::Cli::try_parse_from(["exarch", "--headless", "--prompt", "   \n\t"])
            .expect("whitespace is a value, not a parse error");
        let seed = crate::cli::load_seed(cli.prompt, cli.file, cli.trailing_prompt)
            .expect("no --file to read");
        assert_eq!(seed, None, "a blank seed must never reach the model");

        let err = run(
            &mut converse_trunk("blank-seed", Script::new()),
            &SessionInfo {
                system_size: 0,
                system_files: &[],
                base: "dangerous",
                extend_base: None,
                restrict_files: &[],
                scratch: std::path::Path::new("/tmp/scratch"),
                cwd: "/tmp",
            },
            &Provider::scripted("test-model", Script::new()),
            seed,
            OutputFormat::Text,
        )
        .expect_err("a headless run with no seed has nothing to do");
        assert_eq!(
            err,
            "--headless requires a seed prompt: --prompt, --file, or trailing words after --"
        );

        // The filter trims only to judge: real content keeps its own spacing.
        assert_eq!(
            crate::cli::load_seed(Some(" x ".into()), None, Vec::new())
                .expect("no --file to read")
                .as_deref(),
            Some(" x ")
        );
    }

    /// A host's own writers get the same split `converse` gives the process's
    /// stdout/stderr — reply text on `out`, breadcrumbs on `err`.
    #[test]
    fn converse_on_projects_into_the_given_writers() {
        let mut session = converse_trunk("writers", Script::new().then(Reply::text("hi there")));
        let mut out = Vec::new();
        let mut err = Vec::new();
        converse_on(&mut session, "hello".into(), &mut out, &mut err)
            .expect("a conversing trunk never fails for want of a reply");
        assert!(
            String::from_utf8_lossy(&out).contains("hi there"),
            "reply text must land on `out`: {:?}",
            String::from_utf8_lossy(&out)
        );
        assert!(
            String::from_utf8_lossy(&err).contains("[step"),
            "step breadcrumbs must land on `err`: {:?}",
            String::from_utf8_lossy(&err)
        );
    }

    // ── `converse_settled`: the quiescent exchange driver ──────────────────

    /// A conversing trunk with real spawn fuel, so a test may register a
    /// genuine live child under it — [`converse_trunk`]'s `fuel: 0` exists
    /// precisely to refuse that.
    fn settled_trunk(tag: &str, script: Script, allow_schedule: bool) -> Avatar {
        let scratch = Arc::new(
            crate::bootstrap::Scratch::for_test(crate::bootstrap::EXARCH, tag)
                .expect("scratch dir"),
        );
        let dir = scratch.test_sibling("run").expect("temp run dir");
        Avatar::root(
            RootConfig {
                system: "system".into(),
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir,
                resume: None,
                no_logs: false,
                run_lock: None,
                model: "test-model".into(),
                account: RecordedAccount::for_test("test"),
                allow_schedule,
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: SPAWN_FUEL,
                egress: crate::egress::Egress::for_test(),
                dial: None,
            },
            RootSeat::Identity {
                scratch,
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
            },
            Arc::new(Provider::scripted("test-model", script)),
        )
        .expect("root trunk")
    }

    /// A live child of `parent` with no attend loop of its own. The caller
    /// holds it — that is what keeps it live — and drops it on its own clock,
    /// so a test can hold `parent` on it for exactly as long as it chooses,
    /// deterministically, rather than racing a real child's own thread against
    /// a sleep.
    fn live_child(parent: &Avatar, name: &str) -> Avatar {
        parent
            .fork_named(parent.caps().clone(), name)
            .expect("fork child")
    }

    /// Every signal a caller's own `Sink` can receive, folded into one place —
    /// the seam's own `Record`/`Transient`.
    #[derive(Default)]
    struct Collecting {
        facts: Vec<Record>,
        transients: Vec<Transient>,
    }
    impl Sink for Collecting {
        fn fact(&mut self, _id: AgentId, fact: &Record) {
            self.facts.push(fact.clone());
        }
        fn transient(&mut self, _id: AgentId, t: &Transient) {
            self.transients.push(t.clone());
        }
    }

    /// Signals `release` the first time it sees [`AgentState::WaitingOnAgents`]
    /// pass through, so a test's own background thread can settle its child
    /// exactly once the exchange has genuinely parked on it — never before,
    /// and with no sleep to race.
    struct SignalOnWaiting<S> {
        inner: S,
        release: std::sync::mpsc::SyncSender<()>,
    }

    impl<S: Sink> Sink for SignalOnWaiting<S> {
        fn fact(&mut self, id: AgentId, fact: &Record) {
            self.inner.fact(id, fact);
        }
        fn transient(&mut self, id: AgentId, t: &Transient) {
            if matches!(t, Transient::State(AgentState::WaitingOnAgents)) {
                let _ = self.release.try_send(());
            }
            self.inner.transient(id, t);
        }
    }

    /// Law B, exercised deterministically: the exchange holds open while a
    /// live child is registered but not yet settled, and only completes the
    /// settlement — pushing its result and retiring itself —
    /// once the sink has observed the exchange genuinely park on it
    /// ([`AgentState::WaitingOnAgents`]). A driver that quiesced without
    /// waiting would never signal the release, and the call would hang
    /// rather than pass, so this also proves the hold is real.
    #[test]
    fn converse_settled_holds_for_a_live_child_then_quiesces() {
        let mut session = settled_trunk(
            "slow-child",
            Script::new()
                .then(Reply::text("on it"))
                .then(Reply::text("thanks for the update")),
            false,
        );
        let child = live_child(&session, "helper");
        let generation = child.agent.consumer();
        let parent_mailbox = session.mailbox();
        let (release_tx, release_rx) = std::sync::mpsc::sync_channel::<()>(1);
        let settler = std::thread::spawn(move || {
            release_rx
                .recv_timeout(std::time::Duration::from_secs(5))
                .expect("the exchange must park on the child within the timeout");
            let rejected = parent_mailbox.push(Post::AgentResult(AgentResult {
                name: "helper".into(),
                outcome: AgentOutcome::Stopped("done".into()),
                elapsed: std::time::Duration::from_millis(1),
                generation,
            }));
            assert!(
                rejected.is_ok(),
                "the parent's inbox must accept the result"
            );
            // Deliver, then retire: dropping the avatar is the retirement.
            drop(child);
        });

        let mut sink = SignalOnWaiting {
            inner: Collecting::default(),
            release: release_tx,
        };
        converse_settled(&mut session, "please help".into(), &mut sink)
            .expect("the exchange itself must not fail for want of a reply");
        settler.join().expect("the settling thread must not panic");

        assert!(
            sink.inner
                .transients
                .iter()
                .any(|t| matches!(t, Transient::State(AgentState::WaitingOnAgents))),
            "parking on a live child must emit WaitingOnAgents"
        );
        assert!(
            sink.inner.facts.iter().any(|f| matches!(
                f,
                Record::Display(Display::SubagentDone { name, error, .. })
                    if name == "helper" && error.as_deref() == Some("done")
            )),
            "the settled child's end must reach the exchange's own sink"
        );
    }

    /// A returning child that finishes without ever calling `reply` fails
    /// honestly — its `SubagentDone` names the failure — and the exchange
    /// still ends rather than hanging on a child that will never settle
    /// differently. Driven through the real fork/`spawn_async`/`attend` spine
    /// rather than a hand-fed outcome, so the nudge-then-fail behavior itself
    /// is genuine, not asserted; the child's own script has no tool calls, so
    /// nothing races the parent's independent one.
    #[test]
    fn converse_settled_ends_even_when_a_child_never_replies() {
        let mut session = settled_trunk(
            "child-no-reply",
            Script::new()
                .then(Reply::text("on it"))
                .then(Reply::text("thanks for the update")),
            false,
        );
        let mut no_reply = Script::new();
        for _ in 0..8 {
            no_reply = no_reply.then(Reply::text("prose, but never a reply"));
        }
        let child = session
            .fork_named(session.caps().clone(), "flaky")
            .expect("fork child");
        crate::agent::testkit::set_provider(
            &child,
            crate::agent::testkit::scripted("test-model", no_reply),
        );
        child.seed("go".into());
        let (spawn_emit, _rx) = crate::bus::dummy_emitter();
        spawn_async(
            child,
            AsyncSpawn {
                name: "flaky".into(),
                prompt: None,
            },
            &spawn_emit,
        )
        .expect("spawn must succeed");

        let mut sink = Collecting::default();
        converse_settled(&mut session, "please help".into(), &mut sink)
            .expect("the exchange itself must not fail merely because a child did");

        assert!(
            sink.facts.iter().any(|f| matches!(
                f,
                Record::Display(Display::SubagentDone { name, error, .. })
                    if name == "flaky" && error.is_some()
            )),
            "the child's un-replied finish must surface as a failed SubagentDone"
        );
    }

    /// `allow_schedule` is refused at construction, the same class of refusal
    /// as a missing dialler: an armed self-schedule may fire again with
    /// nothing left to wait it out once the fleet quiesces.
    #[test]
    fn converse_settled_refuses_an_allow_schedule_trunk() {
        let mut session = settled_trunk("allow-schedule", Script::new(), true);
        let mut sink = Collecting::default();
        let err = converse_settled(&mut session, "hello".into(), &mut sink)
            .expect_err("an allow_schedule trunk must be refused, not run");
        assert!(
            err.contains("allow_schedule") || err.to_lowercase().contains("schedule"),
            "the refusal must name what it refuses: {err}"
        );
        assert!(
            sink.facts.is_empty() && sink.transients.is_empty(),
            "a refused construction must touch no bus"
        );
    }
}
