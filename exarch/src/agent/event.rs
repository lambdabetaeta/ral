//! `AgentLog`: one session's handle onto `sessions/<n>/record.jsonl`, the one
//! seam every fact crosses, and [`crate::record::model::Memo`], the model
//! fold's authoritative in-memory projection of it.
//!
//! Every query below reads `model_memo`; nothing here keeps a second copy.
//! `tui::viewport` folds the same log's `Display`/`Forensic` classes into the
//! rendered `user.log`.

use crate::agent::build::RecordedAccount;
use crate::bus::AgentId;
use crate::provider::{ProviderError, Tuning, Usage};
use crate::record::model::{Eviction, Memo, Transcript, View};
use crate::record::{Display, Fold as _, Forensic, Protocol, Record, Recorded, widen};
use genai::chat::{ChatMessage, ChatRole};
use regex::Regex;
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
        #[serde(default, skip_serializing_if = "Option::is_none")]
        status: Option<u16>,
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
#[serde(rename_all = "snake_case")]
pub enum ContextOp {
    /// Every span through `through_exchange` leaves the view at once; the
    /// model reads the harness's index of them in their place, and `note` —
    /// the model's own, never the harness's — beside it.
    Evict {
        through_exchange: u64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
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
                status,
            } => Self::Transient {
                cause: cause.clone(),
                attempts: *attempts,
                body: body.as_deref().cloned(),
                status: *status,
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

/// Why an exchange is ending with no real assistant reply: decides whether it
/// is owed a capstone, and whether it earns a `Forensic::Cancelled`
/// breadcrumb.
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

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContextSpanKind {
    Exchange,
    Import,
    /// An ancestor's exchange, resolved through this log's lineage rather
    /// than its own ledger.
    Inherited,
}

impl ContextSpanKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exchange => "exchange",
            Self::Import => "import",
            Self::Inherited => "inherited",
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
    /// Closed exchanges that have left the view by eviction — the rows of
    /// every [`crate::record::model::Eviction`] the view carries.
    pub evicted: usize,
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

/// `` transcript `index ``'s answer: one element per closed exchange the
/// store holds, in view or not.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct StoreIndexItem {
    pub exchange: u64,
    pub kind: ContextSpanKind,
    pub opening: String,
    pub steps: usize,
    /// The weight the exchange carried while the model held it.
    pub bytes: usize,
    pub in_view: bool,
}

/// One line of one message that matched `` transcript `grep ``'s pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepHit {
    pub exchange: u64,
    pub role: ChatRole,
    /// 1-based, within that message's searched text.
    pub line: usize,
    pub text: String,
}

/// `hits` is capped; `total` is the true count, so a model reading a large
/// one knows to narrow rather than to page.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct GrepAnswer {
    pub hits: Vec<GrepHit>,
    pub total: usize,
}

/// `` transcript `read ``'s answer: one element per named closed exchange.
///
/// In store order, addressed by `exchange` rather than by argument position —
/// [`super::model::AgentLog::read_context`]'s doc comment.
#[derive(Clone, Debug)]
pub struct TranscriptSpan {
    pub exchange: u64,
    pub messages: Vec<TranscriptMessage>,
}

/// One [`genai::chat::ChatMessage`] as the model actually saw it, narrowed
/// to [`TranscriptPart`]s rather than the provider's raw content parts.
#[derive(Clone, Debug)]
pub struct TranscriptMessage {
    pub role: ChatRole,
    pub parts: Vec<TranscriptPart>,
}

/// One arm per [`genai::chat::ContentPart`] variant.
///
/// Holds only what a reader may usefully narrow into — never a serialization
/// of the provider's own struct. `ThoughtSignature` has no arm here: it
/// produces no part.
#[derive(Clone, Debug)]
pub enum TranscriptPart {
    Text(String),
    /// The ral tool call itself: `tool` is always `"ral"` and `source` the
    /// script that ran, when the call is exarch's one tool; for any other
    /// tool, `source` is empty and `keys` names its arguments instead of
    /// their values.
    Program {
        tool: String,
        source: String,
        keys: Vec<String>,
    },
    Result(String),
    Reasoning(String),
    /// Metadata only — a base64 payload is never something a reader can
    /// narrow into, so it never crosses this boundary.
    Binary {
        content_type: String,
        name: String,
        bytes: usize,
    },
    Custom {
        provider: String,
        model: String,
    },
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
    /// Held, with [`Self::account`], so `clear` re-emits `SessionStarted`
    /// unchanged and `fork` passes both down to the child.
    model: String,
    account: RecordedAccount,
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
    /// `Avatar::couple` has attached, in that order, under one lock.
    seam: crate::record::Emitter,
}

/// What a `mnemon` child is forked with: its parent's window, span by span
/// under the parent's own exchange ids, beside the link that makes every
/// older ancestor exchange readable too.
pub struct Inherited {
    /// The parent's `record.jsonl`, or `None` when the parent kept no log.
    pub source: Option<PathBuf>,
    /// The parent's head-marker state, so the child's marker opens where the
    /// parent's stood.
    pub evictions: Vec<Eviction>,
    /// The parent's own exchange floor: ids at or below it are the
    /// ancestry's, and the child mints its first above it.
    pub through_exchange: u64,
    /// One entry per in-view parent span, in view order.
    pub spans: Vec<(u64, Vec<ChatMessage>)>,
}

/// A planned cut: every visible span through `through_exchange` leaves the
/// window, while the newer ones stay verbatim.
pub struct EvictionPlan {
    pub through_exchange: u64,
}

