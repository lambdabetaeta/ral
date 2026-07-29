//! Per-session canonical event log: one session's event vector and the
//! `events.json` it is appended to.
//!
//! This is the model view; `tui::viewport` writes the rendered `user.log`
//! beside it, `agent::transcript` the operational `transcript.jsonl`.
//!
//! `events.json` is a whitespace-separated stream of pretty-printed JSON
//! values — no array, no line per event — so appending closes no bracket and
//! `serde_json::Deserializer::from_reader(f).into_iter::<SessionEvent>()`
//! reads it back.

use crate::bus::AgentId;
use crate::provider::{ProviderError, Tuning, Usage};
use genai::chat::{ChatMessage, ChatRole, ToolResponse};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Write};
use std::path::{Path, PathBuf};

/// One tool result as the model sees it: the rendered, per-section-capped
/// string, never the raw stdout or stderr bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
}

/// Serialisable stand-in for [`Usage`], which derives no serde traits; the log
/// converts in and out rather than ripple the derive through the provider's
/// public surface.
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

/// Serialisable mirror of [`ProviderError`], flattening its `&'static str` and
/// `Duration` fields to owned strings and whole seconds.
///
/// `tui::line` renders from this shape, so `events.json` reconstructs the
/// on-screen block.
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
                body: body.as_deref().cloned(),
            },
            ProviderError::RateLimited {
                retry_after,
                cause,
                body,
            } => Self::RateLimited {
                retry_after_secs: retry_after.map(|d| d.as_secs()),
                cause: cause.clone(),
                body: body.as_deref().cloned(),
            },
            ProviderError::Api {
                status,
                model,
                message,
                body,
            } => Self::Api {
                status: *status,
                model: model.clone(),
                message: message.clone(),
                body: body.as_deref().cloned(),
            },
            ProviderError::Truncated { reason } => Self::Truncated {
                reason: reason.clone(),
            },
            ProviderError::Other(s) => Self::Other { cause: s.clone() },
        }
    }
}

/// One protocol-level event, the variants in the order a session produces them.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum SessionEvent {
    /// Head bookend, carrying enough metadata that the file reads on its own.
    SessionStarted {
        session_id: AgentId,
        parent: Option<AgentId>,
        model: String,
        provider: String,
        system_prompt_bytes: usize,
        log_dir: PathBuf,
    },
    /// A user-role turn: the seed or typed prompt, a nudge continuation the
    /// attend loop self-posts, or a steering message drained after a tool batch.
    UserPrompt {
        text: String,
    },
    /// A message a `mnemon` child inherits from its parent's context.  Replayed
    /// to the provider verbatim but drives no state transition — the child's own
    /// [`Self::UserPrompt`] is still the turn it must answer.
    ContextMessage {
        message: ChatMessage,
    },
    /// One inner step of [`crate::agent::Agent::deliberate`]; several share a
    /// user prompt when the model calls tools.
    StepStarted {
        n: u32,
        tuning: Tuning,
    },
    /// The assistant's reply for this step.  Tool calls live inside
    /// `message.content`; `pending_tool_ids` mirrors them out so the state
    /// machine can match them against the `ToolResults` that follow.
    AssistantMessage {
        message: ChatMessage,
        pending_tool_ids: Vec<String>,
        stop_reason: Option<String>,
    },
    /// Every result for the preceding [`Self::AssistantMessage`], joined; the
    /// ids must match its `pending_tool_ids`.
    ToolResults {
        results: Vec<ToolResult>,
    },
    UsageDelta {
        usage: UsageDelta,
    },
    /// Ctrl-C or Esc mid-exchange; [`AgentLog::quiesce`] has already synthesised
    /// whatever the transcript needed to reach [`State::ReadyForUser`].
    Cancelled,
    /// Archival breadcrumb for a compaction.  The live model view is
    /// [`AgentLog::compaction`] plus the kept suffix, not this.
    Compacted {
        before_bytes: usize,
        after_bytes: usize,
        summary: String,
    },
    /// A diagnostic for the user alone — about the orchestration, not the
    /// conversation, so it never reaches the model.
    Error {
        text: String,
    },
    /// Breadcrumb for a nudge the attend loop posted between iterations.
    Nudge {
        used: u32,
        max: u32,
        cause: String,
    },
    /// An exchange that returned an error instead of an assistant message.
    ProviderError {
        error: ProviderErrorRecord,
    },
    /// A stream that broke after part of the turn had already reached the user.
    /// The [`Self::AssistantMessage`] just before it holds the salvaged prefix,
    /// and the turn resumes rather than ending — which is the whole reason this
    /// is not a [`Self::ProviderError`].
    Stalled {
        error: ProviderErrorRecord,
    },
    /// Tail bookend, written by `Agent`'s [`Drop`] impl — the one funnel every
    /// teardown path runs through.
    SessionEnded,
}

