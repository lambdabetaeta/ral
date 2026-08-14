//! Per-session canonical event log: one session's stable ledger and the
//! `events.jsonl` it is appended to.
//!
//! This is the log whose fold is the model view; `tui::viewport` writes the
//! rendered `user.log` beside it, `agent::transcript` the operational
//! `transcript.jsonl`.
//!
//! `events.jsonl` is one compact JSON value per line, so standard line tools
//! can inspect it while `serde_json::Deserializer` still reads it back.

use crate::bus::AgentId;
use crate::provider::{ProviderError, Tuning, Usage};
use crate::record::{Fold as _, Record};
use genai::chat::{ChatMessage, ChatRole, ToolResponse};
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashSet};
use std::fmt::Write as _;
use std::fs::{self, File, OpenOptions};
use std::io::{self, BufWriter, Read, Seek, SeekFrom, Write};
use std::ops::Range;
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
/// `tui::line` renders from this shape, so `events.jsonl` reconstructs the
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
        at_unix_ms: u64,
    },
    SessionResumed {
        model: String,
        provider: String,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    },
    /// A user-role turn: the seed or typed prompt, a nudge continuation the
    /// attend loop self-posts, or a steering message drained after a tool batch.
    UserPrompt {
        exchange: u64,
        text: String,
    },
    /// A message a `mnemon` child inherits from its parent's context.  Replayed
    /// to the provider verbatim but drives no state transition — the child's own
    /// [`Self::UserPrompt`] is still the turn it must answer.
    ContextMessage {
        exchange: u64,
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
    ContextEdited {
        op: ContextOp,
        by: EditAuthority,
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
            Self::UserPrompt { text, .. } => vec![ChatMessage::user(text)],
            Self::ContextMessage { message, .. }
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

/// The [`Record`] twin a [`SessionEvent`] authors through the seam —
/// field-identical, one arm per variant, so the funnel in
/// [`AgentLog::record`] covers every fact this type can mint and a missed
/// pair cannot arise.  Dies with `SessionEvent` itself in wave 2, when the
/// twin becomes the only form.
fn record_twin(ev: &SessionEvent) -> Record {
    use crate::record::{Forensic, Protocol};
    match ev {
        SessionEvent::SessionStarted {
            session_id,
            parent,
            model,
            provider,
            system_prompt_bytes,
            log_dir,
            at_unix_ms,
        } => Record::Protocol(Protocol::SessionStarted {
            session_id: *session_id,
            parent: *parent,
            model: model.clone(),
            provider: provider.clone(),
            system_prompt_bytes: *system_prompt_bytes,
            log_dir: log_dir.clone(),
            at_unix_ms: *at_unix_ms,
        }),
        SessionEvent::SessionResumed {
            model,
            provider,
            system_prompt_bytes,
            at_unix_ms,
        } => Record::Protocol(Protocol::SessionResumed {
            model: model.clone(),
            provider: provider.clone(),
            system_prompt_bytes: *system_prompt_bytes,
            at_unix_ms: *at_unix_ms,
        }),
        SessionEvent::SessionEnded => Record::Protocol(Protocol::SessionEnded),
        SessionEvent::UserPrompt { exchange, text } => Record::Protocol(Protocol::UserPrompt {
            exchange: *exchange,
            text: text.clone(),
        }),
        SessionEvent::ContextMessage { exchange, message } => {
            Record::Protocol(Protocol::ContextMessage {
                exchange: *exchange,
                message: message.clone(),
            })
        }
        SessionEvent::StepStarted { n, tuning } => Record::Protocol(Protocol::StepStarted {
            n: *n,
            tuning: tuning.clone(),
        }),
        SessionEvent::AssistantMessage {
            message,
            pending_tool_ids,
            stop_reason,
        } => Record::Protocol(Protocol::AssistantMessage {
            message: message.clone(),
            pending_tool_ids: pending_tool_ids.clone(),
            stop_reason: stop_reason.clone(),
        }),
        SessionEvent::ToolResults { results } => Record::Protocol(Protocol::ToolResults {
            results: results.clone(),
        }),
        SessionEvent::ContextEdited { op, by } => Record::Protocol(Protocol::ContextEdited {
            op: op.clone(),
            by: *by,
        }),
        SessionEvent::UsageDelta { usage } => Record::Forensic(Forensic::UsageDelta { usage: *usage }),
        SessionEvent::Cancelled => Record::Forensic(Forensic::Cancelled),
        SessionEvent::Error { text } => Record::Forensic(Forensic::Error { text: text.clone() }),
        SessionEvent::Nudge { used, max, cause } => Record::Forensic(Forensic::Nudge {
            used: *used,
            max: *max,
            cause: cause.clone(),
        }),
        SessionEvent::ProviderError { error } => Record::Forensic(Forensic::ProviderError {
            error: error.clone(),
        }),
        SessionEvent::Stalled { error } => Record::Forensic(Forensic::Stalled {
            error: error.clone(),
        }),
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

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Digest {
    pub through_exchange: u64,
    pub text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub id: u64,
    pub events: Range<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    pub digest: Option<Digest>,
    pub spans: Vec<Span>,
}

/// The append-only event sequence, with stable indices.  The disk log is the
/// identity; a resident event is a cache the edit step discards the moment no
/// view span needs it.  Resident memory is therefore O(view): a freed event
/// keeps only the byte range of its line in `events.jsonl`, coalesced with
/// its neighbours into [`FreedRun`]s, and a lossy event that never reached
/// the disk keeps nothing.
struct Ledger {
    len: usize,
    resident: BTreeMap<usize, (SessionEvent, Option<Range<u64>>)>,
    freed: Vec<FreedRun>,
    events_path: Option<PathBuf>,
}

/// A maximal run of adjacent freed lines: `bytes` in `events.jsonl`, `events`
/// in the stable ledger index space.  Only the from-scratch fold reads one.
struct FreedRun {
    bytes: Range<u64>,
    events: Range<usize>,
}

impl Ledger {
    fn new(events_path: Option<PathBuf>) -> Self {
        Self {
            len: 0,
            resident: BTreeMap::new(),
            freed: Vec::new(),
            events_path,
        }
    }

    fn append(&mut self, event: SessionEvent, offset: Option<Range<u64>>) -> usize {
        let index = self.len;
        self.len += 1;
        self.resident.insert(index, (event, offset));
        index
    }

    fn len(&self) -> usize {
        self.len
    }

    fn event(&self, index: usize) -> Option<&SessionEvent> {
        self.resident.get(&index).map(|(event, _)| event)
    }

    fn last(&self) -> Option<&SessionEvent> {
        self.len.checked_sub(1).and_then(|index| self.event(index))
    }

    fn resident_events(&self, range: Range<usize>) -> Option<Vec<&SessionEvent>> {
        let events: Vec<&SessionEvent> = self
            .resident
            .range(range.clone())
            .map(|(_, (event, _))| event)
            .collect();
        (events.len() == range.len()).then_some(events)
    }

    fn free(&mut self, index: usize) {
        if let Some((_, offset)) = self.resident.remove(&index)
            && let Some(bytes) = offset
        {
            self.insert_freed(FreedRun {
                bytes,
                events: index..index + 1,
            });
        }
    }

    fn evict(&mut self, ranges: &[Range<usize>]) {
        for index in ranges.iter().cloned().flatten() {
            self.free(index);
        }
    }

    fn insert_freed(&mut self, run: FreedRun) {
        let at = self
            .freed
            .partition_point(|existing| existing.events.end <= run.events.start);
        self.freed.insert(at, run);
        if at + 1 < self.freed.len() && runs_adjacent(&self.freed[at], &self.freed[at + 1]) {
            let right = self.freed.remove(at + 1);
            self.freed[at].events.end = right.events.end;
            self.freed[at].bytes.end = right.bytes.end;
        }
        if at > 0 && runs_adjacent(&self.freed[at - 1], &self.freed[at]) {
            let right = self.freed.remove(at);
            self.freed[at - 1].events.end = right.events.end;
            self.freed[at - 1].bytes.end = right.bytes.end;
        }
    }

    fn fold_events(&self) -> io::Result<Vec<(usize, SessionEvent)>> {
        let mut events: Vec<(usize, SessionEvent)> = self
            .resident
            .iter()
            .map(|(index, (event, _))| (*index, event.clone()))
            .collect();
        if !self.freed.is_empty() {
            let Some(path) = self.events_path.as_ref() else {
                return Err(io::Error::other(
                    "the freed event ledger has no readable events.jsonl",
                ));
            };
            let mut file = File::open(path)?;
            for run in &self.freed {
                read_freed_run(&mut file, run, &mut events)?;
            }
            events.sort_unstable_by_key(|(index, _)| *index);
        }
        Ok(events)
    }

    #[cfg(test)]
    fn is_resident(&self, index: usize) -> bool {
        self.event(index).is_some()
    }

    #[cfg(test)]
    fn freed_ranges(&self) -> Vec<Range<u64>> {
        self.freed.iter().map(|run| run.bytes.clone()).collect()
    }
}

fn runs_adjacent(left: &FreedRun, right: &FreedRun) -> bool {
    left.events.end == right.events.start && left.bytes.end == right.bytes.start
}

/// Read one freed run's lines back off `events.jsonl`, one event per line.
fn read_freed_run(
    reader: &mut File,
    run: &FreedRun,
    output: &mut Vec<(usize, SessionEvent)>,
) -> io::Result<()> {
    let length = usize::try_from(run.bytes.end.saturating_sub(run.bytes.start))
        .map_err(|_| io::Error::other("a freed event range is too large to read"))?;
    reader.seek(SeekFrom::Start(run.bytes.start))?;
    let mut bytes = vec![0; length];
    reader.read_exact(&mut bytes)?;
    let mut index = run.events.start;
    for line in bytes.split_inclusive(|byte| *byte == b'\n') {
        if line.last() != Some(&b'\n') {
            return Err(io::Error::other(
                "a freed event range does not end on a complete JSONL record",
            ));
        }
        if index == run.events.end {
            return Err(io::Error::other(
                "a freed event range holds more lines than ledger indices",
            ));
        }
        let event = serde_json::from_slice(&line[..line.len() - 1]).map_err(io::Error::other)?;
        output.push((index, event));
        index += 1;
    }
    if index != run.events.end {
        return Err(io::Error::other(
            "a freed event range holds fewer lines than ledger indices",
        ));
    }
    Ok(())
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Projection {
    state: State,
    view: View,
    max_exchange: u64,
    newest_edit: Option<usize>,
}

impl Default for Projection {
    fn default() -> Self {
        Self {
            state: State::ReadyForUser,
            view: View::default(),
            max_exchange: 0,
            newest_edit: None,
        }
    }
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
    fn add(&mut self, item: ContextSurveyItem) {
        self.total_bytes = self.total_bytes.saturating_add(item.bytes);
        self.total_steps = self.total_steps.saturating_add(item.steps);
        self.items.push(item);
    }
}

/// One session's log: a stable-index ledger, its `events.jsonl` writer, and
/// the projection folded from that ledger.
pub struct AgentLog {
    id: AgentId,
    dir: PathBuf,
    /// Held open, flushed per event, so the file is always tail-able.
    events_file: Option<BufWriter<File>>,
    log: Ledger,
    projection: Projection,
    durable: bool,
    events_offset: u64,
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
    /// The model fold's own projection, over `record.jsonl` — parcel P1 of
    /// `dev/docs/plans/260814_one_seam_one_log.md`.  Advanced inline through
    /// [`Self::advance`] on every fact [`Self::record`] emits, so it stays
    /// consistent with the ledger above by construction; nothing reads from
    /// it yet, since every method above still answers from the ledger it has
    /// always used.  `events.jsonl`'s retirement (wave 2) is what turns this
    /// from a shadow projection into the one this type answers from.
    model_memo: crate::record::model::Memo,
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
    pub(crate) events_error: Option<io::Error>,
}

pub(crate) fn summary_prompt(summary: &str) -> String {
    format!("Summary of prior work in this session:\n\n{summary}")
}

fn opening_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

fn opening_line_for_events(events: &[&SessionEvent]) -> String {
    for event in events {
        match event {
            SessionEvent::UserPrompt { text, .. } => return opening_line(text),
            SessionEvent::ContextMessage { message, .. } if message.role == ChatRole::User => {
                if let Some(text) = message.content.first_text() {
                    return opening_line(text);
                }
            }
            _ => {}
        }
    }
    String::new()
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

    /// Build a mirror-only root without creating events.jsonl.
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

    /// Read the trunk event ledger, repair only a torn final fragment, and
    /// reopen the live file for append.
    ///
    /// # Errors
    /// Returns an error when the ledger is missing, malformed, foreign, or
    /// cannot be reopened after validation.
    pub fn resume(sessions_root: &Path, session_id: AgentId) -> io::Result<Self> {
        if session_id != 0 {
            return Err(io::Error::other(format!(
                "cannot resume session {session_id}: children are transient by design and only session 0 can be resumed"
            )));
        }
        let dir = sessions_root.join(session_id.to_string());
        let events_path = dir.join("events.jsonl");
        let read = read_events(&events_path)?;
        let Some(first) = read.events.first() else {
            return Err(io::Error::other(format!(
                "cannot resume {}: no complete session events were found; is the file truncated?",
                events_path.display()
            )));
        };
        let (model, provider) = match &first.event {
            SessionEvent::SessionStarted {
                session_id: 0,
                parent: None,
                model,
                provider,
                ..
            } => (model.clone(), provider.clone()),
            SessionEvent::SessionStarted {
                session_id: found,
                parent,
                ..
            } => {
                return Err(io::Error::other(format!(
                    "cannot resume {}: line 1 starts session {found:?} with parent {parent:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a child log?",
                    events_path.display()
                )));
            }
            other => {
                return Err(io::Error::other(format!(
                    "cannot resume {}: line 1 is {other:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a copied child log?",
                    events_path.display()
                )));
            }
        };

        validate_resume_events(&read.events, &events_path)?;
        if let Some(fragment) = read.tail {
            quarantine_tail(&events_path, &fragment).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "cannot resume {}: could not quarantine the crash tail in events.jsonl.crash ({error})",
                        events_path.display()
                    ),
                )
            })?;
        }
        let events_file = open_events_append(&dir).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot resume {}: could not reopen events.jsonl in append mode ({error})",
                    events_path.display()
                ),
            )
        })?;
        // The model fold's own projection, off `record.jsonl` — quarantining
        // its torn tail first, so the seam reopened below appends to a clean
        // file.  A pre-plan session has no record log yet; its fold starts
        // empty and its file is created at the first live fact.
        let record_path = dir.join("record.jsonl");
        let model_memo = if record_path.exists() {
            match crate::record::model::resume(&record_path) {
                Ok(memo) => memo,
                Err(error) => {
                    eprintln!(
                        "exarch: could not fold {} into the model view yet ({error}); the shadow projection starts empty",
                        record_path.display()
                    );
                    crate::record::model::Memo::default()
                }
            }
        } else {
            crate::record::model::Memo::default()
        };
        let seam = crate::record::Emitter::append_to(&record_path)?;
        let mut resumed = Self {
            id: session_id,
            dir,
            events_file: Some(events_file),
            log: Ledger::new(Some(events_path.clone())),
            projection: Projection::default(),
            durable: true,
            events_offset: read.complete_len,
            model,
            provider,
            sessions_root: sessions_root.to_path_buf(),
            _scratch: None,
            model_memo,
            seam,
        };
        for recorded in read.events {
            resumed.append_mirror(recorded.event, Some(recorded.bytes));
        }
        let replayed = resumed.projection.clone();
        let folded = fold(&resumed.log).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "cannot resume {}: could not fold the recorded events ({error})",
                    events_path.display()
                ),
            )
        })?;
        if folded != replayed {
            return Err(io::Error::other(format!(
                "cannot resume {}: replay disagreed with the ledger fold",
                events_path.display()
            )));
        }
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
            fs::metadata(self.dir.join("events.jsonl")).map_or(0, |metadata| metadata.len());
        (self.projection.max_exchange, bytes)
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
        self.record(SessionEvent::SessionResumed {
            model: self.model.clone(),
            provider: self.provider.clone(),
            system_prompt_bytes,
            at_unix_ms,
        })
    }

    /// Advance the model fold's `record.jsonl` projection for one witnessed
    /// record — the entry point a caller on the attend thread calls
    /// immediately after `Emitter::emit` returns, per the one-seam-one-log
    /// law (`dev/docs/plans/260814_one_seam_one_log.md`).  A no-op for
    /// `Display`/`Forensic` records: this fold admits `Protocol` alone.
    ///
    /// Nothing in this type reads from the projection this advances yet —
    /// every query below still answers from the ledger `events.jsonl` has
    /// always fed — so this is safe to call from nowhere today and everywhere
    /// once a wave-2 cutover repoints the queries at it.
    pub fn advance(&mut self, record: &crate::record::Recorded<crate::record::Record>) {
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
        matches!(self.state(), State::ReadyForUser)
    }

    pub fn can_compact(&self) -> bool {
        self.is_ready()
    }

    /// Number of event slots still owned by the model view, for the host's
    /// resource probe; the append-only log retains the slots an edit removes.
    pub fn event_count(&self) -> usize {
        let events = self
            .projection
            .view
            .spans
            .iter()
            .map(|span| span.events.len())
            .sum::<usize>();
        events + usize::from(self.projection.view.digest.is_some())
    }

    pub fn view(&self) -> &View {
        &self.projection.view
    }

    /// # Panics
    /// Panics if a view span is not resident in the ledger.
    pub fn context_survey(&self) -> ContextSurvey {
        let mut survey = ContextSurvey::default();
        if let Some(digest) = &self.projection.view.digest {
            let item = ContextSurveyItem {
                exchange: digest.through_exchange,
                kind: ContextSpanKind::Digest,
                opening: opening_line(&digest.text),
                bytes: serde_json::to_string(&ChatMessage::user(summary_prompt(&digest.text)))
                    .map_or(0, |text| text.len()),
                steps: 0,
                live: false,
            };
            survey.add(item);
        }
        for span in &self.projection.view.spans {
            let events = self
                .log
                .resident_events(span.events.clone())
                .expect("view spans are resident in the ledger");
            let kind = match events.first() {
                Some(SessionEvent::ContextMessage { .. }) => ContextSpanKind::Import,
                _ => ContextSpanKind::Exchange,
            };
            let live = self.is_live_exchange(span.id);
            let item = ContextSurveyItem {
                exchange: span.id,
                kind,
                opening: opening_line_for_events(&events),
                bytes: self.span_message_bytes_with_repair(span, !live),
                steps: events
                    .iter()
                    .filter(|event| matches!(event, SessionEvent::StepStarted { .. }))
                    .count(),
                live,
            };
            survey.add(item);
        }
        survey
    }

    /// Read named, closed spans in their model-view order.
    ///
    /// # Errors
    /// Refuses an empty list, a duplicate name, a missing or folded exchange,
    /// or the exchange still in progress.
    pub fn read_context(&self, exchanges: &[u64]) -> Result<String, String> {
        if exchanges.is_empty() {
            return Err("context-read must name at least one exchange".into());
        }
        self.validate_named_exchanges(exchanges)?;
        Ok(render_context_transcript(
            &self.log,
            &self.projection.view,
            &exchanges.iter().copied().collect(),
        ))
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
            self.state(),
            State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults
        ) {
            return Err(format!(
                "cannot render request while session is in state {:?}",
                self.state()
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
        let omit_tail_assistant = matches!(self.state(), State::AwaitingToolResults { .. })
            && matches!(
                self.log.last(),
                Some(SessionEvent::AssistantMessage {
                    pending_tool_ids,
                    ..
                }) if !pending_tool_ids.is_empty()
            );

        self.project_view(omit_tail_assistant)
    }

    /// Import parent context without appending a prompt: a `mnemon` child's
    /// launch prompt arrives through its inbox, and `deliberate` commits it
    /// through [`Self::append_user`] like any other exchange.
    ///
    /// # Errors
    /// The session is not at a ready boundary, or recording a message failed.
    pub fn import_context(&mut self, messages: Vec<ChatMessage>) -> Result<(), String> {
        if !matches!(self.state(), State::ReadyForUser) {
            return Err(format!(
                "cannot import context while session is in state {:?}",
                self.state()
            ));
        }
        let exchange = self.next_exchange();
        for message in messages {
            self.record_or_string_err(SessionEvent::ContextMessage { exchange, message })?;
        }
        Ok(())
    }

    // ── Protocol mutations ────────────────────────────────────────────────

    /// Commit a top-level user prompt, opening a new exchange.
    ///
    /// # Errors
    /// The session is mid-exchange, or recording the prompt failed.
    pub fn append_user(&mut self, text: String, continues: Option<u64>) -> Result<(), String> {
        if !matches!(self.state(), State::ReadyForUser) {
            return Err(format!(
                "cannot accept a new user prompt while session is in state {:?}",
                self.state()
            ));
        }
        let exchange = continues
            .filter(|id| *id == self.projection.max_exchange)
            .filter(|id| self.projection.view.spans.iter().any(|span| span.id == *id))
            .unwrap_or_else(|| self.next_exchange());
        self.record_or_string_err(SessionEvent::UserPrompt { exchange, text })
    }

    /// Append a user message between a complete tool-result batch and the next
    /// provider request.  The only mid-exchange user ingress there is, which is
    /// why it admits [`State::AwaitingAssistantAfterToolResults`] alone.
    ///
    /// # Errors
    /// The tool-result batch is incomplete, or recording the prompt failed.
    pub fn append_steering(&mut self, text: String) -> Result<(), String> {
        if !matches!(self.state(), State::AwaitingAssistantAfterToolResults) {
            return Err(format!(
                "tool results must be complete before accepting a steering prompt; session is in state {:?}",
                self.state()
            ));
        }
        let exchange = self.projection.max_exchange;
        if exchange == 0 {
            return Err("cannot accept a steering prompt before an exchange has started".into());
        }
        self.record_or_string_err(SessionEvent::UserPrompt { exchange, text })
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
            self.state(),
            State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults
        ) {
            return Err(format!(
                "assistant message is not expected while session is in state {:?}",
                self.state()
            ));
        }
        if message.role != ChatRole::Assistant {
            return Err(format!(
                "assistant message has role {:?}; expected Assistant",
                message.role,
            ));
        }
        self.record_or_string_err(SessionEvent::AssistantMessage {
            message,
            pending_tool_ids,
            stop_reason,
        })?;
        Ok(())
    }

    /// Commit the tool-result batch answering the pending tool calls.
    ///
    /// # Errors
    /// No results are expected, they do not match the pending ids (wrong count,
    /// unknown id, duplicate), or recording the batch failed.
    pub fn append_tool_results(&mut self, results: Vec<ToolResult>) -> Result<(), String> {
        let pending_ids = match self.state() {
            State::AwaitingToolResults { pending_ids } => pending_ids.clone(),
            _ => {
                return Err(format!(
                    "tool results are not expected while session is in state {:?}",
                    self.state()
                ));
            }
        };
        validate_result_ids(&pending_ids, &results)?;
        self.record_or_string_err(SessionEvent::ToolResults { results })?;
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
        let stubs = stubs_to_ready(self.state(), reason);
        let quiesced = !stubs.is_empty();
        for stub in stubs {
            self.record_lossy(stub);
        }
        // Only a cancellation earns its own breadcrumb; an abort's marker is
        // the `ProviderError` already on disk.
        if quiesced
            && matches!(reason, QuiesceReason::Cancelled)
            && let Err(error) = self.record(SessionEvent::Cancelled)
        {
            eprintln!("exarch: the cancellation breadcrumb was not recorded: {error}");
        }
    }

    pub fn plan_compaction(&self, keep_budget_bytes: usize) -> Option<CompactionPlan> {
        self.plan_compaction_impl(keep_budget_bytes, None)
    }

    pub fn plan_compaction_before(
        &self,
        keep_budget_bytes: usize,
        before_exchange: u64,
    ) -> Option<CompactionPlan> {
        self.plan_compaction_impl(keep_budget_bytes, Some(before_exchange))
    }

    fn plan_compaction_impl(
        &self,
        keep_budget_bytes: usize,
        before_exchange: Option<u64>,
    ) -> Option<CompactionPlan> {
        let spans = &self.projection.view.spans;
        let cap = before_exchange.map(|id| {
            spans
                .iter()
                .position(|span| span.id >= id)
                .unwrap_or(spans.len())
        });
        let mut suffix_start = cap.unwrap_or(spans.len());
        if suffix_start == 0 {
            return None;
        }

        let mut suffix_bytes = spans[suffix_start..]
            .iter()
            .map(|span| self.span_message_bytes(span))
            .sum::<usize>();
        for index in (0..suffix_start).rev() {
            let bytes = self.span_message_bytes(&spans[index]);
            let Some(total) = suffix_bytes.checked_add(bytes) else {
                break;
            };
            if total > keep_budget_bytes {
                break;
            }
            suffix_bytes = total;
            suffix_start = index;
        }
        if suffix_start == 0 {
            return None;
        }

        Some(CompactionPlan {
            through_exchange: spans[suffix_start - 1].id,
            prefix_messages: self.messages_for_spans(&spans[..suffix_start]),
        })
    }

    /// # Errors
    /// Refuses an empty drop, an unknown or folded exchange, or a live exchange.
    pub fn apply_edit(&mut self, op: ContextOp, by: EditAuthority) -> Result<EditReceipt, String> {
        self.validate_edit(&op)?;
        let before = self.history_bytes();
        self.record_or_string_err(SessionEvent::ContextEdited { op: op.clone(), by })?;
        let after = self.history_bytes();
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
        if let Some(reach) = self.digest_reach() {
            if anchor < reach {
                return Err(folded_exchange_refusal(anchor, reach));
            }
            if anchor == reach {
                let mut exchanges = vec![anchor];
                exchanges.extend(
                    self.projection
                        .view
                        .spans
                        .iter()
                        .filter(|span| span.id >= anchor)
                        .map(|span| span.id),
                );
                return Ok(exchanges);
            }
        }
        if !self
            .projection
            .view
            .spans
            .iter()
            .any(|span| span.id == anchor)
        {
            return Err(rewind_unknown_exchange_refusal(
                anchor,
                self.last_view_exchange(),
            ));
        }
        Ok(self
            .projection
            .view
            .spans
            .iter()
            .filter(|span| span.id >= anchor)
            .map(|span| span.id)
            .collect())
    }

    fn validate_edit(&self, op: &ContextOp) -> Result<(), String> {
        match op {
            ContextOp::Fold {
                through_exchange, ..
            } => self.validate_closed_present(*through_exchange),
            ContextOp::Drop { exchanges } => {
                if exchanges.is_empty() {
                    return Err("a context edit must name at least one exchange".into());
                }
                self.validate_named_exchanges(exchanges)
            }
        }
    }

    /// Each named exchange must be addressable and closed, and named once.
    fn validate_named_exchanges(&self, exchanges: &[u64]) -> Result<(), String> {
        let mut named = HashSet::with_capacity(exchanges.len());
        for &exchange in exchanges {
            if !named.insert(exchange) {
                return Err(format!("exchange {exchange} was named more than once"));
            }
            self.validate_closed_present(exchange)?;
        }
        Ok(())
    }

    /// An exchange is addressable iff it survives in the view — as a span, or
    /// as the digest named by its reach — and closed iff it is not the live
    /// one.
    fn validate_closed_present(&self, exchange: u64) -> Result<(), String> {
        if let Some(reach) = self.digest_reach() {
            if exchange < reach {
                return Err(folded_exchange_refusal(exchange, reach));
            }
            if exchange == reach {
                return Ok(());
            }
        }
        if !self
            .projection
            .view
            .spans
            .iter()
            .any(|span| span.id == exchange)
        {
            return Err(unknown_exchange_refusal(exchange));
        }
        if self.is_live_exchange(exchange) {
            return Err(live_exchange_refusal(exchange));
        }
        Ok(())
    }

    fn digest_reach(&self) -> Option<u64> {
        self.projection
            .view
            .digest
            .as_ref()
            .map(|digest| digest.through_exchange)
    }

    /// `/clear`: wipe the events in memory and on disk, then restart with a
    /// fresh `SessionStarted` so the file stays self-consistent.  A cleared
    /// session is rooted again, so `parent` is dropped, but model and provider
    /// survive.
    ///
    /// # Errors
    /// Reopening `events.jsonl`, or writing `SessionStarted`, failed.
    pub(crate) fn clear(
        &mut self,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
        transcript_path: &Path,
    ) -> io::Result<ClearRecord> {
        if !self.durable {
            self.log = Ledger::new(None);
            self.projection = Projection::default();
            self.model_memo = crate::record::model::Memo::default();
            self.seam = crate::record::Emitter::none();
            self.record_started_lossy(None, system_prompt_bytes, at_unix_ms);
            return Ok(ClearRecord {
                rotation: None,
                events_error: None,
            });
        }

        let events_path = self.events_path();
        if let Some(events_file) = self.events_file.as_mut() {
            events_file.flush().map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "clear was not committed for {}; the event log could not be flushed: {error}",
                        events_path.display()
                    ),
                )
            })?;
        }
        drop(self.events_file.take());

        let record_path = self.dir.join("record.jsonl");
        let rotation = match first_free_rotation(&events_path, transcript_path, &record_path) {
            Ok(n) => n,
            Err(error) => return Err(self.refuse_clear(&error)),
        };
        let rotated = rotation_path(&events_path, rotation);
        if let Err(error) = fs::rename(&events_path, &rotated) {
            return Err(self.refuse_clear(&error));
        }
        let record_error = self.rotate_record(&record_path, rotation);

        self.log = Ledger::new(Some(events_path));
        self.projection = Projection::default();
        self.model_memo = crate::record::model::Memo::default();
        self.events_offset = 0;
        let started = self.started_event(None, system_prompt_bytes, at_unix_ms);
        let seam_error = self.emit_twin(&started).err();
        let events_error = self.establish_head(started);
        Ok(ClearRecord {
            rotation: Some(rotation),
            events_error: join_errors([record_error, seam_error, events_error]),
        })
    }

    /// Rotate `record.jsonl` beside the event log, opening a fresh segment.
    /// A rename or create failure never truncates over the old segment — no
    /// recorded event is ever removed — so the seam keeps appending to
    /// whatever file survives, and the failure is reported, not shrugged.
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:record-file] rotates the session's record.jsonl on /clear; output infra, not turn-time data I/O"
    )]
    fn rotate_record(&mut self, path: &Path, rotation: u64) -> Option<io::Error> {
        let rotated = rotation_path(path, rotation);
        match fs::rename(path, &rotated) {
            Ok(()) => {}
            // A pre-plan session with no record log yet: nothing to keep,
            // and a fresh segment is still due.
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
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

    // ── Meta-events ───────────────────────────────────────────────────────
    //
    // One non-protocol breadcrumb each; all fail only on serialising or
    // writing to `events.jsonl`.

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
        durable: bool,
    ) -> io::Result<Self> {
        let dir = sessions_root.join(session_id.to_string());
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        let events_file = durable.then(|| open_events_file(&dir)).transpose()?;
        let events_path = durable.then(|| dir.join("events.jsonl"));
        let seam = if durable {
            crate::record::Emitter::create(&dir.join("record.jsonl"))?
        } else {
            crate::record::Emitter::none()
        };
        Ok(Self {
            id: session_id,
            dir,
            events_file,
            log: Ledger::new(events_path),
            projection: Projection::default(),
            durable,
            events_offset: 0,
            model,
            provider,
            sessions_root,
            _scratch: None,
            model_memo: crate::record::model::Memo::default(),
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
        self.record(self.started_event(parent, system_prompt_bytes, at_unix_ms))
    }

    fn record_started_lossy(
        &mut self,
        parent: Option<AgentId>,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) {
        self.record_lossy(self.started_event(parent, system_prompt_bytes, at_unix_ms));
    }

    fn started_event(
        &self,
        parent: Option<AgentId>,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) -> SessionEvent {
        SessionEvent::SessionStarted {
            session_id: self.id,
            parent,
            model: self.model.clone(),
            provider: self.provider.clone(),
            system_prompt_bytes,
            log_dir: self.dir.clone(),
            at_unix_ms,
        }
    }

    fn events_path(&self) -> PathBuf {
        self.dir.join("events.jsonl")
    }

    fn refuse_clear(&mut self, error: &io::Error) -> io::Error {
        match open_events_append(&self.dir) {
            Ok(events_file) => {
                self.events_file = Some(events_file);
                self.events_offset =
                    fs::metadata(self.events_path()).map_or(0, |metadata| metadata.len());
                io::Error::new(
                    error.kind(),
                    format!(
                        "clear was not committed for {}; the event log was reopened in append mode: {error}",
                        self.events_path().display()
                    ),
                )
            }
            Err(reopen) => {
                self.events_file = None;
                io::Error::other(format!(
                    "clear was not committed for {}; the event log could not be reopened after {error}: {reopen}",
                    self.events_path().display()
                ))
            }
        }
    }

    fn establish_head(&mut self, started: SessionEvent) -> Option<io::Error> {
        let mut last_error = None;
        for _ in 0..2 {
            self.events_file = match open_events_file(&self.dir) {
                Ok(events_file) => Some(events_file),
                Err(error) => {
                    last_error = Some(error);
                    None
                }
            };
            self.events_offset = 0;
            if self.events_file.is_none() {
                continue;
            }
            match self.write_event(&started) {
                Ok(offset) => {
                    self.append_mirror(started, offset);
                    return None;
                }
                Err(error) => {
                    last_error = Some(error);
                    drop(self.events_file.take());
                }
            }
        }
        self.events_file = None;
        self.append_mirror(started, None);
        Some(io::Error::other(format!(
            "the new event-log head {} could not be established after two attempts; the session is continuing without durable events: {}",
            self.events_path().display(),
            last_error.map_or_else(|| "unknown I/O error".into(), |error| error.to_string())
        )))
    }

    /// The disk half of [`Self::record`], apart so [`Self::record_lossy`] can
    /// go on without it.
    fn write_event(&mut self, ev: &SessionEvent) -> io::Result<Option<Range<u64>>> {
        if !self.durable {
            return Ok(None);
        }
        let mut bytes = serde_json::to_vec(ev).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let length = u64::try_from(bytes.len())
            .map_err(|_| io::Error::other("the serialized session event is too large"))?;
        let start = self.events_offset;
        let end = start
            .checked_add(length)
            .ok_or_else(|| io::Error::other("the session event log is too large"))?;
        let Some(events_file) = self.events_file.as_mut() else {
            return Err(io::Error::other("the session event log is not writable"));
        };
        events_file.write_all(&bytes)?;
        events_file.flush()?;
        self.events_offset = end;
        Ok(Some(start..end))
    }

    /// The one funnel every fact takes: the record seam first — durable and
    /// published in one breath, the model fold advanced on the witnessed
    /// record — then the retiring `events.jsonl` engine, kept in step until
    /// wave 2 deletes it.  Seam-first, so the authoritative log never misses
    /// a fact the old one has.
    fn record(&mut self, ev: SessionEvent) -> io::Result<()> {
        self.emit_twin(&ev)?;
        let offset = self.write_event(&ev)?;
        self.append_mirror(ev, offset);
        Ok(())
    }

    /// Author `ev`'s [`Record`] twin through the seam and advance the model
    /// fold on the witnessed result.
    ///
    /// # Errors
    /// A failed append to the one authoritative log is a session error.
    fn emit_twin(&mut self, ev: &SessionEvent) -> io::Result<()> {
        let recorded = match record_twin(ev) {
            Record::Protocol(p) => crate::record::widen(self.seam.emit(p)?),
            Record::Forensic(f) => crate::record::widen(self.seam.emit(f)?),
            Record::Display(_) => {
                unreachable!("no SessionEvent maps to a display commit")
            }
        };
        self.advance(&recorded);
        Ok(())
    }

    /// [`Self::record`] in the `Result<(), String>` the protocol mutations
    /// return.
    fn record_or_string_err(&mut self, ev: SessionEvent) -> Result<(), String> {
        self.record(ev).map_err(|e| e.to_string())
    }

    /// [`Self::record`] with the disk write made best-effort.  Its remit is
    /// exactly the harness-synthesized events where refusing would wedge the
    /// session: [`Self::quiesce`]'s stubs, which must reach
    /// [`State::ReadyForUser`] even when `events.jsonl` cannot be written,
    /// and the `SessionStarted` bookends of a mirror-only session.
    fn record_lossy(&mut self, ev: SessionEvent) {
        if let Err(error) = self.emit_twin(&ev) {
            eprintln!("exarch: a harness-synthesized event was not recorded in record.jsonl: {error}");
        }
        let offset = self.write_event(&ev).ok().flatten();
        self.append_mirror(ev, offset);
    }

    fn append_mirror(&mut self, event: SessionEvent, offset: Option<Range<u64>>) {
        step(&mut self.log, &mut self.projection, event, offset);
    }

    fn state(&self) -> &State {
        &self.projection.state
    }

    fn next_exchange(&self) -> u64 {
        self.projection.max_exchange.saturating_add(1)
    }

    pub fn current_exchange(&self) -> Option<u64> {
        (self.projection.max_exchange != 0).then_some(self.projection.max_exchange)
    }

    fn last_view_exchange(&self) -> Option<u64> {
        self.projection
            .view
            .spans
            .last()
            .map(|span| span.id)
            .or_else(|| {
                self.projection
                    .view
                    .digest
                    .as_ref()
                    .map(|digest| digest.through_exchange)
            })
    }

    pub fn log_len(&self) -> usize {
        self.log.len()
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.projection
            .newest_edit
            .is_some_and(|index| index >= measured_at)
    }

    fn is_live_exchange(&self, id: u64) -> bool {
        !self.is_ready() && self.projection.max_exchange == id
    }

    fn span_message_bytes(&self, span: &Span) -> usize {
        self.span_message_bytes_with_repair(span, true)
    }

    fn span_message_bytes_with_repair(&self, span: &Span, repair_end: bool) -> usize {
        self.span_messages(span, false, repair_end)
            .iter()
            .map(|message| serde_json::to_string(message).map_or(0, |s| s.len()))
            .sum()
    }

    fn messages_for_spans(&self, spans: &[Span]) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        if let Some(digest) = &self.projection.view.digest {
            messages.push(ChatMessage::user(summary_prompt(&digest.text)));
        }
        for span in spans {
            messages.extend(self.span_messages(span, false, true));
        }
        messages
    }

    fn project_view(&self, omit_tail_assistant: bool) -> Vec<ChatMessage> {
        let spans = &self.projection.view.spans;
        let mut messages = Vec::new();
        if let Some(digest) = &self.projection.view.digest {
            messages.push(ChatMessage::user(summary_prompt(&digest.text)));
        }
        for (index, span) in spans.iter().enumerate() {
            let omit = omit_tail_assistant && index + 1 == spans.len();
            let repair_end = index + 1 < spans.len() && !omit;
            messages.extend(self.span_messages(span, omit, repair_end));
        }
        messages
    }

    fn span_messages(
        &self,
        span: &Span,
        omit_tail_assistant: bool,
        repair_end: bool,
    ) -> Vec<ChatMessage> {
        let end = span.events.end - usize::from(omit_tail_assistant);
        let events = self
            .log
            .resident_events(span.events.start..end)
            .expect("view spans are resident in the ledger");
        if matches!(events.first(), Some(SessionEvent::ContextMessage { .. })) {
            return events
                .into_iter()
                .cloned()
                .flat_map(SessionEvent::into_chat_messages)
                .collect();
        }

        let mut state = State::ReadyForUser;
        let mut messages = Vec::new();
        for event in events {
            if !admissible_event(&state, event) && stub_repairable(event) {
                append_aborted_stubs(&mut state, &mut messages);
            }
            messages.extend(event.clone().into_chat_messages());
            state = advance(&state, event);
        }
        if repair_end && !matches!(state, State::ReadyForUser) {
            append_aborted_stubs(&mut state, &mut messages);
        }
        messages
    }

    fn model_messages(&self) -> Vec<ChatMessage> {
        self.project_view(false)
    }
}

fn advance(state: &State, event: &SessionEvent) -> State {
    match event {
        SessionEvent::UserPrompt { .. } => State::AwaitingAssistantAfterUser,
        SessionEvent::AssistantMessage {
            pending_tool_ids, ..
        } if !pending_tool_ids.is_empty() => State::AwaitingToolResults {
            pending_ids: pending_tool_ids.clone(),
        },
        SessionEvent::AssistantMessage { .. } => State::ReadyForUser,
        SessionEvent::ToolResults { .. } => State::AwaitingAssistantAfterToolResults,
        _ => state.clone(),
    }
}

/// One step of the fold: append to the ledger, advance the projection, and
/// free whatever the view no longer needs — the spans an edit removed, and
/// any event committed past a removed tail span, which belongs to no
/// surviving span and would otherwise stay resident forever.  `record` takes
/// this step live; replay takes the same steps from the first line.
fn step(
    log: &mut Ledger,
    projection: &mut Projection,
    event: SessionEvent,
    offset: Option<Range<u64>>,
) {
    let index = log.append(event, offset);
    let removed = {
        let event = log.event(index).expect("a fresh ledger slot is resident");
        advance_projection(projection, event, index)
    };
    log.evict(&removed);
    let in_view = projection
        .view
        .spans
        .iter()
        .any(|span| span.events.contains(&index));
    if projection.max_exchange > 0 && !in_view {
        log.free(index);
    }
}

fn advance_projection(
    projection: &mut Projection,
    event: &SessionEvent,
    index: usize,
) -> Vec<Range<usize>> {
    projection.state = advance(&projection.state, event);
    record_event_span(projection, event, index);
    if let SessionEvent::ContextEdited { op, .. } = event {
        let removed = removed_event_ranges(&projection.view, op);
        apply_context_op(&mut projection.view, op);
        projection.newest_edit = Some(index);
        removed
    } else {
        Vec::new()
    }
}

fn removed_event_ranges(view: &View, op: &ContextOp) -> Vec<Range<usize>> {
    match op {
        ContextOp::Fold {
            through_exchange, ..
        } => view
            .spans
            .iter()
            .filter(|span| span.id <= *through_exchange)
            .map(|span| span.events.clone())
            .collect(),
        ContextOp::Drop { exchanges } => view
            .spans
            .iter()
            .filter(|span| exchanges.contains(&span.id))
            .map(|span| span.events.clone())
            .collect(),
    }
}

/// The partition half of the step: an id-bearing event extends the span with
/// its id at the tail or opens a new one past the running maximum; every
/// other event extends whichever span last grew.  An id that can do neither
/// joins nothing — no live door records one, and the resume door refuses such
/// a line as foreign — so the step stays total without a dead event ever
/// arising.
fn record_event_span(projection: &mut Projection, event: &SessionEvent, index: usize) {
    if let Some(id) = event_exchange(event) {
        let extended = projection
            .view
            .spans
            .iter_mut()
            .find(|span| span.id == id && span.events.end == index)
            .map(|span| span.events.end = index + 1)
            .is_some();
        if !extended && id > projection.max_exchange {
            projection.view.spans.push(Span {
                id,
                events: index..index + 1,
            });
        }
        projection.max_exchange = projection.max_exchange.max(id);
    } else if projection.max_exchange != 0 {
        let id = projection.max_exchange;
        if let Some(span) = projection
            .view
            .spans
            .iter_mut()
            .find(|span| span.id == id && span.events.end == index)
        {
            span.events.end = index + 1;
        }
    }
}

fn event_exchange(event: &SessionEvent) -> Option<u64> {
    match event {
        SessionEvent::UserPrompt { exchange, .. }
        | SessionEvent::ContextMessage { exchange, .. } => Some(*exchange),
        _ => None,
    }
}

fn apply_context_op(view: &mut View, op: &ContextOp) {
    match op {
        ContextOp::Fold {
            through_exchange,
            digest,
        } => {
            view.digest = Some(Digest {
                through_exchange: *through_exchange,
                text: digest.clone(),
            });
            view.spans.retain(|span| span.id > *through_exchange);
        }
        ContextOp::Drop { exchanges } => {
            let remove_digest = view
                .digest
                .as_ref()
                .is_some_and(|digest| exchanges.contains(&digest.through_exchange));
            if remove_digest {
                view.digest = None;
            }
            view.spans.retain(|span| !exchanges.contains(&span.id));
        }
    }
}

fn fold(log: &Ledger) -> io::Result<Projection> {
    let mut projection = Projection::default();
    for (index, event) in log.fold_events()? {
        advance_projection(&mut projection, &event, index);
    }
    Ok(projection)
}

fn admissible_event(state: &State, event: &SessionEvent) -> bool {
    match event {
        SessionEvent::UserPrompt { .. } => matches!(
            state,
            State::ReadyForUser | State::AwaitingAssistantAfterToolResults,
        ),
        SessionEvent::ContextMessage { .. } => matches!(state, State::ReadyForUser),
        SessionEvent::AssistantMessage { message, .. } => {
            message.role == ChatRole::Assistant
                && matches!(
                    state,
                    State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults,
                )
        }
        SessionEvent::ToolResults { results } => {
            if let State::AwaitingToolResults { pending_ids } = state {
                validate_result_ids(pending_ids, results).is_ok()
            } else {
                false
            }
        }
        _ => true,
    }
}

fn stub_repairable(event: &SessionEvent) -> bool {
    matches!(
        event,
        SessionEvent::UserPrompt { .. } | SessionEvent::ContextMessage { .. }
    )
}

/// The stub events role alternation needs to drive `state` back to
/// [`State::ReadyForUser`]: the one remedy.  `quiesce` records these live;
/// seam repair and resume validation interpose the same events on replay, so
/// a repaired history is one a live quiesce could have written.
fn stubs_to_ready(state: &State, reason: QuiesceReason) -> Vec<SessionEvent> {
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
    let mut stubs = Vec::new();
    let mut state = state.clone();
    if let State::AwaitingToolResults { pending_ids } = &state {
        let results = pending_ids
            .iter()
            .map(|id| ToolResult {
                id: id.clone(),
                content: tool_stub.into(),
            })
            .collect();
        let stub = SessionEvent::ToolResults { results };
        state = advance(&state, &stub);
        stubs.push(stub);
    }
    if !matches!(state, State::ReadyForUser) {
        stubs.push(SessionEvent::AssistantMessage {
            message: ChatMessage::assistant(assistant_stub),
            pending_tool_ids: Vec::new(),
            stop_reason: Some(stop_label.into()),
        });
    }
    stubs
}

fn append_aborted_stubs(state: &mut State, messages: &mut Vec<ChatMessage>) {
    for stub in stubs_to_ready(state, QuiesceReason::Aborted) {
        *state = advance(state, &stub);
        messages.extend(stub.into_chat_messages());
    }
}

fn render_context_transcript(log: &Ledger, view: &View, named: &HashSet<u64>) -> String {
    let mut transcript = String::new();
    let mut first_section = true;
    if let Some(digest) = &view.digest
        && named.contains(&digest.through_exchange)
    {
        context_section(
            &mut transcript,
            &mut first_section,
            &format!("digest through {}", digest.through_exchange),
        );
        render_role(&mut transcript, "user", &digest.text);
    }
    for span in &view.spans {
        if !named.contains(&span.id) {
            continue;
        }
        context_section(
            &mut transcript,
            &mut first_section,
            &format!("exchange {}", span.id),
        );
        let events = log
            .resident_events(span.events.clone())
            .expect("view spans are resident in the ledger");
        for event in events {
            match event {
                SessionEvent::UserPrompt { text, .. } => render_role(&mut transcript, "user", text),
                SessionEvent::ContextMessage { message, .. }
                | SessionEvent::AssistantMessage { message, .. } => {
                    render_chat_message(&mut transcript, message);
                }
                SessionEvent::StepStarted { n, .. } => {
                    let _ = writeln!(transcript, "--- step {n} ---");
                }
                SessionEvent::ToolResults { results } => {
                    for result in results {
                        render_role(
                            &mut transcript,
                            "tool",
                            &format!("{}: {}", result.id, result.content),
                        );
                    }
                }
                _ => {}
            }
        }
    }
    transcript
}

fn context_section(output: &mut String, first: &mut bool, title: &str) {
    if !*first {
        output.push('\n');
    }
    let _ = writeln!(output, "=== {title} ===");
    *first = false;
}

fn render_chat_message(output: &mut String, message: &ChatMessage) {
    let role = match message.role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    };
    let mut rendered = false;
    for part in &message.content {
        let text = part.as_text().map_or_else(
            || serde_json::to_string(part).unwrap_or_else(|_| "<unrenderable content>".into()),
            str::to_string,
        );
        render_role(output, role, &text);
        rendered = true;
    }
    if !rendered {
        render_role(output, role, "");
    }
}

