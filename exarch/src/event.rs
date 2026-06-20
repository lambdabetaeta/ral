//! Per-session canonical event log.
//!
//! [`SessionLog`] is the single source of truth for one continuous agent
//! session.  It owns:
//!
//! - an in-memory `Vec<SessionEvent>` (used to render the next provider
//!   request and to track the protocol state machine),
//! - a pretty-printed `events.json` on disk (appended to as each event is
//!   recorded — survives the process so post-mortem tooling can replay
//!   the exact turn-by-turn exchange).
//!
//! The `events.json` file is the canonical "model view" artefact;
//! the `user.log` sibling — written by the TUI as a separate rendering
//! pass — is the canonical "user view".  Both files come from the same
//! source: the typed event stream below.
//!
//! ### On-disk format
//!
//! Each [`SessionEvent`] is serialised via [`serde_json::to_writer_pretty`]
//! and followed by a single `\n`.  The resulting file is a stream of
//! pretty-printed JSON objects separated by whitespace — fully human-
//! readable yet trivially parseable via
//! `serde_json::Deserializer::from_reader(f).into_iter::<SessionEvent>()`,
//! which skips whitespace between values.  Appending is just
//! `to_writer_pretty + "\n"`; no closing array bracket to maintain.

use crate::bus::SessionId;
use crate::provider::{ProviderError, Usage};
use genai::chat::{ChatMessage, ChatRole, ToolResponse};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// One tool result returned to the model.  This is the *model-facing*
/// content — the rendered, per-section-capped string — not the raw
/// stdout or stderr bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
}

/// A serialisable copy of [`crate::provider::Usage`].  We can't derive
/// `Serialize` on `Usage` itself without rippling the trait into the
/// provider module's public surface, so the event log holds its own
/// shape and converts in/out.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize)]
pub struct UsageDelta {
    pub input: u64,
    pub output: u64,
    pub cache_creation: Option<u64>,
    pub cache_read: Option<u64>,
    pub dollars: f64,
}

impl From<Usage> for UsageDelta {
    fn from(u: Usage) -> Self {
        Self {
            input: u.input,
            output: u.output,
            cache_creation: u.cache_creation,
            cache_read: u.cache_read,
            dollars: u.dollars,
        }
    }
}

/// Serialisable mirror of [`crate::provider::ProviderError`] for the
/// event log.  `ProviderError` carries `&'static str` and `Duration`
/// fields that don't round-trip through serde cleanly; this struct
/// flattens them to owned strings and second counts.  The on-screen
/// renderer reads from this shape, so events.json fully reconstructs
/// the rendered block.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ProviderErrorRecord {
    Cancelled {
        #[serde(rename = "where")]
        where_: String,
    },
    Transient {
        cause: String,
        attempts: u32,
        /// The parsed JSON error body the provider returned, mirrored from
        /// [`crate::provider::ProviderError`].  `default` lets old
        /// `events.json` files — written before this field existed —
        /// deserialise (missing field → `None`); `skip_serializing_if`
        /// keeps new logs lean by omitting it when absent.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<serde_json::Value>,
    },
    RateLimited {
        retry_after_secs: Option<u64>,
        cause: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<serde_json::Value>,
    },
    Api {
        status: Option<u16>,
        model: String,
        message: String,
        url: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        body: Option<serde_json::Value>,
    },
    Truncated {
        reason: String,
    },
    Other {
        cause: String,
    },
}

impl From<&ProviderError> for ProviderErrorRecord {
    fn from(e: &ProviderError) -> Self {
        match e {
            ProviderError::Cancelled(w) => Self::Cancelled {
                where_: (*w).to_string(),
            },
            ProviderError::Transient {
                cause,
                attempts,
                body,
            } => Self::Transient {
                cause: cause.clone(),
                attempts: *attempts,
                body: body.clone(),
            },
            ProviderError::RateLimited {
                retry_after,
                cause,
                body,
            } => Self::RateLimited {
                retry_after_secs: retry_after.map(|d| d.as_secs()),
                cause: cause.clone(),
                body: body.clone(),
            },
            ProviderError::Api {
                status,
                model,
                message,
                url,
                body,
            } => Self::Api {
                status: *status,
                model: model.clone(),
                message: message.clone(),
                url: url.clone(),
                body: body.clone(),
            },
            ProviderError::Truncated { reason } => Self::Truncated {
                reason: reason.clone(),
            },
            ProviderError::Other(s) => Self::Other { cause: s.clone() },
        }
    }
}

