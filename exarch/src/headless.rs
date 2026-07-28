//! Headless frontend: a [`Sink`] projecting the bus onto a pair of writers.
//!
//! Only the root's tokens reach `out`, and only in text mode — under
//! `--output-format json` they are dropped for one result object built from
//! the root's deliberate `reply`; everything else goes to `err`.  A non-CLI
//! host takes that same projection through [`converse_on`], or drives
//! [`converse_sink`] with its own [`Sink`] for `Kind` events unflattened.
//! Recording is not this module's concern: every session writes its own trace
//! at the emit seam, through [`crate::agent::transcript`] and
//! [`crate::agent::event`].

use crate::agent::Agent;
use crate::bus::card::{Card, Mark, Row};
use crate::bus::{AgentId, AgentOutcome, Event, FleetBus, Kind, Sink, pump};
use crate::fleet::Fleet;
use crate::provider::{Engine, Provider, Usage};
use crate::shell_eval::user_json;
use crate::tui::SessionInfo;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Instant;

/// What the root agent's output looks like on stdout in headless mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    Text,
    /// Stdout carries one result object at the end, not the live token stream.
    Json,
}

/// The [`Sink`] behind [`run`] and [`converse_on`], accumulating the run's
/// totals for the closing `[done]` line and the json `result`.
pub struct Headless<'a> {
    out: &'a mut (dyn Write + Send),
    err: &'a mut (dyn Write + Send),
    /// Read once from the bus's shared usage meter at the end of the run, so
    /// it counts muted sub-agents too.  A `Kind::Usage` reaching the sink is
    /// already in that meter and must not be added again.
    usage: Usage,
    root_id: AgentId,
    started: Instant,
    json_mode: bool,
    /// Counted, not tracked: the per-segment `Kind::Step(n)` index restarts
    /// each time the root resumes after an async-spawned block.
    steps: u32,
    last_stop: Option<String>,
    /// The root's `reply`, projected through [`user_json`] — not `FOValue`'s
    /// own transport `serde`, which is internally tagged and carries floats by
    /// bits.  `None` when the root finished without replying.
    result: Option<serde_json::Value>,
    /// `pump` recovers from a worker unwind and returns the worker value as
    /// normal, so without latching the panic here a crashed exchange would
    /// report as a clean, empty success.
    panicked: bool,
    ended_with_newline: bool,
}

