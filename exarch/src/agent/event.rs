//! Per-session canonical event log.
//!
//! [`AgentLog`] is the single source of truth for one continuous agent
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

use crate::bus::AgentId;
use crate::provider::{ProviderError, Tuning, Usage};
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

/// A serialisable copy of [`crate::provider::Usage`].
///
/// Deriving `Serialize` on `Usage` itself would ripple the trait into the
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
/// event log.
///
/// `ProviderError` carries `&'static str` and `Duration`
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

/// One protocol-level event.
///
/// The order of variants tracks the natural
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
        session_id: AgentId,
        parent: Option<AgentId>,
        model: String,
        provider: String,
        system_prompt_bytes: usize,
        log_dir: PathBuf,
    },
    /// A user-authored prompt — the seed / typed turn, a synthetic
    /// continuation injected by the top-level attend loop after a degenerate turn,
    /// or a steering interjection drained after tool results and before the
    /// next assistant reply.
    UserPrompt { text: String },
    /// A model-visible message inherited from a parent agent's context when a
    /// mnemon child is born.  It is replayed verbatim to the provider but
    /// does not drive the local protocol state machine; the child's fresh
    /// [`Self::UserPrompt`] remains the turn it must answer.
    ContextMessage { message: ChatMessage },
    /// Marks the start of one inner step of [`crate::agent::Agent::deliberate`].  Several
    /// steps may share a single user prompt when the model issues tool
    /// calls.
    StepStarted { n: u32, tuning: Tuning },
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
    /// preceding [`Self::AssistantMessage`].
    ToolResults { results: Vec<ToolResult> },
    /// Per-step token / dollar usage as reported by the provider.
    UsageDelta { usage: UsageDelta },
    /// The user pressed Ctrl-C / Esc mid-exchange.  Whatever events were
    /// pending have been synthesised back to a [`State::ReadyForUser`] state by
    /// [`AgentLog::quiesce`].
    Cancelled,
    /// History was compacted.  The live model view (the summary plus the
    /// kept suffix) is held in [`AgentLog::compaction`]; this event is
    /// just the archival breadcrumb so post-mortem tooling can spot where
    /// the truncation happened and read the summary it produced.
    Compacted {
        before_bytes: usize,
        after_bytes: usize,
        summary: String,
    },
    /// User-facing error diagnostic — surfaced as a red `╳ error` block
    /// in the rendered log.  Not visible to the model: errors are about
    /// the orchestration, not the conversation.
    Error { text: String },
    /// Recovery breadcrumb the top-level attend loop writes between
    /// iterations when nudging the model.  Recorded so events.json
    /// shows where the attend loop intervened.
    Nudge { used: u32, max: u32, cause: String },
    /// Structured provider-error block, written when an exchange returns an
    /// error rather than an assistant message.  Recorded as a
    /// [`ProviderErrorRecord`] so the on-screen render is fully
    /// reconstructable from events.json.
    ProviderError { error: ProviderErrorRecord },
    /// Bookend at the tail.  Written by [`Agent`](crate::agent::Agent)'s
    /// [`Drop`] impl, the one funnel every teardown path — a settled `reply`,
    /// the subtree cascade, or the trunk's own end-of-`attend` teardown —
    /// runs through.
    SessionEnded,
}

impl SessionEvent {
    /// Project this event into a [`ChatMessage`], or `None` if the event
    /// is not part of the model-visible transcript (e.g. lifecycle, step,
    /// usage).  Tool result batches expand to one message per result.
    fn into_chat_messages(self) -> Vec<ChatMessage> {
        match self {
            Self::UserPrompt { text } => vec![ChatMessage::user(text)],
            Self::ContextMessage { message }
            // Pass the assistant message back whole — reasoning included.
            // genai owns the per-adapter reasoning policy: Anthropic drops
            // it, the OpenAI Responses adapter ignores it, but the OpenAI
            // chat-completions adapter (DeepSeek/Kimi reasoners) *requires*
            // `reasoning_content` echoed back across a tool-use sequence or
            // it 400s. Stripping it here would break those and buy nothing
            // elsewhere.
            | Self::AssistantMessage { message, .. } => vec![message],
            Self::ToolResults { results } => results
                .into_iter()
                .map(|r| ChatMessage::from(ToolResponse::new(&r.id, &r.content)))
                .collect(),
            _ => Vec::new(),
        }
    }
}