/// One protocol-level event.  The order of variants tracks the natural
/// order in a session: lifecycle bookends wrap a sequence of (prompt,
/// step, assistant, tool-results, …) repetitions plus the occasional
/// meta-event (usage, cancellation, compaction).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Bookend at the head of every session's event log.  Carries
    /// enough static metadata (model, provider, parent) that the file
    /// is self-describing without external context.
    SessionStarted {
        session_id: SessionId,
        parent: Option<SessionId>,
        model: String,
        provider: String,
        system_prompt_bytes: usize,
        log_dir: PathBuf,
    },
    /// A user-authored prompt — the seed / typed turn, a synthetic
    /// continuation injected by the top-level driver after a degenerate turn,
    /// or a steering interjection drained after tool results and before the
    /// next assistant reply.
    UserPrompt { text: String },
    /// Marks the start of one inner step of `Session::apply`.  Several
    /// steps may share a single user prompt when the model issues tool
    /// calls.
    StepStarted { n: u32 },
    /// The assistant's reply for this step.  Tool calls live inside
    /// `message.content` (genai's structured form); `pending_tool_ids`
    /// mirrors them out to the top level so the state machine can match
    /// them against the subsequent `ToolResults`.  `stop_reason` is the
    /// provider's normalised string (`"end_turn"`, `"max_tokens"`, …).
    AssistantMessage {
        message: ChatMessage,
        pending_tool_ids: Vec<String>,
        stop_reason: Option<String>,
    },
    /// All tool results for the most recent assistant turn, joined into
    /// one event.  IDs must match the `pending_tool_ids` of the
    /// preceding `AssistantMessage`.
    ToolResults { results: Vec<ToolResult> },
    /// Per-step token / dollar usage as reported by the provider.
    UsageDelta { usage: UsageDelta },
    /// The user pressed Ctrl-C / Esc mid-turn.  Whatever events were
    /// pending have been synthesised back to a `ReadyForUser` state by
    /// [`SessionLog::quiesce`].
    Cancelled,
    /// History was compacted.  The summary text becomes the body of the
    /// next `UserPrompt` (recorded separately) — this event is just the
    /// archival breadcrumb so post-mortem tooling can spot where the
    /// truncation happened.
    Compacted {
        before_bytes: usize,
        after_bytes: usize,
        summary: String,
    },
    /// User-facing error diagnostic — surfaced as a red `✗ error` block
    /// in the rendered log.  Not visible to the model: errors are about
    /// the orchestration, not the conversation.
    Error { text: String },
    /// Dim informational chrome — compaction progress, parenthetical
    /// notes, etc.  User-only, like [`Self::Error`].
    Dim { text: String },
    /// Recovery breadcrumb the top-level driver writes between
    /// iterations when nudging the model.  Recorded so events.json
    /// shows where the driver intervened.
    Nudge { used: u32, max: u32, cause: String },
    /// Structured provider-error block, written when a turn returns an
    /// error rather than an assistant message.  Recorded as a
    /// [`ProviderErrorRecord`] so the on-screen render is fully
    /// reconstructable from events.json.
    ProviderError { error: ProviderErrorRecord },
    /// Bookend at the tail.  Written when the owning [`SessionLog`] is
    /// dropped (root session) or when a forked child completes.
    SessionEnded,
}