impl SessionEvent {
    /// The model-visible messages for this event — empty for the lifecycle,
    /// step, and usage events; one per result for a tool batch.
    fn into_chat_messages(self) -> Vec<ChatMessage> {
        match self {
            Self::UserPrompt { text } => vec![ChatMessage::user(text)],
            Self::ContextMessage { message }
            // Whole, reasoning included: genai's OpenAI chat-completions
            // adapter (the DeepSeek/Kimi reasoners) 400s unless
            // `reasoning_content` is echoed across a tool-use sequence, and
            // every other adapter drops it itself.
            | Self::AssistantMessage { message, .. } => vec![message],
            Self::ToolResults { results } => results
                .into_iter()
                .map(|r| ChatMessage::from(ToolResponse::new(&r.id, &r.content)))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Transcript protocol phase.  `append_*` and `quiesce` admit only the
/// transition each phase allows, so no model view is ever out of order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    ReadyForUser,
    AwaitingAssistantAfterUser,
    AwaitingToolResults { pending_ids: Vec<String> },
    AwaitingAssistantAfterToolResults,
}

/// Why an exchange is being wound back to [`State::ReadyForUser`] with no real
/// assistant reply; picks the stub text recorded in its place.
#[derive(Clone, Copy)]
pub enum QuiesceReason {
    Cancelled,
    /// A surfaced error — transport failure, exhausted retry budget — before
    /// the assistant replied.
    Aborted,
    /// A sub-agent called `reply`: that round-trip never asked for a closing
    /// assistant message, so the machine sits in
    /// [`State::AwaitingAssistantAfterToolResults`] on purpose, not in error.
    Replied,
}

/// One session's log: the event vector, the `events.json` writer, the protocol
/// state machine, and the origin metadata `clear` and `fork` re-record.
pub struct AgentLog {
    id: AgentId,
    dir: PathBuf,
    /// Held open, flushed per event, so the file is always tail-able.
    events_file: BufWriter<File>,
    /// Mirror of the events on disk.  `/compact` drops from this alone, since
    /// the file only ever grows; `/clear` wipes both.
    events: Vec<SessionEvent>,
    state: State,
    /// See [`Compaction`] for the invariant.  In-memory only; the on-disk
    /// [`SessionEvent::Compacted`] is the breadcrumb, not the state.
    compaction: Option<Compaction>,
    /// Held, with [`Self::provider`], so `clear` re-emits `SessionStarted`
    /// unchanged and `fork` passes both down to the child.
    model: String,
    provider: String,
    /// `<scratch>/sessions/`, under which `fork` makes each child's dir.
    sessions_root: PathBuf,
}

/// The summary standing in for a dropped prefix.  [`AgentLog::apply_compaction`]
/// physically drops that prefix, so `events` *is* the kept verbatim suffix and
/// the model view is this summary followed by all of it — no cut index to carry.
struct Compaction {
    summary: String,
}

/// A planned cut: everything before `suffix_start` becomes `prefix_messages`,
/// the messages to summarise — any prior summary among them.
pub struct CompactionPlan {
    pub suffix_start: usize,
    pub prefix_messages: Vec<ChatMessage>,
}

fn summary_prompt(summary: &str) -> String {
    format!("Summary of prior work in this session:\n\n{summary}")
}

fn event_message_bytes(e: &SessionEvent) -> usize {
    e.clone()
        .into_chat_messages()
        .iter()
        .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
        .sum()
}

impl AgentLog {
    /// Build a fresh root session log.  Wipes any prior directory with the
    /// same session id, so the events file starts empty.
    ///
    /// # Errors
    /// Re-creating the session directory, or writing `SessionStarted`, failed.
    pub fn root(
        sessions_root: &Path,
        session_id: AgentId,
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

    /// Build a forked child log, inheriting `sessions_root`, `model`, and
    /// `provider` and recording this log's id as the child's parent.
    ///
    /// # Errors
    /// Creating the child's directory, or writing `SessionStarted`, failed.
    pub fn fork(&self, child_id: AgentId, system_prompt_bytes: usize) -> io::Result<Self> {
        let mut s = Self::open_fresh(
            self.sessions_root.clone(),
            child_id,
            self.model.clone(),
            self.provider.clone(),
        )?;
        s.record_started(Some(self.id), system_prompt_bytes)?;
        Ok(s)
    }

    pub fn id(&self) -> AgentId {
        self.id
    }

    /// The per-session directory `<sessions_root>/<id>/`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a fresh user prompt is admissible.  The attend loop leaves every
    /// exchange here, and compaction demands it.
    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::ReadyForUser)
    }

    pub fn can_compact(&self) -> bool {
        self.is_ready()
    }

    /// Length of the in-memory mirror, for the host's resource probe: a count,
    /// never the events, and it shrinks with them on `/compact` and `/clear`.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Approximate context size in serialised model-view bytes.  The fallback
    /// compaction trigger in [`crate::agent::Agent::compact`] when the model's
    /// context window is unknown; otherwise that tracks token pressure instead.
    pub fn history_bytes(&self) -> usize {
        self.model_messages()
            .iter()
            .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
            .sum()
    }

    /// Render the model-view messages for the next provider request.
    ///
    /// # Errors
    /// The session is not awaiting an assistant reply.
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

    /// Every committed message whatever the phase; compaction's input.
    pub fn history_messages(&self) -> Vec<ChatMessage> {
        self.model_messages()
    }

    /// The parent context a `mnemon` child inherits.  A spawn from inside a
    /// tool batch leaves the parent waiting on the very call that made the
    /// child, so that unfinished assistant frame is dropped: the child starts
    /// from the request context, not a dangling tool protocol.
    pub fn inherited_context_messages(&self) -> Vec<ChatMessage> {
        let mut events = self.events.as_slice();
        if matches!(self.state, State::AwaitingToolResults { .. })
            && matches!(
                events.last(),
                Some(SessionEvent::AssistantMessage {
                    pending_tool_ids,
                    ..
                }) if !pending_tool_ids.is_empty()
            )
        {
            events = &events[..events.len() - 1];
        }

        self.project(events)
    }

    /// Import parent context without appending a prompt: a `mnemon` child's
    /// launch prompt arrives through its inbox, and `deliberate` commits it
    /// through [`Self::append_user`] like any other exchange.
    ///
    /// # Errors
    /// The session is not at a ready boundary, or recording a message failed.
    pub fn import_context(&mut self, messages: Vec<ChatMessage>) -> Result<(), String> {
        if !matches!(self.state, State::ReadyForUser) {
            return Err(format!(
                "cannot import context while session is in state {:?}",
                self.state
            ));
        }
        for message in messages {
            self.record_or_string_err(SessionEvent::ContextMessage { message })?;
        }
        Ok(())
    }

    // ── Protocol mutations ────────────────────────────────────────────────

    /// Commit a top-level user prompt, opening a new exchange.
    ///
    /// # Errors
    /// The session is mid-exchange, or recording the prompt failed.
    pub fn append_user(&mut self, text: String) -> Result<(), String> {
        if !matches!(self.state, State::ReadyForUser) {
            return Err(format!(
                "cannot accept a new user prompt while session is in state {:?}",
                self.state
            ));
        }
        self.record_or_string_err(SessionEvent::UserPrompt { text })?;
        self.state = State::AwaitingAssistantAfterUser;
        Ok(())
    }

    /// Append a user message between a complete tool-result batch and the next
    /// provider request.  The only mid-exchange user ingress there is, which is
    /// why it admits [`State::AwaitingAssistantAfterToolResults`] alone.
    ///
    /// # Errors
    /// The tool-result batch is incomplete, or recording the prompt failed.
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

    /// Commit an assistant reply, moving the exchange on to await tool results
    /// or — when no tools were called — back to a ready boundary.
    ///
    /// # Errors
    /// No assistant message is expected, `message`'s role is not `Assistant`,
    /// or recording it failed.
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

    /// Commit the tool-result batch answering the pending tool calls.
    ///
    /// # Errors
    /// No results are expected, they do not match the pending ids (wrong count,
    /// unknown id, duplicate), or recording the batch failed.
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

    /// Drive the session back to [`State::ReadyForUser`], synthesising whatever
    /// events role alternation needs, whenever an exchange ends without a real
    /// assistant reply.
    ///
    /// Total: a stub that cannot be written to disk is pushed to the in-memory
    /// mirror anyway ([`Self::record_lossy`]), since the caller has no fallback
    /// state to quiesce *to*.
    pub fn quiesce(&mut self, reason: QuiesceReason) {
        let already_ready = self.is_ready();
        let (tool_stub, assistant_stub, stop_label) = match reason {
            QuiesceReason::Cancelled => (
                "cancelled before tool execution",
                "(cancelled by user)",
                "cancelled",
            ),
            QuiesceReason::Aborted => (
                "no result: exchange aborted before tool execution",
                "(no reply: exchange aborted)",
                "aborted",
            ),
            QuiesceReason::Replied => (
                "no result: exchange ended on reply",
                "(exchange ended: replied to parent)",
                "replied",
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
                    self.record_lossy(SessionEvent::ToolResults { results });
                    self.state = State::AwaitingAssistantAfterToolResults;
                }
                State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults => {
                    let message = ChatMessage::assistant(assistant_stub);
                    self.record_lossy(SessionEvent::AssistantMessage {
                        message,
                        pending_tool_ids: vec![],
                        stop_reason: Some(stop_label.into()),
                    });
                    self.state = State::ReadyForUser;
                }
            }
        }
        // Only a cancellation earns its own breadcrumb; an abort's marker is
        // the `ProviderError` already on disk.
        if !already_ready && matches!(reason, QuiesceReason::Cancelled) {
            let _ = self.record(SessionEvent::Cancelled);
        }
    }