impl<'a> Headless<'a> {
    fn new(
        json_mode: bool,
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
            json_mode,
            steps: 0,
            last_stop: None,
            result: None,
            panicked: false,
            ended_with_newline: false,
        }
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
            Mark::Listing { bytes, more } => {
                let text = String::from_utf8_lossy(bytes);
                for (i, l) in text.lines().enumerate() {
                    out.push(format!("  {:>3} {l}", i + 1));
                }
                if *more {
                    out.push("    ⋮".to_string());
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
        "result": h.result,
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
    #[allow(clippy::match_same_arms)]
    fn handle(&mut self, e: Event) {
        let id = e.id;
        match e.kind {
            // json mode drops the stream because `result` comes from the attend
            // digest's `reply`, not from whatever prose happened to arrive; a
            // sub-agent's tokens are dropped because they would interleave.
            Kind::Token(text) if id == self.root_id && !self.json_mode => {
                let _ = self.out.write_all(text.as_bytes());
                let _ = self.out.flush();
                if let Some(last) = text.as_bytes().last() {
                    self.ended_with_newline = *last == b'\n';
                }
            }
            Kind::Token(_) => {}
            Kind::Usage(_) => {}
            Kind::Step { n, .. } => {
                if id == self.root_id {
                    self.steps += 1;
                    let _ = writeln!(self.err, "[step {n}]");
                }
            }
            Kind::ToolCall {
                tool, cmd, summary, ..
            } if id == self.root_id => {
                let _ = writeln!(self.err, "[tool: {tool}]");
                if let Some(s) = &summary {
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
            Kind::HarnessCall {
                verb,
                subject,
                payload,
                ..
            } if id == self.root_id => {
                let _ = writeln!(self.err, "[tool: {verb}]");
                if let Some(s) = &subject {
                    let _ = writeln!(self.err, "  {s}");
                }
                for line in payload.lines() {
                    let _ = writeln!(self.err, "  {line}");
                }
            }
            Kind::ToolCall { .. } | Kind::HarnessCall { .. } => {}
            Kind::ToolResult(_) | Kind::HarnessResult(_) => {}
            Kind::StopReason(raw) => {
                if id == self.root_id {
                    self.last_stop = Some(raw.clone());
                }
                let _ = writeln!(self.err, "[stop: {raw}]");
            }
            Kind::Error(msg) => {
                if msg.starts_with(crate::bus::WORKER_PANIC_PREFIX) {
                    self.panicked = true;
                }
                let _ = writeln!(self.err, "error: {msg}");
            }
            Kind::SystemNote(text) => {
                let _ = writeln!(self.err, "{text}");
            }
            Kind::Nudge {
                used, max, cause, ..
            } if id == self.root_id => {
                let _ = writeln!(self.err, "[nudge {used}/{max}: {cause}]");
            }
            Kind::Nudge { .. } => {}
            Kind::ProviderError(error) => {
                let _ = writeln!(self.err, "provider error: {error:?}");
            }
            Kind::Stalled(error) => {
                let _ = writeln!(self.err, "stream stalled, turn resumes: {error:?}");
            }
            // Every kind carrying a card projects the same way: only the card,
            // since the structural fact riding beside it is the trace's business.
            Kind::Card(card)
            | Kind::Io { card, .. }
            | Kind::Done { card, .. }
            | Kind::Notice { card, .. }
            | Kind::Resources { card, .. } => {
                for line in card_stderr(&card) {
                    let _ = writeln!(self.err, "{line}");
                }
            }
            // Sub-agent tokens never reach `out`, so this breadcrumb is the
            // only live sign of a child; its own log dir holds the rest.
            Kind::SubagentDone {
                name,
                outcome,
                text,
                elapsed,
            } => {
                let secs = elapsed.as_secs_f64();
                match outcome.breadcrumb(&text).1 {
                    Some(reason) => {
                        let _ =
                            writeln!(self.err, "[agent: {name} failed in {secs:.1}s — {reason}]");
                    }
                    None => {
                        let _ = writeln!(self.err, "[agent: {name} done in {secs:.1}s]");
                    }
                }
            }
            // Interactive-only, or pure presentation.  Nothing is lost: a
            // boundary carries no value (`result` comes from the attend digest),
            // and reasoning round-trips on the assistant message anyway.
            Kind::Boundary
            | Kind::Born { .. }
            | Kind::Died
            | Kind::UserPromptEcho(_)
            | Kind::Reasoning { .. }
            | Kind::Thinking(_)
            | Kind::Phase(_) => {}
            // A pin overwrites a TUI register; headless has none.
            Kind::Pin { .. } | Kind::Unpin { .. } => {}
        }
    }
}

/// Attend `session` to quiescence in a one-shot headless run from `seed`,
/// over the process's own stdout/stderr.
///
/// # Errors
/// Returns `Err` if no launch prompt was supplied, if the attend worker
/// panics, if the sink's drive fails, or if the run ends without a `reply`
/// (stopped, cancelled, or failed).
pub fn run(
    session: &mut Agent,
    info: &SessionInfo<'_>,
    p: &Provider,
    seed: Option<String>,
    format: OutputFormat,
    engine: Arc<Engine>,
) -> Result<(), String> {
    let prompt = seed
        .ok_or("--headless requires a seed prompt: --prompt, --file, or trailing words after --")?;
    eprintln!(
        "exarch: provider={} model={} base={}",
        p.id().label(),
        p.model(),
        info.base
    );
    let json = format == OutputFormat::Json;
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    let mut headless = Headless::new(json, session.id, &mut stdout, &mut stderr);
    // Bound before `session` is borrowed into the worker closure.
    let root_transcript = session.transcript();
    // A per-exchange bus over the trunk's *own* inbox, so the attend worker and
    // any in-exchange producer share one queue.  It closes when the worker
    // finishes, muting async children on the display — never in the trace.
    let fleet = Fleet {
        agents: session.agents.clone(),
        bus: FleetBus::per_exchange(&session.inbox()),
        engine,
    };
    // A headless trunk is a returning agent that does not park, so `attend`
    // runs this seeded work and returns once idle.
    session.seed(prompt);
    // No slash commands here, so `Control` is a no-op; declared out here so its
    // `&mut` borrow outlives the closure `pump` runs on its scoped thread.
    let mut control = crate::agent::NoControl;
    let root_id = session.id;
    let outcome = pump(
        &mut headless,
        &fleet.bus,
        root_id,
        root_transcript,
        |emit| session.attend(&mut control, emit),
    );
    // The attend digest: an outcome driving `is_error`/`error`, and the root's
    // `reply`.  A panic arrives as `Ok(None)`, already latched by the sink.
    let r: Result<(), String> = match outcome {
        Ok(Some((agent_outcome, payload))) => {
            headless.result = payload.as_ref().map(user_json);
            match agent_outcome {
                // A deliberate empty reply still honours the contract.
                AgentOutcome::Complete | AgentOutcome::Empty => Ok(()),
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
    headless.usage = fleet.bus.usage_total();
    let elapsed = headless.started.elapsed();
    if json {
        println!("{}", result_json(&headless, &r, elapsed));
    } else if !headless.ended_with_newline {
        // So the next shell prompt lands at column 1.
        println!();
    }
    eprintln!("Agent log: {}", session.log_dir().display());
    eprintln!(
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
pub fn converse(session: &mut Agent, message: String, engine: Arc<Engine>) -> Result<(), String> {
    let mut stdout = io::stdout();
    let mut stderr = io::stderr();
    converse_on(session, message, engine, &mut stdout, &mut stderr)
}

/// One exchange of a converse session, streaming what [`run`]'s text mode
/// streams — tokens to `out`, everything else to `err`.
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
    session: &mut Agent,
    message: String,
    engine: Arc<Engine>,
    out: &mut (dyn Write + Send),
    err: &mut (dyn Write + Send),
) -> Result<(), String> {
    let mut sink = Headless::new(false, session.id, out, err);
    let outcome = converse_sink(session, message, engine, &mut sink);
    if outcome.is_ok() && !sink.ended_with_newline {
        let _ = writeln!(sink.out);
    }
    outcome
}

/// One exchange of a converse session, projected onto any [`Sink`].
///
/// The structured twin of [`converse_on`], which carries the contract: here the
/// caller supplies the sink and so sees `Kind` events unflattened.
///
/// # Errors
/// Returns `Err` if the attend worker panics or the sink's drive fails.
pub fn converse_sink<S: Sink>(
    session: &mut Agent,
    message: String,
    engine: Arc<Engine>,
    sink: &mut S,
) -> Result<(), String> {
    let root_transcript = session.transcript();
    // Per-exchange: this call's channel closes when the exchange parks, so
    // draining can never block on a message the caller has not sent yet.
    let fleet = Fleet {
        agents: session.agents.clone(),
        bus: FleetBus::per_exchange(&session.inbox()),
        engine,
    };
    session.seed(message);
    let root_id = session.id;
    let outcome = pump(sink, &fleet.bus, root_id, root_transcript, |emit| {
        session.attend_backlog(emit)
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
    use crate::agent::{RootConfig, RootSeat};
    use crate::provider::Tuning;
    use crate::provider::scripted::{Reply, Script};

    /// A fresh conversing trunk over a scripted provider, in a throwaway run dir.
    fn converse_trunk(tag: &str, script: Script) -> Agent {
        let dir =
            std::env::temp_dir().join(format!("exarch-converse-{tag}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("temp run dir");
        let scratch = Arc::new(
            crate::bootstrap::Scratch::for_test(crate::bootstrap::EXARCH, tag)
                .expect("scratch dir"),
        );
        Agent::root(
            RootConfig {
                system: "system".into(),
                caps: ral_core::types::Capabilities::default(),
                run_dir: dir,
                model: "test-model".into(),
                provider_label: "test".into(),
                allow_schedule: false,
                interactive: true,
                chat: false,
                disk_warn_bytes: None,
                fuel: 0,
                egress: crate::egress::Egress::for_test(),
            },
            RootSeat::Identity {
                scratch,
                cwd: std::env::current_dir().expect("test process has a cwd"),
                detach: false,
            },
            Arc::new(Provider::scripted(
                "test-model",
                crate::provider::ProviderKind::Openai,
                script,
            )),
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
        let engine = Engine::new();
        converse(&mut session, "first message".into(), engine.clone())
            .expect("a conversing trunk never fails for want of a reply");
        converse(&mut session, "second message".into(), engine)
            .expect("the second exchange runs on the same, unbroken session");

        let rendered = format!("{:?}", session.rendered_messages());
        assert!(rendered.contains("first message"), "{rendered}");
        assert!(rendered.contains("hello back"), "{rendered}");
        assert!(rendered.contains("second message"), "{rendered}");
    }

    /// The conversing trunk parks rather than returning: no payload rises, and
    /// the digest never wears `Complete`, the tag only a deliberate `reply` earns.
    #[test]
    fn the_interactive_trunk_parks_rather_than_replying() {
        let mut session = converse_trunk("parks", Script::new().then(Reply::text("hi there")));
        session.seed("hello".into());
        let (tx, _rx) = crate::bus::channel();
        let emit = crate::bus::Emitter::new(tx, session.id);
        let (outcome, payload) = session.attend_backlog(&emit);
        assert!(
            payload.is_none(),
            "a conversing trunk carries no reply payload"
        );
        assert!(
            !matches!(outcome, AgentOutcome::Complete),
            "a conversing trunk never completes by returning a value: {outcome:?}"
        );
    }

    /// `pump` absorbs the unwind and returns normally, so `run` builds its
    /// result from `Ok(())`; only the bus's `Kind::Error` betrays the panic.
    #[test]
    fn recovered_worker_panic_reports_error_not_success() {
        let root: AgentId = 1;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(true, root, &mut sink_out, &mut sink_err);
        h.handle(Event {
            id: root,
            kind: Kind::Error(format!("{}boom", crate::bus::WORKER_PANIC_PREFIX)),
        });
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert_eq!(v["is_error"], serde_json::json!(true), "{out}");
        assert_eq!(v["stop_reason"], serde_json::json!("panicked"), "{out}");
    }

    /// The `Kind::Step(n)` index restarts each time the root resumes after an
    /// async-spawned block, so storing `n` would report only the last segment.
    #[test]
    fn num_steps_counts_root_steps_across_segments() {
        let root: AgentId = 1;
        let sub: AgentId = 2;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(true, root, &mut sink_out, &mut sink_err);
        // Two segments whose indices reset (1..3, then 1..2).
        for n in [1, 2, 3, 1, 2] {
            h.handle(Event {
                id: root,
                kind: Kind::Step {
                    n,
                    tuning: Tuning::default(),
                },
            });
        }
        h.handle(Event {
            id: sub,
            kind: Kind::Step {
                n: 1,
                tuning: Tuning::default(),
            },
        });
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert_eq!(v["num_steps"], serde_json::json!(5), "{out}");
    }

    /// An object stays an object: stringifying it would double-encode the
    /// structure, leaving the harness to parse JSON out of a JSON string.
    #[test]
    fn structured_result_is_faithful_not_stringified() {
        let root: AgentId = 1;
        let mut sink_out = Vec::new();
        let mut sink_err = Vec::new();
        let mut h = Headless::new(true, root, &mut sink_out, &mut sink_err);
        h.result = Some(serde_json::json!({ "files": ["a.rs", "b.rs"] }));
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
        let h = Headless::new(true, root, &mut sink_out, &mut sink_err);
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
        let mut h = Headless::new(true, root, &mut sink_out, &mut sink_err);
        let payload = ral_core::serial::FOValue::String {
            value: "plain text reply".into(),
        };
        h.result = Some(user_json(&payload));
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
        let mut h = Headless::new(true, root, &mut sink_out, &mut sink_err);
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
        h.result = Some(user_json(&payload));
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert!(v["result"].is_object(), "{out}");
        assert_eq!(v["result"]["files"][0], serde_json::json!("a.rs"), "{out}");
    }

    /// A host's own writers get the same split `converse` gives the process's
    /// stdout/stderr — reply text on `out`, breadcrumbs on `err`.
    #[test]
    fn converse_on_projects_into_the_given_writers() {
        let mut session = converse_trunk("writers", Script::new().then(Reply::text("hi there")));
        let engine = Engine::new();
        let mut out = Vec::new();
        let mut err = Vec::new();
        converse_on(&mut session, "hello".into(), engine, &mut out, &mut err)
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

    /// A caller's own `Sink` receives `Kind` events whole, rather than through
    /// `Headless`'s token/card writer split.
    #[test]
    fn converse_sink_delivers_structured_events() {
        struct Collecting(Vec<Kind>);
        impl Sink for Collecting {
            fn handle(&mut self, e: Event) {
                self.0.push(e.kind);
            }
        }

        let mut session = converse_trunk("sink", Script::new().then(Reply::text("hi there")));
        let engine = Engine::new();
        let mut sink = Collecting(Vec::new());
        converse_sink(&mut session, "hello".into(), engine, &mut sink)
            .expect("a conversing trunk never fails for want of a reply");

        assert!(
            sink.0
                .iter()
                .any(|k| matches!(k, Kind::Token(t) if t.contains("hi there"))),
            "reply text must arrive as a Kind::Token among {} events",
            sink.0.len()
        );
        assert!(
            sink.0.iter().any(|k| matches!(k, Kind::Step { .. })),
            "the exchange boundary must arrive as a Kind::Step among {} events",
            sink.0.len()
        );
    }
}
