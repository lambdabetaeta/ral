//! Per-session operational trace.
//!
//! Every [`Agent`](crate::agent::Agent) — the root and each forked
//! child, in both the TUI and headless — owns a [`Transcript`]: a
//! `transcript.jsonl` in the session's log dir, one JSON object per bus event.
//! It is the *operational* view — what the agent did: every tool call and its
//! result, step, usage delta, structural I/O effect, stop reason, error, and
//! sub-agent boundary — the sibling of the model-view `events.json` the
//! [`AgentLog`](crate::event::AgentLog) keeps.
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
/// [`Kind::Card`] / [`Kind::Phase`], the assistant [`Kind::Token`] stream
/// (prose belongs to the model view), and the interactive-only chrome
/// ([`Kind::Boundary`], [`Kind::UserPromptEcho`], the pin register). The
/// exhaustive match means a new [`Kind`] won't silently fall out of the trace.
pub(crate) fn event_record(t_ms: u128, id: AgentId, kind: &Kind) -> Option<serde_json::Value> {
    use serde_json::json;
    let (name, mut obj) = match kind {
        Kind::Born {
            log_dir,
            title,
            parent,
        } => (
            "born",
            json!({ "log_dir": log_dir.to_string_lossy(), "title": title, "parent": parent }),
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
        Kind::ToolResult(text) => ("tool_result", json!({ "text": text })),
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
        // The raw structural effect a card erases — kept so a post-mortem reads
        // the read/write/exec/grep shape. The card composed from it is a
        // rendering and lives in the TUI's `user.log`, never here.
        Kind::Io { event, .. } => ("io", json!({ "event": event })),
        // The lease chain's raw reap fact — which worker, spelled how, and
        // which lease fired — kept beside the one-liner `card` the same way
        // `Kind::Io` keeps its structural `event` beside its rendering.
        Kind::WorkerReaped { cmd, cause, .. } => {
            let cause = match cause {
                ral_core::types::ReapCause::Idle => "idle",
                ral_core::types::ReapCause::Backstop => "backstop",
            };
            ("worker_reaped", json!({ "cmd": cmd, "cause": cause }))
        }
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
    map.insert("t_ms".into(), json!(t_ms as u64));
    map.insert("id".into(), json!(id));
    map.insert("kind".into(), json!(name));
    Some(obj)
}