    /// Indices of the top-level user prompts — the boundaries a compaction may
    /// cut at.  Replays the ready/in-exchange state so a steering prompt is
    /// skipped and no cut can split an assistant message from its tool results.
    fn exchange_start_indices(&self) -> Vec<usize> {
        let mut ready = true;
        let mut starts = Vec::new();
        for (i, e) in self.events.iter().enumerate() {
            match e {
                SessionEvent::UserPrompt { .. } if ready => {
                    starts.push(i);
                    ready = false;
                }
                SessionEvent::AssistantMessage {
                    pending_tool_ids, ..
                } => {
                    ready = pending_tool_ids.is_empty();
                }
                SessionEvent::ToolResults { .. } => ready = false,
                // A compaction only ever commits at a ready boundary, so
                // whatever state the scan carried into this marker is
                // superseded.
                SessionEvent::Compacted { .. } => ready = true,
                _ => {}
            }
        }
        starts
    }

    /// Plan a compaction: the recent exchanges fitting in `keep_budget_bytes`
    /// stay verbatim, the older prefix becomes the messages to summarise.
    /// `None` when everything visible already fits.
    pub fn plan_compaction(&self, keep_budget_bytes: usize) -> Option<CompactionPlan> {
        let candidates = self.exchange_start_indices();

        // Walk the starts newest-first; the cut is the oldest one the budget
        // still holds.  The default keeps nothing, so a lone exchange bigger
        // than the whole budget falls back to summarising everything.
        let mut cut = self.events.len();
        let mut acc = 0usize;
        let mut upper = self.events.len();
        for &cand in candidates.iter().rev() {
            acc += self.events[cand..upper]
                .iter()
                .map(event_message_bytes)
                .sum::<usize>();
            if acc <= keep_budget_bytes {
                cut = cand;
                upper = cand;
            } else {
                break;
            }
        }
        if cut == 0 {
            return None;
        }

        Some(CompactionPlan {
            suffix_start: cut,
            prefix_messages: self.project(&self.events[..cut]),
        })
    }