impl SessionEvent {
    /// Project this event into a [`ChatMessage`], or `None` if the event
    /// is not part of the model-visible transcript (e.g. lifecycle, step,
    /// usage).  Tool result batches expand to one message per result.
    fn into_chat_messages(self) -> Vec<ChatMessage> {
        match self {
            SessionEvent::UserPrompt { text } => vec![ChatMessage::user(text)],
            // Pass the assistant message back whole — reasoning included.
            // genai owns the per-adapter reasoning policy: Anthropic drops
            // it, the OpenAI Responses adapter ignores it, but the OpenAI
            // chat-completions adapter (DeepSeek/Kimi reasoners) *requires*
            // `reasoning_content` echoed back across a tool-use sequence or
            // it 400s. Stripping it here would break those and buy nothing
            // elsewhere.
            SessionEvent::AssistantMessage { message, .. } => vec![message],
            SessionEvent::ToolResults { results } => results
                .into_iter()
                .map(|r| ChatMessage::from(ToolResponse::new(&r.id, &r.content)))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Transcript protocol phase.  Mirrors the prior `Transcript` state
/// machine — moved here unchanged so the role-alternation invariant is
/// still enforced at the event-log boundary.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    ReadyForUser,
    AwaitingAssistantAfterUser,
    AwaitingToolResults { pending_ids: Vec<String> },
    AwaitingAssistantAfterToolResults,
}

/// Why a turn is being wound back to `ReadyForUser` without a natural
/// assistant reply — selects the synthetic stub text recorded in the
/// transcript so a post-mortem reads the cause in place.
#[derive(Clone, Copy)]
pub enum QuiesceReason {
    /// The user pressed Ctrl-C / Esc mid-turn.
    Cancelled,
    /// The turn ended on a surfaced error — a transport failure or an
    /// exhausted retry budget — before the assistant replied.
    Aborted,
}

/// Per-session canonical log.  Holds the in-memory event vector, the
/// on-disk writer, the protocol state machine, and the static origin
/// metadata (model, provider, sessions root) needed to (re)record
/// `SessionStarted` on `clear` and to set up forked children — keyed
/// throughout to one [`SessionId`].
pub struct SessionLog {
    id: SessionId,
    dir: PathBuf,
    /// Append-only writer over `events.json`.  Held open so each
    /// `record` is a buffered append + flush — no reopen-per-event cost,
    /// and the file is always tail-able.
    events_file: BufWriter<File>,
    /// In-memory mirror of every event written to disk.  Reads back to
    /// render the next provider request and to drive the state machine;
    /// truncated by `/compact` and by `/clear`.
    events: Vec<SessionEvent>,
    state: State,
    /// Index *inside `events`* of the most recent `Compacted` event, if
    /// any.  Events at or before this index are excluded from the
    /// model-facing render — the summary takes their place via the
    /// `UserPrompt` that the compaction wrote right after.
    compact_cut: Option<usize>,
    /// Model identifier this log records.  Stored so `clear` can
    /// re-emit `SessionStarted` with the same value, and `fork`
    /// inherits it for the child's first event.
    model: String,
    /// Provider label.  Stored alongside [`Self::model`] for the same
    /// reason — the value is invariant across the life of one process.
    provider: String,
    /// `<scratch>/sessions/` — the root every forked child's dir lives
    /// under.  Stored so `fork` doesn't need to thread it through from
    /// the binary.
    sessions_root: PathBuf,
}

impl SessionLog {
    /// Build a fresh root session log under `sessions_root` (typically
    /// `<scratch>/sessions/`).  Wipes any prior directory with the same
    /// session id so the events file starts empty.
    pub fn root(
        sessions_root: &Path,
        session_id: SessionId,
        model: &str,
        provider: &str,
        system_prompt_bytes: usize,
    ) -> io::Result<Self> {
        let mut s = Self::open_fresh(
            sessions_root.to_path_buf(),
            session_id,
            model.to_string(),
            provider.to_string(),
        )?;
        s.record_started(None, system_prompt_bytes)?;
        Ok(s)
    }

    /// Build a forked child session log.  Inherits the parent's
    /// `sessions_root`, `model`, and `provider`, recording the parent
    /// id inside the child's `SessionStarted` event so the on-disk file
    /// is self-describing.
    pub fn fork(&self, child_id: SessionId, system_prompt_bytes: usize) -> io::Result<Self> {
        let mut s = Self::open_fresh(
            self.sessions_root.clone(),
            child_id,
            self.model.clone(),
            self.provider.clone(),
        )?;
        s.record_started(Some(self.id), system_prompt_bytes)?;
        Ok(s)
    }

    pub fn id(&self) -> SessionId {
        self.id
    }

    /// The per-session directory `<sessions_root>/<id>/`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether the session is at a settled turn boundary: the committed
    /// transcript is complete and a fresh user prompt is admissible.
    /// The invariant the driver relies on — every turn hands the session
    /// back here — and the precondition compaction needs.
    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::ReadyForUser)
    }