fn render_role(output: &mut String, role: &str, text: &str) {
    let _ = writeln!(output, "[{role}]");
    output.push_str(text);
    if !text.ends_with('\n') {
        output.push('\n');
    }
}

/// The survey's standing caution to the model, beside the refusal sentences
/// so every model-facing context sentence is minted here.
pub const CACHE_SENTENCE: &str =
    "editing before the cache watermark re-reads the prefix on the next request";

fn live_exchange_refusal(id: u64) -> String {
    format!("exchange {id} is the one you are in — a context edit may only name closed exchanges")
}

fn folded_exchange_refusal(id: u64, reach: u64) -> String {
    format!(
        "exchange {id} is folded into the digest through {reach} — name {reach} to drop the digest whole, or fold further"
    )
}

fn unknown_exchange_refusal(id: u64) -> String {
    format!("exchange {id} is not present in the current view")
}

fn rewind_unknown_exchange_refusal(id: u64, last: Option<u64>) -> String {
    match last {
        Some(last) => format!(
            "exchange {id} is not present in the current view — the last exchange is {last}"
        ),
        None => format!(
            "exchange {id} is not present in the current view — there is no last exchange to rewind"
        ),
    }
}

struct CrashTail {
    bytes: Vec<u8>,
    complete_len: u64,
}

