//! Per-session operational trace: a `transcript.jsonl` in the session's log
//! dir, one JSON object per bus event.
//!
//! What the agent *did* — every tool call and result, step, usage delta,
//! structural I/O effect, stop reason, error, sub-agent boundary — sibling to
//! the model-view `events.jsonl` that `AgentLog` keeps. A rendering is not an
//! effect, so composed cards and progress labels stay out; they are the TUI's
//! `user.log`. The writer is fed at the emit seam (`Emitter::emit`), not by
//! whoever drains the live bus, so a child muted off the display still records
//! its full trace.

use crate::agent::event::EditReceipt;
use crate::bus::card::observation_json;
use crate::bus::{AgentId, Emitter, Kind};
use std::fs::{File, OpenOptions};
use std::io::{self, BufWriter, Write};
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
    file: Mutex<Option<BufWriter<File>>>,
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
            file: Mutex::new(Some(BufWriter::new(file))),
            started: Instant::now(),
        }))))
    }

    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:transcript-file] appends a resume marker to the session trace"
    )]
    /// Open the trace without truncating it and write the resume marker first.
    ///
    /// # Errors
    /// Returns an error if the trace cannot be opened or the marker cannot be
    /// written.
    pub fn open_append(path: &Path, at_unix_ms: u64) -> io::Result<Self> {
        let file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(path)?;
        let transcript = Self(Some(Arc::new(Inner {
            file: Mutex::new(Some(BufWriter::new(file))),
            started: Instant::now(),
        })));
        transcript.write_marker("resumed", at_unix_ms)?;
        Ok(transcript)
    }

    /// A trace that records nothing — tests, and any emitter with no log dir.
    pub fn none() -> Self {
        Self(None)
    }

    /// Rotate the shared trace to its first free numbered sibling.
    ///
    /// # Errors
    /// Returns an error if the old trace cannot be sealed, renamed, or
    /// replaced.
    pub fn rotate(&self, path: &Path) -> io::Result<()> {
        let n = first_free_rotation(path)?;
        self.rotate_at(path, n, crate::bootstrap::now_unix_ms())
    }

    pub(crate) fn rotate_at(&self, path: &Path, n: u64, at_unix_ms: u64) -> io::Result<()> {
        let Some(inner) = &self.0 else {
            return Ok(());
        };
        let mut file = inner
            .file
            .lock()
            .map_err(|_| io::Error::other("transcript writer lock poisoned"))?;
        let result = (|| {
            let Some(writer) = file.as_mut() else {
                return Err(io::Error::other(
                    "the transcript writer is already dead; no further records can be rotated",
                ));
            };
            writer.flush()?;
            drop(file.take());
            let rotated = rotation_path(path, n);
            std::fs::rename(path, &rotated)?;
            let replacement = File::create(path)?;
            let mut replacement = BufWriter::new(replacement);
            write_marker_to(&mut replacement, "cleared", at_unix_ms)?;
            replacement.flush()?;
            *file = Some(replacement);
            Ok(())
        })();
        if result.is_err() {
            *file = None;
        }
        result
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
        if let Ok(mut file) = inner.file.lock()
            && let Some(file) = file.as_mut()
        {
            let _ = writeln!(file, "{line}");
            let _ = file.flush();
        }
    }

    fn write_marker(&self, kind: &str, at_unix_ms: u64) -> io::Result<()> {
        let Some(inner) = &self.0 else {
            return Ok(());
        };
        let mut file = inner
            .file
            .lock()
            .map_err(|_| io::Error::other("transcript writer lock poisoned"))?;
        let result = {
            let Some(file) = file.as_mut() else {
                return Err(io::Error::other("the transcript writer is not writable"));
            };
            write_marker_to(file, kind, at_unix_ms)?;
            file.flush()
        };
        drop(file);
        result
    }
}

fn write_marker_to(file: &mut BufWriter<File>, kind: &str, at_unix_ms: u64) -> io::Result<()> {
    let marker = serde_json::json!({
        "kind": kind,
        "at_unix_ms": at_unix_ms,
    });
    serde_json::to_writer(&mut *file, &marker).map_err(io::Error::other)?;
    file.write_all(b"\n")
}

fn first_free_rotation(path: &Path) -> io::Result<u64> {
    let mut n = 0;
    loop {
        if !rotation_path(path, n).exists() {
            return Ok(n);
        }
        n = n
            .checked_add(1)
            .ok_or_else(|| io::Error::other("no free transcript rotation number remains"))?;
    }
}

fn rotation_path(path: &Path, n: u64) -> std::path::PathBuf {
    let name = path
        .file_name()
        .map_or_else(|| "record".into(), |name| name.to_string_lossy().into_owned());
    path.with_file_name(format!("{name}.{n}"))
}

pub(crate) fn emit_context_edited(emit: &Emitter, receipt: &EditReceipt) {
    emit.emit(Kind::ContextEdited {
        op: receipt.op.clone(),
        by: receipt.by.clone(),
    });
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
        Kind::ContextEdited { op, by } => ("context_edited", json!({ "op": op, "by": by })),
        Kind::Nudge { used, max, cause } => {
            ("nudge", json!({ "used": used, "max": max, "cause": cause }))
        }
        Kind::ProviderError(error) => ("provider_error", json!({ "error": error })),
        Kind::Stalled(error) => ("stalled", json!({ "error": error })),
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
        Kind::Io { event, .. } => ("io", json!({ "event": observation_json(event) })),
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
        Kind::Context { rows, .. } => ("context", json!({ "rows": rows })),
        // Rendering, prose, and interactive chrome — plus the pin register,
        // which is state that *is*, not a thing that happened.
        Kind::Card(_)
        | Kind::State(_)
        | Kind::Token(_)
        | Kind::Thinking(_)
        | Kind::Boundary
        | Kind::Cleared
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::event::{ContextOp, EditAuthority};

    #[test]
    fn context_edit_transcript_records_each_authority() {
        for by in [
            EditAuthority::Model,
            EditAuthority::User,
            EditAuthority::Harness,
        ] {
            let op = ContextOp::Drop { exchanges: vec![7] };
            let kind = Kind::ContextEdited {
                op: op.clone(),
                by: by.clone(),
            };
            let record = event_record(0, 42, &kind).expect("context edits are operational");
            assert_eq!(record["kind"], "context_edited");
            assert_eq!(record["by"], serde_json::to_value(&by).unwrap());
            assert_eq!(record["op"], serde_json::to_value(op).unwrap());
        }
    }
}