    pub fn can_compact(&self) -> bool {
        self.is_ready()
    }

    /// Approximate context size as serialised model-view message bytes.
    /// Drives the `compact_threshold` decision in
    /// [`crate::session::Session::compact`].
    pub fn history_bytes(&self) -> usize {
        self.model_messages()
            .iter()
            .map(|m| serde_json::to_string(m).map(|s| s.len()).unwrap_or(0))
            .sum()
    }

    /// Render the model-view messages for the next provider request.
    ///
    /// Valid only when the state machine is awaiting an assistant reply.
    pub fn render_messages(&self) -> Result<Vec<ChatMessage>, String> {
        if !matches!(
            self.state,
            State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults
        ) {
            return Err(format!(
                "cannot render request while session is in state {:?}",
                self.state
            ));
        }
        Ok(self.model_messages())
    }

    /// Render every committed message regardless of phase.  Used by
    /// compaction to feed the summariser.
    pub fn history_messages(&self) -> Vec<ChatMessage> {
        self.model_messages()
    }

    // ── Protocol mutations ────────────────────────────────────────────────

    pub fn append_user(&mut self, text: String) -> Result<(), String> {
        if !matches!(self.state, State::ReadyForUser) {
            return Err(format!(
                "tool results are pending; cannot accept new user prompt while session is in state {:?}",
                self.state
            ));
        }
        self.record_or_string_err(SessionEvent::UserPrompt { text })?;
        self.state = State::AwaitingAssistantAfterUser;
        Ok(())
    }

    /// Append a user steering message after a complete tool-result batch, before
    /// the next provider request.  This is the only mid-turn user ingress: the
    /// tool protocol is first brought back to
    /// `AwaitingAssistantAfterToolResults`, then the queued prompt becomes the
    /// next model-visible message.
    pub fn append_steering(&mut self, text: String) -> Result<(), String> {
        if !matches!(self.state, State::AwaitingAssistantAfterToolResults) {
            return Err(format!(
                "tool results must be complete before accepting a steering prompt; session is in state {:?}",
                self.state
            ));
        }
        self.record_or_string_err(SessionEvent::UserPrompt { text })?;
        self.state = State::AwaitingAssistantAfterUser;
        Ok(())
    }

    pub fn append_assistant(
        &mut self,
        message: ChatMessage,
        pending_tool_ids: Vec<String>,
        stop_reason: Option<String>,
    ) -> Result<(), String> {
        if !matches!(
            self.state,
            State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults
        ) {
            return Err(format!(
                "assistant message is not expected while session is in state {:?}",
                self.state
            ));
        }
        if message.role != ChatRole::Assistant {
            return Err(format!(
                "assistant message has role {:?}; expected Assistant",
                message.role,
            ));
        }
        let next = if pending_tool_ids.is_empty() {
            State::ReadyForUser
        } else {
            State::AwaitingToolResults {
                pending_ids: pending_tool_ids.clone(),
            }
        };
        self.record_or_string_err(SessionEvent::AssistantMessage {
            message,
            pending_tool_ids,
            stop_reason,
        })?;
        self.state = next;
        Ok(())
    }

    pub fn append_tool_results(&mut self, results: Vec<ToolResult>) -> Result<(), String> {
        let pending_ids = match &self.state {
            State::AwaitingToolResults { pending_ids } => pending_ids.clone(),
            _ => {
                return Err(format!(
                    "tool results are not expected while session is in state {:?}",
                    self.state
                ));
            }
        };
        validate_result_ids(&pending_ids, &results)?;
        self.record_or_string_err(SessionEvent::ToolResults { results })?;
        self.state = State::AwaitingAssistantAfterToolResults;
        Ok(())
    }