    /// Commit a planned compaction: record the `Compacted` breadcrumb, then
    /// physically drop `events[..suffix_start]` from the in-memory mirror —
    /// heap reclaimed, not merely a narrower read-time view.  Nothing leaves
    /// `events.json`, and the failure path drops nothing at all.
    ///
    /// # Errors
    /// Tool results are pending, or recording the `Compacted` marker failed.
    pub fn apply_compaction(&mut self, summary: String, suffix_start: usize) -> Result<(), String> {
        if !self.can_compact() {
            return Err("cannot compact while tool results are pending".into());
        }
        let before_bytes = self.history_bytes();
        self.record_or_string_err(SessionEvent::Compacted {
            before_bytes,
            // Patched in memory below; the on-disk `0` stands, rather than
            // rewrite a pretty-printed object in place.
            after_bytes: 0,
            summary: summary.clone(),
        })?;
        // The marker just recorded sits at the tail, at or past `suffix_start`,
        // so the drain takes only the summarised prefix.
        self.events.drain(..suffix_start);
        self.compaction = Some(Compaction { summary });
        self.state = State::ReadyForUser;
        let after = self.history_bytes();
        if let Some(SessionEvent::Compacted { after_bytes, .. }) = self.events.last_mut() {
            *after_bytes = after;
        }
        Ok(())
    }

