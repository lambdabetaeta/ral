//! Per-session operational trace.
//!
//! Every [`Agent`](crate::agent::Agent) — the root and each forked
//! child, in both the TUI and headless — owns a [`Transcript`]: a
//! `transcript.jsonl` in the session's log dir, one JSON object per bus event.
//! It is the *operational* view — what the agent did: every tool call and its
//! result, step, usage delta, structural I/O effect, stop reason, error, and
//! sub-agent boundary — the sibling of the model-view `events.json` the
//! [`AgentLog`](crate::agent::event::AgentLog) keeps.
//!
//! Pure rendering events — a composed [`Card`](crate::card::Card), the card a
//! [`Kind::Io`] draws, a [`Kind::Phase`] progress label — are deliberately
//! *not* recorded here; they are a presentation the TUI captures in its
//! `user.log`, not an effect. The exhaustive match in [`event_record`] means a
//! new [`Kind`] variant cannot silently fall out of the trace.
//!
//! The writer is fed at the emit seam ([`Emitter::emit`](crate::bus::Emitter)),
//! independent of who drains the live bus for display. A headless child muted
//! from the screen still records its full trace, because recording is a
//! property of the session, not of the frontend.

use crate::bus::{AgentId, Kind};
use std::fs::File;
use std::io::{BufWriter, Write};
use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Instant;

/// A cheap, cloneable handle to one session's `transcript.jsonl` writer.
///
/// Clones share the same file — the token-stream emitter clone, a derived
/// child emitter that re-uses the parent's id — so a session writes through a
/// single handle whoever holds it. A *forked* session is given its own
/// [`Transcript`]. [`Transcript::none`] is the no-op handle tests and
/// dir-less emitters carry; it records nothing.
#[derive(Clone)]
pub struct Transcript(Option<Arc<Inner>>);

struct Inner {
    /// Buffered, flushed per record so the file is always tail-able — the
    /// same discipline `events.json` keeps.
    file: Mutex<BufWriter<File>>,
    /// Monotonic origin for the `t_ms` offset stamped on each record.
    started: Instant,
}

impl Transcript {
    /// Open `path` for this session's trace, truncating any prior file.
    ///
    /// # Errors
    /// Returns `Err` if creating (truncating) the transcript file fails.
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

    /// Record one bus event as a JSONL line, projected to its operational
    /// shape by [`event_record`]. Rendering-only events project to `None` and
    /// are skipped before the lock is ever taken, so the token stream — the
    /// one high-frequency `Kind` — costs nothing here.
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

/// Project one bus event into its `transcript.jsonl` record, or `None` for the
/// variants the operational trace deliberately omits: the rendering-only
/// [`Kind::Card`] / [`Kind::Phase`], the assistant [`Kind::Token`],
/// [`Kind::Thinking`], and [`Kind::Reasoning`] streams (prose and deliberation
/// belong to the model view), and the interactive-only chrome
/// ([`Kind::Boundary`], [`Kind::UserPromptEcho`], the pin register). The
/// exhaustive match means a new [`Kind`] won't silently fall out of the trace.
pub(crate) fn event_record(t_ms: u128, id: AgentId, kind: &Kind) -> Option<serde_json::Value> {
    use serde_json::json;
    let (name, mut obj) = match kind {
        Kind::Born {
            log_dir,
            title,
            parent,
            branch,
        } => (
            "born",
            json!({ "log_dir": log_dir.to_string_lossy(), "title": title, "parent": parent, "branch": branch }),
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
        // A desk verb's `HarnessCall`/`HarnessResult` records identically to
        // a `ral` `ToolCall`/`ToolResult` — the operational trace keeps
        // showing every acting call the same way; only the live `Kind`
        // vocabulary distinguishes a genuine provider-boundary tool call
        // from a harness verb that never crossed it.
        Kind::ToolCall { tool, cmd, summary } | Kind::HarnessCall { verb: tool, cmd, summary } => (
            "tool_call",
            json!({ "tool": tool, "cmd": cmd, "summary": summary }),
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
            title,
            outcome,
            text,
            elapsed,
        } => {
            let (text, error) = outcome.breadcrumb(text);
            #[allow(clippy::cast_possible_truncation, reason="elapsed-ms fits u64 for any real run")]
            let elapsed_ms = elapsed.as_millis() as u64;
            (
                "subagent_done",
                json!({
                    "title": title,
                    "text": text,
                    "error": error,
                    "elapsed_ms": elapsed_ms,
                }),
            )
        }
        // Io, Done, Notice, and Resources each carry a raw fact beside the
        // `card` the TUI renders from it (bus.rs documents each pairing); the
        // trace keeps the fact — structural read/write/exec/grep shape, worker
        // settlement, ready-boundary housekeeping, `/resources` rows — never
        // the rendering, which lives only in the TUI's `user.log`.
        Kind::Io { event, .. } => ("io", json!({ "event": event })),
        Kind::Done { outcome, .. } => {
            use crate::card::DoneOutcome;
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
            use crate::card::Notice;
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
                Notice::LargeBinding { name, bytes } => {
                    ("large_binding", json!({ "name": name, "bytes": bytes }))
                }
            }
        }
        // The frontend's own rows are appended at render time and stay
        // presentation, so they are deliberately absent from these raw rows.
        Kind::Resources { rows, .. } => ("resources", json!({ "rows": rows })),
        // Rendering- and presentation-only: a composed card, a progress phase,
        // the assistant token stream, the interactive-only live lines, and the
        // pin register (state that *is*, not a thing that happened).
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
    #[allow(clippy::cast_possible_truncation, reason="elapsed-ms fits u64 for any real run")]
    let t_ms_u64 = t_ms as u64;
    map.insert("t_ms".into(), json!(t_ms_u64));
    map.insert("id".into(), json!(id));
    map.insert("kind".into(), json!(name));
    Some(obj)
}