    /// Drive the session back to `ReadyForUser`, synthesising whatever
    /// tool-result and assistant events the role-alternation invariant
    /// needs.  Called whenever a turn ends without a natural assistant
    /// reply — a user cancellation or a surfaced error.  `reason`
    /// selects the synthetic stub text and the optional breadcrumb.
    pub fn quiesce(&mut self, reason: QuiesceReason) {
        let already_ready = self.is_ready();
        let (tool_stub, assistant_stub, stop_label) = match reason {
            QuiesceReason::Cancelled => (
                "cancelled before tool execution",
                "(cancelled by user)",
                "cancelled",
            ),
            QuiesceReason::Aborted => (
                "no result: turn aborted before tool execution",
                "(no reply: turn aborted)",
                "aborted",
            ),
        };
        loop {
            match &self.state {
                State::ReadyForUser => break,
                State::AwaitingToolResults { pending_ids } => {
                    let results = pending_ids
                        .iter()
                        .map(|id| ToolResult {
                            id: id.clone(),
                            content: tool_stub.into(),
                        })
                        .collect();
                    self.append_tool_results(results)
                        .expect("synthetic results match pending ids");
                }
                State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults => {
                    let msg = ChatMessage::assistant(assistant_stub);
                    self.append_assistant(msg, vec![], Some(stop_label.into()))
                        .expect("stub assistant is valid here");
                }
            }
        }
        // Only a cancellation gets a dedicated breadcrumb; an abort's
        // marker is the `ProviderError` already on disk.  Best-effort: a
        // failed flush mustn't poison the already-quiesced state.
        if !already_ready && matches!(reason, QuiesceReason::Cancelled) {
            let _ = self.record(SessionEvent::Cancelled);
        }
    }

    /// Replace prior history with a compact summary.  Records the
    /// `Compacted` breadcrumb on disk, then a `UserPrompt` carrying the
    /// summary, and shifts the model-view cutoff so future renders skip
    /// everything before.
    pub fn replace_with_summary(&mut self, summary: String) -> Result<(), String> {
        if !self.can_compact() {
            return Err("cannot compact while tool results are pending".into());
        }
        let before_bytes = self.history_bytes();
        self.record_or_string_err(SessionEvent::Compacted {
            before_bytes,
            // `after_bytes` is computed by `history_bytes` *after* the
            // summary prompt is in, so the on-disk record is faithful;
            // we patch it in just below.
            after_bytes: 0,
            summary: summary.clone(),
        })?;
        // Future model-render only sees events strictly after this one.
        self.compact_cut = Some(self.events.len() - 1);
        self.record_or_string_err(SessionEvent::UserPrompt {
            text: format!("Summary of prior work in this session:\n\n{summary}"),
        })?;
        self.state = State::ReadyForUser;
        // Now patch the recorded `Compacted.after_bytes`.  Disk already
        // shows `0` — we accept that minor staleness because rewriting
        // a pretty-printed JSON object in place is not worth the
        // complexity; the in-memory accumulator stays accurate.
        let after = self.history_bytes();
        if let Some(SessionEvent::Compacted { after_bytes, .. }) =
            self.events.get_mut(self.compact_cut.unwrap())
        {
            *after_bytes = after;
        }
        Ok(())
    }

    /// `/clear`: wipe in-memory and on-disk events, restart with a
    /// fresh `SessionStarted` so the log file is internally consistent.
    /// `parent` is dropped — a cleared session is rooted again — but
    /// the model/provider metadata is preserved across the wipe.
    pub fn clear(&mut self, system_prompt_bytes: usize) -> io::Result<()> {
        self.events.clear();
        self.state = State::ReadyForUser;
        self.compact_cut = None;
        // Truncate the events file in place.
        self.events_file = open_events_file(&self.dir)?;
        self.record_started(None, system_prompt_bytes)
    }

    // ── Meta-events ───────────────────────────────────────────────────────

    pub fn record_step(&mut self, n: u32) -> io::Result<()> {
        self.record(SessionEvent::StepStarted { n })
    }

    pub fn record_usage(&mut self, usage: UsageDelta) -> io::Result<()> {
        self.record(SessionEvent::UsageDelta { usage })
    }

    pub fn record_session_ended(&mut self) -> io::Result<()> {
        self.record(SessionEvent::SessionEnded)
    }

