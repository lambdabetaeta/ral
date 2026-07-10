//! One-shot frontend over the bus event stream.
//!
//! In text mode the root
//! agent's assistant tokens stream to stdout as live narration; under
//! `--output-format json` they are dropped and a single result object —
//! built from the root's deliberate `reply` value — is emitted at the end
//! ([[decisions/260623_reply-terminates-returning-agents]]).  Every other
//! event — and sub-agent activity, as breadcrumbs — goes to stderr; the
//! process exits after one seed turn.  Takes the default [`Sink::drive`];
//! only [`Sink::handle`] is custom.
//!
//! This frontend is a *display* — it projects the bus to stdout/stderr.
//! The durable record is not its concern: each session writes its own
//! `transcript.jsonl` (operational view) and `events.json` (model view)
//! through the [`crate::transcript`] and [`crate::event`] seams, in headless
//! exactly as in the TUI, for the root and every forked child alike.

use crate::agent::Agent;
use crate::bus::{AgentId, AgentOutcome, Event, FleetBus, Kind, Row, Sink, pump};
use crate::card::{Card, FieldVal, Mark};
use crate::fleet::Fleet;
use crate::provider::{Engine, Provider, Usage};
use crate::tui::SessionInfo;
use std::io::{self, Write};
use std::sync::Arc;
use std::time::Instant;

/// What the root agent's output looks like on stdout in headless mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Stream the root agent's assistant text to stdout as it arrives.
    Text,
    /// Drop the streamed text and emit one JSON result object to stdout
    /// when the run ends — its `result` is the root's `reply` value.
    Json,
}

pub struct Headless {
    /// The run total, read once from the bus's [`UsageMeter`](crate::bus::UsageMeter)
    /// at the end of the run — the root plus every sub-agent, muted or live,
    /// since each tees its usage to the shared meter at the emit seam.  It is
    /// **not** a per-event sink accumulation: a `Kind::Usage` arriving here is
    /// already counted in the meter, so the sink must not add it again.
    usage: Usage,
    /// The root session's id.  Only this session's tokens reach
    /// stdout; sub-agent token streams stay off the main surface
    /// (they'd interleave) and surface as `SubagentDone` breadcrumbs.
    root_id: AgentId,
    /// Start instant for the run's wall-clock, surfaced at the end and in
    /// the JSON result.
    started: Instant,
    /// When true, stdout carries a single JSON result object emitted at
    /// the end instead of the live token stream.
    json_mode: bool,
    /// Cumulative count of root-session steps (LLM round-trips) — the
    /// turn count surfaced at the end and in the JSON result. The
    /// per-segment `Kind::Step(n)` index resets when the root resumes
    /// after an async-spawned block, so this counts rather than tracks it.
    turns: u32,
    /// Last non-routine stop reason surfaced by the root, if any.
    last_stop: Option<String>,
    /// The root's deliberate return value — the faithful payload it passed to
    /// `reply`, captured from the drive digest after the run
    /// ([[decisions/260623_reply-terminates-returning-agents]]).  It is the
    /// `result` field of the JSON output: a string stays a string, an
    /// object/array stays structured (no double-encoding into a JSON string).
    /// `None` when the root finished without replying, which renders as a JSON
    /// null and pairs with `is_error`.
    result: Option<serde_json::Value>,
    /// Set when a worker thread unwound this run.  `pump` recovers from the
    /// panic, emits it as a [`WORKER_PANIC_PREFIX`](crate::bus::WORKER_PANIC_PREFIX)
    /// error, and returns the worker value as normal, so without latching it
    /// here the JSON result would report a crashed turn as a clean, empty
    /// success.
    panicked: bool,
    /// Whether the last stdout byte written in text mode was a newline, so
    /// the closing newline is emitted only when the streamed reply did not
    /// already end with one.
    ended_with_newline: bool,
}

impl Headless {
    fn new(json_mode: bool, root_id: AgentId) -> Self {
        Self {
            usage: Usage::default(),
            root_id,
            started: Instant::now(),
            json_mode,
            turns: 0,
            last_stop: None,
            result: None,
            panicked: false,
            ended_with_newline: false,
        }
    }
}