struct LoggedEvent {
    event: SessionEvent,
    bytes: Range<u64>,
}

struct ReadEvents {
    events: Vec<LoggedEvent>,
    tail: Option<CrashTail>,
    complete_len: u64,
}

fn read_events(path: &Path) -> io::Result<ReadEvents> {
    let data = fs::read(path).map_err(|error| {
        io::Error::new(
            error.kind(),
            format!(
                "cannot resume {}: could not read events.jsonl ({error}); check that the target path is a session log",
                path.display()
            ),
        )
    })?;
    let mut events = Vec::new();
    let mut complete_len = 0usize;
    for (line_index, fragment) in data.split_inclusive(|byte| *byte == b'\n').enumerate() {
        if fragment.last() != Some(&b'\n') {
            break;
        }
        let line = line_index + 1;
        let start = complete_len as u64;
        let value = &fragment[..fragment.len() - 1];
        let event = serde_json::from_slice(value).map_err(|error| {
            io::Error::other(format!(
                "cannot resume {}: line {line} is not a valid session event ({error}); was this session written by a newer exarch or hand-edited?",
                path.display()
            ))
        })?;
        events.push(LoggedEvent {
            event,
            bytes: start..start + fragment.len() as u64,
        });
        complete_len += fragment.len();
    }
    let tail = (complete_len < data.len()).then(|| CrashTail {
        bytes: data[complete_len..].to_vec(),
        complete_len: complete_len as u64,
    });
    Ok(ReadEvents {
        events,
        tail,
        complete_len: complete_len as u64,
    })
}