    pub fn record_error(&mut self, text: String) -> io::Result<()> {
        self.record(SessionEvent::Error { text })
    }

    pub fn record_dim(&mut self, text: String) -> io::Result<()> {
        self.record(SessionEvent::Dim { text })
    }

    pub fn record_nudge(&mut self, used: u32, max: u32, cause: String) -> io::Result<()> {
        self.record(SessionEvent::Nudge { used, max, cause })
    }

    pub fn record_provider_error(&mut self, e: &ProviderError) -> io::Result<()> {
        self.record(SessionEvent::ProviderError { error: e.into() })
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:session-dir] (re)creates the session log dir; event-log infra, not turn-time data I/O"
    )]
    fn open_fresh(
        sessions_root: PathBuf,
        session_id: SessionId,
        model: String,
        provider: String,
    ) -> io::Result<Self> {
        let dir = sessions_root.join(session_id.to_string());
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        let events_file = open_events_file(&dir)?;
        Ok(Self {
            id: session_id,
            dir,
            events_file,
            events: Vec::new(),
            state: State::ReadyForUser,
            compact_cut: None,
            model,
            provider,
            sessions_root,
        })
    }

    /// Record the `SessionStarted` bookend with this log's stored
    /// origin metadata.  Used by `root`, `fork`, and `clear`.
    fn record_started(
        &mut self,
        parent: Option<SessionId>,
        system_prompt_bytes: usize,
    ) -> io::Result<()> {
        let ev = SessionEvent::SessionStarted {
            session_id: self.id,
            parent,
            model: self.model.clone(),
            provider: self.provider.clone(),
            system_prompt_bytes,
            log_dir: self.dir.clone(),
        };
        self.record(ev)
    }

    fn record(&mut self, ev: SessionEvent) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut self.events_file, &ev).map_err(io::Error::other)?;
        self.events_file.write_all(b"\n")?;
        self.events_file.flush()?;
        self.events.push(ev);
        Ok(())
    }

    /// `record` lifted into `Result<(), String>` for the protocol-mutation
    /// methods, which already plumb string errors back to the caller.
    fn record_or_string_err(&mut self, ev: SessionEvent) -> Result<(), String> {
        self.record(ev).map_err(|e| e.to_string())
    }

    /// Slice of events visible to the model — drops everything at or
    /// before the most recent `Compacted` cut, then keeps the rest in
    /// arrival order.
    fn model_events(&self) -> &[SessionEvent] {
        match self.compact_cut {
            Some(i) => &self.events[i + 1..],
            None => &self.events,
        }
    }

    /// Project model events into the `Vec<ChatMessage>` shape genai
    /// wants.  Lifecycle / step / usage events drop out here.
    fn model_messages(&self) -> Vec<ChatMessage> {
        self.model_events()
            .iter()
            .flat_map(|e| e.clone().into_chat_messages())
            .collect()
    }
}

/// Always-open `events.json` writer.  Truncate-on-open — callers use
/// this from constructors and `clear`, both of which want a fresh file.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:events-file] opens events.json for the session event log; infra, not turn-time data I/O"
)]
fn open_events_file(dir: &Path) -> io::Result<BufWriter<File>> {
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join("events.json"))?;
    Ok(BufWriter::new(f))
}

fn validate_result_ids(pending_ids: &[String], results: &[ToolResult]) -> Result<(), String> {
    if pending_ids.len() != results.len() {
        return Err(format!(
            "tool result count mismatch: expected {}, got {}",
            pending_ids.len(),
            results.len()
        ));
    }
    let expected: HashSet<&str> = pending_ids.iter().map(String::as_str).collect();
    let mut seen: HashSet<&str> = HashSet::with_capacity(results.len());
    for r in results {
        if !expected.contains(r.id.as_str()) {
            return Err(format!("unknown tool result id {}", r.id));
        }
        if !seen.insert(r.id.as_str()) {
            return Err(format!("duplicate tool result id {}", r.id));
        }
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "[io-door:test] test fs/process scaffolding")]
mod tests {
    use super::*;
    use genai::chat::{ContentPart, ToolCall};

