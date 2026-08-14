//! `AgentLog`: one session's handle onto `sessions/<n>/record.jsonl`, the one
//! seam every fact crosses, and [`crate::record::model::Memo`], the model
//! fold's authoritative in-memory projection of it.
//!
//! Every query below reads `model_memo`; nothing here keeps a second copy.
//! `tui::viewport` folds the same log's `Display`/`Forensic` classes into the
//! rendered `user.log`; `agent::transcript` still keeps its own operational
//! trace, fed at the bus emit seam rather than this one.

use crate::bus::AgentId;
use crate::provider::{ProviderError, Tuning, Usage};
use crate::record::model::{Memo, View};
use crate::record::{Fold as _, Forensic, Protocol, Record, Recorded, widen};
use genai::chat::{ChatMessage, ChatRole};
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs;
use std::io;
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
    /// Defaulted on deserialize so pre-existing logs, written before the
    /// round trip back to [`Usage`] existed, still read.
    #[serde(default)]
    pub unmetered: bool,
}

impl From<Usage> for UsageDelta {
    fn from(u: Usage) -> Self {
        Self {
            input: u.input,
            output: u.output,
            cache_creation: u.cache_creation,
            cache_read: u.cache_read,
            dollars: u.dollars,
            unmetered: u.unmetered,
        }
    }
}

impl From<&UsageDelta> for Usage {
    fn from(d: &UsageDelta) -> Self {
        Self {
            input: d.input,
            output: d.output,
            cache_creation: d.cache_creation,
            cache_read: d.cache_read,
            dollars: d.dollars,
            unmetered: d.unmetered,
        }
    }
}