/// Replay the doors' own predicates over a file before admitting it: protocol
/// admissibility (interposing the stubs a live quiesce would have recorded)
/// and the exchange-id discipline (an id opens past the running maximum or
/// extends the tail span).  A line that fails either is foreign data — no
/// live door records one — and the projection would silently misplace it.
fn validate_resume_events(events: &[LoggedEvent], path: &Path) -> io::Result<()> {
    let mut state = State::ReadyForUser;
    let mut projection = Projection::default();
    for (index, recorded) in events.iter().enumerate() {
        let event = &recorded.event;
        if !admissible_event(&state, event) {
            if !stub_repairable(event) {
                return Err(io::Error::other(format!(
                    "cannot resume {}: line {} is foreign protocol data, not a seam that quiesce can repair; was the file hand-edited or written by an incompatible exarch?",
                    path.display(),
                    index + 1
                )));
            }
            append_aborted_stubs(&mut state, &mut Vec::new());
        }
        state = advance(&state, event);
        if let Some(id) = event_exchange(event) {
            let joins = id > projection.max_exchange
                || projection
                    .view
                    .spans
                    .iter()
                    .any(|span| span.id == id && span.events.end == index);
            if !joins {
                return Err(io::Error::other(format!(
                    "cannot resume {}: line {} names exchange {id}, which the log had already moved past; no live session records a stale exchange id — was the file hand-edited?",
                    path.display(),
                    index + 1
                )));
            }
        }
        advance_projection(&mut projection, event, index);
    }
    Ok(())
}