pub(crate) struct ClearRecord {
    pub(crate) rotation_error: Option<io::Error>,
}

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
        account: &RecordedAccount,
        system_prompt_bytes: usize,
    ) -> io::Result<Self> {
        let mut s = Self::open_fresh(
            sessions_root.to_path_buf(),
            session_id,
            model.to_string(),
            account.clone(),
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
    pub fn for_test(
        session_id: AgentId,
        model: &str,
        account: &RecordedAccount,
    ) -> io::Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix("exarch-agent-log-test-")
            .tempdir()?;
        let log = Self::root(scratch.path(), session_id, model, account, 0)?;
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
        account: &RecordedAccount,
        system_prompt_bytes: usize,
    ) -> io::Result<Self> {
        let mut s = Self::open_fresh(
            sessions_root.to_path_buf(),
            session_id,
            model.to_string(),
            account.clone(),
            false,
        )?;
        s.record_started_lossy(None, system_prompt_bytes, crate::bootstrap::now_unix_ms());
        Ok(s)
    }

    /// Build a forked child log, inheriting `sessions_root` and recording this
    /// log's id as the child's parent.
    ///
    /// `model` and `account` are the child's *own*, handed in rather than
    /// copied off this log: a spawn may name another selection, and this log's
    /// pair was fixed at session start, so a `/model` since would make a
    /// copied header a lie.
    ///
    /// # Errors
    /// Creating the child's directory, or writing `SessionStarted`, failed.
    pub fn fork(
        &self,
        child_id: AgentId,
        system_prompt_bytes: usize,
        model: &str,
        account: &RecordedAccount,
    ) -> io::Result<Self> {
        let mut s = Self::open_fresh(
            self.sessions_root.clone(),
            child_id,
            model.to_string(),
            account.clone(),
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
        let (model_memo, model, label) =
            crate::record::model::resume(&record_path).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!("cannot resume {}: {error}", record_path.display()),
                )
            })?;
        // Service and account are unknown from history alone — resume reads
        // back only the label a record's first line names itself by — and
        // `record_resumed` overwrites this with the live selection's full
        // identity before anything else observes it.
        let account = RecordedAccount {
            label,
            service: String::new(),
            id: String::new(),
        };
        let seam = crate::record::Emitter::append_to(&record_path)?;
        let mut resumed = Self {
            id: session_id,
            dir,
            durable: true,
            model,
            account,
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
        account: &RecordedAccount,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) -> io::Result<()> {
        self.model = model.to_string();
        self.account = account.clone();
        self.record_protocol(Protocol::SessionResumed {
            model: self.model.clone(),
            label: self.account.label.clone(),
            service: Some(self.account.service.clone()),
            account: Some(self.account.id.clone()),
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
            // Recorded<Record> — a caller bug, not a runtime condition. A
            // release build must crash here too: silently skipping the step
            // would desync the memo from record.jsonl with nothing to show
            // for it.
            unreachable!("model fold refused a live record: {refusal}");
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
    /// exchange here, and an eviction demands it.
    pub fn is_ready(&self) -> bool {
        self.model_memo.is_ready()
    }

    pub fn can_evict(&self) -> bool {
        self.model_memo.can_evict()
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
    pub fn context_survey(&mut self) -> ContextSurvey {
        self.model_memo.context_survey()
    }

    /// Read named, closed exchanges in store order — in view or evicted.
    ///
    /// # Errors
    /// Refuses an empty list, a duplicate name, an exchange this session never
    /// recorded, the exchange still in progress, or an evicted exchange in a
    /// session that keeps no log.
    pub fn read_context(&mut self, exchanges: &[u64]) -> Result<Vec<TranscriptSpan>, String> {
        self.model_memo.read_context(exchanges)
    }

    /// Every closed exchange the store holds, oldest first.
    pub fn store_index(&mut self) -> Vec<StoreIndexItem> {
        self.model_memo.store_index()
    }

    /// Search the closed exchanges' text — the named ones, or the whole
    /// store when `exchanges` is `None`.
    ///
    /// # Errors
    /// Refuses a named list the way [`Self::read_context`] does, and a named
    /// evicted exchange in a session that keeps no log.
    pub fn grep_store(
        &mut self,
        pattern: &Regex,
        exchanges: Option<&[u64]>,
    ) -> Result<GrepAnswer, String> {
        self.model_memo.grep_store(pattern, exchanges)
    }

    /// Approximate context size in serialised model-view bytes.  The fallback
    /// eviction trigger in [`crate::agent::Avatar::evict`] when the model's
    /// context window is unknown; otherwise that tracks token pressure instead.
    pub fn history_bytes(&mut self) -> usize {
        self.model_memo.history_bytes()
    }

    /// Render the model-view transcript for the next provider request.
    ///
    /// # Errors
    /// The session is not awaiting an assistant reply.
    pub fn render_messages(&mut self) -> Result<Transcript, String> {
        if !self.model_memo.is_awaiting_assistant() {
            return Err(format!(
                "cannot render request while session is in state {}",
                self.model_memo.state_description()
            ));
        }
        Ok(self.model_memo.transcript())
    }

    /// Every committed message whatever the phase.
    pub fn history_transcript(&mut self) -> Transcript {
        self.model_memo.transcript()
    }

    /// The parent half of a `mnemon` fork — the seed, where ownership
    /// genuinely transfers into the child's own ledger via
    /// [`Self::import_context`]. The tail span is cut to the longest run
    /// that owes no tool result, whatever left it that way — a batch in
    /// flight, or a `ContextEdited` record landing after the assistant frame
    /// it answers.
    pub fn inherited_context(&mut self) -> Inherited {
        let source = self.durable.then(|| self.dir.join("record.jsonl"));
        let evictions = self.model_memo.view().evictions.clone();
        let through_exchange = self.model_memo.exchange_floor();
        Inherited {
            source,
            evictions,
            through_exchange,
            spans: self.model_memo.inherited_context(),
        }
    }

    /// Import a parent's context without appending a prompt: a `mnemon`
    /// child's launch prompt arrives through its inbox, and `deliberate`
    /// commits it through [`Self::append_user`] like any other exchange.
    ///
    /// The link goes down first, then one `ContextMessage` per message under
    /// the parent's own exchange id, so the child's view reproduces the
    /// parent's spans and can name any of them.
    ///
    /// # Errors
    /// The session is not at a ready boundary, or recording a record failed.
    pub fn import_context(&mut self, inherited: Inherited) -> Result<(), String> {
        self.ready_to_import()?;
        let Inherited {
            source,
            evictions,
            through_exchange,
            spans,
        } = inherited;
        self.record_protocol(Protocol::Inherited {
            source,
            evictions,
            through_exchange,
        })
        .map_err(|e| e.to_string())?;
        for (exchange, messages) in spans {
            for message in messages {
                self.record_protocol(Protocol::ContextMessage { exchange, message })
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// One harness-authored message imported as an exchange of this log's
    /// own — the resume note, which inherits nothing and links to no one.
    ///
    /// # Errors
    /// The session is not at a ready boundary, or recording failed.
    pub fn import_note(&mut self, message: ChatMessage) -> Result<(), String> {
        self.ready_to_import()?;
        let exchange = self.next_exchange();
        self.record_protocol(Protocol::ContextMessage { exchange, message })
            .map_err(|e| e.to_string())
    }

    fn ready_to_import(&self) -> Result<(), String> {
        if self.model_memo.is_ready() {
            return Ok(());
        }
        Err(format!(
            "cannot import context while session is in state {}",
            self.model_memo.state_description()
        ))
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
            .filter(|id| {
                self.model_memo
                    .view()
                    .spans
                    .iter()
                    .any(|span| span.id == *id)
            })
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

    /// End an exchange that produced no real assistant reply, recording only
    /// what the log still owes: an answer to tool calls that never ran, and a
    /// capstone for an exchange that ended on `reply`.  An exchange abandoned
    /// short of a reply is left as it lies — the model reads no trace of it,
    /// and the next prompt opens a fresh one over the top.
    pub fn quiesce(&mut self, reason: QuiesceReason) {
        for record in self.model_memo.quiesce_records(reason) {
            self.record_protocol_lossy(record);
        }
        // Only a cancellation earns its own breadcrumb; an abort's marker is
        // the `ProviderError` already on disk.
        if matches!(reason, QuiesceReason::Cancelled)
            && let Err(error) = self.record_forensic(Forensic::Cancelled)
        {
            eprintln!("exarch: the cancellation breadcrumb was not recorded: {error}");
        }
    }

    pub fn plan_eviction(&mut self, keep_budget_bytes: usize) -> Option<EvictionPlan> {
        self.model_memo.plan_eviction(keep_budget_bytes, None)
    }

    pub fn plan_eviction_before(
        &mut self,
        keep_budget_bytes: usize,
        before_exchange: u64,
    ) -> Option<EvictionPlan> {
        self.model_memo
            .plan_eviction(keep_budget_bytes, Some(before_exchange))
    }

    /// # Errors
    /// Refuses an empty drop, an unknown or already-evicted exchange, or a
    /// live exchange.
    pub fn apply_edit(&mut self, op: ContextOp, by: EditAuthority) -> Result<(), String> {
        self.model_memo.validate_edit(&op)?;
        self.record_protocol(Protocol::ContextEdited { op: op.clone(), by })
            .map_err(|e| e.to_string())?;
        // The display twin: the screen never derives from the protocol
        // record it duplicates a field of.
        self.record_display(Display::ContextEdited { op, by })
            .map_err(|e| e.to_string())
    }

    /// Resolve a user rewind into the whole visible suffix beginning at its
    /// anchor. The anchor is checked before the suffix is derived.
    ///
    /// # Errors
    /// Refuses an absent anchor or one that has already left the view.
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
    ) -> io::Result<ClearRecord> {
        if !self.durable {
            self.model_memo = Memo::new(None);
            let rotation_error = self.seam.rotate(None).err();
            self.record_started_lossy(None, system_prompt_bytes, at_unix_ms);
            return Ok(ClearRecord { rotation_error });
        }

        let record_path = self.dir.join("record.jsonl");
        let rotation = match first_free_rotation(&record_path) {
            Ok(n) => n,
            Err(error) => {
                return Err(io::Error::other(format!(
                    "clear was not committed for {}: {error}",
                    record_path.display()
                )));
            }
        };
        let rotation_error = self.rotate_record(&record_path, rotation);
        self.model_memo = Memo::new(Some(record_path));
        let started = self.started_event(None, system_prompt_bytes, at_unix_ms);
        let seam_error = self.record_protocol(started).err();
        Ok(ClearRecord {
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
    fn rotate_record(&self, path: &Path, rotation: u64) -> Option<io::Error> {
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
        self.seam.rotate(Some(path)).err().map(|error| {
            io::Error::new(
                error.kind(),
                format!(
                    "clear committed, but a fresh {} could not be opened; the old record segment stays live: {error}",
                    path.display()
                ),
            )
        })
    }

    // ── Meta-records ──────────────────────────────────────────────────────
    //
    // One non-protocol breadcrumb each; all fail only on serialising or
    // appending to `record.jsonl`.

    /// # Errors
    /// See the meta-records note above.
    pub fn record_step(&mut self, n: u32, tuning: Tuning) -> io::Result<()> {
        self.record_protocol(Protocol::StepStarted { n, tuning })?;
        // The display twin: `tuning` never showed on screen, so only `n`
        // duplicates across the pair.
        self.record_display(Display::Step { n })
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
        account: RecordedAccount,
        durable: bool,
    ) -> io::Result<Self> {
        let dir = sessions_root.join(session_id.to_string());
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        let source = durable.then(|| dir.join("record.jsonl"));
        let seam = match &source {
            Some(path) => crate::record::Emitter::create(path)?,
            None => crate::record::Emitter::none(),
        };
        Ok(Self {
            id: session_id,
            dir,
            durable,
            model,
            account,
            sessions_root,
            _scratch: None,
            model_memo: Memo::new(source),
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
            label: self.account.label.clone(),
            service: Some(self.account.service.clone()),
            account: Some(self.account.id.clone()),
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

    /// [`Self::record_protocol`] for the [`Display`] class — no model fold to
    /// advance, since a display commit is the view fold's alone.
    ///
    /// # Errors
    /// See [`Self::record_protocol`].
    fn record_display(&self, d: Display) -> io::Result<()> {
        let _recorded = self.seam.emit(d)?;
        Ok(())
    }

    /// [`Self::record_protocol`] with the emit made best-effort: its remit is
    /// exactly the harness-synthesized records where refusing would wedge the
    /// session — what [`Self::quiesce`] owes, and the `SessionStarted` bookends
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

    /// Exchange ids are lineage-monotone: a child's first is minted above its
    /// ancestry's reach, not above its own view, which a fork may leave
    /// empty.
    fn next_exchange(&self) -> u64 {
        self.model_memo.exchange_floor().saturating_add(1)
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

/// The next rotation number free for `record.jsonl`'s own `.N` sidecar.
fn first_free_rotation(record: &Path) -> io::Result<u64> {
    let mut n = 0;
    loop {
        if !rotation_path(record, n).exists() {
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
    use crate::agent::digest::suffix_keep_budget;
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
        AgentLog::for_test(0, "model", &RecordedAccount::for_test("provider")).expect("log")
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

    fn regex(pattern: &str) -> Regex {
        Regex::new(pattern).expect("a test pattern is a regex")
    }

    fn record_path(log: &AgentLog) -> PathBuf {
        log.dir().join("record.jsonl")
    }

    fn records(log: &AgentLog) -> Vec<Record> {
        crate::record::read_records(&record_path(log)).expect("record.jsonl round-trip")
    }

    #[test]
    fn span_partition_records_steering_imports_and_unaddressable_prefixes() {
        let mut s = fresh_root();
        s.record_error("before the first prompt".into()).unwrap();
        s.import_note(ChatMessage::user("inherited")).unwrap();
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
            !s.history_transcript()
                .messages()
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
            ContextOp::Evict {
                through_exchange: 1,
                note: None,
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
    fn edit_admissibility_refuses_live_unknown_departed_and_empty_names() {
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

        complete_exchange(&mut unknown, "two", "two");
        unknown
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 1,
                    note: None,
                },
                EditAuthority::Harness,
            )
            .unwrap();
        let err = unknown
            .apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::Model)
            .unwrap_err();
        assert_eq!(
            err,
            "exchange 1 has already left your context — the earliest still in view is 2"
        );
    }

    /// One [`TranscriptSpan`] per named span, addressed by its own
    /// `exchange` field rather than by argument order, since the order is
    /// the view's. A step marker carries no model content, so it contributes
    /// no message — [`into_chat_messages`] drops it the same way for a live
    /// provider request.
    #[test]
    fn transcript_addresses_spans_by_exchange_and_carries_their_messages() {
        let mut s = fresh_root();
        s.append_user("first prompt".into(), None).unwrap();
        s.record_step(1, Tuning::default()).unwrap();
        s.append_assistant(ChatMessage::assistant("first answer"), vec![], None)
            .unwrap();
        complete_exchange(&mut s, "second prompt", "second answer");

        let transcript = s.read_context(&[1]).expect("closed exchange is readable");
        let [exchange] = transcript.as_slice() else {
            panic!("one named span answers one TranscriptSpan, got {transcript:?}")
        };
        assert_eq!(exchange.exchange, 1);
        let [user_msg, assistant_msg] = exchange.messages.as_slice() else {
            panic!(
                "a step marker carries no message, got {:?}",
                exchange.messages
            )
        };
        assert_eq!(user_msg.role, ChatRole::User);
        assert!(
            matches!(user_msg.parts.as_slice(), [TranscriptPart::Text(text)] if text == "first prompt")
        );
        assert_eq!(assistant_msg.role, ChatRole::Assistant);
        assert!(
            matches!(assistant_msg.parts.as_slice(), [TranscriptPart::Text(text)] if text == "first answer")
        );

        complete_exchange(&mut s, "third prompt", "third answer");
        s.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
            .unwrap();
        assert_eq!(
            s.rewind_exchanges(9).unwrap_err(),
            "exchange 9 is not present in the current view — the last exchange is 3"
        );
    }

    /// The head marker's first message, or `None` when nothing has been
    /// evicted.
    fn head_marker(s: &mut AgentLog) -> Option<String> {
        s.history_transcript()
            .messages()
            .next()
            .and_then(|message| message.content.first_text())
            .filter(|text| text.starts_with("[EXARCH // Exchange"))
            .map(str::to_string)
    }

    #[test]
    fn evict_of_evict_indexes_both_cuts_and_keeps_the_suffix() {
        let mut s = fresh_root();
        for prompt in ["one", "two", "three"] {
            complete_exchange(&mut s, prompt, prompt);
        }
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 1,
                note: Some("the parser is fixed".into()),
            },
            EditAuthority::Model,
        )
        .unwrap();
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 2,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();

        assert_eq!(s.view().evictions.len(), 2);
        assert_eq!(
            s.view()
                .evictions
                .iter()
                .flat_map(|eviction| eviction.rows.iter())
                .map(|row| row.exchange)
                .collect::<Vec<_>>(),
            vec![1, 2]
        );
        assert_eq!(
            s.view()
                .spans
                .iter()
                .map(|span| span.id)
                .collect::<Vec<_>>(),
            vec![3]
        );
        let marker = head_marker(&mut s).expect("two evictions render one marker");
        assert!(marker.starts_with("[EXARCH // Exchanges 1–2 have left your context."));
        assert!(marker.contains("Your note at eviction: \"the parser is fixed\""));
    }

    /// The prompt cache reads message 0 byte for byte, so the marker must
    /// depend on the evictions and nothing else — not on how many times it is
    /// rendered, and not on whether the view was built live or refolded.
    #[test]
    fn head_marker_is_a_pure_function_of_evictions() {
        let sessions = sessions_root("head-marker-purity");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        for prompt in ["one", "two", "three"] {
            complete_exchange(&mut live, prompt, prompt);
        }
        live.apply_edit(
            ContextOp::Evict {
                through_exchange: 2,
                note: Some("keep going".into()),
            },
            EditAuthority::Model,
        )
        .unwrap();
        let first = head_marker(&mut live).expect("an eviction renders a marker");
        assert_eq!(head_marker(&mut live).as_ref(), Some(&first));
        drop(live);

        let mut resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert_eq!(head_marker(&mut resumed), Some(first));
    }

    #[test]
    fn head_marker_collapses_rows_past_the_cap() {
        let mut s = fresh_root();
        for n in 1..=45u64 {
            let prompt = format!("prompt {n}");
            complete_exchange(&mut s, &prompt, "answer");
        }
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 44,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        let marker = head_marker(&mut s).expect("an eviction renders a marker");
        assert!(
            marker.contains("1–4  (4 earlier exchanges — transcript `index)"),
            "the rows past the cap collapse to one line, got: {marker}"
        );
        assert!(
            !marker.lines().any(|line| line.starts_with("   1  "))
                && marker.lines().any(|line| line.starts_with("   5  ")),
            "the collapsed rows are the oldest, got: {marker}"
        );
        assert_eq!(
            marker.lines().count(),
            1 + 1 + 40,
            "one sentence, one collapse line, and the capped rows"
        );
    }

    /// A note is drawn only alongside a row of its own eviction, so an
    /// eviction collapsed away entirely takes its note with it — message 0
    /// stays bounded by the row cap rather than growing with how many
    /// evictions the session has run.
    #[test]
    fn a_note_collapses_with_its_rows() {
        let mut s = fresh_root();
        for n in 1..=45u64 {
            let prompt = format!("prompt {n}");
            complete_exchange(&mut s, &prompt, "answer");
            s.apply_edit(
                ContextOp::Evict {
                    through_exchange: n,
                    note: Some(format!("note {n}")),
                },
                EditAuthority::Model,
            )
            .unwrap();
        }
        let marker = head_marker(&mut s).expect("45 evictions render one marker");
        assert!(
            marker.contains("\"note 45\""),
            "the newest note must survive, got: {marker}"
        );
        assert!(
            !marker.contains("\"note 1\""),
            "a note collapsed with its rows must not survive, got: {marker}"
        );
        assert_eq!(
            marker.lines().count(),
            1 + 1 + 2 * 40,
            "bounded by the row cap, not by how many evictions ran"
        );
    }

    /// A drop of a late exchange keeps the provider's prefix cached, so it
    /// must leave message 0 exactly as it was.
    #[test]
    fn drop_does_not_touch_the_head_marker() {
        let mut s = fresh_root();
        for prompt in ["one", "two", "three", "four"] {
            complete_exchange(&mut s, prompt, prompt);
        }
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 2,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        let before = head_marker(&mut s).expect("an eviction renders a marker");
        s.apply_edit(ContextOp::Drop { exchanges: vec![3] }, EditAuthority::Model)
            .unwrap();
        assert_eq!(head_marker(&mut s), Some(before));
        assert!(
            s.view()
                .evictions
                .iter()
                .all(|eviction| eviction.rows.iter().all(|row| row.exchange != 3))
        );
    }

    #[test]
    fn evict_refuses_an_exchange_already_gone() {
        let mut s = fresh_root();
        for prompt in ["one", "two", "three"] {
            complete_exchange(&mut s, prompt, prompt);
        }
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 2,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        let err = s
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 1,
                    note: None,
                },
                EditAuthority::Model,
            )
            .unwrap_err();
        assert_eq!(
            err,
            "exchange 1 has already left your context — the earliest still in view is 3"
        );
    }

    #[test]
    fn evict_refuses_the_live_exchange() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        s.append_user("two".into(), None).unwrap();
        let err = s
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 2,
                    note: None,
                },
                EditAuthority::Model,
            )
            .unwrap_err();
        assert_eq!(
            err,
            "exchange 2 is the one you are in — a context edit may only name closed exchanges"
        );
    }

    /// What comes back from the log is what the model was sent: the records
    /// hold the clipped strings, and one rendering serves both.
    #[test]
    fn read_context_reads_an_evicted_exchange_byte_identically() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "answer one");
        complete_exchange(&mut s, "two", "answer two");
        let before = format!("{:?}", s.read_context(&[1]).expect("in view"));
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 1,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        let after = format!("{:?}", s.read_context(&[1]).expect("evicted but recorded"));
        assert_eq!(after, before);
        assert_eq!(
            s.read_context(&[9]).unwrap_err(),
            "exchange 9 was never recorded — the last closed exchange is 2"
        );
    }

    /// The store is every closed exchange, wherever it lies: the evicted one
    /// carries the weight it left at, the in-view ones the weight they still
    /// cost, and the exchange in flight is in no store at all.
    #[test]
    fn store_index_lists_in_view_and_evicted() {
        let mut s = fresh_root();
        for prompt in ["one", "two", "three"] {
            complete_exchange(&mut s, prompt, prompt);
        }
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 1,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        s.append_user("live".into(), None).unwrap();

        let index = s.store_index();
        assert_eq!(
            index
                .iter()
                .map(|item| (item.exchange, item.in_view))
                .collect::<Vec<_>>(),
            vec![(1, false), (2, true), (3, true)]
        );
        assert_eq!(index[0].opening, "one");
        assert!(
            index
                .iter()
                .all(|item| item.bytes > 0 && item.kind == ContextSpanKind::Exchange)
        );
    }

    /// One search over both sides of the ledger: the freed records read back
    /// off `record.jsonl`, the resident ones off the render cache, and the
    /// hits in store order across the seam between them.
    #[test]
    fn grep_walks_resident_then_freed() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "the parser is broken", "I will fix the parser");
        complete_exchange(&mut s, "the lexer is fine", "nothing to do");
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 1,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();

        let answer = s
            .grep_store(&regex(r"\bthe\b"), None)
            .expect("the whole store is searchable");
        assert_eq!(answer.total, 3);
        assert_eq!(
            answer
                .hits
                .iter()
                .map(|hit| (hit.exchange, hit.role.clone(), hit.line))
                .collect::<Vec<_>>(),
            vec![
                (1, ChatRole::User, 1),
                (1, ChatRole::Assistant, 1),
                (2, ChatRole::User, 1),
            ]
        );
        assert_eq!(answer.hits[0].text, "the parser is broken");

        let named = s
            .grep_store(&regex("parser"), Some(&[2]))
            .expect("a named closed exchange is searchable");
        assert_eq!(named.total, 0);
        assert_eq!(
            s.grep_store(&regex("parser"), Some(&[9])).unwrap_err(),
            "exchange 9 was never recorded — the last closed exchange is 2"
        );
    }

    /// A pattern that matches everything answers a window, not the store:
    /// `total` is what tells the model to narrow.
    #[test]
    fn grep_caps_hits_and_reports_total() {
        let mut s = fresh_root();
        let many = (1..=150)
            .map(|n| format!("line {n} matches"))
            .collect::<Vec<_>>()
            .join("\n");
        complete_exchange(&mut s, "count", &many);

        let answer = s
            .grep_store(&regex("matches"), None)
            .expect("the whole store is searchable");
        assert_eq!(answer.total, 150);
        assert_eq!(answer.hits.len(), 100);
        assert_eq!(answer.hits[0].line, 1);
        assert_eq!(
            answer.hits[99].line, 100,
            "the oldest hundred, in store order"
        );
    }

    #[test]
    fn no_logs_refuses_reading_an_evicted_exchange_and_names_the_mode() {
        let sessions = sessions_root("no-logs-read-back");
        let mut s = AgentLog::root_without_logs(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: 1,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        assert_eq!(
            s.read_context(&[1]).unwrap_err(),
            "this session runs without a log (`--no-logs`), so exchange 1 left your context for good; only exchanges still in view can be read back."
        );
        assert_eq!(
            s.grep_store(&regex("one"), Some(&[1])).unwrap_err(),
            "this session runs without a log (`--no-logs`), so exchange 1 left your context for good; only exchanges still in view can be read back."
        );
        assert_eq!(
            s.grep_store(&regex("one"), None)
                .expect("what is resident is still searchable")
                .total,
            0,
            "the head marker has already said the evicted exchanges are unreadable"
        );
        let marker = head_marker(&mut s).expect("an eviction renders a marker");
        assert!(
            marker.contains("They are not readable: this session keeps no log."),
            "a session with no store says so, got: {marker}"
        );
        assert!(!marker.contains("transcript"));
    }

    #[test]
    fn eviction_plan_walks_newest_spans_and_cuts_by_exchange() {
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
        let plan = s.plan_eviction(keep).expect("old spans to shed");
        assert_eq!(plan.through_exchange, 2);
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: plan.through_exchange,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        assert_eq!(s.context_survey().evicted, 2);
        assert!(
            !s.history_transcript()
                .messages()
                .any(|message| { message.content.first_text() == Some("one") })
        );
    }

    /// A newest exchange heavy enough to fill the whole keep budget on its
    /// own must never be the one a plan names: that would leave the model
    /// with nothing, and the id would be the live exchange `validate_edit`
    /// refuses.
    #[test]
    fn a_plan_never_takes_the_newest_span() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        let big = "x".repeat(100_000);
        complete_exchange(&mut s, "two", &big);
        let keep = suffix_keep_budget(s.history_bytes());
        let plan = s.plan_eviction(keep).expect("the older exchange to shed");
        assert_eq!(plan.through_exchange, 1);
        s.apply_edit(
            ContextOp::Evict {
                through_exchange: plan.through_exchange,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        assert_eq!(
            s.view().spans.iter().map(|span| span.id).collect::<Vec<_>>(),
            vec![2],
            "the newest span must survive the eviction it could not itself be named by"
        );
    }

    #[test]
    fn a_lone_span_is_never_planned_away() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        assert!(s.plan_eviction(0).is_none());
    }

    #[test]
    fn user_ending_import_span_is_never_torn() {
        let mut s = fresh_root();
        s.import_note(ChatMessage::user("imported user")).unwrap();
        complete_exchange(&mut s, "normal", "answer");

        let transcript = s.history_transcript();
        assert_eq!(
            transcript
                .messages()
                .filter_map(|message| message.content.first_text())
                .collect::<Vec<_>>(),
            vec!["imported user", "normal", "answer"]
        );
    }

    /// An exchange abandoned before any reply is closed by the next prompt,
    /// not by a fabricated one: nothing is recorded in its place, and none of
    /// its content reaches the model — only a user-voice note that it happened.
    #[test]
    fn an_abandoned_exchange_is_kept_whole_and_read_as_a_note() {
        let mut s = fresh_root();
        s.append_user("interrupted".into(), None).unwrap();
        s.quiesce(QuiesceReason::Cancelled);
        assert!(s.is_ready());
        assert!(
            !records(&s).iter().any(|record| matches!(
                record,
                Record::Protocol(Protocol::AssistantMessage { .. })
            )),
            "no assistant turn the model never took may enter the log"
        );
        complete_exchange(&mut s, "next", "answer");
        let transcript = s.history_transcript();
        let read: Vec<&str> = transcript
            .messages()
            .filter_map(|message| message.content.first_text())
            .collect();
        assert_eq!(
            read,
            vec![
                "[EXARCH // An exchange here was interrupted before any reply; \
                 its content is not in your context. No tool had been called.]",
                "next",
                "answer"
            ]
        );
        assert!(
            transcript
                .messages()
                .all(|message| message.role == ChatRole::User
                    || message.content.first_text() == Some("answer")),
            "the note speaks as the user, never as the assistant"
        );
        assert!(
            records(&s).iter().any(|record| matches!(
                record,
                Record::Protocol(Protocol::UserPrompt { text, .. }) if text == "interrupted"
            )),
            "record.jsonl keeps the abandoned exchange unabridged"
        );
    }

    /// The note names the one fact that outlives the context the exchange
    /// lost: whether tools had run, and so whether the world was touched.
    #[test]
    fn an_abandoned_exchange_that_ran_tools_says_so() {
        let mut s = fresh_root();
        s.append_user("run the tool".into(), None).unwrap();
        s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        s.append_tool_results(vec![ToolResult {
            id: "call".into(),
            content: "result".into(),
        }])
        .unwrap();
        s.quiesce(QuiesceReason::Cancelled);
        complete_exchange(&mut s, "next", "answer");
        assert!(
            s.history_transcript().messages().any(|message| {
                message
                    .content
                    .first_text()
                    .is_some_and(|text| text.contains("any effects on the shell and filesystem"))
            }),
            "an exchange that touched the world must say so"
        );
    }

    /// Tool calls that never ran are the one thing a quiesce still owes: the
    /// calls were really made, and a dangling tool-call block is not a legal
    /// request.
    #[test]
    fn quiesce_answers_tool_calls_that_never_ran_and_nothing_else() {
        let mut s = fresh_root();
        s.append_user("run the tool".into(), None).unwrap();
        s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        assert!(!s.is_ready(), "outstanding tool calls hold the log");
        s.quiesce(QuiesceReason::Aborted);
        assert!(s.is_ready());
        assert!(records(&s).iter().any(|record| matches!(
            record,
            Record::Protocol(Protocol::ToolResults { results })
                if results.iter().any(|result| result.id == "call")
        )));
        assert!(
            !records(&s).iter().any(|record| matches!(
                record,
                Record::Protocol(Protocol::AssistantMessage { stop_reason: Some(r), .. })
                    if r == "aborted"
            )),
            "the answer is owed; a closing assistant turn is not"
        );
        s.append_user("next".into(), None).unwrap();
    }

    /// A `reply` ends a complete exchange, so its capstone is earned — and
    /// without one the fold could not tell it from an interrupted exchange,
    /// and would drop the whole turn from the child's own view.
    #[test]
    fn a_replied_exchange_keeps_its_capstone_and_stays_visible() {
        let mut s = fresh_root();
        s.append_user("do the work".into(), None).unwrap();
        s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        s.append_tool_results(vec![ToolResult {
            id: "call".into(),
            content: "done".into(),
        }])
        .unwrap();
        s.quiesce(QuiesceReason::Replied);
        complete_exchange(&mut s, "follow-up", "answer");
        assert!(
            s.history_transcript().messages().any(|message| {
                message.content.first_text()
                    == Some("[EXARCH // Exchange ended: replied to parent.]")
            }),
            "a replied exchange stays whole in the child's own view"
        );
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

    /// A child's opening bookend names the selection it was handed, not the
    /// one its parent's file opened with: a spawn may send the child to
    /// another model, and a `/model` since has already moved the parent's own.
    #[test]
    fn a_forked_log_records_the_selection_it_was_handed() {
        let parent = fresh_root();
        let child = parent
            .fork(
                1,
                0,
                "other-model",
                &RecordedAccount::for_test("other-provider"),
            )
            .expect("child log");
        let opened = records(&child)
            .into_iter()
            .find_map(|r| match r {
                Record::Protocol(Protocol::SessionStarted { model, label, .. }) => {
                    Some((model, label))
                }
                _ => None,
            })
            .expect("the child's opening bookend");
        assert_eq!(
            opened,
            ("other-model".to_string(), "other-provider".to_string())
        );
    }

    #[test]
    fn clear_rotates_record_jsonl_and_resets_the_view() {
        let mut s = fresh_root();
        complete_exchange(&mut s, "one", "one");
        complete_exchange(&mut s, "two", "two");
        s.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
            .unwrap();

        let record = s.dir().join("record.jsonl");
        s.clear(0, 2).expect("clear");
        assert!(record.with_extension("jsonl.0").exists());
        assert!(s.view().spans.is_empty());
        assert!(s.is_ready());
    }

    #[test]
    fn transient_eviction_keeps_no_resident_state() {
        let sessions = sessions_root("residency-no-logs");
        let mut s = AgentLog::root_without_logs(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_exchange(&mut s, "one", "one");
        s.apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::User)
            .unwrap();
        assert_eq!(s.event_count(), 0);
    }

    #[test]
    fn resume_replays_a_scripted_history_and_preserves_the_model_view() {
        let sessions = sessions_root("resume-round-trip");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        live.import_note(ChatMessage::user("inherited")).unwrap();
        complete_exchange(&mut live, "one", "answer one");
        complete_exchange(&mut live, "two", "answer two");
        live.apply_edit(
            ContextOp::Evict {
                through_exchange: 2,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        let expected =
            serde_json::to_vec(&live.history_transcript().messages().collect::<Vec<_>>()).unwrap();
        drop(live);

        let mut resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert!(resumed.is_ready());
        assert_eq!(
            serde_json::to_vec(&resumed.history_transcript().messages().collect::<Vec<_>>())
                .unwrap(),
            expected
        );
    }

    #[test]
    fn resume_quiesces_a_torn_exchange_after_reopening_append_mode() {
        let sessions = sessions_root("resume-mid-exchange");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        live.append_user("run the tool".into(), None).unwrap();
        live.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        drop(live);

        let mut resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert!(resumed.is_ready());
        let recorded = records(&resumed);
        assert!(
            recorded
                .iter()
                .any(|record| matches!(record, Record::Protocol(Protocol::ToolResults { .. })))
        );
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
        let live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
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
        let live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(b"garbage\n").unwrap();
        file.flush().unwrap();
        let before = fs::read(&path).unwrap();

        assert!(
            AgentLog::resume(sessions.path(), 0).is_err(),
            "garbage is not a crash tail"
        );
        assert_eq!(fs::read(&path).unwrap(), before);
    }

    #[test]
    fn resume_refuses_foreign_protocol_data_without_mutating_the_file() {
        let sessions = sessions_root("resume-foreign");
        let live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let foreign = Record::Protocol(Protocol::AssistantMessage {
            message: ChatMessage::user("wrong role"),
            pending_tool_ids: Vec::new(),
            stop_reason: None,
        });
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&crate::record::envelope_line(&foreign))
            .unwrap();
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
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_exchange(&mut live, "one", "one");
        complete_exchange(&mut live, "two", "two");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let stale = Record::Protocol(Protocol::UserPrompt {
            exchange: 1,
            text: "stale".into(),
        });
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&crate::record::envelope_line(&stale))
            .unwrap();
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
        let child = AgentLog::root(
            sessions.path(),
            1,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
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
    fn resume_repairs_an_interior_tool_answer_lost_from_disk() {
        let sessions = sessions_root("resume-interior-seam");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        live.append_user("first".into(), None).unwrap();
        live.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        live.quiesce(QuiesceReason::Aborted);
        complete_exchange(&mut live, "second", "answer");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);

        let data = fs::read(&path).unwrap();
        let mut repaired = Vec::with_capacity(data.len());
        for fragment in data.split_inclusive(|byte| *byte == b'\n') {
            let record = crate::record::record_from_line(&fragment[..fragment.len() - 1]).unwrap();
            if matches!(record, Record::Protocol(Protocol::ToolResults { .. })) {
                continue;
            }
            repaired.extend_from_slice(fragment);
        }
        fs::write(&path, repaired).unwrap();

        let mut resumed = AgentLog::resume(sessions.path(), 0).expect("interior seam repair");
        assert!(resumed.is_ready());
        let transcript = resumed.history_transcript();
        assert!(
            transcript.messages().any(|message| {
                message
                    .content
                    .first_text()
                    .is_some_and(|text| text.starts_with("[EXARCH // An exchange here"))
            }),
            "the repaired seam admits the log; the abandoned exchange reads as its note"
        );
        assert!(
            !transcript
                .messages()
                .any(|message| message.content.first_text() == Some("first")),
            "none of the abandoned exchange's own content reaches the model"
        );
        resumed.append_user("next".into(), None).unwrap();
    }

    #[test]
    fn resume_matches_live_after_a_small_edit_sequence_family() {
        for pattern in 0..8 {
            let sessions = sessions_root(&format!("resume-edits-{pattern}"));
            let mut live = AgentLog::root(
                sessions.path(),
                0,
                "model",
                &RecordedAccount::for_test("provider"),
                0,
            )
            .unwrap();
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
                        ContextOp::Evict {
                            through_exchange: 2,
                            note: None,
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
                        ContextOp::Evict {
                            through_exchange: 2,
                            note: Some("halfway".into()),
                        },
                        EditAuthority::Harness,
                    )
                    .unwrap();
                    live.apply_edit(ContextOp::Drop { exchanges: vec![3] }, EditAuthority::Model)
                        .unwrap();
                }
                5 => {
                    live.apply_edit(ContextOp::Drop { exchanges: vec![2] }, EditAuthority::User)
                        .unwrap();
                    complete_exchange(&mut live, "four", "four");
                }
                6 => {
                    live.apply_edit(
                        ContextOp::Evict {
                            through_exchange: 1,
                            note: None,
                        },
                        EditAuthority::Harness,
                    )
                    .unwrap();
                    live.apply_edit(
                        ContextOp::Evict {
                            through_exchange: 2,
                            note: Some("second cut".into()),
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
            let expected =
                serde_json::to_vec(&live.history_transcript().messages().collect::<Vec<_>>())
                    .unwrap();
            drop(live);
            let mut resumed = AgentLog::resume(sessions.path(), 0).expect("resume edit sequence");
            assert!(resumed.is_ready());
            assert_eq!(
                serde_json::to_vec(&resumed.history_transcript().messages().collect::<Vec<_>>())
                    .unwrap(),
                expected,
                "pattern {pattern}"
            );
        }
    }

    /// `resume` refuses unless its from-scratch refold agrees with the
    /// incremental memo, and an eviction's rows are part of that agreement —
    /// so a refold that re-rendered a freed span differently would refuse
    /// here rather than serve the model a marker that had drifted.
    #[test]
    fn fold_equals_memo_across_evict() {
        let sessions = sessions_root("fold-equals-memo-evict");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        live.append_user("interrupted".into(), None).unwrap();
        live.quiesce(QuiesceReason::Cancelled);
        for prompt in ["one", "two", "three"] {
            complete_exchange(&mut live, prompt, prompt);
        }
        live.apply_edit(
            ContextOp::Evict {
                through_exchange: 3,
                note: Some("the abandoned exchange is indexed too".into()),
            },
            EditAuthority::Model,
        )
        .unwrap();
        let evictions = live.view().evictions.clone();
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert_eq!(resumed.view().evictions, evictions);
    }

    /// A `mnemon` child of `parent`: its own log under the same sessions
    /// root, opened with the spans and the link the parent hands over.
    fn mnemon(parent: &mut AgentLog, child_id: AgentId) -> AgentLog {
        let inherited = parent.inherited_context();
        let mut child = parent
            .fork(child_id, 0, "model", &RecordedAccount::for_test("provider"))
            .expect("child log");
        child.import_context(inherited).expect("import");
        child
    }

    fn seed_has_a_tool_call(log: &mut AgentLog) -> bool {
        log.history_transcript().messages().any(|message| {
            message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::ToolCall(_)))
        })
    }

    /// A `ContextEdited` record with no exchange id of its own glues onto
    /// whatever span is still open, so a `drop` run mid-batch lands right
    /// after the dangling assistant frame rather than replacing it. The seed
    /// must still cut at the tool call, not at the edit that followed it.
    #[test]
    fn a_seed_cut_never_owes_a_tool_result() {
        let mut parent = fresh_root();
        complete_exchange(&mut parent, "one", "one");
        parent.append_user("two".into(), None).unwrap();
        parent
            .append_assistant(assistant_with_tool("call-1"), vec!["call-1".into()], None)
            .unwrap();
        parent
            .apply_edit(ContextOp::Drop { exchanges: vec![1] }, EditAuthority::Model)
            .unwrap();

        let mut child = mnemon(&mut parent, 1);
        assert!(
            !seed_has_a_tool_call(&mut child),
            "a mnemon seed must never carry an unanswered tool call"
        );
        assert!(
            child
                .history_transcript()
                .messages()
                .any(|message| message.content.first_text() == Some("two")),
            "the prompt behind the dangling call must still seed"
        );
    }

    /// The plain in-batch fork: no edit lands after the dangling assistant
    /// frame, so the frame itself is the span's last record, and the cut
    /// must still land in front of it.
    #[test]
    fn a_seed_cut_never_owes_a_tool_result_with_no_edit() {
        let mut parent = fresh_root();
        complete_exchange(&mut parent, "one", "one");
        parent.append_user("two".into(), None).unwrap();
        parent
            .append_assistant(assistant_with_tool("call-1"), vec!["call-1".into()], None)
            .unwrap();

        let mut child = mnemon(&mut parent, 1);
        assert!(
            !seed_has_a_tool_call(&mut child),
            "the plain in-batch fork must not regress"
        );
        assert!(
            child
                .history_transcript()
                .messages()
                .any(|message| message.content.first_text() == Some("two")),
        );
    }

    fn openings(span: &TranscriptSpan) -> Vec<&str> {
        span.messages
            .iter()
            .filter_map(|message| match message.parts.first() {
                Some(TranscriptPart::Text(text)) => Some(text.as_str()),
                _ => None,
            })
            .collect()
    }

    /// The lineage is one address space: what the parent evicted before the
    /// fork is still the child's to read, under the number the parent gave
    /// it, and the child's own ids start above every one of them.
    #[test]
    fn a_mnemon_child_reads_an_ancestors_evicted_exchange_and_mints_ids_above_it() {
        let sessions = sessions_root("lineage-read");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        for prompt in ["one", "two", "three", "four"] {
            complete_exchange(&mut parent, prompt, prompt);
        }
        parent
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 3,
                    note: None,
                },
                EditAuthority::Harness,
            )
            .unwrap();

        let mut child = mnemon(&mut parent, 1);
        assert_eq!(
            child
                .view()
                .spans
                .iter()
                .map(|span| span.id)
                .collect::<Vec<_>>(),
            vec![4],
            "the parent's surviving span crosses under the parent's own id"
        );
        complete_exchange(&mut child, "the child's own", "answer");
        assert_eq!(child.current_exchange(), Some(5));

        let read = child
            .read_context(&[2])
            .expect("an ancestor's evicted exchange is in the lineage's store");
        let [span] = read.as_slice() else {
            panic!("one named exchange answers one span, got {read:?}")
        };
        assert_eq!(span.exchange, 2);
        assert_eq!(openings(span), vec!["two", "two"]);

        let mut grandchild = mnemon(&mut child, 2);
        let read = grandchild
            .read_context(&[2])
            .expect("two links back is still the same store");
        assert_eq!(openings(&read[0]), vec!["two", "two"]);
        complete_exchange(&mut grandchild, "the grandchild's own", "answer");
        assert_eq!(grandchild.current_exchange(), Some(6));
    }

    /// A `/rewind` at a ready boundary can leave a parent with no span at
    /// all; the ids it minted are still spent, and the child says so.
    #[test]
    fn an_empty_parent_view_still_floors_the_childs_first_exchange() {
        let sessions = sessions_root("lineage-floor");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_exchange(&mut parent, "one", "one");
        complete_exchange(&mut parent, "two", "two");
        parent
            .apply_edit(
                ContextOp::Drop {
                    exchanges: vec![1, 2],
                },
                EditAuthority::User,
            )
            .unwrap();
        assert!(parent.view().spans.is_empty());

        let mut child = mnemon(&mut parent, 1);
        assert!(child.view().spans.is_empty());
        child.append_user("first".into(), None).unwrap();
        assert_eq!(child.current_exchange(), Some(3));
    }

    /// A link to a parent that kept no log ends the lineage, and the refusal
    /// names the parent rather than blaming the child's own mode.
    #[test]
    fn a_no_logs_link_refuses_naming_the_ancestor() {
        let sessions = sessions_root("lineage-no-logs");
        let mut parent = AgentLog::root_without_logs(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_exchange(&mut parent, "one", "one");
        complete_exchange(&mut parent, "two", "two");
        parent
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 1,
                    note: None,
                },
                EditAuthority::Harness,
            )
            .unwrap();

        let mut child = mnemon(&mut parent, 1);
        assert_eq!(
            child.read_context(&[1]).unwrap_err(),
            "an ancestor of this session ran `--no-logs`, so exchange 1 is unreadable; only what was handed down at the fork is still in your view."
        );
    }

    /// Only a fork's opening may link a log to an ancestry: a link found
    /// after the log has a context of its own is foreign data.
    #[test]
    fn admission_refuses_inherited_after_the_first_exchange() {
        let sessions = sessions_root("lineage-late-link");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_exchange(&mut live, "one", "one");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let late = Record::Protocol(Protocol::Inherited {
            source: None,
            evictions: Vec::new(),
            through_exchange: 0,
        });
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&crate::record::envelope_line(&late))
            .unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("a late link is foreign data");
        let text = error.to_string();
        assert!(text.contains("only a fork's opening may do"), "{text}");
    }

    /// The link is folded, not just recorded: a refold reads the parent's
    /// head-marker state back off it, so the `fold == memo` law covers an
    /// inherited view as it covers an evicted one.
    #[test]
    fn fold_equals_memo_across_evict_and_inherited() {
        let ancestry = sessions_root("fold-equals-memo-ancestor");
        let mut ancestor = AgentLog::root(
            ancestry.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        for prompt in ["one", "two", "three"] {
            complete_exchange(&mut ancestor, prompt, prompt);
        }
        ancestor
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 2,
                    note: Some("the parser is fixed".into()),
                },
                EditAuthority::Model,
            )
            .unwrap();
        let inherited = ancestor.inherited_context();

        let sessions = sessions_root("fold-equals-memo-inherited");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        live.import_context(inherited).unwrap();
        complete_exchange(&mut live, "own", "own");
        live.apply_edit(
            ContextOp::Evict {
                through_exchange: 3,
                note: None,
            },
            EditAuthority::Harness,
        )
        .unwrap();
        let evictions = live.view().evictions.clone();
        assert_eq!(evictions.len(), 2, "the parent's cut and this log's own");
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert_eq!(resumed.view().evictions, evictions);
    }

    /// The index is the whole lineage, oldest first, each exchange listed
    /// once: an inherited exchange the child holds itself is an import of its
    /// own, never a second row from the ancestry.
    #[test]
    fn store_index_lists_in_view_and_evicted_and_inherited() {
        let sessions = sessions_root("lineage-index");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        for prompt in ["one", "two", "three"] {
            complete_exchange(&mut parent, prompt, prompt);
        }
        parent
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 2,
                    note: None,
                },
                EditAuthority::Harness,
            )
            .unwrap();

        let mut child = mnemon(&mut parent, 1);
        complete_exchange(&mut child, "four", "four");

        let index = child.store_index();
        assert_eq!(
            index
                .iter()
                .map(|item| (item.exchange, item.kind, item.in_view))
                .collect::<Vec<_>>(),
            vec![
                (1, ContextSpanKind::Inherited, false),
                (2, ContextSpanKind::Inherited, false),
                (3, ContextSpanKind::Import, true),
                (4, ContextSpanKind::Exchange, true),
            ]
        );
        assert_eq!(index[0].opening, "one");
        assert!(index.iter().all(|item| item.bytes > 0));
    }

    /// One search over the whole lineage: the child's resident span, its own
    /// freed records, and then the ancestor's file — in store order across
    /// all three, and never the same exchange twice.
    #[test]
    fn grep_walks_resident_then_freed_then_ancestors() {
        let sessions = sessions_root("lineage-grep");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_exchange(&mut parent, "the parser is broken", "I will fix the parser");
        complete_exchange(&mut parent, "the lexer is fine", "nothing to do");
        parent
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 1,
                    note: None,
                },
                EditAuthority::Harness,
            )
            .unwrap();

        let mut child = mnemon(&mut parent, 1);
        complete_exchange(&mut child, "the tests pass", "good");
        child
            .apply_edit(
                ContextOp::Evict {
                    through_exchange: 2,
                    note: None,
                },
                EditAuthority::Harness,
            )
            .unwrap();

        let answer = child
            .grep_store(&regex(r"\bthe\b"), None)
            .expect("the lineage's whole store is searchable");
        assert_eq!(answer.total, 4);
        assert_eq!(
            answer
                .hits
                .iter()
                .map(|hit| (hit.exchange, hit.role.clone()))
                .collect::<Vec<_>>(),
            vec![
                (1, ChatRole::User),
                (1, ChatRole::Assistant),
                (2, ChatRole::User),
                (3, ChatRole::User),
            ]
        );
        assert_eq!(answer.hits[0].text, "the parser is broken");
    }

    #[test]
    fn mirror_only_roots_have_no_durable_records() {
        let sessions = sessions_root("no-logs");
        let log = AgentLog::root_without_logs(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        assert!(!log.is_durable());
        assert!(!log.dir().join("record.jsonl").exists());
    }
}