    /// Fresh `sessions/` root in a test-unique tempdir.  Each test gets
    /// its own so concurrent runs don't share state.
    fn sessions_root(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("exarch-event-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn fresh_root(tag: &str) -> SessionLog {
        let root = sessions_root(tag);
        SessionLog::root(&root, 0, "model", "provider", 0).expect("log")
    }

    fn assistant_with_tool(id: &str) -> ChatMessage {
        ChatMessage::assistant(vec![ContentPart::ToolCall(ToolCall {
            call_id: id.into(),
            fn_name: "ral".into(),
            fn_arguments: serde_json::json!({"cmd": "pwd"}),
            thought_signatures: None,
        })])
    }

    #[test]
    fn rejects_user_while_tool_results_pending() {
        let mut s = fresh_root("user-while-pending");
        s.append_user("p".into()).unwrap();
        s.append_assistant(assistant_with_tool("a"), vec!["a".into()], None)
            .unwrap();
        let err = s.append_user("next".into()).unwrap_err();
        assert!(err.contains("tool results are pending"));
    }

    #[test]
    fn validates_tool_result_ids() {
        let mut s = fresh_root("validate-ids");
        s.append_user("p".into()).unwrap();
        s.append_assistant(assistant_with_tool("a"), vec!["a".into()], None)
            .unwrap();
        let err = s
            .append_tool_results(vec![ToolResult {
                id: "b".into(),
                content: "x".into(),
            }])
            .unwrap_err();
        assert!(err.contains("unknown tool result id"));
    }

    #[test]
    fn renders_tool_results_as_chat_messages() {
        let mut s = fresh_root("render-results");
        s.append_user("p".into()).unwrap();
        s.append_assistant(assistant_with_tool("t1"), vec!["t1".into()], None)
            .unwrap();
        s.append_tool_results(vec![ToolResult {
            id: "t1".into(),
            content: "ok".into(),
        }])
        .unwrap();
        let ms = s.render_messages().unwrap();
        let last = ms.last().unwrap();
        assert_eq!(last.role, ChatRole::Tool);
    }

    #[test]
    fn rejects_compaction_while_tools_pending() {
        let mut s = fresh_root("compact-pending");
        s.append_user("p".into()).unwrap();
        s.append_assistant(assistant_with_tool("a"), vec!["a".into()], None)
            .unwrap();
        let err = s.replace_with_summary("summary".into()).unwrap_err();
        assert!(err.contains("cannot compact"));
    }

    #[test]
    fn compaction_truncates_model_view_but_preserves_disk_log() {
        let mut s = fresh_root("compact-view");
        s.append_user("first".into()).unwrap();
        s.append_assistant(ChatMessage::assistant("done"), vec![], None)
            .unwrap();
        s.replace_with_summary("did stuff".into()).unwrap();
        // Compaction lands in `ReadyForUser`; mimic the REPL by
        // priming a fresh user prompt before asking what the model
        // sees on the next turn.
        s.append_user("next".into()).unwrap();
        let ms = s.render_messages().unwrap();
        // Model view skips the pre-compaction history: just the
        // summary prompt plus the new user message.
        assert_eq!(ms.len(), 2);
        assert!(matches!(ms[0].role, ChatRole::User));
        assert!(matches!(ms[1].role, ChatRole::User));
        // On-disk events still include the pre-compaction history.
        let contents = fs::read_to_string(s.dir().join("events.json")).unwrap();
        assert!(contents.contains("first"));
        assert!(contents.contains("compacted"));
    }

    #[test]
    fn quiesce_after_user_admits_next_prompt() {
        let mut s = fresh_root("quiesce-user");
        s.append_user("p".into()).unwrap();
        s.quiesce(QuiesceReason::Cancelled);
        s.append_user("next".into()).unwrap();
    }

    #[test]
    fn quiesce_during_tool_results_admits_next_prompt() {
        let mut s = fresh_root("quiesce-tools");
        s.append_user("p".into()).unwrap();
        s.append_assistant(assistant_with_tool("a"), vec!["a".into()], None)
            .unwrap();
        s.quiesce(QuiesceReason::Cancelled);
        s.append_user("next".into()).unwrap();
    }

    #[test]
    fn quiesce_after_tool_results_admits_next_prompt() {
        let mut s = fresh_root("quiesce-after");
        s.append_user("p".into()).unwrap();
        s.append_assistant(assistant_with_tool("a"), vec!["a".into()], None)
            .unwrap();
        s.append_tool_results(vec![ToolResult {
            id: "a".into(),
            content: "ok".into(),
        }])
        .unwrap();
        s.quiesce(QuiesceReason::Cancelled);
        s.append_user("next".into()).unwrap();
    }

    #[test]
    fn quiesce_when_ready_is_a_noop() {
        let mut s = fresh_root("quiesce-noop");
        s.quiesce(QuiesceReason::Cancelled);
        // No `cancelled` breadcrumb when nothing was pending.
        assert!(
            !s.events
                .iter()
                .any(|e| matches!(e, SessionEvent::Cancelled))
        );
    }

    /// A turn that ends on a surfaced error — not a cancel — must still
    /// wind the committed-but-unanswered prompt back to `ReadyForUser`
    /// so the next prompt is admitted.  Regression for the
    /// transport-failure wedge: a stranded `AwaitingAssistantAfterUser`
    /// rejected every subsequent prompt until `/clear`.
    #[test]
    fn quiesce_after_abort_admits_next_prompt() {
        let mut s = fresh_root("quiesce-abort");
        s.append_user("p".into()).unwrap();
        // No assistant reply arrives — the provider call errored out.
        assert!(!s.is_ready());
        s.quiesce(QuiesceReason::Aborted);
        assert!(s.is_ready());
        s.append_user("next".into()).unwrap();
        // An abort writes no `Cancelled` breadcrumb — the surfaced
        // `ProviderError` is the on-disk marker.
        assert!(
            !s.events
                .iter()
                .any(|e| matches!(e, SessionEvent::Cancelled))
        );
    }

    #[test]
    fn events_json_is_appended_pretty_per_event() {
        let mut s = fresh_root("pretty");
        s.append_user("hi".into()).unwrap();
        let body = fs::read_to_string(s.dir().join("events.json")).unwrap();
        // Pretty printing puts the `"kind"` field on its own indented line.
        assert!(body.contains("\n  \"kind\":"));
        // Round-trip parse must yield the same set of events as the log
        // (session_started + user_prompt = 2 here).
        let parsed: Vec<SessionEvent> = serde_json::Deserializer::from_str(&body)
            .into_iter::<SessionEvent>()
            .collect::<Result<_, _>>()
            .expect("round-trip");
        assert_eq!(parsed.len(), 2);
        assert!(matches!(parsed[0], SessionEvent::SessionStarted { .. }));
        assert!(matches!(parsed[1], SessionEvent::UserPrompt { .. }));
    }

    /// Diagnostic chrome must round-trip through `events.json` as
    /// typed events — the property the typed-chrome refactor was
    /// added to satisfy.  Replaying `events.json` (the canonical
    /// "model view" file) now also captures everything the user saw
    /// chrome-side, so `user.log` is in principle reconstructable
    /// from `events.json` alone.
    #[test]
    fn diagnostic_events_round_trip_through_disk() {
        let mut s = fresh_root("diagnostic-roundtrip");
        s.record_error("boom".into()).unwrap();
        s.record_dim("[compacting]".into()).unwrap();
        s.record_nudge(2, 3, "stop=length".into()).unwrap();
        let body = fs::read_to_string(s.dir().join("events.json")).unwrap();
        let parsed: Vec<SessionEvent> = serde_json::Deserializer::from_str(&body)
            .into_iter::<SessionEvent>()
            .collect::<Result<_, _>>()
            .expect("round-trip");
        // session_started + 3 diagnostics = 4 events.
        assert_eq!(parsed.len(), 4);
        assert!(matches!(&parsed[1], SessionEvent::Error { text } if text == "boom"));
        assert!(matches!(&parsed[2], SessionEvent::Dim { text } if text == "[compacting]"));
        assert!(matches!(
            &parsed[3],
            SessionEvent::Nudge { used: 2, max: 3, cause } if cause == "stop=length"
        ));
    }
}