    /// `/clear`: wipe the events in memory and on disk, then restart with a
    /// fresh `SessionStarted` so the file stays self-consistent.  A cleared
    /// session is rooted again, so `parent` is dropped, but model and provider
    /// survive.
    ///
    /// # Errors
    /// Reopening `events.json`, or writing `SessionStarted`, failed.
    pub fn clear(&mut self, system_prompt_bytes: usize) -> io::Result<()> {
        self.events.clear();
        self.state = State::ReadyForUser;
        self.compaction = None;
        self.events_file = open_events_file(&self.dir)?;
        self.record_started(None, system_prompt_bytes)
    }

    // ── Meta-events ───────────────────────────────────────────────────────
    //
    // One non-protocol breadcrumb each; all fail only on serialising or
    // writing to `events.json`.

    /// # Errors
    /// See the meta-events note above.
    pub fn record_step(&mut self, n: u32, tuning: Tuning) -> io::Result<()> {
        self.record(SessionEvent::StepStarted { n, tuning })
    }

    /// # Errors
    /// See the meta-events note above.
    pub fn record_usage(&mut self, usage: UsageDelta) -> io::Result<()> {
        self.record(SessionEvent::UsageDelta { usage })
    }

    /// # Errors
    /// See the meta-events note above.
    pub fn record_session_ended(&mut self) -> io::Result<()> {
        self.record(SessionEvent::SessionEnded)
    }

    /// # Errors
    /// See the meta-events note above.
    pub fn record_error(&mut self, text: String) -> io::Result<()> {
        self.record(SessionEvent::Error { text })
    }

    /// # Errors
    /// See the meta-events note above.
    pub fn record_nudge(&mut self, used: u32, max: u32, cause: String) -> io::Result<()> {
        self.record(SessionEvent::Nudge { used, max, cause })
    }

    /// # Errors
    /// See the meta-events note above.
    pub fn record_provider_error(&mut self, e: &ProviderError) -> io::Result<()> {
        self.record(SessionEvent::ProviderError { error: e.into() })
    }