/// Condense a surfaced [`Card`] to stderr lines — the human projection of
/// the mark tree, walked generically: a `diff` reproduces the old patch
/// lines, a `measure` its bracketed readout, `fields` its aligned pairs,
/// `text` its plain prose, `raw` its bytes lossily.  The card is a rendering;
/// the operational trace keeps only an io card's structural `event`, never
/// the mark tree itself.
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
                    out.push(format!("  {}: {}", f.label, field_value_text(&f.value)));
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

/// The plain text of a fields-row value for the stderr condenser.
fn field_value_text(v: &FieldVal) -> String {
    match v {
        FieldVal::Inline(spans) => spans.iter().map(|s| s.text.as_str()).collect(),
        FieldVal::Measure(m) => match m.max {
            Some(mx) => format!("{}/{mx}", m.value),
            None => format!("{}{}", m.value, m.unit.as_deref().unwrap_or("")),
        },
    }
}

/// The `--output-format json` result object on stdout: the root's deliberate
/// `reply` value plus the run's stop reason, turn count, wall-clock, and token
/// usage + cost.  Field names mirror Claude Code's headless result so a harness
/// can parse either agent identically; `result` deliberately diverges in *type*
/// — it is the faithful value (`string | object | array | null`), not always a
/// string, so a structured reply is not double-encoded into a JSON string.
fn result_json(h: &Headless, r: &Result<(), String>, elapsed: std::time::Duration) -> String {
    use serde_json::json;
    let u = &h.usage;
    let mut obj = json!({
        "type": "result",
        "is_error": r.is_err() || h.panicked,
        "result": h.result,
        "stop_reason": h.last_stop.clone().unwrap_or_else(|| {
            // A recovered worker panic leaves no StopReason; report it as
            // such rather than letting it default to "completed".
            if h.panicked {
                "panicked".into()
            } else if r.is_err() {
                // The run errored with no provider stop reason: it ended
                // without producing a `reply` value (a no-reply finish, or a
                // failure before one).  Report that rather than the misleading
                // "completed"; the `error` field below carries the detail.
                "no_reply".into()
            } else {
                "completed".into()
            }
        }),
        "num_turns": h.turns,
        "duration_ms": elapsed.as_millis() as u64,
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

impl Sink for Headless {
    #[allow(clippy::match_same_arms)]
    fn handle(&mut self, e: Event) {
        let id = e.id;
        match e.kind {
            // Only the root agent's tokens reach the user, and only in text
            // mode — there they stream to stdout as live narration.  In json
            // mode they are dropped: the `result` is the root's deliberate
            // `reply` value, captured from the drive digest, not whatever prose
            // happened to stream.  Sub-agent token streams would interleave
            // unreadably and are an internal detail of the root's turn; their
            // full transcript lives in that session's own log dir.
            Kind::Token(text) if id == self.root_id && !self.json_mode => {
                let mut out = io::stdout();
                let _ = out.write_all(text.as_bytes());
                let _ = out.flush();
                if let Some(last) = text.as_bytes().last() {
                    self.ended_with_newline = *last == b'\n';
                }
            }
            Kind::Token(_) => {}
            Kind::Usage(_) => {}
            Kind::Step { n, .. } => {
                if id == self.root_id {
                    self.turns += 1;
                    eprintln!("[step {n}]");
                }
            }
            Kind::ToolCall {
                tool, cmd, summary, ..
            } if id == self.root_id => {
                eprintln!("[tool: {tool}]");
                if let Some(s) = &summary {
                    for line in s.lines() {
                        eprintln!("  {line}");
                    }
                }
                for line in cmd.lines() {
                    eprintln!("  {line}");
                }
            }
            Kind::ToolCall { .. } => {}
            Kind::ToolResult(_) => {}
            Kind::StopReason(raw) => {
                if id == self.root_id {
                    self.last_stop = Some(raw.clone());
                }
                eprintln!("[stop: {raw}]");
            }
            Kind::Error(msg) => {
                if msg.starts_with(crate::bus::WORKER_PANIC_PREFIX) {
                    self.panicked = true;
                }
                eprintln!("error: {msg}");
            }
            Kind::SystemNote(text) => eprintln!("{text}"),
            Kind::Nudge {
                used, max, cause, ..
            } if id == self.root_id => eprintln!("[nudge {used}/{max}: {cause}]"),
            Kind::Nudge { .. } => {}
            Kind::ProviderError(error) => eprintln!("provider error: {error:?}"),
            // A surfaced render document, a structural I/O event paired with
            // the card composed from it, a detached worker's settlement, a
            // core-pushed ready-boundary notice (a reap, a binding prune, a
            // large-binding warning), or the `/resources` fold.  In headless
            // we condense the card's marks to stderr lines generically; the
            // raw structural fact behind each is kept in `transcript.jsonl`
            // (the card is a rendering and is not).
            Kind::Card(card)
            | Kind::Io { card, .. }
            | Kind::Done { card, .. }
            | Kind::Notice { card, .. }
            | Kind::Resources { card, .. } => {
                for line in card_stderr(&card) {
                    eprintln!("{line}");
                }
            }
            // A sub-agent finished.  Headless keeps sub-agents off the
            // main surface — their tokens never reach stdout — so this
            // breadcrumb is the only live trace of the child.  The full
            // sub-agent transcript lives in its own session log dir.
            Kind::SubagentDone {
                title,
                outcome,
                text,
                elapsed,
            } => {
                let secs = elapsed.as_secs_f64();
                match outcome.breadcrumb(&text).1 {
                    Some(reason) => {
                        eprintln!("[agent: {title} failed in {secs:.1}s — {reason}]");
                    }
                    None => eprintln!("[agent: {title} done in {secs:.1}s]"),
                }
            }
            // Boundary / Born / Died / UserPromptEcho are interactive-only;
            // Phase is a progress label — a rendering — so headless neither
            // prints it (it would clutter the stderr stream) nor records it
            // (the operational trace omits presentation events).  A message
            // boundary is no longer scraped for the json `result`: that value
            // is the root's deliberate `reply`, captured from the drive digest.
            // Reasoning / thinking are TUI-only presentation blocks; the
            // content already round-trips to the model on the assistant
            // message, so headless neither prints nor records them.
            Kind::Boundary
            | Kind::Born { .. }
            | Kind::Died
            | Kind::UserPromptEcho(_)
            | Kind::Reasoning { .. }
            | Kind::Thinking(_)
            | Kind::Phase(_) => {}
            // Pinned state is a TUI register; headless has no register to
            // overwrite, so a pin neither prints nor logs.
            Kind::Pin { .. } | Kind::Unpin { .. } => {}
        }
    }
}

/// Drive a one-shot headless run of `session` from `seed` to quiescence,
/// emitting the trace and (for `Json`) the final `result` to stdout.
///
/// # Errors
/// Returns `Err` if no launch prompt was supplied, if the drive worker
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
    let prompt = seed.ok_or_else(|| "--headless requires --prompt or --file".to_string())?;
    eprintln!(
        "exarch: provider={} model={} base={}",
        p.id().label(),
        p.model(),
        info.base
    );
    let json = format == OutputFormat::Json;
    let mut headless = Headless::new(json, session.id);
    // The root's trace handle, attached to the emitter `pump` drives it
    // through; bound before `session` is borrowed into the worker closure.
    let root_transcript = session.transcript();
    // The fleet for this one-shot run: the trunk's shared registry, focus
    // handle, and human-attachment, plus a per-turn bus over the trunk's *own*
    // inbox (so the drive worker's emitter and any in-turn producer share one
    // queue).  Headless has no idle loop and no tabs, so the channel closes when
    // the worker finishes and async children stay muted *on the display* — but
    // each still records its own trace, the behaviour we want everywhere.
    let fleet = Fleet::new(
        session.agents.clone(),
        FleetBus::per_turn(session.inbox()),
        session.focus_handle(),
        session.interactive(),
        engine,
    );
    // Seed the launch prompt into that same inbox; the headless trunk is a
    // returning agent that does not park, so `drive` runs the seeded work and
    // returns once it is idle.
    session.seed(prompt);
    // Headless has no slash commands, so the drive loop's `Control` is a no-op;
    // declared here so its `&mut` borrow outlives the worker closure `pump`
    // runs on its scoped thread.  The trunk drives on its own provider handle.
    let mut control = crate::agent::NoControl;
    // Drive on a scoped worker thread while the main thread drives the sink.
    // `pump` returns the worker's `(outcome, text)`, or `None` if it panicked.
    let root_id = session.id;
    let outcome = pump(
        &mut headless,
        &fleet.bus,
        root_id,
        root_transcript,
        |emit| session.drive(&mut control, emit),
    );
    // `pump` returns the drive digest — the outcome and the root's faithful
    // `reply` payload.  The payload is the json `result` (a string stays a
    // string, an object/array stays structured); the outcome drives the
    // top-level `is_error`/`error` fields.  The sink already latched `panicked`
    // from the WORKER_PANIC_PREFIX error a worker unwind emits.
    let r: Result<(), String> = match outcome {
        Ok(Some((agent_outcome, payload))) => {
            headless.result = payload;
            match agent_outcome {
                // A reply (or a deliberate empty reply) completed the contract.
                AgentOutcome::Complete | AgentOutcome::Empty => Ok(()),
                // A finish without `reply`, a stop, or a failure: no result.
                AgentOutcome::Stopped(s) => Err(s),
                AgentOutcome::Cancelled => Err("cancelled".to_string()),
                AgentOutcome::Failed(e) => Err(e),
            }
        }
        Ok(None) => Err("worker panicked".to_string()),
        Err(e) => Err(e.to_string()),
    };
    // The run total, summed at the emit seam across the root and every
    // sub-agent — including async children muted on this per-turn bus, whose
    // usage never reached the sink.  This is the single source of truth the
    // JSON result and the `[done]` line both read.
    headless.usage = fleet.bus.usage_total();
    let elapsed = headless.started.elapsed();
    if json {
        // The streamed text was held back, so this single result object
        // is the only thing on stdout.
        println!("{}", result_json(&headless, &r, elapsed));
    } else if !headless.ended_with_newline {
        // Trailing newline so the next shell prompt lands at column 1
        // when the streamed reply did not end with one.
        println!();
    }
    eprintln!("Agent log: {}", session.log_dir().display());
    eprintln!(
        "[done] {} turns · {:.1}s · {}",
        headless.turns,
        elapsed.as_secs_f64(),
        headless.usage
    );
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::Tuning;

    /// A recovered worker panic must surface in the JSON result as an error,
    /// not a clean completion: `pump` absorbs the unwind and returns the worker
    /// value, so `run` builds the result from `Ok(())` — the panic is
    /// recognised only by the `Kind::Error` the bus emits.
    #[test]
    fn recovered_worker_panic_reports_error_not_success() {
        let root: AgentId = 1;
        let mut h = Headless::new(true, root);
        h.handle(Event {
            id: root,
            kind: Kind::Error(format!("{}boom", crate::bus::WORKER_PANIC_PREFIX)),
        });
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert_eq!(v["is_error"], serde_json::json!(true), "{out}");
        assert_eq!(v["stop_reason"], serde_json::json!("panicked"), "{out}");
    }

    /// `num_turns` is the cumulative count of root steps, not the last
    /// per-segment index: the `Kind::Step(n)` index restarts each time the
    /// root resumes after an async-spawned block, so storing `n` would
    /// report only the final segment. Sub-agent steps must not count.
    #[test]
    fn num_turns_counts_root_steps_across_segments() {
        let root: AgentId = 1;
        let sub: AgentId = 2;
        let mut h = Headless::new(true, root);
        // Two root segments whose per-segment indices reset (1..3, then
        // 1..2), with a sub-agent step interleaved that must be ignored.
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
        assert_eq!(v["num_turns"], serde_json::json!(5), "{out}");
    }

    /// A structured `reply` value reaches the json `result` faithfully — an
    /// object stays an object, not a pretty-printed string.  Stringifying it
    /// would double-encode the structure so the harness re-parses JSON out of a
    /// JSON string; the union-typed `result` is the deliberate divergence from
    /// Claude Code's always-string shape.
    #[test]
    fn structured_result_is_faithful_not_stringified() {
        let root: AgentId = 1;
        let mut h = Headless::new(true, root);
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

    /// A root that finished without calling `reply` yields an honest failure:
    /// no `result` value (a JSON null), `is_error` true, and a `no_reply` stop
    /// reason rather than the misleading "completed".
    #[test]
    fn no_reply_root_is_error_with_null_result() {
        let root: AgentId = 1;
        // `result` stays `None` — the run errored before producing a reply.
        let h = Headless::new(true, root);
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
}
