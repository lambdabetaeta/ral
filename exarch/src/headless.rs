//! One-shot frontend over the bus event stream.  In text mode the root
//! agent's assistant tokens stream to stdout; under `--output-format
//! json` they are held back for a single result object emitted at the
//! end.  Every other event — and sub-agent activity, as breadcrumbs —
//! goes to stderr; the process exits after one seed turn.  Takes the
//! default [`Sink::drive`]; only [`Sink::handle`] is custom.
//!
//! Independently of what reaches stdout/stderr, the full structured
//! event stream is mirrored to `transcript.jsonl` in the session log
//! dir — one JSON object per event, tagged with its session id — so a
//! post-mortem reader gets the lossless trace (every tool `cmd` and
//! result, step, usage delta, patch, subagent boundary) whatever the
//! human stdout/stderr projection shows, and sub-agent output
//! de-multiplexes cleanly by id.

use crate::bus::{Event, Kind, Row, SessionBus, SessionId, Sink, pump};
use crate::card::{Card, FieldVal, Mark};
use crate::provider::{Provider, Usage};
use crate::session::Session;
use crate::tui::SessionInfo;
use std::fs::File;
use std::io::{self, BufWriter, Write};
use std::time::Instant;

/// What the root agent's output looks like on stdout in headless mode.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Stream the root agent's assistant text to stdout as it arrives.
    Text,
    /// Hold the text back and emit one JSON result object to stdout
    /// when the run ends.
    Json,
}

pub struct Headless {
    usage: Usage,
    /// The root session's id.  Only this session's tokens reach
    /// stdout; sub-agent token streams stay off the main surface
    /// (they'd interleave) and surface as `SubagentDone` breadcrumbs.
    root_id: SessionId,
    /// Structured mirror of the bus: every event except the token
    /// stream and the interactive-only live lines is written here as
    /// one JSON object per line. Independent of `verbose` — this is
    /// the lossless trace; stdout/stderr are its human projection.
    events: BufWriter<File>,
    /// Start instant for the monotonic `t_ms` offset stamped on each
    /// JSONL record.
    started: Instant,
    /// When true, stdout carries a single JSON result object emitted at
    /// the end instead of the live token stream.
    json_mode: bool,
    /// Highest root-session step number seen — the turn count surfaced
    /// at the end and in the JSON result.
    turns: u32,
    /// Last non-routine stop reason surfaced by the root, if any.
    last_stop: Option<String>,
    /// Root assistant tokens for the message currently streaming (json
    /// mode only); rolled into `final_msg` at each message boundary.
    cur_msg: String,
    /// The root's last completed assistant message — the `result` field
    /// of the JSON output.
    final_msg: String,
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
    fn new(json_mode: bool, events: BufWriter<File>, root_id: SessionId) -> Self {
        Self {
            usage: Usage::default(),
            root_id,
            events,
            started: Instant::now(),
            json_mode,
            turns: 0,
            last_stop: None,
            cur_msg: String::new(),
            final_msg: String::new(),
            panicked: false,
            ended_with_newline: false,
        }
    }
}

/// Project one event into its `transcript.jsonl` record, or `None` for
/// the variants we deliberately omit: the `Token` stream (assistant
/// prose is already on stdout) and the interactive-only `Boundary` and
/// `UserPromptEcho`. The exhaustive match means a new [`Kind`] variant
/// won't silently fall out of the trace.
pub(crate) fn event_record(t_ms: u128, id: SessionId, kind: &Kind) -> Option<serde_json::Value> {
    use serde_json::json;
    let (name, mut obj) = match kind {
        Kind::Born { log_dir, title } => (
            "born",
            json!({ "log_dir": log_dir.to_string_lossy(), "title": title }),
        ),
        Kind::Died => ("died", json!({})),
        Kind::Usage(u) => (
            "usage",
            json!({
                "input": u.input,
                "output": u.output,
                "cache_creation": u.cache_creation,
                "cache_read": u.cache_read,
                "dollars": u.dollars,
            }),
        ),
        Kind::Step(n) => ("step", json!({ "n": n })),
        Kind::ToolCall { tool, cmd, summary } => (
            "tool_call",
            json!({ "tool": tool, "cmd": cmd, "summary": summary }),
        ),
        Kind::ToolResult(text) => ("tool_result", json!({ "text": text })),
        Kind::StopReason(raw) => ("stop_reason", json!({ "raw": raw })),
        Kind::Error(msg) => ("error", json!({ "msg": msg })),
        Kind::Dim(text) => ("dim", json!({ "text": text })),
        Kind::ProviderError(error) => ("provider_error", json!({ "error": error })),
        Kind::SubagentDone {
            title,
            outcome,
            text,
            elapsed,
        } => {
            let (text, error) = outcome.breadcrumb(text);
            (
                "subagent_done",
                json!({
                    "title": title,
                    "text": text,
                    "error": error,
                    "elapsed_ms": elapsed.as_millis() as u64,
                }),
            )
        }
        // The whole mark tree, so the machine log stays structured; only a
        // `raw` mark is opaque, and honestly so.
        Kind::Card(card) => ("card", json!({ "card": card })),
        // The raw structural effect beside the mark tree composed from it:
        // the log keeps the io event's structure (which the rendered card
        // erases) for a post-mortem, alongside the card the rail drew.
        Kind::Io { event, card } => ("io", json!({ "event": event, "card": card })),
        Kind::Phase(label) => ("phase", json!({ "label": label })),
        // Pinned state is what is *currently true*, not a thing that happened —
        // it is never an `events.json` row, exactly as the matrix is not.
        Kind::Token(_)
        | Kind::Boundary
        | Kind::UserPromptEcho(_)
        | Kind::Pin { .. }
        | Kind::Unpin { .. } => return None,
    };
    let map = obj
        .as_object_mut()
        .expect("event_record arms are JSON objects");
    map.insert("t_ms".into(), json!(t_ms as u64));
    map.insert("id".into(), json!(id));
    map.insert("kind".into(), json!(name));
    Some(obj)
}