    /// # Errors
    /// See the meta-events note above.
    pub fn record_stall(&mut self, e: &ProviderError) -> io::Result<()> {
        self.record(SessionEvent::Stalled { error: e.into() })
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:session-dir] (re)creates the session log dir; event-log infra, not turn-time data I/O"
    )]
    fn open_fresh(
        sessions_root: PathBuf,
        session_id: AgentId,
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
            compaction: None,
            model,
            provider,
            sessions_root,
        })
    }

    /// The `SessionStarted` bookend `root`, `fork`, and `clear` all write.
    fn record_started(
        &mut self,
        parent: Option<AgentId>,
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

    /// The disk half of [`Self::record`], apart so [`Self::record_lossy`] can
    /// go on without it.
    fn write_event(&mut self, ev: &SessionEvent) -> io::Result<()> {
        serde_json::to_writer_pretty(&mut self.events_file, ev).map_err(io::Error::other)?;
        self.events_file.write_all(b"\n")?;
        self.events_file.flush()
    }

    fn record(&mut self, ev: SessionEvent) -> io::Result<()> {
        self.write_event(&ev)?;
        self.events.push(ev);
        Ok(())
    }

    /// [`Self::record`] in the `Result<(), String>` the protocol mutations
    /// return.
    fn record_or_string_err(&mut self, ev: SessionEvent) -> Result<(), String> {
        self.record(ev).map_err(|e| e.to_string())
    }

    /// [`Self::record`] with the disk write made best-effort.  Only
    /// [`Self::quiesce`] may use it: the protocol must reach
    /// [`State::ReadyForUser`] even when `events.json` cannot be written.
    fn record_lossy(&mut self, ev: SessionEvent) {
        let _ = self.write_event(&ev);
        self.events.push(ev);
    }

    /// The one place the model view's shape is spelled out: any live compaction
    /// summary as a leading user message, then the slice.
    fn project(&self, events: &[SessionEvent]) -> Vec<ChatMessage> {
        let mut msgs = Vec::new();
        if let Some(c) = &self.compaction {
            msgs.push(ChatMessage::user(summary_prompt(&c.summary)));
        }
        for e in events {
            msgs.extend(e.clone().into_chat_messages());
        }
        msgs
    }

    /// [`Self::project`] over every live event.
    fn model_messages(&self) -> Vec<ChatMessage> {
        self.project(&self.events)
    }
}