/// Serialisable mirror of [`ProviderError`], flattening its `&'static str` and
/// `Duration` fields to owned strings and whole seconds.
///
/// `tui::line` renders from this shape, so `record.jsonl` reconstructs the
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditAuthority {
    Model,
    User,
    Harness,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContextOp {
    Fold {
        through_exchange: u64,
        digest: String,
    },
    Drop {
        exchanges: Vec<u64>,
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

/// Why an exchange is being wound back to ready with no real assistant
/// reply; picks the stub text recorded in its place.
#[derive(Clone, Copy)]
pub enum QuiesceReason {
    Cancelled,
    /// A surfaced error — transport failure, exhausted retry budget — before
    /// the assistant replied.
    Aborted,
    /// A sub-agent called `reply`: that round-trip never asked for a closing
    /// assistant message, so the machine sits awaiting one on purpose, not in
    /// error.
    Replied,
}

/// The result of a context edit, including its effect on the model-visible
/// message budget.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct EditReceipt {
    pub op: ContextOp,
    pub by: EditAuthority,
    pub bytes_delta: i64,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSpanKind {
    Exchange,
    Import,
    Digest,
}

impl ContextSpanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exchange => "exchange",
            Self::Import => "import",
            Self::Digest => "digest",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct ContextSurveyItem {
    pub exchange: u64,
    pub kind: ContextSpanKind,
    pub opening: String,
    pub bytes: usize,
    pub steps: usize,
    pub live: bool,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextSurvey {
    pub items: Vec<ContextSurveyItem>,
    pub total_bytes: usize,
    pub total_steps: usize,
}

impl ContextSurvey {
    pub(crate) fn add(&mut self, item: ContextSurveyItem) {
        self.total_bytes = self.total_bytes.saturating_add(item.bytes);
        self.total_steps = self.total_steps.saturating_add(item.steps);
        self.items.push(item);
    }
}

/// One session's handle onto `sessions/<n>/record.jsonl`.
///
/// Carries the seam every fact authors through, and the model fold's
/// [`Memo`] — the one authoritative in-memory projection, per
/// `dev/docs/plans/260814_one_seam_one_log.md`.
pub struct AgentLog {
    id: AgentId,
    dir: PathBuf,
    durable: bool,
    /// Held, with [`Self::provider`], so `clear` re-emits `SessionStarted`
    /// unchanged and `fork` passes both down to the child.
    model: String,
    provider: String,
    /// `<scratch>/sessions/`, under which `fork` makes each child's dir.
    sessions_root: PathBuf,
    /// The directory this log has to itself, when a test made it: dropped with
    /// the log.  A real session's log lives under the session's scratch and
    /// owns no directory of its own.
    _scratch: Option<tempfile::TempDir>,
    /// The model fold's own projection over `record.jsonl` — the type every
    /// query in this file answers from; advanced inline through
    /// [`Self::advance`] on every fact this log authors.
    model_memo: Memo,
    /// The one seam: every fact this log authors crosses here as a `Record` —
    /// appended to `sessions/<n>/record.jsonl` and published on whatever bus
    /// `Agent::couple` has attached, in that order, under one lock.
    seam: crate::record::Emitter,
}

/// A planned cut: the visible prefix through `through_exchange` becomes the
/// messages to summarise, while the newer spans stay verbatim.
pub struct CompactionPlan {
    pub through_exchange: u64,
    pub prefix_messages: Vec<ChatMessage>,
}

pub(crate) struct ClearRecord {
    pub(crate) rotation: Option<u64>,
    pub(crate) rotation_error: Option<io::Error>,
}

pub(crate) fn summary_prompt(summary: &str) -> String {
    format!("Summary of prior work in this session:\n\n{summary}")
}

/// The survey's standing caution to the model, beside the refusal sentences
/// so every model-facing context sentence is minted here.
pub const CACHE_SENTENCE: &str =
    "editing before the cache watermark re-reads the prefix on the next request";

impl AgentLog {
    /// Build a fresh root session log.  Wipes any prior directory with the
    /// same session id, so `record.jsonl` starts empty.
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
            true,
        )?;
        s.record_started(None, system_prompt_bytes, crate::bootstrap::now_unix_ms())?;
        Ok(s)
    }

    /// A root log in a directory of its own, deleted when the log is.
    ///
    /// A forked child lives inside that directory without owning it, so a test
    /// keeping a child must keep its root alive too.
    ///
    /// # Errors
    /// Returns `Err` if the directory or the session log cannot be created.
    #[doc(hidden)]
    pub fn for_test(session_id: AgentId, model: &str, provider: &str) -> io::Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix("exarch-agent-log-test-")
            .tempdir()?;
        let log = Self::root(scratch.path(), session_id, model, provider, 0)?;
        Ok(Self {
            _scratch: Some(scratch),
            ..log
        })
    }

    /// Build a mirror-only root: `--no-logs`, no `record.jsonl` ever created.
    ///
    /// # Errors
    /// Returns an error when the session directory cannot be created.
    pub fn root_without_logs(
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
            false,
        )?;
        s.record_started_lossy(None, system_prompt_bytes, crate::bootstrap::now_unix_ms());
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
            self.durable,
        )?;
        if self.durable {
            s.record_started(
                Some(self.id),
                system_prompt_bytes,
                crate::bootstrap::now_unix_ms(),
            )?;
        } else {
            s.record_started_lossy(
                Some(self.id),
                system_prompt_bytes,
                crate::bootstrap::now_unix_ms(),
            );
        }
        Ok(s)
    }

    /// Fold `record.jsonl` into the model view, quarantining a torn tail, and
    /// reopen the seam in append mode.
    ///
    /// A session recorded before `record.jsonl` existed cannot be resumed —
    /// an accepted loss named in
    /// `dev/docs/plans/260814_one_seam_one_log.md` — so a directory with no
    /// `record.jsonl` refuses cleanly rather than silently starting an empty
    /// session.
    ///
    /// # Errors
    /// Returns an error when the log is missing, malformed, foreign, or
    /// cannot be reopened after validation.
    pub fn resume(sessions_root: &Path, session_id: AgentId) -> io::Result<Self> {
        if session_id != 0 {
            return Err(io::Error::other(format!(
                "cannot resume session {session_id}: children are transient by design and only session 0 can be resumed"
            )));
        }
        let dir = sessions_root.join(session_id.to_string());
        let record_path = dir.join("record.jsonl");
        if !record_path.exists() {
            return Err(io::Error::other(format!(
                "cannot resume {}: no record.jsonl was found here; a session recorded before this exarch's one-seam-one-log change cannot be resumed — was this session started with an older exarch, or is {} the right directory to resume?",
                record_path.display(),
                dir.display()
            )));
        }
        let (model_memo, model, provider) =
            crate::record::model::resume(&record_path).map_err(|error| {
                io::Error::new(error.kind(), format!("cannot resume {}: {error}", record_path.display()))
            })?;
        let seam = crate::record::Emitter::append_to(&record_path)?;
        let mut resumed = Self {
            id: session_id,
            dir,
            durable: true,
            model,
            provider,
            sessions_root: sessions_root.to_path_buf(),
            _scratch: None,
            model_memo,
            seam,
        };
        if !resumed.is_ready() {
            resumed.quiesce(QuiesceReason::Aborted);
        }
        Ok(resumed)
    }

    pub fn is_durable(&self) -> bool {
        self.durable
    }

    pub fn resumed_summary(&self) -> (u64, u64) {
        let bytes =
            fs::metadata(self.dir.join("record.jsonl")).map_or(0, |metadata| metadata.len());
        (self.model_memo.current_exchange().unwrap_or(0), bytes)
    }

    /// Record the live model selection and the shared resume boundary stamp.
    ///
    /// # Errors
    /// Returns an error if the resumed breadcrumb cannot be appended.
    pub fn record_resumed(
        &mut self,
        model: &str,
        provider: &str,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) -> io::Result<()> {
        self.model = model.to_string();
        self.provider = provider.to_string();
        self.record_protocol(Protocol::SessionResumed {
            model: self.model.clone(),
            provider: self.provider.clone(),
            system_prompt_bytes,
            at_unix_ms,
        })
    }

    /// Advance the model fold for one witnessed record — called immediately
    /// after every [`crate::record::Emitter::emit`] this log makes itself. A
    /// no-op for `Display`/`Forensic` records: this fold admits `Protocol`
    /// alone.
    fn advance(&mut self, record: &Recorded<Record>) {
        if let Err(refusal) = crate::record::model::Model::step(&mut self.model_memo, record) {
            // Live authorship is typestate-correct by construction (see
            // record::model's own doc on why its Refusal never fires here),
            // so reaching this arm means a caller handed `advance` a foreign
            // Recorded<Record> — a caller bug, not a runtime condition.
            debug_assert!(false, "model fold refused a live record: {refusal}");
        }
    }

    /// The record seam this session authors through — a cheap clone whoever
    /// couples a live bus, records a display commit, or feeds the chopper
    /// takes for themselves.
    pub fn record_emitter(&self) -> crate::record::Emitter {
        self.seam.clone()
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
        self.model_memo.is_ready()
    }

    pub fn can_compact(&self) -> bool {
        self.model_memo.can_compact()
    }

    /// Number of event slots still owned by the model view, for the host's
    /// resource probe; the append-only log retains the slots an edit removes.
    pub fn event_count(&self) -> usize {
        self.model_memo.event_count()
    }

    pub fn view(&self) -> &View {
        self.model_memo.view()
    }

    /// # Panics
    /// Panics if a view span is not resident in the ledger.
    pub fn context_survey(&self) -> ContextSurvey {
        self.model_memo.context_survey()
    }

    /// Read named, closed spans in their model-view order.
    ///
    /// # Errors
    /// Refuses an empty list, a duplicate name, a missing or folded exchange,
    /// or the exchange still in progress.
    pub fn read_context(&self, exchanges: &[u64]) -> Result<String, String> {
        self.model_memo.read_context(exchanges)
    }

    /// Approximate context size in serialised model-view bytes.  The fallback
    /// compaction trigger in [`crate::agent::Agent::compact`] when the model's
    /// context window is unknown; otherwise that tracks token pressure instead.
    pub fn history_bytes(&self) -> usize {
        self.model_memo.history_bytes()
    }

    /// Render the model-view messages for the next provider request.
    ///
    /// # Errors
    /// The session is not awaiting an assistant reply.
    pub fn render_messages(&self) -> Result<Vec<ChatMessage>, String> {
        if !self.model_memo.is_awaiting_assistant() {
            return Err(format!(
                "cannot render request while session is in state {}",
                self.model_memo.state_description()
            ));
        }
        Ok(self.model_memo.model_messages())
    }

    /// Every committed message whatever the phase; compaction's input.
    pub fn history_messages(&self) -> Vec<ChatMessage> {
        self.model_memo.model_messages()
    }

    /// The parent context a `mnemon` child inherits.  A spawn from inside a
    /// tool batch leaves the parent waiting on the very call that made the
    /// child, so that unfinished assistant frame is dropped: the child starts
    /// from the request context, not a dangling tool protocol.
    pub fn inherited_context_messages(&self) -> Vec<ChatMessage> {
        self.model_memo.inherited_context_messages()
    }

    /// Import parent context without appending a prompt: a `mnemon` child's
    /// launch prompt arrives through its inbox, and `deliberate` commits it
    /// through [`Self::append_user`] like any other exchange.
    ///
    /// # Errors
    /// The session is not at a ready boundary, or recording a message failed.
    pub fn import_context(&mut self, messages: Vec<ChatMessage>) -> Result<(), String> {
        if !self.model_memo.is_ready() {
            return Err(format!(
                "cannot import context while session is in state {}",
                self.model_memo.state_description()
            ));
        }
        let exchange = self.next_exchange();
        for message in messages {
            self.record_protocol(Protocol::ContextMessage { exchange, message })
                .map_err(|e| e.to_string())?;
        }
        Ok(())
    }

    // ── Protocol mutations ────────────────────────────────────────────────

    /// Commit a top-level user prompt, opening a new exchange.
    ///
    /// # Errors
    /// The session is mid-exchange, or recording the prompt failed.
    pub fn append_user(&mut self, text: String, continues: Option<u64>) -> Result<(), String> {
        if !self.model_memo.is_ready() {
            return Err(format!(
                "cannot accept a new user prompt while session is in state {}",
                self.model_memo.state_description()
            ));
        }
        let max_exchange = self.model_memo.current_exchange().unwrap_or(0);
        let exchange = continues
            .filter(|id| *id == max_exchange)
            .filter(|id| self.model_memo.view().spans.iter().any(|span| span.id == *id))
            .unwrap_or_else(|| self.next_exchange());
        self.record_protocol(Protocol::UserPrompt { exchange, text })
            .map_err(|e| e.to_string())
    }

    /// Append a user message between a complete tool-result batch and the next
    /// provider request.  The only mid-exchange user ingress there is, which is
    /// why it admits the tool-results-awaited phase alone.
    ///
    /// # Errors
    /// The tool-result batch is incomplete, or recording the prompt failed.
    pub fn append_steering(&mut self, text: String) -> Result<(), String> {
        if !self.model_memo.is_awaiting_steering() {
            return Err(format!(
                "tool results must be complete before accepting a steering prompt; session is in state {}",
                self.model_memo.state_description()
            ));
        }
        let Some(exchange) = self.model_memo.current_exchange() else {
            return Err("cannot accept a steering prompt before an exchange has started".into());
        };
        self.record_protocol(Protocol::UserPrompt { exchange, text })
            .map_err(|e| e.to_string())
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
        if !self.model_memo.is_awaiting_assistant() {
            return Err(format!(
                "assistant message is not expected while session is in state {}",
                self.model_memo.state_description()
            ));
        }
        if message.role != ChatRole::Assistant {
            return Err(format!(
                "assistant message has role {:?}; expected Assistant",
                message.role,
            ));
        }
        self.record_protocol(Protocol::AssistantMessage {
            message,
            pending_tool_ids,
            stop_reason,
        })
        .map_err(|e| e.to_string())
    }

    /// Commit the tool-result batch answering the pending tool calls.
    ///
    /// # Errors
    /// No results are expected, they do not match the pending ids (wrong count,
    /// unknown id, duplicate), or recording the batch failed.
    pub fn append_tool_results(&mut self, results: Vec<ToolResult>) -> Result<(), String> {
        let Some(pending_ids) = self.model_memo.pending_tool_results() else {
            return Err(format!(
                "tool results are not expected while session is in state {}",
                self.model_memo.state_description()
            ));
        };
        validate_result_ids(&pending_ids, &results)?;
        self.record_protocol(Protocol::ToolResults { results })
            .map_err(|e| e.to_string())
    }

    /// Drive the session back to a ready boundary, synthesising whatever
    /// records role alternation needs, whenever an exchange ends without a
    /// real assistant reply.
    pub fn quiesce(&mut self, reason: QuiesceReason) {
        let stubs = self.model_memo.quiesce_stubs(reason);
        let quiesced = !stubs.is_empty();
        for stub in stubs {
            self.record_protocol_lossy(stub);
        }
        // Only a cancellation earns its own breadcrumb; an abort's marker is
        // the `ProviderError` already on disk.
        if quiesced
            && matches!(reason, QuiesceReason::Cancelled)
            && let Err(error) = self.record_forensic(Forensic::Cancelled)
        {
            eprintln!("exarch: the cancellation breadcrumb was not recorded: {error}");
        }
    }

    pub fn plan_compaction(&self, keep_budget_bytes: usize) -> Option<CompactionPlan> {
        self.model_memo.plan_compaction(keep_budget_bytes, None)
    }

    pub fn plan_compaction_before(
        &self,
        keep_budget_bytes: usize,
        before_exchange: u64,
    ) -> Option<CompactionPlan> {
        self.model_memo
            .plan_compaction(keep_budget_bytes, Some(before_exchange))
    }

    /// # Errors
    /// Refuses an empty drop, an unknown or folded exchange, or a live exchange.
    pub fn apply_edit(&mut self, op: ContextOp, by: EditAuthority) -> Result<EditReceipt, String> {
        self.model_memo.validate_edit(&op)?;
        let before = self.model_memo.history_bytes();
        self.record_protocol(Protocol::ContextEdited { op: op.clone(), by })
            .map_err(|e| e.to_string())?;
        let after = self.model_memo.history_bytes();
        let before = i64::try_from(before).unwrap_or(i64::MAX);
        let after = i64::try_from(after).unwrap_or(i64::MAX);
        Ok(EditReceipt {
            op,
            by,
            bytes_delta: before.saturating_sub(after),
        })
    }

    /// Resolve a user rewind into the whole visible suffix beginning at its
    /// anchor. The anchor is checked before the suffix is derived.
    ///
    /// # Errors
    /// Refuses an absent anchor or one strictly inside the current digest.
    pub fn rewind_exchanges(&self, anchor: u64) -> Result<Vec<u64>, String> {
        self.model_memo.rewind_exchanges(anchor)
    }

    /// `/clear`: wipe the record in memory and on disk, then restart with a
    /// fresh `SessionStarted` so the file stays self-consistent.  A cleared
    /// session is rooted again, so `parent` is dropped, but model and provider
    /// survive.
    ///
    /// # Errors
    /// Reopening `record.jsonl`, or writing `SessionStarted`, failed.
    pub(crate) fn clear(
        &mut self,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
        transcript_path: &Path,
    ) -> io::Result<ClearRecord> {
        if !self.durable {
            self.model_memo = Memo::default();
            self.seam = crate::record::Emitter::none();
            self.record_started_lossy(None, system_prompt_bytes, at_unix_ms);
            return Ok(ClearRecord {
                rotation: None,
                rotation_error: None,
            });
        }

        let record_path = self.dir.join("record.jsonl");
        let rotation = match first_free_rotation(&record_path, transcript_path) {
            Ok(n) => n,
            Err(error) => {
                return Err(io::Error::other(format!(
                    "clear was not committed for {}: {error}",
                    record_path.display()
                )));
            }
        };
        let rotation_error = self.rotate_record(&record_path, rotation);
        self.model_memo = Memo::default();
        let started = self.started_event(None, system_prompt_bytes, at_unix_ms);
        let seam_error = self.record_protocol(started).err();
        Ok(ClearRecord {
            rotation: Some(rotation),
            rotation_error: join_errors([rotation_error, seam_error]),
        })
    }

    /// Rotate `record.jsonl`, opening a fresh segment.  A rename or create
    /// failure never truncates over the old segment — no recorded fact is
    /// ever removed — so the seam keeps appending to whatever file survives,
    /// and the failure is reported, not shrugged.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:record-file] rotates the session's record.jsonl on /clear; output infra, not turn-time data I/O"
    )]
    fn rotate_record(&mut self, path: &Path, rotation: u64) -> Option<io::Error> {
        let rotated = rotation_path(path, rotation);
        match fs::rename(path, &rotated) {
            Ok(()) => {}
            Err(error) => {
                return Some(io::Error::new(
                    error.kind(),
                    format!(
                        "clear committed, but {} could not be rotated; the record log continues in place: {error}",
                        path.display()
                    ),
                ));
            }
        }
        match crate::record::Emitter::create(path) {
            Ok(seam) => {
                self.seam = seam;
                None
            }
            Err(error) => Some(io::Error::new(
                error.kind(),
                format!(
                    "clear committed, but a fresh {} could not be opened; the old record segment stays live: {error}",
                    path.display()
                ),
            )),
        }
    }

    // ── Meta-records ──────────────────────────────────────────────────────
    //
    // One non-protocol breadcrumb each; all fail only on serialising or
    // appending to `record.jsonl`.

    /// # Errors
    /// See the meta-records note above.
    pub fn record_step(&mut self, n: u32, tuning: Tuning) -> io::Result<()> {
        self.record_protocol(Protocol::StepStarted { n, tuning })
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_usage(&mut self, usage: UsageDelta) -> io::Result<()> {
        self.record_forensic(Forensic::UsageDelta { usage })
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_session_ended(&mut self) -> io::Result<()> {
        self.record_protocol(Protocol::SessionEnded)
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_error(&mut self, text: String) -> io::Result<()> {
        self.record_forensic(Forensic::Error { text })
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_nudge(&mut self, used: u32, max: u32, cause: String) -> io::Result<()> {
        self.record_forensic(Forensic::Nudge { used, max, cause })
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_provider_error(&mut self, e: &ProviderError) -> io::Result<()> {
        self.record_forensic(Forensic::ProviderError { error: e.into() })
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_stall(&mut self, e: &ProviderError) -> io::Result<()> {
        self.record_forensic(Forensic::Stalled { error: e.into() })
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
        durable: bool,
    ) -> io::Result<Self> {
        let dir = sessions_root.join(session_id.to_string());
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        let seam = if durable {
            crate::record::Emitter::create(&dir.join("record.jsonl"))?
        } else {
            crate::record::Emitter::none()
        };
        Ok(Self {
            id: session_id,
            dir,
            durable,
            model,
            provider,
            sessions_root,
            _scratch: None,
            model_memo: Memo::default(),
            seam,
        })
    }

    /// The `SessionStarted` bookend `root`, `fork`, and `clear` all write.
    fn record_started(
        &mut self,
        parent: Option<AgentId>,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) -> io::Result<()> {
        let started = self.started_event(parent, system_prompt_bytes, at_unix_ms);
        self.record_protocol(started)
    }

    fn record_started_lossy(
        &mut self,
        parent: Option<AgentId>,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) {
        let started = self.started_event(parent, system_prompt_bytes, at_unix_ms);
        self.record_protocol_lossy(started);
    }

    fn started_event(
        &self,
        parent: Option<AgentId>,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) -> Protocol {
        Protocol::SessionStarted {
            session_id: self.id,
            parent,
            model: self.model.clone(),
            provider: self.provider.clone(),
            system_prompt_bytes,
            log_dir: self.dir.clone(),
            at_unix_ms,
        }
    }

    /// Emit a protocol fact through the seam and advance the model fold on
    /// the witnessed result — the one funnel every protocol mutation takes.
    ///
    /// # Errors
    /// A failed append to the one authoritative log is a session error.
    fn record_protocol(&mut self, p: Protocol) -> io::Result<()> {
        let recorded = widen(self.seam.emit(p)?);
        self.advance(&recorded);
        Ok(())
    }

    /// [`Self::record_protocol`] for the [`Forensic`] class.
    ///
    /// # Errors
    /// See [`Self::record_protocol`].
    fn record_forensic(&mut self, f: Forensic) -> io::Result<()> {
        let recorded = widen(self.seam.emit(f)?);
        self.advance(&recorded);
        Ok(())
    }

    /// [`Self::record_protocol`] with the emit made best-effort: its remit is
    /// exactly the harness-synthesized records where refusing would wedge the
    /// session — [`Self::quiesce`]'s stubs, and the `SessionStarted` bookends
    /// of a mirror-only session, which a `--no-logs` seam never actually fails
    /// to accept.  A genuine append failure here still leaves the model fold
    /// unadvanced for this one record, since there is no witnessed result to
    /// advance it with — the honest price of the law that a failed append is
    /// a session error, not a shrug, even when the caller cannot propagate it.
    fn record_protocol_lossy(&mut self, p: Protocol) {
        match self.seam.emit(p) {
            Ok(recorded) => self.advance(&widen(recorded)),
            Err(error) => eprintln!(
                "exarch: a harness-synthesized record was not recorded in record.jsonl: {error}"
            ),
        }
    }

    fn next_exchange(&self) -> u64 {
        self.model_memo.current_exchange().unwrap_or(0).saturating_add(1)
    }

    pub fn current_exchange(&self) -> Option<u64> {
        self.model_memo.current_exchange()
    }

    pub fn log_len(&self) -> usize {
        self.model_memo.log_len()
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.model_memo.token_measure_is_stale(measured_at)
    }
}

/// The one `io::Error` a multi-file boundary reports, joining whatever
/// half-failures it collected; `None` when every leg succeeded.
fn join_errors<const N: usize>(errors: [Option<io::Error>; N]) -> Option<io::Error> {
    let mut it = errors.into_iter().flatten();
    let first = it.next()?;
    let rest: Vec<String> = it.map(|error| error.to_string()).collect();
    if rest.is_empty() {
        return Some(first);
    }
    Some(io::Error::new(
        first.kind(),
        format!("{first}; {}", rest.join("; ")),
    ))
}

/// The next rotation number free for both `record.jsonl` and
/// `transcript.jsonl`'s own `.N` sidecar — kept paired since both files
/// rotate together on every `/clear`, even though `transcript.jsonl`'s own
/// writer lives in `agent::transcript`, fed at the bus emit seam.
fn first_free_rotation(record: &Path, transcript: &Path) -> io::Result<u64> {
    let mut n = 0;
    loop {
        let free = [record, transcript]
            .iter()
            .all(|path| !rotation_path(path, n).exists());
        if free {
            return Ok(n);
        }
        n = n
            .checked_add(1)
            .ok_or_else(|| io::Error::other("no free rotation number remains"))?;
    }
}

fn rotation_path(path: &Path, n: u64) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "record".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!("{name}.{n}"))
}

pub(crate) fn validate_result_ids(
    pending_ids: &[String],
    results: &[ToolResult],
) -> Result<(), String> {
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
    use std::io::Write as _;

    /// A sessions root for one test, deleted when the returned guard falls.
    /// Hold the guard: binding only its path deletes the directory on the spot.
    fn sessions_root(tag: &str) -> tempfile::TempDir {
        tempfile::Builder::new()
            .prefix(&format!("exarch-event-test-{tag}-"))
            .tempdir()
            .expect("sessions root")
    }

    fn fresh_root() -> AgentLog {
        AgentLog::for_test(0, "model", "provider").expect("log")
    }

    fn assistant_with_tool(id: &str) -> ChatMessage {
        ChatMessage::assistant(vec![ContentPart::ToolCall(ToolCall {
            call_id: id.into(),
            fn_name: "ral".into(),
            fn_arguments: serde_json::json!({"cmd": "pwd"}),
            thought_signatures: None,
        })])
    }

    fn complete_exchange(s: &mut AgentLog, user: &str, answer: &str) {
        s.append_user(user.into(), None).unwrap();
        s.append_assistant(ChatMessage::assistant(answer), vec![], None)
            .unwrap();
    }

    fn record_path(log: &AgentLog) -> PathBuf {
        log.dir().join("record.jsonl")
    }

    fn records(log: &AgentLog) -> Vec<Record> {
        let file = fs::File::open(record_path(log)).unwrap();
        serde_json::Deserializer::from_reader(file)
            .into_iter::<Record>()
            .collect::<Result<_, _>>()
            .expect("record.jsonl round-trip")
    }

    #[test]
    fn span_partition_records_steering_imports_and_unaddressable_prefixes() {
        let mut s = fresh_root();
        s.record_error("before the first prompt".into()).unwrap();
        s.import_context(vec![ChatMessage::user("inherited")])
            .unwrap();
        complete_exchange(&mut s, "prompt", "answer");
        s.append_user("tool prompt".into(), None).unwrap();
        s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        s.append_tool_results(vec![ToolResult {
            id: "call".into(),
            content: "result".into(),
        }])
        .unwrap();
        s.append_steering("steer".into()).unwrap();
        s.append_assistant(ChatMessage::assistant("finished"), vec![], None)
            .unwrap();

        assert_eq!(
            s.view()
                .spans
                .iter()
                .map(|span| span.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert!(
            !s.history_messages()
                .iter()
                .any(|message| message.content.first_text() == Some("before the first prompt"))
        );
    }

    #[test]
    fn exchange_ids_mint_monotonically_across_edits() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        assert_eq!(s.current_exchange(), Some(2));
        s.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::Model)
            .unwrap();
        complete_exchange(&mut s, "three", "three");
        assert_eq!(s.current_exchange(), Some(3));
        s.apply_edit(
            ContextOp::Fold {
                through_exchange: 1,
                digest: "old".into(),
            },
            EditAuthority::Harness,
        )
        .unwrap();
        complete_exchange(&mut s, "four", "four");
        assert_eq!(s.current_exchange(), Some(4));
    }

    #[test]
    fn continues_resolution_requires_maximum_and_survival() {
        let mut joins = fresh_root();
        complete_exchange(&mut joins, "one", "one");
        joins.append_user("nudge".into(), Some(1)).unwrap();
        assert_eq!(joins.current_exchange(), Some(1));
        assert_eq!(joins.view().spans.len(), 1);

        let mut rewound = fresh_root();
        complete_exchange(&mut rewound, "one", "one");
        rewound
            .apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::Model)
            .unwrap();
        rewound.append_user("fresh".into(), Some(1)).unwrap();
        assert_eq!(rewound.current_exchange(), Some(2));

        let mut intervened = fresh_root();
        complete_exchange(&mut intervened, "one", "one");
        complete_exchange(&mut intervened, "two", "two");
        intervened.append_user("fresh".into(), Some(1)).unwrap();
        assert_eq!(intervened.current_exchange(), Some(3));

        let mut drop_n_plus_one = fresh_root();
        complete_exchange(&mut drop_n_plus_one, "one", "one");
        complete_exchange(&mut drop_n_plus_one, "two", "two");
        drop_n_plus_one
            .apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::Model)
            .unwrap();
        drop_n_plus_one
            .append_user("fresh".into(), Some(1))
            .unwrap();
        assert_eq!(
            drop_n_plus_one.current_exchange(),
            Some(3),
            "last-in-view is not enough when the log has moved past n"
        );
    }

    #[test]
    fn edit_admissibility_refuses_live_unknown_folded_and_empty_names() {
        let mut live = fresh_root();
        live.append_user("live".into(), None).unwrap();
        let err = live
            .apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::Model)
            .unwrap_err();
        assert_eq!(
            err,
            "exchange 1 is the one you are in — a context edit may only name closed exchanges"
        );

        let mut unknown = fresh_root();
        complete_exchange(&mut unknown, "one", "one");
        let err = unknown
            .apply_edit(ContextOp::Drop { exchanges: vec![7] }, EditAuthority::User)
            .unwrap_err();
        assert_eq!(err, "exchange 7 is not present in the current view");

        let err = unknown
            .apply_edit(ContextOp::Drop { exchanges: vec![] }, EditAuthority::User)
            .unwrap_err();
        assert_eq!(err, "a context edit must name at least one exchange");

        unknown
            .apply_edit(
                ContextOp::Fold {
                    through_exchange: 1,
                    digest: "summary".into(),
                },
                EditAuthority::Harness,
            )
            .unwrap();
        let err = unknown
            .apply_edit(ContextOp::Drop { exchanges: vec![0] }, EditAuthority::Model)
            .unwrap_err();
        assert_eq!(
            err,
            "exchange 0 is folded into the digest through 1 — name 1 to drop the digest whole, or fold further"
        );
    }

    #[test]
    fn context_read_marks_roles_delimits_steps_and_addresses_digest_by_reach() {
        let mut s = fresh_root();
        s.append_user("first prompt".into(), None).unwrap();
        s.record_step(1, Tuning::default()).unwrap();
        s.append_assistant(ChatMessage::assistant("first answer"), vec![], None)
            .unwrap();
        complete_exchange(&mut s, "second prompt", "second answer");

        let transcript = s.read_context(&[1]).expect("closed exchange is readable");
        assert!(transcript.contains("=== exchange 1 ==="));
        assert!(transcript.contains("[user]\nfirst prompt"));
        assert!(transcript.contains("--- step 1 ---"));
        assert!(transcript.contains("[assistant]\nfirst answer"));

        s.apply_edit(
            ContextOp::Fold {
                through_exchange: 2,
                digest: "the old work is complete".into(),
            },
            EditAuthority::Harness,
        )
        .unwrap();
        let digest = s
            .read_context(&[2])
            .expect("the digest reach is addressable");
        assert!(digest.contains("the old work is complete"));
        assert!(!digest.contains("first prompt"));
        assert_eq!(
            s.read_context(&[1]).unwrap_err(),
            "exchange 1 is folded into the digest through 2 — name 2 to drop the digest whole, or fold further"
        );

        complete_exchange(&mut s, "third prompt", "third answer");
        s.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
            .unwrap();
        assert_eq!(
            s.rewind_exchanges(9).unwrap_err(),
            "exchange 9 is not present in the current view — the last exchange is 3"
        );
    }

    #[test]
    fn fold_of_fold_updates_one_digest_and_keeps_the_suffix() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        s.apply_edit(
            ContextOp::Fold {
                through_exchange: 1,
                digest: "first digest".into(),
            },
            EditAuthority::Harness,
        )
        .unwrap();
        s.apply_edit(
            ContextOp::Fold {
                through_exchange: 1,
                digest: "second digest".into(),
            },
            EditAuthority::Model,
        )
        .unwrap();

        assert_eq!(
            s.view().digest.as_ref().map(|d| d.text.clone()),
            Some("second digest".into())
        );
        assert_eq!(
            s.view()
                .spans
                .iter()
                .map(|span| span.id)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn digest_addressing_drops_the_digest_at_its_reach() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        s.apply_edit(
            ContextOp::Fold {
                through_exchange: 1,
                digest: "digest".into(),
            },
            EditAuthority::Harness,
        )
        .unwrap();
        s.apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::User)
            .unwrap();
        assert!(s.view().digest.is_none());
        assert_eq!(
            s.view()
                .spans
                .iter()
                .map(|span| span.id)
                .collect::<Vec<_>>(),
            vec![2]
        );
    }

    #[test]
    fn compaction_plan_walks_newest_spans_and_folds_by_exchange() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        complete_exchange(&mut s, "three", "three");
        let keep = s
            .context_survey()
            .items
            .last()
            .expect("the newest exchange has a survey row")
            .bytes;
        let plan = s.plan_compaction(keep).expect("old spans to fold");
        assert_eq!(plan.through_exchange, 2);
        assert!(
            plan.prefix_messages
                .iter()
                .any(|message| { message.content.first_text() == Some("one") })
        );
        s.apply_edit(
            ContextOp::Fold {
                through_exchange: plan.through_exchange,
                digest: "summary".into(),
            },
            EditAuthority::Harness,
        )
        .unwrap();
        assert_eq!(s.view().digest.as_ref().unwrap().through_exchange, 2);
        assert!(
            !s.history_messages()
                .iter()
                .any(|message| { message.content.first_text() == Some("one") })
        );
    }

    #[test]
    fn user_ending_import_span_is_never_torn() {
        let mut s = fresh_root();
        s.import_context(vec![ChatMessage::user("imported user")])
            .unwrap();
        complete_exchange(&mut s, "normal", "answer");

        let messages = s.history_messages();
        assert_eq!(
            messages
                .iter()
                .filter_map(|message| message.content.first_text())
                .collect::<Vec<_>>(),
            vec!["imported user", "normal", "answer"]
        );
    }

    #[test]
    fn quiesce_after_abort_admits_next_prompt() {
        let mut s = fresh_root();
        s.append_user("p".into(), None).unwrap();
        s.quiesce(QuiesceReason::Aborted);
        assert!(s.is_ready());
        s.append_user("next".into(), None).unwrap();
    }

    #[test]
    fn record_jsonl_round_trip_contains_context_edit_records() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        s.apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::User)
            .unwrap();

        assert!(records(&s).iter().any(|record| {
            matches!(
                record,
                Record::Protocol(Protocol::ContextEdited {
                    op: ContextOp::Drop { exchanges },
                    by: EditAuthority::User,
                }) if exchanges == &[1]
            )
        }));
    }

    #[test]
    fn diagnostic_records_round_trip_through_record_jsonl() {
        let mut s = fresh_root();
        s.record_error("boom".into()).unwrap();
        s.record_nudge(2, 3, "stop=length".into()).unwrap();
        let parsed = records(&s);
        assert!(parsed.iter().any(
            |record| matches!(record, Record::Forensic(Forensic::Error { text }) if text == "boom")
        ));
        assert!(parsed.iter().any(|record| matches!(
            record,
            Record::Forensic(Forensic::Nudge { used: 2, max: 3, cause }) if cause == "stop=length"
        )));
    }

    #[test]
    fn clear_rotates_record_jsonl_and_resets_the_view() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        s.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
            .unwrap();

        let record = s.dir().join("record.jsonl");
        let transcript = s.dir().join("transcript.jsonl");
        let result = s.clear(0, 2, &transcript).expect("clear");
        assert_eq!(result.rotation, Some(0));
        assert!(record.with_extension("jsonl.0").exists());
        assert!(s.view().spans.is_empty());
        assert!(s.is_ready());
    }

    #[test]
    fn transient_eviction_keeps_no_resident_state() {
        let sessions = sessions_root("residency-no-logs");
        let mut s = AgentLog::root_without_logs(sessions.path(), 0, "model", "provider", 0).unwrap();
        complete_exchange(&mut s, "one", "one");
        s.apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::User)
            .unwrap();
        assert_eq!(s.event_count(), 0);
    }

    #[test]
    fn resume_replays_a_scripted_history_and_preserves_the_model_view() {
        let sessions = sessions_root("resume-round-trip");
        let mut live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        live.import_context(vec![ChatMessage::user("inherited")])
            .unwrap();
        complete_exchange(&mut live, "one", "answer one");
        complete_exchange(&mut live, "two", "answer two");
        live.apply_edit(
            ContextOp::Fold {
                through_exchange: 2,
                digest: "the old work is complete".into(),
            },
            EditAuthority::Harness,
        )
        .unwrap();
        let expected = serde_json::to_vec(&live.history_messages()).unwrap();
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert!(resumed.is_ready());
        assert_eq!(
            serde_json::to_vec(&resumed.history_messages()).unwrap(),
            expected
        );
    }

    #[test]
    fn resume_quiesces_a_torn_exchange_after_reopening_append_mode() {
        let sessions = sessions_root("resume-mid-exchange");
        let mut live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        live.append_user("run the tool".into(), None).unwrap();
        live.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        drop(live);

        let mut resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert!(resumed.is_ready());
        let recorded = records(&resumed);
        assert!(recorded.iter().any(|record| matches!(
            record,
            Record::Protocol(Protocol::ToolResults { .. })
        )));
        assert!(recorded.iter().any(|record| matches!(
            record,
            Record::Protocol(Protocol::AssistantMessage {
                stop_reason: Some(reason),
                ..
            }) if reason == "aborted"
        )));
        resumed.append_user("continue".into(), None).unwrap();
        let recorded = records(&resumed);
        assert!(matches!(
            recorded.last(),
            Some(Record::Protocol(Protocol::UserPrompt { text, .. })) if text == "continue"
        ));
    }

    #[test]
    fn resume_quarantines_only_an_unterminated_final_fragment() {
        let sessions = sessions_root("resume-tail");
        let live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let prefix = fs::read(&path).unwrap();
        let fragment = b"{\"Protocol\":{\"kind\":\"user_prompt\"";
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(fragment).unwrap();
        file.flush().unwrap();

        let resumed = AgentLog::resume(sessions.path(), 0).expect("torn tail is recoverable");
        assert!(resumed.is_ready());
        assert_eq!(fs::read(&path).unwrap(), prefix);
        assert_eq!(
            fs::read(sessions.path().join("0/record.jsonl.crash")).unwrap(),
            fragment
        );
    }

    #[test]
    fn resume_refuses_a_complete_garbage_line_without_mutating_the_file() {
        let sessions = sessions_root("resume-garbage");
        let live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"garbage\n").unwrap();
        file.flush().unwrap();
        let before = fs::read(&path).unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("garbage is not a crash tail");
        assert_eq!(fs::read(&path).unwrap(), before);
        let _ = error;
    }

    #[test]
    fn resume_refuses_foreign_protocol_data_without_mutating_the_file() {
        let sessions = sessions_root("resume-foreign");
        let live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let foreign = Record::Protocol(Protocol::AssistantMessage {
            message: ChatMessage::user("wrong role"),
            pending_tool_ids: Vec::new(),
            stop_reason: None,
        });
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        serde_json::to_writer(&mut file, &foreign).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        let before = fs::read(&path).unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("foreign data must refuse");
        let text = error.to_string();
        assert!(text.contains("foreign protocol data"), "{text}");
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn resume_refuses_a_stale_exchange_id_as_foreign_data() {
        let sessions = sessions_root("resume-stale-id");
        let mut live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        complete_exchange(&mut live, "one", "one");
        complete_exchange(&mut live, "two", "two");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let stale = Record::Protocol(Protocol::UserPrompt {
            exchange: 1,
            text: "stale".into(),
        });
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        serde_json::to_writer(&mut file, &stale).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        let before = fs::read(&path).unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("a stale exchange id is foreign data");
        let text = error.to_string();
        assert!(text.contains("names exchange 1"), "{text}");
        assert!(text.contains("moved past"), "{text}");
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn resume_refuses_nonzero_session_ids_as_transient_children() {
        let sessions = sessions_root("resume-child");
        let child = AgentLog::root(sessions.path(), 1, "model", "provider", 0).unwrap();
        drop(child);
        let error = AgentLog::resume(sessions.path(), 1)
            .err()
            .expect("child logs are transient");
        assert!(
            error
                .to_string()
                .contains("children are transient by design")
        );
    }

    #[test]
    fn resume_refuses_a_pre_plan_session_with_no_record_log() {
        let sessions = sessions_root("resume-pre-plan");
        let dir = sessions.path().join("0");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("events.jsonl"), b"").unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("a directory with no record.jsonl must refuse, not start empty");
        let text = error.to_string();
        assert!(text.contains("record.jsonl"), "{text}");
        assert!(!dir.join("record.jsonl").exists());
    }

    #[test]
    fn resume_repairs_an_interior_stub_lost_from_disk() {
        let sessions = sessions_root("resume-interior-seam");
        let mut live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        live.append_user("first".into(), None).unwrap();
        live.quiesce(QuiesceReason::Aborted);
        complete_exchange(&mut live, "second", "answer");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);

        let data = fs::read(&path).unwrap();
        let mut repaired = Vec::with_capacity(data.len());
        for fragment in data.split_inclusive(|byte| *byte == b'\n') {
            let record: Record = serde_json::from_slice(&fragment[..fragment.len() - 1]).unwrap();
            if matches!(
                record,
                Record::Protocol(Protocol::AssistantMessage {
                    stop_reason: Some(ref reason),
                    ..
                }) if reason == "aborted"
            ) {
                continue;
            }
            repaired.extend_from_slice(fragment);
        }
        fs::write(&path, repaired).unwrap();

        let mut resumed = AgentLog::resume(sessions.path(), 0).expect("interior seam repair");
        assert!(resumed.is_ready());
        assert!(resumed.history_messages().iter().any(|message| {
            message.content.first_text() == Some("(no reply: exchange aborted)")
        }));
        resumed.append_user("next".into(), None).unwrap();
    }

    #[test]
    fn resume_matches_live_after_a_small_edit_sequence_family() {
        for pattern in 0..8 {
            let sessions = sessions_root(&format!("resume-edits-{pattern}"));
            let mut live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
            complete_exchange(&mut live, "one", "one");
            complete_exchange(&mut live, "two", "two");
            complete_exchange(&mut live, "three", "three");
            match pattern {
                0 => {}
                1 => {
                    live.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
                        .unwrap();
                }
                2 => {
                    live.apply_edit(
                        ContextOp::Fold {
                            through_exchange: 2,
                            digest: "folded".into(),
                        },
                        EditAuthority::Harness,
                    )
                    .unwrap();
                }
                3 => {
                    live.apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::Model)
                        .unwrap();
                    live.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
                        .unwrap();
                }
                4 => {
                    live.apply_edit(
                        ContextOp::Fold {
                            through_exchange: 2,
                            digest: "folded".into(),
                        },
                        EditAuthority::Harness,
                    )
                    .unwrap();
                    live.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::Model)
                        .unwrap();
                }
                5 => {
                    live.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
                        .unwrap();
                    complete_exchange(&mut live, "four", "four");
                }
                6 => {
                    live.apply_edit(
                        ContextOp::Fold {
                            through_exchange: 1,
                            digest: "first".into(),
                        },
                        EditAuthority::Harness,
                    )
                    .unwrap();
                    live.apply_edit(
                        ContextOp::Fold {
                            through_exchange: 1,
                            digest: "second".into(),
                        },
                        EditAuthority::Model,
                    )
                    .unwrap();
                }
                7 => {
                    live.record_error("forensics".into()).unwrap();
                    live.record_nudge(1, 2, "retry".into()).unwrap();
                }
                _ => unreachable!(),
            }
            let expected = serde_json::to_vec(&live.history_messages()).unwrap();
            drop(live);
            let resumed = AgentLog::resume(sessions.path(), 0).expect("resume edit sequence");
            assert!(resumed.is_ready());
            assert_eq!(
                serde_json::to_vec(&resumed.history_messages()).unwrap(),
                expected,
                "pattern {pattern}"
            );
        }
    }

    #[test]
    fn mirror_only_roots_have_no_durable_records() {
        let sessions = sessions_root("no-logs");
        let log = AgentLog::root_without_logs(sessions.path(), 0, "model", "provider", 0).unwrap();
        assert!(!log.is_durable());
        assert!(!log.dir().join("record.jsonl").exists());
        assert!(!log.dir().join("transcript.jsonl").exists());
    }
}