/// Transcript protocol phase.  Enforces the role-alternation invariant at
/// the event-log boundary: the `append_*`/`quiesce` methods below admit only
/// the transition each phase allows, so the model view can never carry a
/// user/assistant/tool-result sequence out of order.
#[derive(Clone, Debug, PartialEq, Eq)]
enum State {
    ReadyForUser,
    AwaitingAssistantAfterUser,
    AwaitingToolResults { pending_ids: Vec<String> },
    AwaitingAssistantAfterToolResults,
}

/// Why an exchange is being wound back to [`State::ReadyForUser`] without a natural
/// assistant reply — selects the synthetic stub text recorded in the
/// transcript so a post-mortem reads the cause in place.
#[derive(Clone, Copy)]
pub enum QuiesceReason {
    /// The user pressed Ctrl-C / Esc mid-exchange.
    Cancelled,
    /// The exchange ended on a surfaced error — a transport failure or an
    /// exhausted retry budget — before the assistant replied.
    Aborted,
    /// A sub-agent called `reply`: the last round-trip dispatched the reply
    /// and its result but never asked for a closing assistant message, so the
    /// machine sits in [`State::AwaitingAssistantAfterToolResults`].  This winds it
    /// back cleanly — the exchange ended on purpose, not on an error.
    Replied,
}

/// Per-session canonical log.
///
/// Holds the in-memory event vector, the
/// on-disk writer, the protocol state machine, and the static origin
/// metadata (model, provider, sessions root) needed to (re)record
/// [`SessionEvent::SessionStarted`] on `clear` and to set up forked children — keyed
/// throughout to one [`AgentId`].
pub struct AgentLog {
    id: AgentId,
    dir: PathBuf,
    /// Append-only writer over `events.json`.  Held open so each
    /// [`Self::record`] is a buffered append + flush — no reopen-per-event cost,
    /// and the file is always tail-able.
    events_file: BufWriter<File>,
    /// In-memory mirror of every event written to disk.  Reads back to
    /// render the next provider request and to drive the state machine;
    /// truncated by `/compact` and by `/clear`.
    events: Vec<SessionEvent>,
    state: State,
    /// Active compaction, if any — see [`Compaction`] for the invariant.
    /// In-memory only (logs are per-run); the on-disk [`SessionEvent::Compacted`] event is
    /// the archival breadcrumb.
    compaction: Option<Compaction>,
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

/// The live state of an applied compaction: the summary standing in for
/// the dropped prefix.
///
/// The compaction invariant, authoritative here: `AgentLog::apply_compaction`
/// physically drops the summarised prefix from `events`, so `events` *is*
/// the kept verbatim suffix from that point on — there is no cut index to
/// carry. The model view is this summary followed by `events` in full
/// ([`AgentLog::project`]).
struct Compaction {
    summary: String,
}

/// A planned compaction: the cut to apply (`suffix_start`) and the older
/// messages to summarise (`prefix_messages`, including any prior summary).
pub struct CompactionPlan {
    pub suffix_start: usize,
    pub prefix_messages: Vec<ChatMessage>,
}

/// The user-message wrapper a compaction summary takes in the model view.
fn summary_prompt(summary: &str) -> String {
    format!("Summary of prior work in this session:\n\n{summary}")
}

/// Serialised model-message bytes an event contributes — the per-event
/// unit [`AgentLog::history_bytes`] sums and the suffix budget measures.
fn event_message_bytes(e: &SessionEvent) -> usize {
    e.clone()
        .into_chat_messages()
        .iter()
        .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
        .sum()
}

impl AgentLog {
    /// Build a fresh root session log under `sessions_root` (typically
    /// `<scratch>/sessions/`).  Wipes any prior directory with the same
    /// session id so the events file starts empty.
    ///
    /// # Errors
    /// Returns `Err` if re-creating the session directory or writing the
    /// initial `SessionStarted` event fails.
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

    /// Build a forked child session log.  Inherits the parent's
    /// `sessions_root`, `model`, and `provider`, recording the parent
    /// id inside the child's `SessionStarted` event so the on-disk file
    /// is self-describing.
    ///
    /// # Errors
    /// Returns `Err` if creating the child's session directory or writing
    /// its initial `SessionStarted` event fails.
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