/// Truncate-on-open: both callers, the constructor and `clear`, want it fresh.
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use genai::chat::{ContentPart, ToolCall};

    /// A `sessions/` root keyed to `tag` and this process, so tests never share.
    fn sessions_root(tag: &str) -> PathBuf {
        let p =
            std::env::temp_dir().join(format!("exarch-event-test-{}-{tag}", std::process::id()));
        let _ = fs::remove_dir_all(&p);
        p
    }

    fn fresh_root(tag: &str) -> AgentLog {
        let root = sessions_root(tag);
        AgentLog::root(&root, 0, "model", "provider", 0).expect("log")
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
        assert!(err.contains("cannot accept a new user prompt"));
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
        let err = s.apply_compaction("summary".into(), 0).unwrap_err();
        assert!(err.contains("cannot compact"));
    }

    #[test]
    fn inherited_context_drops_the_unanswered_tool_call() {
        let mut s = fresh_root("inherit-pending");
        s.append_user("parent task".into()).unwrap();
        s.append_assistant(assistant_with_tool("a"), vec!["a".into()], None)
            .unwrap();

        let inherited = s.inherited_context_messages();

        assert_eq!(inherited.len(), 1);
        assert_eq!(inherited[0].role, ChatRole::User);
        let view = serde_json::to_string(&inherited).unwrap();
        assert!(view.contains("parent task"));
        assert!(!view.contains("ToolCall"));
        assert!(
            !view.contains("\"ral\""),
            "the pending assistant tool-call frame must not be inherited: {view}"
        );
    }

    #[test]
    fn import_context_keeps_the_child_ready_for_a_seeded_prompt() {
        let mut s = fresh_root("import-context");
        s.import_context(vec![
            ChatMessage::user("old question"),
            ChatMessage::assistant("old answer"),
        ])
        .unwrap();
        assert!(s.is_ready());
        s.append_user("fresh task".into()).unwrap();

        let ms = s.render_messages().unwrap();
        assert_eq!(
            ms.iter().map(|m| m.role.clone()).collect::<Vec<_>>(),
            vec![ChatRole::User, ChatRole::Assistant, ChatRole::User]
        );
        let view = serde_json::to_string(&ms).unwrap();
        assert!(view.contains("old question"));
        assert!(view.contains("old answer"));
        assert!(
            view.rfind("fresh task") > view.rfind("old answer"),
            "the child prompt must be the final model-visible message: {view}"
        );
    }

    #[test]
    fn compaction_truncates_model_view_but_preserves_disk_log() {
        let mut s = fresh_root("compact-view");
        s.append_user("first".into()).unwrap();
        s.append_assistant(ChatMessage::assistant("done"), vec![], None)
            .unwrap();
        // Keep no suffix, as when no exchange fits the budget at all.
        let cut = s.events.len();
        s.apply_compaction("did stuff".into(), cut).unwrap();
        // Compaction lands ready; prime a prompt the way the REPL would.
        s.append_user("next".into()).unwrap();
        let ms = s.render_messages().unwrap();
        // Summary prompt plus the new user message, and no history.
        assert_eq!(ms.len(), 2);
        assert!(matches!(ms[0].role, ChatRole::User));
        assert!(matches!(ms[1].role, ChatRole::User));
        let contents = fs::read_to_string(s.dir().join("events.json")).unwrap();
        assert!(contents.contains("first"));
        assert!(contents.contains("compacted"));
    }

    #[test]
    fn compaction_keeps_the_recent_suffix_and_summarises_the_prefix() {
        let mut s = fresh_root("compact-suffix");
        for (u, a) in [("USER1", "ASST1"), ("USER2", "ASST2"), ("USER3", "ASST3")] {
            s.append_user(u.into()).unwrap();
            s.append_assistant(ChatMessage::assistant(a), vec![], None)
                .unwrap();
        }

        // A budget holding exactly the last exchange keeps it verbatim.
        let last_exchange = &s.events[s.events.len() - 2..];
        let keep = last_exchange.iter().map(event_message_bytes).sum::<usize>();
        let plan = s.plan_compaction(keep).expect("a prefix to summarise");
        s.apply_compaction("PRIOR-WORK".into(), plan.suffix_start)
            .unwrap();

        let view = serde_json::to_string(&s.history_messages()).unwrap();
        assert!(view.contains("PRIOR-WORK"));
        assert!(view.contains("USER3") && view.contains("ASST3"));
        // The summarised prefix is gone from the model view…
        assert!(!view.contains("USER1") && !view.contains("USER2"));
        // …but still on disk.
        let disk = fs::read_to_string(s.dir().join("events.json")).unwrap();
        assert!(disk.contains("USER1") && disk.contains("ASST2"));
    }

    /// A successful compaction reclaims heap: `event_count` and `history_bytes`
    /// shrink, not just the read-time model view.
    #[test]
    fn apply_compaction_physically_drops_the_prefix_from_the_event_mirror() {
        let mut s = fresh_root("compact-drops-prefix");
        for (u, a) in [("USER1", "ASST1"), ("USER2", "ASST2"), ("USER3", "ASST3")] {
            s.append_user(u.into()).unwrap();
            s.append_assistant(ChatMessage::assistant(a), vec![], None)
                .unwrap();
        }
        let count_before = s.event_count();
        let bytes_before = s.history_bytes();

        let last_exchange = &s.events[s.events.len() - 2..];
        let keep = last_exchange.iter().map(event_message_bytes).sum::<usize>();
        let plan = s.plan_compaction(keep).expect("a prefix to summarise");
        let kept_len = s.events.len() - plan.suffix_start;
        s.apply_compaction("PRIOR-WORK".into(), plan.suffix_start)
            .unwrap();

        // The mirror is the kept suffix plus the marker just recorded.
        assert_eq!(
            s.event_count(),
            kept_len + 1,
            "event_count drops to summary + suffix, not before-compaction size"
        );
        assert!(
            s.event_count() < count_before,
            "the mirror is strictly smaller than before compaction"
        );
        assert!(
            s.history_bytes() < bytes_before,
            "history_bytes drops along with the physically-dropped prefix"
        );
    }

    /// Reclamation is for heap, never history: what was on disk before a
    /// compaction survives as an exact prefix of what is on disk after.
    #[test]
    fn apply_compaction_leaves_the_durable_events_file_byte_for_byte_intact() {
        let mut s = fresh_root("compact-disk-untouched");
        for (u, a) in [("USER1", "ASST1"), ("USER2", "ASST2"), ("USER3", "ASST3")] {
            s.append_user(u.into()).unwrap();
            s.append_assistant(ChatMessage::assistant(a), vec![], None)
                .unwrap();
        }
        let before = fs::read_to_string(s.dir().join("events.json")).unwrap();

        let last_exchange = &s.events[s.events.len() - 2..];
        let keep = last_exchange.iter().map(event_message_bytes).sum::<usize>();
        let plan = s.plan_compaction(keep).expect("a prefix to summarise");
        s.apply_compaction("PRIOR-WORK".into(), plan.suffix_start)
            .unwrap();

        let after = fs::read_to_string(s.dir().join("events.json")).unwrap();
        assert!(
            after.starts_with(&before),
            "the pre-compaction file contents survive untouched as a prefix; \
             compaction only ever appends, never rewrites"
        );
        assert!(
            after.len() > before.len(),
            "the Compacted breadcrumb is appended, so the file grows"
        );
    }

    /// A failed compaction drops nothing: the drop only ever runs after the
    /// breadcrumb is durably recorded.
    #[test]
    fn apply_compaction_failure_drops_nothing() {
        let mut s = fresh_root("compact-fails-drops-nothing");
        s.append_user("p".into()).unwrap();
        s.append_assistant(assistant_with_tool("a"), vec!["a".into()], None)
            .unwrap();
        let count_before = s.event_count();
        let bytes_before = s.history_bytes();

        let err = s.apply_compaction("summary".into(), 0).unwrap_err();
        assert!(err.contains("cannot compact"));
        assert_eq!(
            s.event_count(),
            count_before,
            "a failed compaction drops nothing from the mirror"
        );
        assert_eq!(
            s.history_bytes(),
            bytes_before,
            "a failed compaction leaves history_bytes unchanged"
        );
        assert!(s.compaction.is_none(), "no compaction state was installed");
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
        // Nothing was pending, so no breadcrumb.
        assert!(
            !s.events
                .iter()
                .any(|e| matches!(e, SessionEvent::Cancelled))
        );
    }

    /// An abort, not a cancel, must still wind a committed-but-unanswered
    /// prompt back: stranded, the session rejects every prompt until `/clear`.
    #[test]
    fn quiesce_after_abort_admits_next_prompt() {
        let mut s = fresh_root("quiesce-abort");
        s.append_user("p".into()).unwrap();
        // The provider call errored out, so no assistant reply arrives.
        assert!(!s.is_ready());
        s.quiesce(QuiesceReason::Aborted);
        assert!(s.is_ready());
        s.append_user("next".into()).unwrap();
        // An abort's marker is the surfaced `ProviderError`, not `Cancelled`.
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
        // session_started + user_prompt = 2.
        let parsed: Vec<SessionEvent> = serde_json::Deserializer::from_str(&body)
            .into_iter::<SessionEvent>()
            .collect::<Result<_, _>>()
            .expect("round-trip");
        assert_eq!(parsed.len(), 2);
        assert!(matches!(parsed[0], SessionEvent::SessionStarted { .. }));
        assert!(matches!(parsed[1], SessionEvent::UserPrompt { .. }));
    }

    /// The breadcrumbs — an error diagnostic, a nudge — round-trip as typed
    /// events.  Operational notes deliberately do not: they belong only to
    /// `transcript.jsonl`.
    #[test]
    fn diagnostic_events_round_trip_through_disk() {
        let mut s = fresh_root("diagnostic-roundtrip");
        s.record_error("boom".into()).unwrap();
        s.record_nudge(2, 3, "stop=length".into()).unwrap();
        let body = fs::read_to_string(s.dir().join("events.json")).unwrap();
        let parsed: Vec<SessionEvent> = serde_json::Deserializer::from_str(&body)
            .into_iter::<SessionEvent>()
            .collect::<Result<_, _>>()
            .expect("round-trip");
        // session_started + 2 diagnostics.
        assert_eq!(parsed.len(), 3);
        assert!(matches!(&parsed[1], SessionEvent::Error { text } if text == "boom"));
        assert!(matches!(
            &parsed[2],
            SessionEvent::Nudge { used: 2, max: 3, cause } if cause == "stop=length"
        ));
    }
}