fn quarantine_tail(path: &Path, tail: &CrashTail) -> io::Result<()> {
    let sidecar = path.with_file_name("events.jsonl.crash");
    let mut quarantine = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sidecar)?;
    quarantine.write_all(&tail.bytes)?;
    quarantine.flush()?;
    quarantine.sync_all()?;

    let events = OpenOptions::new().write(true).open(path)?;
    events.set_len(tail.complete_len)?;
    events.sync_all()?;
    eprintln!(
        "exarch: quarantined {} bytes from {} in {} before trimming the live log",
        tail.bytes.len(),
        path.display(),
        sidecar.display()
    );
    Ok(())
}

fn first_free_rotation(events: &Path, transcript: &Path, record: &Path) -> io::Result<u64> {
    let mut n = 0;
    loop {
        let free = [events, transcript, record]
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

fn rotation_path(path: &Path, n: u64) -> PathBuf {
    let name = path.file_name().map_or_else(
        || "record".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!("{name}.{n}"))
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:events-file] opens events.jsonl for the session event log; infra, not turn-time data I/O"
)]
fn open_events_file(dir: &Path) -> io::Result<BufWriter<File>> {
    let f = OpenOptions::new()
        .write(true)
        .create(true)
        .truncate(true)
        .open(dir.join("events.jsonl"))?;
    Ok(BufWriter::new(f))
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:events-file] reopens the durable session log for append during resume or clear recovery"
)]
fn open_events_append(dir: &Path) -> io::Result<BufWriter<File>> {
    let f = OpenOptions::new()
        .append(true)
        .open(dir.join("events.jsonl"))?;
    Ok(BufWriter::new(f))
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

    fn assert_wiring(s: &AgentLog) {
        assert_eq!(
            fold(&s.log).expect("from-scratch fold"),
            s.projection,
            "fold(log) == memo after the append or edit"
        );
    }

    fn logical_events(log: &Ledger) -> Vec<SessionEvent> {
        log.fold_events()
            .expect("logical event sequence")
            .into_iter()
            .map(|(_, event)| event)
            .collect()
    }

    fn replay_resident(events: Vec<SessionEvent>) -> (Ledger, Projection) {
        let mut log = Ledger::new(None);
        let mut projection = Projection::default();
        for event in events {
            step(&mut log, &mut projection, event, None);
        }
        (log, projection)
    }

    fn replay_without_eviction(events: Vec<SessionEvent>) -> (Ledger, Projection) {
        let mut log = Ledger::new(None);
        let mut projection = Projection::default();
        for event in events {
            let index = log.append(event, None);
            let event = log.event(index).expect("the replay event is resident");
            advance_projection(&mut projection, event, index);
        }
        (log, projection)
    }

    /// The step's oracle: the two-pass batch construction the design demoted
    /// from spec to test.  Pass one partitions the whole history into spans
    /// with no edit interleaved; pass two replays the edits over the finished
    /// partition.  Deliberately not the step: a shared single pass would
    /// reproduce its own defects.
    fn independent_batch_reference(log: &[SessionEvent]) -> Projection {
        // Pass one: partition, running maximum, protocol state, newest edit.
        let mut spans: Vec<Span> = Vec::new();
        let mut projection = Projection::default();
        for (index, event) in log.iter().enumerate() {
            projection.state = independent_advance(&projection.state, event);
            match event_exchange(event) {
                Some(id) if spans.last().is_none_or(|span| span.id != id) => {
                    spans.push(Span {
                        id,
                        events: index..index + 1,
                    });
                }
                _ => {
                    if let Some(span) = spans.last_mut() {
                        span.events.end = index + 1;
                    }
                }
            }
            if let Some(id) = event_exchange(event) {
                projection.max_exchange = projection.max_exchange.max(id);
            }
            if matches!(event, SessionEvent::ContextEdited { .. }) {
                projection.newest_edit = Some(index);
            }
        }

        // Pass two: the edits, in log order, over the whole partition.
        let mut digest = None;
        for event in log {
            let SessionEvent::ContextEdited { op, .. } = event else {
                continue;
            };
            match op {
                ContextOp::Fold {
                    through_exchange,
                    digest: text,
                } => {
                    digest = Some(Digest {
                        through_exchange: *through_exchange,
                        text: text.clone(),
                    });
                    spans.retain(|span| span.id > *through_exchange);
                }
                ContextOp::Drop { exchanges } => {
                    if digest
                        .as_ref()
                        .is_some_and(|digest| exchanges.contains(&digest.through_exchange))
                    {
                        digest = None;
                    }
                    spans.retain(|span| !exchanges.contains(&span.id));
                }
            }
        }
        projection.view = View { digest, spans };
        projection
    }

    fn independent_advance(state: &State, event: &SessionEvent) -> State {
        match event {
            SessionEvent::UserPrompt { .. } => State::AwaitingAssistantAfterUser,
            SessionEvent::AssistantMessage {
                pending_tool_ids, ..
            } => {
                if pending_tool_ids.is_empty() {
                    State::ReadyForUser
                } else {
                    State::AwaitingToolResults {
                        pending_ids: pending_tool_ids.clone(),
                    }
                }
            }
            SessionEvent::ToolResults { .. } => State::AwaitingAssistantAfterToolResults,
            _ => state.clone(),
        }
    }

    fn independent_messages(log: &[SessionEvent], projection: &Projection) -> Vec<ChatMessage> {
        let mut messages = Vec::new();
        if let Some(digest) = &projection.view.digest {
            messages.push(ChatMessage::user(summary_prompt(&digest.text)));
        }
        for span in &projection.view.spans {
            for event in &log[span.events.clone()] {
                messages.extend(event.clone().into_chat_messages());
            }
        }
        messages
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
        assert_eq!(s.view().spans[0].events.start, 2);
        assert_eq!(
            s.log
                .resident_events(s.view().spans[2].events.clone())
                .expect("view span is resident")
                .into_iter()
                .filter_map(event_exchange)
                .collect::<Vec<_>>(),
            vec![3, 3]
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
            s.view().digest,
            Some(Digest {
                through_exchange: 1,
                text: "second digest".into(),
            })
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
    fn fold_log_equals_memo_proves_wiring() {
        let mut s = fresh_root();
        assert_wiring(&s);
        s.record_error("breadcrumb".into()).unwrap();
        assert_wiring(&s);
        complete_exchange(&mut s, "one", "one");
        assert_wiring(&s);
        s.apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::User)
            .unwrap();
        assert_wiring(&s);
    }

    #[test]
    fn independent_batch_reference_proves_the_step() {
        let mut s = fresh_root();
        s.record_error("before".into()).unwrap();
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
        s.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::Model)
            .unwrap();

        let events = logical_events(&s.log);
        let reference = independent_batch_reference(&events);
        assert_eq!(reference, s.projection);
        assert_eq!(
            serde_json::to_vec(&independent_messages(&events, &reference)).unwrap(),
            serde_json::to_vec(&s.history_messages()).unwrap(),
        );
    }

    #[test]
    fn fold_only_histories_keep_model_messages_byte_identical() {
        let mut s = fresh_root();
        s.record_error("not visible".into()).unwrap();
        complete_exchange(&mut s, "one", "one");
        s.append_user("steering".into(), Some(1)).unwrap();
        s.append_assistant(ChatMessage::assistant("done"), vec![], None)
            .unwrap();

        let events = logical_events(&s.log);
        let projection = fold(&s.log).expect("from-scratch fold");
        let expected = independent_messages(&events, &projection);
        assert_eq!(
            serde_json::to_vec(&expected).unwrap(),
            serde_json::to_vec(&s.history_messages()).unwrap()
        );
    }

    #[test]
    fn edits_free_removed_spans_and_the_tail_edit_itself() {
        let mut dropped = fresh_root();
        complete_exchange(&mut dropped, "one", "one");
        complete_exchange(&mut dropped, "two", "two");
        complete_exchange(&mut dropped, "three", "three");
        let mut removed = dropped.view().spans[1].events.clone();
        dropped
            .apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
            .unwrap();
        assert!(removed.all(|index| !dropped.log.is_resident(index)));
        assert_eq!(
            dropped.log.freed_ranges().len(),
            1,
            "one contiguous span frees to one coalesced byte range"
        );
        assert_wiring(&dropped);

        let mut tail = fresh_root();
        complete_exchange(&mut tail, "one", "one");
        complete_exchange(&mut tail, "two", "two");
        let mut removed_tail = tail.view().spans[1].events.clone();
        tail.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
            .unwrap();
        let edit_index = tail.log_len() - 1;
        assert!(!tail.log.is_resident(edit_index));
        assert!(removed_tail.all(|index| !tail.log.is_resident(index)));
        assert_eq!(
            tail.log.freed_ranges().len(),
            1,
            "the edit's own line coalesces with the span it removed"
        );
        assert_wiring(&tail);

        let mut folded = fresh_root();
        complete_exchange(&mut folded, "one", "one");
        complete_exchange(&mut folded, "two", "two");
        complete_exchange(&mut folded, "three", "three");
        let mut first = folded.view().spans[0].events.clone();
        let mut second = folded.view().spans[1].events.clone();
        folded
            .apply_edit(
                ContextOp::Fold {
                    through_exchange: 2,
                    digest: "old work".into(),
                },
                EditAuthority::Harness,
            )
            .unwrap();
        assert!(first.all(|index| !folded.log.is_resident(index)));
        assert!(second.all(|index| !folded.log.is_resident(index)));
        assert_eq!(
            folded.log.freed_ranges().len(),
            1,
            "adjacent folded spans free to one coalesced byte range"
        );
        assert_wiring(&folded);
    }

    #[test]
    fn freed_projection_and_resume_match_the_logical_history() {
        let sessions = sessions_root("residency-resume");
        let mut live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        complete_exchange(&mut live, "one", "one");
        complete_exchange(&mut live, "two", "two");
        complete_exchange(&mut live, "three", "three");
        live.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
            .unwrap();

        let events = logical_events(&live.log);
        assert!(!live.log.freed_ranges().is_empty());
        let (never_freed_log, never_freed_projection) = replay_without_eviction(events.clone());
        let mut never_freed =
            AgentLog::root_without_logs(sessions.path(), 1, "model", "provider", 0).unwrap();
        never_freed.log = never_freed_log;
        never_freed.projection = never_freed_projection;
        let expected = serde_json::to_vec(&never_freed.model_messages()).unwrap();
        assert_eq!(
            serde_json::to_vec(&live.model_messages()).unwrap(),
            expected
        );
        let reference = fold(&live.log).expect("from-scratch fold");
        assert_eq!(
            serde_json::to_vec(&independent_messages(&events, &reference)).unwrap(),
            expected
        );
        let path = sessions.path().join("0/events.jsonl");
        let before = fs::read(&path).unwrap();
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert_eq!(
            serde_json::to_vec(&resumed.model_messages()).unwrap(),
            expected
        );
        assert_eq!(fs::read(path).unwrap(), before);
    }

    #[test]
    fn clear_discards_the_old_ledger_with_the_rotated_file() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        s.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
            .unwrap();
        assert!(!s.log.freed_ranges().is_empty());

        let events = s.dir().join("events.jsonl");
        let transcript = s.dir().join("transcript.jsonl");
        let result = s.clear(0, 2, &transcript).expect("clear");
        assert_eq!(result.rotation, Some(0));
        assert!(events.with_extension("jsonl.0").exists());
        assert!(s.log.freed_ranges().is_empty());
        assert_eq!(s.log_len(), 1);
        assert!(s.view().spans.is_empty());
        assert!(s.log.is_resident(0));
    }

    #[test]
    fn transient_eviction_keeps_no_byte_ranges() {
        let sessions = sessions_root("residency-no-logs");
        let mut s = AgentLog::root_without_logs(sessions.path(), 0, "model", "provider", 0).unwrap();
        complete_exchange(&mut s, "one", "one");
        s.apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::User)
            .unwrap();
        assert!(s.log.freed_ranges().is_empty());
        assert_eq!(s.event_count(), 0);
    }

    #[test]
    fn interior_lost_stub_seam_is_repaired_by_projection() {
        let mut s = fresh_root();
        s.append_user("first".into(), None).unwrap();
        s.quiesce(QuiesceReason::Aborted);
        complete_exchange(&mut s, "second", "answer");

        let mut events = logical_events(&s.log);
        let removed = events
            .iter()
            .position(|event| {
                matches!(
                    event,
                    SessionEvent::AssistantMessage {
                        stop_reason: Some(reason),
                        ..
                    } if reason == "aborted"
                )
            })
            .expect("aborted stub");
        events.remove(removed);
        let (log, projection) = replay_resident(events);
        s.log = log;
        s.projection = projection;

        let messages = s.history_messages();
        assert!(
            messages.iter().any(|message| {
                message.content.first_text() == Some("(no reply: exchange aborted)")
            }),
            "the lost interior stub must be interposed"
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
    fn compaction_plan_walks_newest_spans_and_folds_by_exchange() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        complete_exchange(&mut s, "three", "three");
        let keep = s.span_message_bytes(&s.view().spans[2]);
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
    fn jsonl_round_trip_contains_context_edit_events() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        s.apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::User)
            .unwrap();

        let file = File::open(s.dir().join("events.jsonl")).unwrap();
        let parsed: Vec<SessionEvent> = serde_json::Deserializer::from_reader(file)
            .into_iter::<SessionEvent>()
            .collect::<Result<_, _>>()
            .expect("round-trip");
        assert!(parsed.iter().any(|event| {
            matches!(
                event,
                SessionEvent::ContextEdited {
                    op: ContextOp::Drop { exchanges },
                    by: EditAuthority::User,
                } if exchanges == &[1]
            )
        }));
    }

    #[test]
    fn diagnostic_events_round_trip_through_jsonl() {
        let mut s = fresh_root();
        s.record_error("boom".into()).unwrap();
        s.record_nudge(2, 3, "stop=length".into()).unwrap();
        let file = File::open(s.dir().join("events.jsonl")).unwrap();
        let parsed: Vec<SessionEvent> = serde_json::Deserializer::from_reader(file)
            .into_iter::<SessionEvent>()
            .collect::<Result<_, _>>()
            .expect("round-trip");
        assert_eq!(parsed.len(), 3);
        assert!(matches!(&parsed[1], SessionEvent::Error { text } if text == "boom"));
        assert!(matches!(
            &parsed[2],
            SessionEvent::Nudge { used: 2, max: 3, cause } if cause == "stop=length"
        ));
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
        let expected = serde_json::to_vec(&live.model_messages()).unwrap();
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert!(resumed.is_ready());
        assert_eq!(
            serde_json::to_vec(&resumed.model_messages()).unwrap(),
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

        let path = sessions.path().join("0/events.jsonl");
        let mut resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert!(resumed.is_ready());
        let events = logical_events(&resumed.log);
        assert!(
            events
                .iter()
                .any(|event| matches!(event, SessionEvent::ToolResults { .. }))
        );
        assert!(events.iter().any(|event| {
            matches!(
                event,
                SessionEvent::AssistantMessage {
                    stop_reason: Some(reason),
                    ..
                } if reason == "aborted"
            )
        }));
        resumed.append_user("continue".into(), None).unwrap();
        let events = read_events(&path)
            .unwrap()
            .events
            .into_iter()
            .map(|recorded| recorded.event)
            .collect::<Vec<_>>();
        assert!(
            matches!(events.last(), Some(SessionEvent::UserPrompt { text, .. }) if text == "continue")
        );
    }

    #[test]
    fn resume_quarantines_only_an_unterminated_final_fragment() {
        let sessions = sessions_root("resume-tail");
        let live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        let path = sessions.path().join("0/events.jsonl");
        drop(live);
        let prefix = fs::read(&path).unwrap();
        let fragment = b"{\"kind\":\"user_prompt\"";
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(fragment).unwrap();
        file.flush().unwrap();

        let resumed = AgentLog::resume(sessions.path(), 0).expect("torn tail is recoverable");
        assert!(resumed.is_ready());
        assert_eq!(fs::read(&path).unwrap(), prefix);
        assert_eq!(
            fs::read(sessions.path().join("0/events.jsonl.crash")).unwrap(),
            fragment
        );
    }

    #[test]
    fn resume_refuses_a_complete_garbage_line_without_mutating_the_file() {
        let sessions = sessions_root("resume-garbage");
        let live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        let path = sessions.path().join("0/events.jsonl");
        drop(live);
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"garbage\n").unwrap();
        file.flush().unwrap();
        let before = fs::read(&path).unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("garbage is not a crash tail");
        let text = error.to_string();
        assert!(text.contains(&path.display().to_string()));
        assert!(text.contains("line 2"));
        assert!(text.contains("newer exarch"));
        assert_eq!(fs::read(&path).unwrap(), before);
        assert!(!sessions.path().join("0/events.jsonl.crash").exists());
    }

    #[test]
    fn resume_refuses_foreign_protocol_data_without_mutating_the_file() {
        let sessions = sessions_root("resume-foreign");
        let live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        let path = sessions.path().join("0/events.jsonl");
        drop(live);
        let foreign = SessionEvent::AssistantMessage {
            message: ChatMessage::user("wrong role"),
            pending_tool_ids: Vec::new(),
            stop_reason: None,
        };
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
        serde_json::to_writer(&mut file, &foreign).unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();
        let before = fs::read(&path).unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("foreign data must refuse");
        let text = error.to_string();
        assert!(text.contains(&path.display().to_string()));
        assert!(text.contains("line 2"));
        assert!(text.contains("foreign protocol data"));
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn resume_refuses_a_stale_exchange_id_as_foreign_data() {
        let sessions = sessions_root("resume-stale-id");
        let mut live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        complete_exchange(&mut live, "one", "one");
        complete_exchange(&mut live, "two", "two");
        let path = sessions.path().join("0/events.jsonl");
        drop(live);
        let stale = SessionEvent::UserPrompt {
            exchange: 1,
            text: "stale".into(),
        };
        let mut file = OpenOptions::new().append(true).open(&path).unwrap();
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
    fn resume_repairs_an_interior_stub_lost_from_disk() {
        let sessions = sessions_root("resume-interior-seam");
        let mut live = AgentLog::root(sessions.path(), 0, "model", "provider", 0).unwrap();
        live.append_user("first".into(), None).unwrap();
        live.quiesce(QuiesceReason::Aborted);
        complete_exchange(&mut live, "second", "answer");
        let path = sessions.path().join("0/events.jsonl");
        drop(live);

        let data = fs::read(&path).unwrap();
        let mut repaired = Vec::with_capacity(data.len());
        for fragment in data.split_inclusive(|byte| *byte == b'\n') {
            let event: SessionEvent =
                serde_json::from_slice(&fragment[..fragment.len() - 1]).unwrap();
            if matches!(
                event,
                SessionEvent::AssistantMessage {
                    stop_reason: Some(reason),
                    ..
                } if reason == "aborted"
            ) {
                continue;
            }
            repaired.extend_from_slice(fragment);
        }
        fs::write(&path, repaired).unwrap();

        let mut resumed = AgentLog::resume(sessions.path(), 0).expect("interior seam repair");
        assert!(resumed.is_ready());
        assert!(resumed.model_messages().iter().any(|message| {
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
            let events = logical_events(&live.log);
            assert_eq!(
                independent_batch_reference(&events),
                live.projection,
                "pattern {pattern}"
            );
            let expected = serde_json::to_vec(&live.model_messages()).unwrap();
            drop(live);
            let resumed = AgentLog::resume(sessions.path(), 0).expect("resume edit sequence");
            assert!(resumed.is_ready());
            assert_eq!(
                serde_json::to_vec(&resumed.model_messages()).unwrap(),
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
        assert!(!log.dir().join("events.jsonl").exists());
        assert!(!log.dir().join("transcript.jsonl").exists());
    }
}
