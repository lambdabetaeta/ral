//! Per-session operational trace: a `transcript.jsonl` in the session's log
//! dir, one JSON object per bus event.
//!
//! What the agent *did* — every tool call and result, step, usage delta,
//! structural I/O effect, stop reason, error, sub-agent boundary — sibling to
//! the model-view `events.json` that `AgentLog` keeps. A rendering is not an
//! effect, so composed cards and progress labels stay out; they are the TUI's
//! `user.log`. The writer is fed at the emit seam (`Emitter::emit`), not by
//! whoever drains the live bus, so a child muted off the display still records
//! its full trace.

use crate::bus::{AgentId, Kind};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A cheap, cloneable handle to one session's `transcript.jsonl` writer.
///
/// Clones share the one file, so a session writes through a single handle
/// whoever holds it; a forked session gets its own `Transcript`.
#[derive(Clone)]
pub struct Transcript(Option<Arc<Inner>>);

struct Inner {
    /// Flushed per record, so the file is always tail-able.
    file: Mutex<BufWriter<File>>,
    /// Origin of the `t_ms` offset stamped on each record.
    started: Instant,
}

impl Transcript {
    /// Open `path` for this session's trace, truncating any prior file.
    ///
    /// # Errors
    /// Returns `Err` if the file cannot be created.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:transcript-file] creates the session's transcript.jsonl; output infra, not turn-time data I/O"
    )]
    pub fn create(path: &Path) -> std::io::Result<Self> {
        let file = File::create(path)?;
        Ok(Self(Some(Arc::new(Inner {
            file: Mutex::new(BufWriter::new(file)),
            started: Instant::now(),
        }))))
    }

    /// A trace that records nothing — tests, and any emitter with no log dir.
    pub fn none() -> Self {
        Self(None)
    }

    /// Record one bus event as a JSONL line. Rendering-only events project to
    /// `None` before the lock is taken, so the token stream — the one
    /// high-frequency `Kind` — costs nothing here.
    pub fn record(&self, id: AgentId, kind: &Kind) {
        let Some(inner) = &self.0 else { return };
        let t_ms = inner.started.elapsed().as_millis();
        let Some(rec) = event_record(t_ms, id, kind) else {
            return;
        };
        let Ok(line) = serde_json::to_string(&rec) else {
            return;
        };
        if let Ok(mut file) = inner.file.lock() {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }
}

/// Project one bus event into its `transcript.jsonl` record, or `None` for a
/// variant the operational trace omits. The exhaustive match means a new
/// [`Kind`] cannot silently fall out of the trace.
pub(crate) fn event_record(t_ms: u128, id: AgentId, kind: &Kind) -> Option<serde_json::Value> {
    use serde_json::json;
    let (name, mut obj) = match kind {
        Kind::Born {
            log_dir,
            name,
            parent,
            branch,
        } => (
            "born",
            json!({ "log_dir": log_dir.to_string_lossy(), "name": name, "parent": parent, "branch": branch }),
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
        Kind::Step { n, tuning } => ("step", json!({ "n": n, "tuning": tuning })),
        Kind::ToolCall { tool, cmd, summary } => (
            "tool_call",
            json!({ "tool": tool, "cmd": cmd, "summary": summary }),
        ),
        // An act's subject and payload have no slot in `tool_call`; its result
        // does record as `tool_result`, so the forensic pair stays intact.
        Kind::HarnessCall {
            verb,
            subject,
            payload,
            failed,
        } => (
            "harness_call",
            json!({ "verb": verb, "subject": subject, "payload": payload, "failed": failed }),
        ),
        Kind::ToolResult(text) | Kind::HarnessResult(text) => {
            ("tool_result", json!({ "text": text }))
        }
        Kind::StopReason(raw) => ("stop_reason", json!({ "raw": raw })),
        Kind::Error(msg) => ("error", json!({ "msg": msg })),
        Kind::SystemNote(text) => ("system_note", json!({ "text": text })),
        Kind::Nudge { used, max, cause } => {
            ("nudge", json!({ "used": used, "max": max, "cause": cause }))
        }
        Kind::ProviderError(error) => ("provider_error", json!({ "error": error })),
        Kind::SubagentDone {
            name,
            outcome,
            text,
            elapsed,
        } => {
            let (text, error) = outcome.breadcrumb(text);
            #[allow(
                clippy::cast_possible_truncation,
                reason = "elapsed-ms fits u64 for any real run"
            )]
            let elapsed_ms = elapsed.as_millis() as u64;
            (
                "subagent_done",
                json!({
                    "name": name,
                    "text": text,
                    "error": error,
                    "elapsed_ms": elapsed_ms,
                }),
            )
        }
        // Io, Done, Notice, and Resources each carry a raw fact beside the
        // `card` the TUI renders from it; the trace keeps the fact, never the
        // rendering, which lives only in the TUI's `user.log`.
        Kind::Io { event, .. } => ("io", json!({ "event": event })),
        Kind::Done { outcome, .. } => {
            use crate::bus::card::DoneOutcome;
            match outcome {
                DoneOutcome::Ok => ("done", json!({ "outcome": "ok" })),
                DoneOutcome::Err { message, status } => (
                    "done",
                    json!({ "outcome": "err", "message": message, "status": status }),
                ),
                DoneOutcome::Panic { message } => {
                    ("done", json!({ "outcome": "panic", "message": message }))
                }
            }
        }
        Kind::Notice { notice, .. } => {
            use crate::bus::card::Notice;
            match notice {
                Notice::Reap { cmd, cause } => {
                    let cause = match cause {
                        ral_core::types::ReapCause::Idle => "idle",
                        ral_core::types::ReapCause::Backstop => "backstop",
                        ral_core::types::ReapCause::Retention => "retention",
                    };
                    ("worker_reaped", json!({ "cmd": cmd, "cause": cause }))
                }
                Notice::Prune { names, idle_calls } => (
                    "bindings_pruned",
                    json!({ "names": names, "idle_calls": idle_calls }),
                ),
            }
        }
        // The rows the TUI owns are appended at render time, so only the
        // agent's own accumulators appear here.
        Kind::Resources { rows, .. } => ("resources", json!({ "rows": rows })),
        // Rendering, prose, and interactive chrome — plus the pin register,
        // which is state that *is*, not a thing that happened.
        Kind::Card(_)
        | Kind::Phase(_)
        | Kind::Token(_)
        | Kind::Thinking(_)
        | Kind::Boundary
        | Kind::Reasoning { .. }
        | Kind::UserPromptEcho(_)
        | Kind::Pin { .. }
        | Kind::Unpin { .. } => return None,
    };
    let map = obj
        .as_object_mut()
        .expect("event_record arms are JSON objects");
    #[allow(
        clippy::cast_possible_truncation,
        reason = "elapsed-ms fits u64 for any real run"
    )]
    let t_ms_u64 = t_ms as u64;
    map.insert("t_ms".into(), json!(t_ms_u64));
    map.insert("id".into(), json!(id));
    map.insert("kind".into(), json!(name));
    Some(obj)
}