    /// Whether the session is at a settled exchange boundary: the committed
    /// transcript is complete and a fresh user prompt is admissible.
    /// The invariant the attend loop relies on — every exchange hands the session
    /// back here — and the precondition compaction needs.
    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::ReadyForUser)
    }

    pub fn can_compact(&self) -> bool {
        self.is_ready()
    }

    /// Length of the in-memory event mirror — the event-vector probe
    /// figure for the host's resource fold. Truncated by `/compact` and
    /// `/clear` like the mirror itself; a count, never the events.
    pub fn event_count(&self) -> usize {
        self.events.len()
    }

    /// Approximate context size as serialised model-view message bytes.
    /// The fallback compaction trigger in
    /// [`crate::agent::Agent::compact`], used when the model's context
    /// window is unknown; otherwise compaction tracks token pressure
    /// against the window.
    pub fn history_bytes(&self) -> usize {
        self.model_messages()
            .iter()
            .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
            .sum()
    }

    /// Render the model-view messages for the next provider request.
    ///
    /// Valid only when the state machine is awaiting an assistant reply.
    ///
    /// # Errors
    /// Returns `Err` if the session is not awaiting an assistant reply.
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

    /// The parent context a mnemon child should inherit.  When called from
    /// a tool-call batch, the parent may be waiting for the very tool result this
    /// child is about to produce; drop that unfinished assistant frame so the
    /// child starts from the request context, not a dangling tool protocol.
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

    /// Import parent context messages without appending a prompt.
    ///
    /// A `mnemon` child receives its launch prompt through its inbox, so
    /// the attend loop commits it through [`Self::append_user`] at deliberate time: the
    /// same path every other exchange takes.
    ///
    /// # Errors
    /// Returns `Err` if the session is not at a ready exchange boundary, or if
    /// recording an imported context message fails.
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
    /// Returns `Err` if the session is not at a ready exchange boundary (tool
    /// results are still pending), or if recording the prompt fails.
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

    /// Append a user steering message after a complete tool-result batch, before
    /// the next provider request.  This is the only mid-exchange user ingress: the
    /// tool protocol is first brought back to
    /// [`State::AwaitingAssistantAfterToolResults`], then the queued prompt becomes the
    /// next model-visible message.
    ///
    /// # Errors
    /// Returns `Err` if the current tool-result batch is not yet complete, or
    /// if recording the steering prompt fails.
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

    /// Commit an assistant reply, moving the exchange to await tool results or
    /// (when no tools were called) back to a ready boundary.
    ///
    /// # Errors
    /// Returns `Err` if an assistant message is not expected in the current
    /// state, if `message`'s role is not `Assistant`, or if recording the
    /// message fails.
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
    /// Returns `Err` if tool results are not expected in the current state,
    /// if the results do not match the pending ids (count mismatch, unknown
    /// id, or duplicate), or if recording the batch fails.
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
    /// tool-result and assistant events the role-alternation invariant
    /// needs.  Called whenever an exchange ends without a natural assistant
    /// reply — a user cancellation or a surfaced error.  `reason`
    /// selects the synthetic stub text and the optional breadcrumb.
    ///
    /// Total: a disk-write failure while synthesising a stub event is
    /// swallowed ([`Self::record_lossy`]) rather than left unresolved,
    /// since the caller has no fallback state to quiesce *to* — the
    /// in-memory mirror still advances so the state machine reaches
    /// [`State::ReadyForUser`] regardless of `events.json`'s fate.
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
        // Only a cancellation gets a dedicated breadcrumb; an abort's
        // marker is the `ProviderError` already on disk.  Best-effort: a
        // failed flush mustn't poison the already-quiesced state.
        if !already_ready && matches!(reason, QuiesceReason::Cancelled) {
            let _ = self.record(SessionEvent::Cancelled);
        }
    }

    /// Indices in `events` of top-level user prompts — the exchange boundaries a
    /// compaction may cut at.  Replays the ready/in-exchange state so a mid-exchange
    /// steering prompt (not a clean boundary) is skipped, and a cut never
    /// splits an assistant message from its tool results.  `events` is
    /// already the live view (see [`Compaction`]), so the whole vector is
    /// candidates — no separate "visible from" index to skip past.
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
                // superseded — true regardless of where a prior compaction's
                // marker landed in the (now further-grown) vector.
                SessionEvent::Compacted { .. } => ready = true,
                _ => {}
            }
        }
        starts
    }

    /// Plan a compaction: choose the cut so the most recent exchanges fitting
    /// in `keep_budget_bytes` stay verbatim, and gather the older prefix
    /// (plus any prior summary) as the messages to summarise.  `None` when
    /// everything currently visible fits the budget — nothing older than
    /// the kept suffix to summarise.
    pub fn plan_compaction(&self, keep_budget_bytes: usize) -> Option<CompactionPlan> {
        let candidates = self.exchange_start_indices();

        // Walk exchange starts newest-first, growing the kept suffix until it
        // would exceed the budget; the cut is the oldest start still
        // inside it.  Default: keep nothing (cut past the end) — a single
        // exchange larger than the whole budget falls back to summarising all.
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

    /// Commit a planned compaction: record the archival `Compacted`
    /// breadcrumb, then physically drop the summarised prefix
    /// (`events[..suffix_start]`) from the in-memory mirror — heap
    /// reclamation, not just a narrower read-time view. The durable
    /// `events.json` is untouched; `events.len()` and `history_bytes()` both
    /// shrink to summary + suffix on success, and nothing is dropped on the
    /// early-return failure path.
    ///
    /// # Errors
    /// Returns `Err` if the session is not at a ready boundary (tool results
    /// are pending), or if recording the `Compacted` marker fails.
    pub fn apply_compaction(&mut self, summary: String, suffix_start: usize) -> Result<(), String> {
        if !self.can_compact() {
            return Err("cannot compact while tool results are pending".into());
        }
        let before_bytes = self.history_bytes();
        self.record_or_string_err(SessionEvent::Compacted {
            before_bytes,
            // Patched below, once the new model view is in place; the
            // on-disk `0` is accepted rather than rewriting a
            // pretty-printed object in place.
            after_bytes: 0,
            summary: summary.clone(),
        })?;
        // The just-recorded `Compacted` marker landed at the tail (index
        // `suffix_start` or later), so it survives this drain along with the
        // rest of the kept suffix; only the summarised prefix is dropped.
        self.events.drain(..suffix_start);
        self.compaction = Some(Compaction { summary });
        self.state = State::ReadyForUser;
        let after = self.history_bytes();
        if let Some(SessionEvent::Compacted { after_bytes, .. }) = self.events.last_mut() {
            *after_bytes = after;
        }
        Ok(())
    }

    /// `/clear`: wipe in-memory and on-disk events, restart with a
    /// fresh `SessionStarted` so the log file is internally consistent.
    /// `parent` is dropped — a cleared session is rooted again — but
    /// the model/provider metadata is preserved across the wipe.
    ///
    /// # Errors
    /// Returns `Err` if truncating/reopening `events.json` or writing the
    /// fresh `SessionStarted` event fails.
    pub fn clear(&mut self, system_prompt_bytes: usize) -> io::Result<()> {
        self.events.clear();
        self.state = State::ReadyForUser;
        self.compaction = None;
        // Truncate the events file in place.
        self.events_file = open_events_file(&self.dir)?;
        self.record_started(None, system_prompt_bytes)
    }

    // ── Meta-events ───────────────────────────────────────────────────────
    //
    // Each appends one non-protocol breadcrumb to the log; all return `Err`
    // if serialising the event or writing it to `events.json` fails.

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

    /// Record the `SessionStarted` bookend with this log's stored
    /// origin metadata.  Used by `root`, `fork`, and `clear`.
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

    /// Append `ev` to `events.json` — the disk half of [`Self::record`],
    /// factored out so [`Self::record_lossy`] can push to the in-memory
    /// mirror even when this fails.
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

    /// [`Self::record`] lifted into `Result<(), String>` for the protocol-mutation
    /// methods, which already plumb string errors back to the caller.
    fn record_or_string_err(&mut self, ev: SessionEvent) -> Result<(), String> {
        self.record(ev).map_err(|e| e.to_string())
    }

    /// Best-effort [`Self::record`]: push to the in-memory mirror regardless of
    /// whether the disk write succeeds.  Used only by [`Self::quiesce`],
    /// which has no fallback state to leave the machine in — the
    /// protocol must reach [`State::ReadyForUser`] even when `events.json` can't
    /// be written.
    fn record_lossy(&mut self, ev: SessionEvent) {
        let _ = self.write_event(&ev);
        self.events.push(ev);
    }

    /// Project an event slice into the `Vec<ChatMessage>` shape genai
    /// wants: the active compaction summary (as a user message) when one is
    /// live, followed by the slice's events.  Lifecycle / step / usage
    /// events drop out in [`SessionEvent::into_chat_messages`].  The single
    /// place the summary-prompt-plus-projection shape is spelled.
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

    /// The full model view: [`project`](Self::project) over every live
    /// event.
    fn model_messages(&self) -> Vec<ChatMessage> {
        self.project(&self.events)
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
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:test] test fs/process scaffolding"
)]
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
        // Keep no suffix (cut past the end) — the model view collapses to
        // the summary alone, as it does when no recent exchange fits the budget.
        let cut = s.events.len();
        s.apply_compaction("did stuff".into(), cut).unwrap();
        // Compaction lands in `ReadyForUser`; mimic the REPL by
        // priming a fresh user prompt before asking what the model
        // sees on the next exchange.
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
    fn compaction_keeps_the_recent_suffix_and_summarises_the_prefix() {
        let mut s = fresh_root("compact-suffix");
        for (u, a) in [("USER1", "ASST1"), ("USER2", "ASST2"), ("USER3", "ASST3")] {
            s.append_user(u.into()).unwrap();
            s.append_assistant(ChatMessage::assistant(a), vec![], None)
                .unwrap();
        }

        // A budget that holds exactly the last exchange keeps it verbatim and
        // summarises everything older.
        let last_exchange = &s.events[s.events.len() - 2..];
        let keep = last_exchange.iter().map(event_message_bytes).sum::<usize>();
        let plan = s.plan_compaction(keep).expect("a prefix to summarise");
        s.apply_compaction("PRIOR-WORK".into(), plan.suffix_start)
            .unwrap();

        let view = serde_json::to_string(&s.history_messages()).unwrap();
        // Summary stands in for the dropped exchanges; the last exchange survives.
        assert!(view.contains("PRIOR-WORK"));
        assert!(view.contains("USER3") && view.contains("ASST3"));
        // The summarised prefix is gone from the model view…
        assert!(!view.contains("USER1") && !view.contains("USER2"));
        // …but still on disk.
        let disk = fs::read_to_string(s.dir().join("events.json")).unwrap();
        assert!(disk.contains("USER1") && disk.contains("ASST2"));
    }

    /// A successful compaction physically drops the summarised prefix from
    /// the in-memory event mirror — `event_count`/`history_bytes` shrink to
    /// summary + suffix, not just the read-time model view.
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

        // The mirror shrank to the kept suffix plus the Compacted marker
        // just recorded — not the full pre-compaction history.
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

    /// The append-only durable files are untouched by compaction: everything
    /// written before the compaction survives as an exact prefix of what is
    /// on disk afterward — reclamation is for heap, never history.
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

    /// A failed compaction (tool results still pending) drops nothing from
    /// the in-memory mirror — the physical-drop path only ever runs after
    /// the archival breadcrumb is durably recorded.
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
        // No `cancelled` breadcrumb when nothing was pending.
        assert!(
            !s.events
                .iter()
                .any(|e| matches!(e, SessionEvent::Cancelled))
        );
    }

    /// An exchange that ends on a surfaced error — not a cancel — must still
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

    /// The model-view forensic breadcrumbs — an error diagnostic, a recovery
    /// nudge — round-trip through `events.json` as typed events.  Operational
    /// system notes are deliberately *not* here: they live only in
    /// `transcript.jsonl`, the operational view, so `events.json` carries only
    /// what the model saw plus these breadcrumbs.
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
        // session_started + 2 diagnostics = 3 events.
        assert_eq!(parsed.len(), 3);
        assert!(matches!(&parsed[1], SessionEvent::Error { text } if text == "boom"));
        assert!(matches!(
            &parsed[2],
            SessionEvent::Nudge { used: 2, max: 3, cause } if cause == "stop=length"
        ));
    }
}