/// Condense a surfaced [`Card`] to stderr lines — the human projection of
/// the mark tree, walked generically: a `diff` reproduces the old patch
/// lines, a `measure` its bracketed readout, `fields` its aligned pairs,
/// `text` its plain prose, `raw` its bytes lossily.  The structured form
/// stays in `transcript.jsonl`.
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
                        out.push(match row {
                            Row::Context(l) => format!("    {l}"),
                            Row::Del(l) => format!("  - {l}"),
                            Row::Add(l) => format!("  + {l}"),
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

/// The `--output-format json` result object on stdout: the root's final
/// message plus the run's stop reason, turn count, wall-clock, and token
/// usage + cost.  Field names mirror Claude Code's headless result so a
/// harness can parse either agent identically.
fn result_json(h: &Headless, r: &Result<(), String>, elapsed: std::time::Duration) -> String {
    use serde_json::json;
    let u = &h.usage;
    let mut obj = json!({
        "type": "result",
        "is_error": r.is_err() || h.panicked,
        "result": h.final_msg,
        "stop_reason": h.last_stop.clone().unwrap_or_else(|| {
            // A recovered worker panic leaves no StopReason; report it as
            // such rather than letting it default to "completed".
            if h.panicked { "panicked".into() } else { "completed".into() }
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
    fn handle(&mut self, e: Event) {
        let t_ms = self.started.elapsed().as_millis();
        let id = e.id;
        if let Some(rec) = event_record(t_ms, id, &e.kind)
            && let Ok(line) = serde_json::to_string(&rec)
        {
            let _ = writeln!(self.events, "{line}");
        }
        match e.kind {
            // Only the root agent's tokens reach the user — in text mode
            // they stream to stdout; in json mode they accumulate so the
            // last completed message becomes the `result` field.  Sub-
            // agent token streams would interleave unreadably and are an
            // internal detail of the root's turn; their full transcript
            // lives in that session's own log dir.
            Kind::Token(text) if id == self.root_id => {
                if self.json_mode {
                    self.cur_msg.push_str(&text);
                } else {
                    let mut out = io::stdout();
                    let _ = out.write_all(text.as_bytes());
                    let _ = out.flush();
                    if let Some(last) = text.as_bytes().last() {
                        self.ended_with_newline = *last == b'\n';
                    }
                }
            }
            Kind::Token(_) => {}
            Kind::Usage(u) => self.usage += u,
            Kind::Step(n) => {
                if id == self.root_id {
                    self.turns = n;
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
            Kind::Dim(text) => eprintln!("{text}"),
            Kind::ProviderError(error) => eprintln!("provider error: {error:?}"),
            // A surfaced render document, or a structural I/O event paired
            // with the card composed from it.  In headless we condense the
            // card's marks to stderr lines generically; the canonical
            // structured form (the mark tree, and for io the raw event
            // beside it) is in `transcript.jsonl`.
            Kind::Card(card) | Kind::Io { card, .. } => {
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
                        eprintln!("[agent: {title} failed in {secs:.1}s — {reason}]")
                    }
                    None => eprintln!("[agent: {title} done in {secs:.1}s]"),
                }
            }
            // A message boundary: in json mode the root's just-completed
            // message becomes the candidate `result` (last one wins).
            Kind::Boundary if self.json_mode && id == self.root_id => {
                self.final_msg = std::mem::take(&mut self.cur_msg);
            }
            // Boundary / Born / Died / UserPromptEcho are interactive-only;
            // Phase is already captured in `events.json` by `event_record`
            // and would only clutter the live stderr stream.
            Kind::Boundary
            | Kind::Born { .. }
            | Kind::Died
            | Kind::UserPromptEcho(_)
            | Kind::Phase(_) => {}
            // Pinned state is a TUI register; headless has no register to
            // overwrite, so a pin neither prints nor logs.
            Kind::Pin { .. } | Kind::Unpin { .. } => {}
        }
    }
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:transcript-file] creates the headless run's transcript.jsonl; output infra, not turn-time data I/O"
)]
pub fn run(
    session: &mut Session,
    provider: &std::sync::Arc<Provider>,
    info: &SessionInfo<'_>,
    seed: Option<String>,
    format: OutputFormat,
) -> Result<(), String> {
    let prompt = seed.ok_or_else(|| "--headless requires --prompt or --file".to_string())?;
    eprintln!(
        "exarch: provider={} model={} base={}",
        info.provider, info.model, info.base
    );
    let log_dir = session.log_dir();
    let transcript = log_dir.join("transcript.jsonl");
    let file = File::create(&transcript).map_err(|e| format!("{}: {e}", transcript.display()))?;
    let json = format == OutputFormat::Json;
    let mut headless = Headless::new(json, BufWriter::new(file), session.id);
    // A per-turn bus over the session's *own* inbox, so the drive worker's
    // emitter and any in-turn producer share one queue.  Headless has no idle
    // loop and no tabs, so the channel closes when the worker finishes and
    // async children stay muted to their own log — the behaviour headless has
    // always had.
    let bus = SessionBus::per_turn(session.inbox());
    // Seed the launch prompt into that same inbox; the headless root is
    // `park_when_idle=false`, so `drive` runs the seeded work and returns once
    // it is idle.
    session.seed(prompt);
    // A fixed provider handle — headless never swaps the model mid-run.
    let provider = crate::session::ProviderHandle::new(std::sync::Arc::clone(provider));
    // Headless has no slash commands, so the drive loop's `Control` is a no-op;
    // declared here so its `&mut` borrow outlives the worker closure `pump`
    // runs on its scoped thread.
    let mut control = crate::session::NoControl;
    // Drive on a scoped worker thread while the main thread drives the sink.
    // `pump` returns the worker's `(outcome, text)`, or `None` if it panicked.
    let root_id = session.id;
    let outcome = pump(&mut headless, &bus, root_id, |emit| {
        session.drive(provider.clone(), &mut control, emit)
    });
    // The sink already latched `panicked` from the WORKER_PANIC_PREFIX error a
    // worker unwind emits, so this `Result` only drives the top-level
    // `is_error`/`error` fields; `result_json` reads the message and stop
    // reason from the sink, not from here.
    let r: Result<(), String> = match outcome {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err("worker panicked".to_string()),
        Err(e) => Err(e.to_string()),
    };
    let _ = headless.events.flush();
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
    eprintln!("Session log: {}", session.log_dir().display());
    eprintln!(
        "[done] {} turns · {:.1}s · {} · ${:.4}",
        headless.turns,
        elapsed.as_secs_f64(),
        headless.usage,
        headless.usage.dollars
    );
    r
}

#[cfg(test)]
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;

    /// A recovered worker panic must surface in the JSON result as an error,
    /// not a clean completion: `pump` absorbs the unwind and returns the worker
    /// value, so `run` builds the result from `Ok(())` — the panic is
    /// recognised only by the `Kind::Error` the bus emits.
    #[test]
    fn recovered_worker_panic_reports_error_not_success() {
        let root: SessionId = 1;
        let path =
            std::env::temp_dir().join(format!("exarch-panic-result-{}.jsonl", std::process::id()));
        let file = File::create(&path).expect("events file");
        let mut h = Headless::new(true, BufWriter::new(file), root);
        h.handle(Event {
            id: root,
            kind: Kind::Error(format!("{}boom", crate::bus::WORKER_PANIC_PREFIX)),
        });
        let out = result_json(&h, &Ok(()), std::time::Duration::ZERO);
        let v: serde_json::Value = serde_json::from_str(&out).expect("result is JSON");
        assert_eq!(v["is_error"], serde_json::json!(true), "{out}");
        assert_eq!(v["stop_reason"], serde_json::json!("panicked"), "{out}");
    }
}
