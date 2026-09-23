//! `AgentLog`: one session's handle onto `sessions/<n>/record.jsonl`, the one
//! seam every fact crosses, and [`Context`], the model fold's authoritative
//! in-memory structure over it.
//!
//! Every query below reads `context`; nothing here keeps a second copy.
//! `tui::scrollback` folds the same log's `Display`/`Forensic` classes into the
//! rendered `user.log`.

use crate::agent::build::RecordedAccount;
use crate::bus::AgentId;
use crate::provider::{CutShort, ProviderError, Tuning, Usage};
use crate::record::model::{Context, Linked, TranscriptRead, TurnRow};
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
        cause: CutShortRecord,
    },
    Other {
        cause: String,
    },
}

/// Serialisable mirror of [`CutShort`].
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "cut", rename_all = "snake_case")]
pub enum CutShortRecord {
    OutputCap {
        stop_reason: String,
    },
    /// Boxed for the same reason [`ProviderError::Truncated`] boxes its cause:
    /// the mirror is recursive too.
    Stalled {
        error: Box<ProviderErrorRecord>,
    },
}

impl ProviderErrorRecord {
    /// The failure that broke the stream, for a truncation the streamed prefix
    /// survived — and `None` for every failure that ends the turn.  The
    /// one place that reading is derived: the TUI fold, synod's seam and the
    /// headless printer all grade a stall below a fatal error, and each asks
    /// here rather than re-matching the shape.
    pub fn stall_cause(&self) -> Option<&Self> {
        match self {
            Self::Truncated {
                cause: CutShortRecord::Stalled { error },
            } => Some(error),
            _ => None,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditAuthority {
    Model,
    User,
    Harness,
}

/// One eviction as recorded: the resident turns it took — resolved by the
/// writer, so replay departs exactly these — and the model's note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cut {
    pub turns: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
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
            ProviderError::Truncated { cause } => Self::Truncated {
                cause: match cause.as_ref() {
                    CutShort::OutputCap { stop_reason } => CutShortRecord::OutputCap {
                        stop_reason: stop_reason.clone(),
                    },
                    CutShort::Stalled(error) => CutShortRecord::Stalled {
                        error: Box::new(error.into()),
                    },
                },
            },
            ProviderError::Other(s) => Self::Other { cause: s.clone() },
        }
    }
}

/// Why the work in hand is ending with no real assistant reply: decides whether it
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

/// Who took a turn: a prompt (or an import's opening) is the user's; the
/// assistant message, its tool results and any steering are the assistant's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// Where a turn came from: this session's own work, a harness-authored
/// import, or a turn a fork inherited from its parent's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    Own,
    Import,
    Inherited,
}

impl TurnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Import => "import",
            Self::Inherited => "inherited",
        }
    }
}

/// `` exarch-context `survey ``'s answer: one row per turn in the context, beside
/// the truth about what is sent.
///
/// `total_bytes` is the assembled context's own weight, not the sum of the
/// rows: each hole's marker weighs too, and is no turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextSurvey {
    pub rows: Vec<TurnRow>,
    pub total_bytes: usize,
}

/// One line of one message that matched `` exarch-transcript `grep ``'s pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepHit {
    /// The turn the line lies in — the search walks turn by turn, so a hit
    /// names one outright.
    pub turn: u64,
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

/// `` exarch-transcript `read ``'s answer: one element per turn, in transcript
/// order, each naming whose turn it was.
#[derive(Clone, Debug)]
pub struct TranscriptTurn {
    pub turn: u64,
    pub role: Role,
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
/// [`Context`] — the one authoritative in-memory structure, per
/// `dev/docs/plans/260814_one_seam_one_log.md`.
pub struct AgentLog {
    id: AgentId,
    dir: PathBuf,
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
    /// The model fold's own structure over `record.jsonl` — the type every
    /// query in this file answers from; advanced inline through
    /// [`Self::advance`] on every fact this log authors.
    context: Context,
    /// The one seam: every fact this log authors crosses here as a `Record` —
    /// appended to `sessions/<n>/record.jsonl` and published on whatever bus
    /// `Avatar::couple` has attached, in that order, under one lock.
    seam: crate::record::Emitter,
}

/// What a `mnemon` child is forked with: its parent's whole turn table, turn
/// by turn under the parent's own ids, each row carrying the address that
/// makes it readable whoever recorded it first.
pub struct Inherited {
    /// The parent's table at the fork, each row beside the address its
    /// transcript copy lies at.
    pub turns: Vec<Linked>,
    /// The notes made at those evictions: the child folds them in, so its
    /// markers are the same projection the parent's were.
    pub notes: Vec<Option<String>>,
    /// The material for each resident turn the seed could carry — turn id,
    /// messages — in id order.
    pub seed: Vec<(u64, Vec<ChatMessage>)>,
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
        )?;
        s.record_started(
            Some(self.id),
            system_prompt_bytes,
            crate::bootstrap::now_unix_ms(),
        )?;
        Ok(s)
    }

    /// Fold `record.jsonl` into the turn table, quarantining a torn tail, and
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
        let dir = Self::dir_of(sessions_root, session_id);
        let record_path = dir.join("record.jsonl");
        if !record_path.exists() {
            return Err(io::Error::other(format!(
                "cannot resume {}: no record.jsonl was found here; a session recorded before this exarch's one-seam-one-log change cannot be resumed — was this session started with an older exarch, or is {} the right directory to resume?",
                record_path.display(),
                dir.display()
            )));
        }
        let (context, model, label) =
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
            model,
            account,
            sessions_root: sessions_root.to_path_buf(),
            _scratch: None,
            context,
            seam,
        };
        if !resumed.is_ready() {
            resumed.quiesce(QuiesceReason::Aborted);
        }
        Ok(resumed)
    }

    pub fn resumed_summary(&self) -> (u64, u64) {
        let bytes =
            fs::metadata(self.dir.join("record.jsonl")).map_or(0, |metadata| metadata.len());
        (self.context.current_turn().unwrap_or(0), bytes)
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
        self.record_forensic(Forensic::SessionResumed {
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
        if let Err(refusal) = crate::record::model::Model::step(&mut self.context, record) {
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

    /// Where session `id`'s log lives under `sessions_root`, before it opens.
    pub(crate) fn dir_of(sessions_root: &Path, id: AgentId) -> PathBuf {
        sessions_root.join(id.to_string())
    }

    /// The per-session directory `<sessions_root>/<id>/`.
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// Whether a fresh user prompt is admissible.  The attend loop leaves every
    /// turn here, and an eviction demands it.
    pub fn is_ready(&self) -> bool {
        self.context.is_ready()
    }

    pub fn can_evict(&self) -> bool {
        self.context.can_evict()
    }

    /// Number of records still owned by the context, for the host's resource
    /// probe; the structure retains the rows an edit moves out of it.
    pub fn event_count(&self) -> usize {
        self.context.event_count()
    }

    /// The model's own line to its future self, one slot per eviction.
    pub fn notes(&self) -> &[Option<String>] {
        self.context.notes()
    }

    pub fn context_survey(&self) -> ContextSurvey {
        self.context.context_survey()
    }

    /// Read the turns named, in transcript order, whether they are in the
    /// context or departed. Both halves in one call; the desk splits them
    /// across its lock.
    ///
    /// # Errors
    /// Refuses a read that names no turn, a turn this lineage never
    /// recorded, the turn still being written, or one whose file will not
    /// read back.
    pub fn read_transcript(&self, turns: &[u64]) -> Result<Vec<TranscriptTurn>, String> {
        self.context.read_transcript(turns)
    }

    /// [`Self::read_transcript`]'s locating half, for a caller that means to
    /// read outside the session lock.
    ///
    /// # Errors
    /// Refuses whatever [`Self::read_transcript`] refuses.
    pub(crate) fn locate_read(&self, turns: &[u64]) -> Result<TranscriptRead, String> {
        self.context.locate_read(turns)
    }

    /// Every turn the transcript holds, in id order, each saying whether it
    /// is still in the context.
    pub fn transcript_index(&self) -> Vec<TurnRow> {
        self.context.transcript_index()
    }

    /// Search the closed turns' text — those a narrowing names, or the whole
    /// transcript when none is given. Both halves in one call; the desk
    /// splits them across its lock.
    ///
    /// # Errors
    /// Refuses a narrowing the way [`Self::read_transcript`] does.
    pub fn grep_transcript(
        &self,
        pattern: &Regex,
        turns: Option<&[u64]>,
    ) -> Result<GrepAnswer, String> {
        self.context.grep_transcript(pattern, turns)
    }

    /// [`Self::grep_transcript`]'s locating half, for a caller that means to
    /// search outside the session lock.
    ///
    /// # Errors
    /// Refuses whatever [`Self::grep_transcript`] refuses.
    pub(crate) fn locate_grep(&self, turns: Option<&[u64]>) -> Result<TranscriptRead, String> {
        self.context.locate_grep(turns)
    }

    /// Approximate context size in serialised bytes.  The fallback eviction
    /// trigger in [`crate::agent::Avatar::evict`] when the model's context
    /// window is unknown; otherwise that tracks token pressure instead.
    pub fn history_bytes(&self) -> usize {
        self.context.history_bytes()
    }

    /// Render the context for the next provider request.
    ///
    /// # Errors
    /// The session is not awaiting an assistant reply.
    pub fn render_messages(&self) -> Result<Vec<ChatMessage>, String> {
        if !self.context.is_awaiting_assistant() {
            return Err(format!(
                "cannot render request while the session is {}",
                self.context.waiting_for()
            ));
        }
        Ok(self.context.rendered())
    }

    /// Every committed message whatever the phase.
    pub fn history_rendered(&self) -> Vec<ChatMessage> {
        self.context.rendered()
    }

    /// The parent half of a `mnemon` fork — the seed, where ownership
    /// genuinely transfers into the child's own ledger via
    /// [`Self::import_context`]. The last turn is cut to the longest run
    /// that owes no tool result, whatever left it that way — a batch in
    /// flight, or a `ContextEdited` record landing after the assistant frame
    /// it answers.
    pub fn inherited_context(&self) -> Inherited {
        Inherited {
            turns: self.context.linked(),
            notes: self.context.notes().to_vec(),
            seed: self.context.inherited_seed(),
        }
    }

    /// Import a parent's context without appending a prompt: a `mnemon`
    /// child's launch prompt arrives through its inbox, and `deliberate`
    /// commits it through [`Self::append_user`] like any other prompt.
    ///
    /// The link goes down first, carrying the parent's whole table and the
    /// notes made at its evictions, then one
    /// `ContextMessage` per message under the parent's own turn id — so the
    /// child's context reproduces the parent's turns and can name any of
    /// them.
    ///
    /// # Errors
    /// The session is not at a ready boundary, or recording a record failed.
    pub fn import_context(&mut self, inherited: Inherited) -> Result<(), String> {
        self.ready_to_import()?;
        let Inherited { turns, notes, seed } = inherited;
        self.record_protocol(Protocol::Inherited { turns, notes })
            .map_err(|e| e.to_string())?;
        for (id, messages) in seed {
            for message in messages {
                self.record_protocol(Protocol::ContextMessage { id, message })
                    .map_err(|e| e.to_string())?;
            }
        }
        Ok(())
    }

    /// One harness-authored message imported as a turn of this log's own —
    /// the resume note, which inherits nothing and links to no one.
    ///
    /// # Errors
    /// The session is not at a ready boundary, or recording failed.
    pub fn import_note(&mut self, message: ChatMessage) -> Result<(), String> {
        self.ready_to_import()?;
        let id = self.context.next_id();
        self.record_protocol(Protocol::ContextMessage { id, message })
            .map_err(|e| e.to_string())
    }

    fn ready_to_import(&self) -> Result<(), String> {
        if self.context.is_ready() {
            return Ok(());
        }
        Err(format!(
            "cannot import context while the session is {}",
            self.context.waiting_for()
        ))
    }

    // ── Protocol mutations ────────────────────────────────────────────────

    /// Commit a top-level user prompt, opening a turn of its own — or
    /// extending the turn `continues` names, where that prompt is still the
    /// live one.
    ///
    /// # Errors
    /// The session is mid-turn, or recording the prompt failed.
    pub fn append_user(&mut self, text: String, continues: Option<u64>) -> Result<(), String> {
        if !self.context.is_ready() {
            return Err(format!(
                "cannot accept a new user prompt while the session is {}",
                self.context.waiting_for()
            ));
        }
        // A continuation extends the newest turn, so it is honoured only while
        // that turn is in the context and `id` is the prompt it answers.
        let record = if continues.is_some_and(|id| self.context.live_prompt() == Some(id)) {
            Protocol::Steering { text }
        } else {
            Protocol::UserPrompt {
                turn: self.context.next_id(),
                text,
            }
        };
        self.record_protocol(record).map_err(|e| e.to_string())
    }

    /// Append a user message between a complete tool-result batch and the next
    /// provider request.  The only mid-turn user ingress there is, which is
    /// why it admits the tool-results-awaited phase alone.
    ///
    /// # Errors
    /// The tool-result batch is incomplete, or recording the prompt failed.
    pub fn append_steering(&mut self, text: String) -> Result<(), String> {
        if !self.context.is_awaiting_steering() {
            return Err(format!(
                "tool results must be complete before accepting a steering prompt; the session is {}",
                self.context.waiting_for()
            ));
        }
        if self.context.current_turn().is_none() {
            return Err("cannot accept a steering prompt before a turn has started".into());
        }
        self.record_protocol(Protocol::Steering { text })
            .map_err(|e| e.to_string())
    }

    /// Commit an assistant reply, moving the turn on to await tool results
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
        if !self.context.is_awaiting_assistant() {
            return Err(format!(
                "assistant message is not expected while the session is {}",
                self.context.waiting_for()
            ));
        }
        if message.role != ChatRole::Assistant {
            return Err(format!(
                "a {} message arrived where the assistant's reply was expected",
                role_label(&message.role)
            ));
        }
        let turn = self.context.next_id();
        self.record_protocol(Protocol::AssistantMessage {
            turn,
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
        let Some(pending_ids) = self.context.pending_tool_results() else {
            return Err(format!(
                "tool results are not expected while the session is {}",
                self.context.waiting_for()
            ));
        };
        validate_result_ids(&pending_ids, &results)?;
        self.record_protocol(Protocol::ToolResults { results })
            .map_err(|e| e.to_string())
    }

    /// End work that produced no real assistant reply, recording only what
    /// the log still owes: an answer to tool calls that never ran, and a
    /// capstone for work that ended on `reply`.  Work abandoned short of a
    /// reply is left as it lies, and the next prompt opens a turn after it.
    pub fn quiesce(&mut self, reason: QuiesceReason) {
        for record in self.context.quiesce_records(reason) {
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

    /// The resolved turns the harness's own pressure cut would take, to spend
    /// no more than `keep_budget_bytes` on what stays; `None` when nothing is
    /// old enough to shed.
    pub fn plan_eviction(&self, keep_budget_bytes: usize) -> Option<Vec<u64>> {
        self.context.plan_eviction(keep_budget_bytes)
    }

    /// Take `turns` out of the context at once, leaving a marker where they
    /// stood and `note` beneath it. The set is resolved before it is
    /// recorded, so replay departs exactly the ids on disk.
    ///
    /// # Errors
    /// Refuses an eviction naming no turn, a turn never recorded, one that
    /// has already left, the turn being written now, and a set that would
    /// take nothing.
    pub fn evict(
        &mut self,
        turns: &[u64],
        note: Option<String>,
        by: EditAuthority,
    ) -> Result<(), String> {
        let turns = self.context.resolve_cut(turns)?;
        let cut = Cut { turns, note };
        self.record_protocol(Protocol::Evicted {
            cut: cut.clone(),
            by,
        })
        .map_err(|e| e.to_string())?;
        // The display twin: the screen never derives from the protocol
        // record it duplicates a field of.
        self.record_display(Display::Evicted { cut, by })
            .map_err(|e| e.to_string())
    }

    /// Every resident turn from `anchor` on — what a user rewind takes. The
    /// anchor is checked before the suffix is derived.
    ///
    /// # Errors
    /// Refuses an absent anchor or one that has already left the context.
    pub fn suffix_from(&self, anchor: u64) -> Result<Vec<u64>, String> {
        self.context.suffix_from(anchor)
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
        self.context = Context::new(record_path);
        let started = self.started_event(None, system_prompt_bytes, at_unix_ms);
        let seam_error = self.record_forensic(started).err();
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
        reason = "[silent:record-file] rotates the session's record.jsonl on /clear; output infra, not turn-time data I/O"
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

    /// The act of taking a turn: the meta half through the protocol class,
    /// and the id the request will produce through the display one.
    ///
    /// # Errors
    /// See the meta-records note above.
    pub fn record_turn_start(&mut self, tuning: Tuning, route: Option<String>) -> io::Result<()> {
        let id = self.context.next_id();
        self.record_forensic(Forensic::TurnStarted { tuning, route })?;
        // The display twin: neither `tuning` nor `route` ever showed on
        // screen, and the id is the screen's alone — nothing duplicates
        // across the pair.
        self.record_display(Display::Turn { id })
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_usage(&mut self, usage: UsageDelta) -> io::Result<()> {
        self.record_forensic(Forensic::UsageDelta { usage })
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_session_ended(&mut self) -> io::Result<()> {
        self.record_forensic(Forensic::SessionEnded)
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

    // ── Internal helpers ──────────────────────────────────────────────────

    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:session-dir] (re)creates the session log dir; event-log infra, not turn-time data I/O"
    )]
    fn open_fresh(
        sessions_root: PathBuf,
        session_id: AgentId,
        model: String,
        account: RecordedAccount,
    ) -> io::Result<Self> {
        let dir = Self::dir_of(&sessions_root, session_id);
        if dir.exists() {
            fs::remove_dir_all(&dir)?;
        }
        fs::create_dir_all(&dir)?;
        let source = dir.join("record.jsonl");
        let seam = crate::record::Emitter::create(&source)?;
        Ok(Self {
            id: session_id,
            dir,
            model,
            account,
            sessions_root,
            _scratch: None,
            context: Context::new(source),
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
        self.record_forensic(started)
    }

    fn started_event(
        &self,
        parent: Option<AgentId>,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) -> Forensic {
        Forensic::SessionStarted {
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
    /// session — what [`Self::quiesce`] owes.  A genuine append failure here
    /// still leaves the model fold unadvanced for this one record, since there
    /// is no witnessed result to advance it with — the honest price of the law
    /// that a failed append is a session error, not a shrug, even when the
    /// caller cannot propagate it.
    fn record_protocol_lossy(&mut self, p: Protocol) {
        match self.seam.emit(p) {
            Ok(recorded) => self.advance(&widen(recorded)),
            Err(error) => eprintln!(
                "exarch: a harness-synthesized record was not recorded in record.jsonl: {error}"
            ),
        }
    }

    /// The newest turn's id — what every tool result's `TURN:` stamp names.
    pub fn current_turn(&self) -> Option<u64> {
        self.context.current_turn()
    }

    /// The newest user turn's id: the prompt the work in hand answers.
    pub fn current_prompt(&self) -> Option<u64> {
        self.context.current_prompt()
    }

    pub fn log_len(&self) -> usize {
        self.context.log_len()
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.context.token_measure_is_stale(measured_at)
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

/// The four spellings a role crosses under, whether as a message's tag, a
/// grep hit's `Str`, or the subject of a refusal.
pub(crate) fn role_label(role: &ChatRole) -> &'static str {
    match role {
        ChatRole::System => "system",
        ChatRole::User => "user",
        ChatRole::Assistant => "assistant",
        ChatRole::Tool => "tool",
    }
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
    reason = "[test] test fs/process scaffolding"
)]
mod tests {
    use super::*;
    use crate::agent::digest::suffix_keep_budget;
    use crate::record::model::Held;
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

    fn complete_round(s: &mut AgentLog, user: &str, answer: &str) {
        s.append_user(user.into(), None).unwrap();
        s.append_assistant(ChatMessage::assistant(answer), vec![], None)
            .unwrap();
    }

    fn regex(pattern: &str) -> Regex {
        Regex::new(pattern).expect("a test pattern is a regex")
    }

    /// The resident turns at or below `through`: what a prefix cut names, now
    /// spelled as the set it always was.
    fn prefix(log: &AgentLog, through: u64) -> Vec<u64> {
        log.context_survey()
            .rows
            .iter()
            .map(|row| row.id)
            .filter(|id| *id <= through)
            .collect()
    }

    /// Every resident turn of the named prompts: each prompt and the turns
    /// answering it, up to the next prompt.
    fn answered(log: &AgentLog, prompts: &[u64]) -> Vec<u64> {
        let mut under = false;
        log.context_survey()
            .rows
            .iter()
            .filter(|row| {
                if row.role == Role::User {
                    under = prompts.contains(&row.id);
                }
                under
            })
            .map(|row| row.id)
            .collect()
    }

    fn record_path(log: &AgentLog) -> PathBuf {
        log.dir().join("record.jsonl")
    }

    fn records(log: &AgentLog) -> Vec<Record> {
        crate::record::read_records(&record_path(log)).expect("record.jsonl round-trip")
    }
    /// Every id-bearing record opens a turn; everything else extends the last.
    /// Steering names the turn in hand, so it joins the assistant turn it
    /// answers rather than opening one.
    #[test]
    fn turn_partition_joins_steering_and_opens_one_turn_per_id() {
        let mut s = fresh_root();
        s.record_error("before the first prompt".into()).unwrap();
        s.import_note(ChatMessage::user("inherited")).unwrap();
        complete_round(&mut s, "prompt", "answer");
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
            s.transcript_index()
                .iter()
                .map(|row| (row.id, row.role))
                .collect::<Vec<_>>(),
            vec![
                (1, Role::User),
                (2, Role::User),
                (3, Role::Assistant),
                (4, Role::User),
                (5, Role::Assistant),
                (6, Role::Assistant),
            ]
        );
        assert_eq!(s.transcript_index()[0].kind, TurnKind::Import);
        assert!(
            !s.history_rendered()
                .iter()
                .any(|message| message.content.first_text() == Some("before the first prompt"))
        );
    }

    /// One id space, so a prompt's id says nothing about how many came before
    /// it — intended.
    #[test]
    fn ids_mint_monotonically_across_edits() {
        let mut s = fresh_root();
        complete_round(&mut s, "one", "one");
        complete_round(&mut s, "two", "two");
        assert_eq!(s.current_prompt(), Some(3));
        s.evict(&answered(&s, &[3]), None, EditAuthority::Model)
            .unwrap();
        complete_round(&mut s, "three", "three");
        assert_eq!(s.current_prompt(), Some(5));
        s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
            .unwrap();
        complete_round(&mut s, "four", "four");
        assert_eq!(s.current_prompt(), Some(7));
        assert_eq!(s.current_turn(), Some(8), "the newest turn is the answer");
    }

    #[test]
    fn continues_resolution_requires_the_live_prompt() {
        let mut joins = fresh_root();
        complete_round(&mut joins, "one", "one");
        joins.append_user("nudge".into(), Some(1)).unwrap();
        assert_eq!(joins.current_prompt(), Some(1));
        assert_eq!(joins.transcript_index().len(), 2, "steering opens no turn");

        let mut rewound = fresh_root();
        complete_round(&mut rewound, "one", "one");
        rewound
            .evict(&answered(&rewound, &[1]), None, EditAuthority::Model)
            .unwrap();
        rewound.append_user("fresh".into(), Some(1)).unwrap();
        assert_eq!(rewound.current_prompt(), Some(3));

        let mut intervened = fresh_root();
        complete_round(&mut intervened, "one", "one");
        complete_round(&mut intervened, "two", "two");
        intervened.append_user("fresh".into(), Some(1)).unwrap();
        assert_eq!(intervened.current_prompt(), Some(5));

        let mut moved_past = fresh_root();
        complete_round(&mut moved_past, "one", "one");
        complete_round(&mut moved_past, "two", "two");
        moved_past
            .evict(&answered(&moved_past, &[3]), None, EditAuthority::Model)
            .unwrap();
        moved_past.append_user("fresh".into(), Some(1)).unwrap();
        assert_eq!(
            moved_past.current_prompt(),
            Some(5),
            "turn 1 is no longer the live prompt, whatever is still in context"
        );
    }

    /// The three refusals an eviction makes by name, and the empty set.
    #[test]
    fn evict_refuses_the_unclosed_turn_the_unknown_the_departed_and_the_empty_set() {
        let mut live = fresh_root();
        live.append_user("live".into(), None).unwrap();
        assert_eq!(
            live.evict(&[1], None, EditAuthority::Model).unwrap_err(),
            "turn 1 is being written now — an eviction keeps the work in hand"
        );

        let mut unknown = fresh_root();
        complete_round(&mut unknown, "one", "one");
        assert_eq!(
            unknown.evict(&[7], None, EditAuthority::User).unwrap_err(),
            "turn 7 is not recorded — the latest is 2"
        );
        assert_eq!(
            unknown.evict(&[], None, EditAuthority::User).unwrap_err(),
            "an eviction must name at least one turn"
        );

        complete_round(&mut unknown, "two", "two");
        unknown
            .evict(&prefix(&unknown, 2), None, EditAuthority::Harness)
            .unwrap();
        assert_eq!(
            unknown.evict(&[1], None, EditAuthority::Model).unwrap_err(),
            "turn 1 has already left your context — the earliest still in it is 3"
        );
    }

    /// A prompt one of whose answers the set does not name stays with it; a
    /// set the rule would empty is refused, and says which turns to add.
    #[test]
    fn the_survivor_rule_keeps_a_prompt_and_refuses_a_cut_that_takes_nothing() {
        let mut s = fresh_root();
        s.append_user("work on the parser".into(), None).unwrap();
        s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        s.append_tool_results(vec![ToolResult {
            id: "call".into(),
            content: "result".into(),
        }])
        .unwrap();
        s.append_assistant(ChatMessage::assistant("done"), vec![], None)
            .unwrap();
        assert_eq!(
            s.evict(&[1], None, EditAuthority::Model).unwrap_err(),
            "turn 1 is the prompt whose turns 2–3 are still in your context, and a prompt \
             stays with them — name them too, or leave it"
        );
        s.evict(&[1, 2], None, EditAuthority::Model).unwrap();
        assert_eq!(
            s.context_survey()
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![1, 3],
            "the prompt stays with the reply that survived the cut"
        );
    }

    /// One [`TranscriptTurn`] per turn named, in transcript order, each
    /// addressed by its own `turn` field rather than by argument order. A
    /// turn marker carries no model content, so it contributes no message.
    #[test]
    fn the_door_answers_one_element_per_turn() {
        let mut s = fresh_root();
        s.append_user("first prompt".into(), None).unwrap();
        s.record_turn_start(Tuning::default(), None).unwrap();
        s.append_assistant(ChatMessage::assistant("first answer"), vec![], None)
            .unwrap();
        complete_round(&mut s, "second prompt", "second answer");

        let read = s
            .read_transcript(&[1, 2])
            .expect("closed turns are readable");
        let [prompt, reply] = read.as_slice() else {
            panic!("two named turns answer two records, got {read:?}")
        };
        assert_eq!((prompt.turn, prompt.role), (1, Role::User));
        assert_eq!((reply.turn, reply.role), (2, Role::Assistant));
        let [user_msg] = prompt.messages.as_slice() else {
            panic!(
                "a turn marker carries no message, got {:?}",
                prompt.messages
            )
        };
        assert_eq!(user_msg.role, ChatRole::User);
        assert!(
            matches!(user_msg.parts.as_slice(), [TranscriptPart::Text(text)] if text == "first prompt")
        );
        let [assistant_msg] = reply.messages.as_slice() else {
            panic!("one turn, one message, got {:?}", reply.messages)
        };
        assert_eq!(assistant_msg.role, ChatRole::Assistant);
        assert!(
            matches!(assistant_msg.parts.as_slice(), [TranscriptPart::Text(text)] if text == "first answer")
        );

        complete_round(&mut s, "third prompt", "third answer");
        assert_eq!(
            s.suffix_from(9).unwrap_err(),
            "turn 9 is not recorded — the latest is 6"
        );
        assert_eq!(
            s.suffix_from(3).expect("a resident anchor"),
            vec![3, 4, 5, 6],
            "a rewind takes every resident turn from its anchor on"
        );
    }

    /// The marker standing at the head hole — message 0 — or `None` when the
    /// context does not open with a hole.
    fn head_marker(s: &AgentLog) -> Option<String> {
        s.history_rendered()
            .first()
            .and_then(|message| message.content.first_text())
            .filter(|text| text.contains("left your context"))
            .map(str::to_string)
    }

    /// A cut leaves a prompt with a surviving answer standing, and the marker
    /// says which turns went.
    #[test]
    fn a_cut_between_a_prompt_and_its_answer_keeps_the_prompt() {
        let mut s = fresh_root();
        s.append_user("work on the parser".into(), None).unwrap();
        s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        s.append_tool_results(vec![ToolResult {
            id: "call".into(),
            content: "result".into(),
        }])
        .unwrap();
        s.append_assistant(ChatMessage::assistant("done"), vec![], None)
            .unwrap();
        assert_eq!(
            s.transcript_index()
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );

        s.evict(&prefix(&s, 2), None, EditAuthority::Model).unwrap();
        assert_eq!(
            s.context_survey()
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![1, 3],
            "the prompt stays with the reply that survived the cut"
        );
        // The marker stands where the turn did: after the prompt, before the
        // reply that survived it.
        let rendered: Vec<String> = s
            .history_rendered()
            .iter()
            .map(|message| message.content.first_text().unwrap_or_default().to_string())
            .collect();
        let [prompt, marker, done] = rendered.as_slice() else {
            panic!("the prompt, one marker, and the surviving reply, got {rendered:?}")
        };
        assert_eq!(prompt, "work on the parser");
        assert!(
            marker.starts_with("[EXARCH // Turn 2 has left your context."),
            "{marker}"
        );
        assert!(
            marker.contains(
                "`exarch-transcript `read [turns: !{range 2 3}]` reads turn 2 back as material"
            ),
            "{marker}"
        );
        assert_eq!(done, "done");
    }

    #[test]
    fn evict_of_evict_indexes_both_cuts_and_keeps_the_suffix() {
        let mut s = fresh_root();
        for prompt in ["one", "two", "three"] {
            complete_round(&mut s, prompt, prompt);
        }
        s.evict(
            &prefix(&s, 2),
            Some("the parser is fixed".into()),
            EditAuthority::Model,
        )
        .unwrap();
        s.evict(&prefix(&s, 4), None, EditAuthority::Harness)
            .unwrap();

        assert_eq!(s.notes().len(), 2);
        assert_eq!(
            s.context_survey()
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![5, 6]
        );
        let marker = head_marker(&s).expect("two cuts render one marker");
        assert!(
            marker.starts_with("[EXARCH // Turns 1–4 have left your context."),
            "{marker}"
        );
        assert!(marker.contains("Your note: \"the parser is fixed\""));
    }

    /// The prompt cache reads message 0 byte for byte, so the marker must
    /// depend on the table and its cuts and nothing else — not on how many
    /// times it is rendered, and not on whether the fold was built live or
    /// refolded.
    #[test]
    fn head_marker_is_a_pure_function_of_the_table() {
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
            complete_round(&mut live, prompt, prompt);
        }
        live.evict(
            &prefix(&live, 4),
            Some("keep going".into()),
            EditAuthority::Model,
        )
        .unwrap();
        let first = head_marker(&live).expect("a cut renders a marker");
        assert_eq!(head_marker(&live).as_ref(), Some(&first));
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert_eq!(head_marker(&resumed), Some(first));
    }

    #[test]
    fn head_marker_collapses_rows_past_the_cap() {
        let mut s = fresh_root();
        for n in 1..=45u64 {
            let prompt = format!("prompt {n}");
            complete_round(&mut s, &prompt, "answer");
        }
        // 45 rounds hold turns 1..90; a cut through turn 88 takes 88 of them.
        s.evict(&prefix(&s, 88), None, EditAuthority::Harness)
            .unwrap();
        let marker = head_marker(&s).expect("a cut renders a marker");
        assert!(
            marker.contains("1–48  (48 earlier turns — exarch-transcript `index)"),
            "the rows past the cap collapse to one line, got: {marker}"
        );
        assert!(
            !marker.lines().any(|line| line.starts_with("   1  "))
                && marker.lines().any(|line| line.starts_with("  49  ")),
            "the collapsed rows are the oldest, got: {marker}"
        );
        assert_eq!(
            marker.lines().count(),
            1 + 1 + 40,
            "one sentence, one collapse line, and the capped rows"
        );
    }

    /// A note is drawn only alongside a fragment of its own cut, so a cut
    /// collapsed away entirely takes its note with it — message 0 stays
    /// bounded by the row cap rather than growing with how many cuts the
    /// session has run.
    #[test]
    fn a_note_collapses_with_its_rows() {
        let mut s = fresh_root();
        for n in 1..=45u64 {
            let prompt = format!("prompt {n}");
            complete_round(&mut s, &prompt, "answer");
            if n > 1 {
                s.evict(
                    &prefix(&s, 2 * (n - 1)),
                    Some(format!("note {n}")),
                    EditAuthority::Model,
                )
                .unwrap();
            }
        }
        let marker = head_marker(&s).expect("44 cuts render one marker");
        assert!(
            marker.contains("\"note 45\""),
            "the newest note must survive, got: {marker}"
        );
        assert!(
            !marker.contains("\"note 2\""),
            "a note collapsed with its rows must not survive, got: {marker}"
        );
        assert_eq!(
            marker.lines().count(),
            1 + 1 + 40 + 20,
            "bounded by the row cap — 40 rows, and a note per cut that kept one"
        );
    }

    /// A later cut opens a hole of its own, so the provider re-reads from
    /// there and message 0 stays byte for byte what it was. A cut adjacent
    /// to the head hole joins it instead: a hole is a maximal run.
    #[test]
    fn a_late_cut_leaves_the_head_marker_untouched() {
        let mut s = fresh_root();
        for prompt in ["one", "two", "three", "four", "five"] {
            complete_round(&mut s, prompt, prompt);
        }
        s.evict(&prefix(&s, 4), None, EditAuthority::Harness)
            .unwrap();
        let before = head_marker(&s).expect("a cut renders a marker");
        s.evict(&answered(&s, &[7]), None, EditAuthority::Model)
            .unwrap();
        assert_eq!(head_marker(&s), Some(before));
        assert!(
            s.transcript_index()
                .iter()
                .filter(|row| [7, 8].contains(&row.id))
                .all(|row| row.held == Held::Evicted { cut: 1 })
        );
        assert_eq!(
            s.history_rendered().len(),
            6,
            "marker, turns 5 and 6, marker, turns 9 and 10"
        );
    }

    #[test]
    fn evict_refuses_a_turn_already_gone_and_the_unclosed_one() {
        let mut s = fresh_root();
        for prompt in ["one", "two", "three"] {
            complete_round(&mut s, prompt, prompt);
        }
        s.evict(&prefix(&s, 4), None, EditAuthority::Harness)
            .unwrap();
        assert_eq!(
            s.evict(&[2], None, EditAuthority::Model).unwrap_err(),
            "turn 2 has already left your context — the earliest still in it is 5"
        );
        s.append_user("four".into(), None).unwrap();
        assert_eq!(
            s.evict(&[7], None, EditAuthority::Model).unwrap_err(),
            "turn 7 is being written now — an eviction keeps the work in hand"
        );
        assert_eq!(
            s.evict(&[99], None, EditAuthority::Model).unwrap_err(),
            "turn 99 is not recorded — the latest is 7"
        );
    }

    /// What comes back from the log is what the model was sent: the records
    /// hold the clipped strings, and one rendering serves both.
    #[test]
    fn read_transcript_reads_evicted_turns_byte_identically() {
        let mut s = fresh_root();
        complete_round(&mut s, "one", "answer one");
        complete_round(&mut s, "two", "answer two");
        let before = format!("{:?}", s.read_transcript(&[1, 2]).expect("in context"));
        s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
            .unwrap();
        let after = format!(
            "{:?}",
            s.read_transcript(&[1, 2]).expect("evicted but recorded")
        );
        assert_eq!(after, before);
        assert_eq!(
            s.read_transcript(&[9]).unwrap_err(),
            "turn 9 is not recorded — the latest is 4"
        );
    }

    /// A locus measures a file, not a path: bytes that changed under a live
    /// range are refused as the mismatch they are, not as bad JSON.
    #[test]
    fn a_record_that_does_not_hash_to_its_locus_is_refused_by_name() {
        let mut s = fresh_root();
        complete_round(&mut s, "alpha", "alpha answered");
        complete_round(&mut s, "beta", "beta answered");
        s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
            .unwrap();

        // Equal-length substitution: every locus's byte range still names a
        // whole, well-formed record — just not the one it measured.
        let path = record_path(&s);
        let recorded = fs::read_to_string(&path).expect("record.jsonl is utf-8");
        let rewritten = recorded.replace("alpha", "gamma");
        assert_ne!(recorded, rewritten, "the substitution must reach the log");
        assert_eq!(
            recorded.len(),
            rewritten.len(),
            "the loci's byte ranges must still hold whole records"
        );
        fs::write(&path, &rewritten).expect("rewrite record.jsonl in place");

        let refusal = s
            .read_transcript(&[1, 2])
            .expect_err("a record that no longer hashes to its locus is unreadable");
        assert!(
            refusal.contains("did not hash to the locus that named it"),
            "the refusal must name the digest mismatch, got: {refusal}"
        );
    }

    /// The index is every turn the transcript holds, whatever took it out of
    /// the context — and the turn in flight is listed like any other.
    #[test]
    fn transcript_index_lists_every_turn_with_how_it_is_held() {
        let mut s = fresh_root();
        for prompt in ["one", "two", "three"] {
            complete_round(&mut s, prompt, prompt);
        }
        s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
            .unwrap();
        s.evict(&answered(&s, &[3]), None, EditAuthority::Model)
            .unwrap();
        s.append_user("live".into(), None).unwrap();

        let index = s.transcript_index();
        assert_eq!(
            index
                .iter()
                .map(|row| (row.id, row.role, row.held))
                .collect::<Vec<_>>(),
            vec![
                (1, Role::User, Held::Evicted { cut: 0 }),
                (2, Role::Assistant, Held::Evicted { cut: 0 }),
                (3, Role::User, Held::Evicted { cut: 1 }),
                (4, Role::Assistant, Held::Evicted { cut: 1 }),
                (5, Role::User, Held::Resident),
                (6, Role::Assistant, Held::Resident),
                (7, Role::User, Held::Resident),
            ]
        );
        assert_eq!(index[0].label, "one");
        assert!(
            index
                .iter()
                .all(|row| row.bytes > 0 && row.kind == TurnKind::Own)
        );
    }

    /// The one turn no door reads back is the one being written; the closed
    /// turns beside it read back like any other.
    #[test]
    fn the_door_refuses_the_turn_being_written() {
        let mut s = fresh_root();
        s.append_user("work".into(), None).unwrap();
        s.append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        s.append_tool_results(vec![ToolResult {
            id: "call".into(),
            content: "result".into(),
        }])
        .unwrap();
        assert_eq!(
            s.read_transcript(&[1, 2]).unwrap_err(),
            "turn 2 is being written now — it is the one turn the transcript cannot read back yet"
        );
        assert_eq!(
            s.read_transcript(&[1])
                .expect("the closed prompt reads back")
                .len(),
            1
        );
        assert_eq!(
            s.read_transcript(&[]).unwrap_err(),
            "`turns` names no turn — `!{range a b}` builds a run of ids"
        );
    }

    /// One search over both sides of the ledger: the freed records read back
    /// off `record.jsonl`, the resident ones rendered in place, and the hits
    /// in transcript order across the seam between them.
    #[test]
    fn grep_walks_resident_then_freed() {
        let mut s = fresh_root();
        complete_round(&mut s, "the parser is broken", "I will fix the parser");
        complete_round(&mut s, "the lexer is fine", "nothing to do");
        s.evict(&prefix(&s, 2), None, EditAuthority::Harness)
            .unwrap();

        let answer = s
            .grep_transcript(&regex(r"\bthe\b"), None)
            .expect("the whole transcript is searchable");
        assert_eq!(answer.total, 3);
        assert_eq!(
            answer
                .hits
                .iter()
                .map(|hit| (hit.turn, hit.role.clone(), hit.line))
                .collect::<Vec<_>>(),
            vec![
                (1, ChatRole::User, 1),
                (2, ChatRole::Assistant, 1),
                (3, ChatRole::User, 1),
            ]
        );
        assert_eq!(answer.hits[0].text, "the parser is broken");

        let narrowed = s
            .grep_transcript(&regex(r"\bthe\b"), Some(&[1, 4]))
            .expect("a list with a gap searches exactly what it names");
        assert_eq!(narrowed.total, 1, "turns 2 and 3 are not searched");
        assert_eq!(narrowed.hits[0].turn, 1);
        assert_eq!(
            s.grep_transcript(&regex("parser"), Some(&[9])).unwrap_err(),
            "turn 9 is not recorded — the latest is 4"
        );
        assert_eq!(
            s.grep_transcript(&regex("parser"), Some(&[])).unwrap_err(),
            "`turns` names no turn — omit it to search the whole transcript"
        );
    }

    /// A pattern that matches everything answers one page, not the whole
    /// transcript: `total` is what tells the model to narrow.
    #[test]
    fn grep_caps_hits_and_reports_total() {
        let mut s = fresh_root();
        let many = (1..=150)
            .map(|n| format!("line {n} matches"))
            .collect::<Vec<_>>()
            .join("\n");
        complete_round(&mut s, "count", &many);

        let answer = s
            .grep_transcript(&regex("matches"), None)
            .expect("the whole transcript is searchable");
        assert_eq!(answer.total, 150);
        assert_eq!(answer.hits.len(), 100);
        assert_eq!(answer.hits[0].line, 1);
        assert_eq!(
            answer.hits[99].line, 100,
            "the oldest hundred, in transcript order"
        );
    }

    #[test]
    fn eviction_plan_walks_back_from_the_turn_in_hand() {
        let mut s = fresh_root();
        complete_round(&mut s, "one", "one");
        complete_round(&mut s, "two", "two");
        complete_round(&mut s, "three", "three");
        let rows = s.context_survey().rows;
        let keep = rows[4].bytes + rows[5].bytes;
        let plan = s.plan_eviction(keep).expect("old turns to shed");
        assert_eq!(plan, vec![1, 2, 3, 4]);
        s.evict(&plan, None, EditAuthority::Harness).unwrap();
        assert_eq!(
            s.context_survey()
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![5, 6],
            "the planned turns left the context"
        );
        assert!(
            !s.history_rendered()
                .iter()
                .any(|message| { message.content.first_text() == Some("one") })
        );
    }

    /// A turn in hand heavy enough to fill the whole keep budget on its own
    /// must never be the one a plan names: that would leave the model with
    /// nothing, and the set would be one `resolve_cut` refuses.
    #[test]
    fn a_plan_never_takes_the_newest_turn() {
        let mut s = fresh_root();
        complete_round(&mut s, "one", "one");
        let big = "x".repeat(100_000);
        complete_round(&mut s, "two", &big);
        let keep = suffix_keep_budget(s.history_bytes());
        let plan = s.plan_eviction(keep).expect("the older turns to shed");
        assert_eq!(plan, vec![1, 2]);
        s.evict(&plan, None, EditAuthority::Harness).unwrap();
        assert_eq!(
            s.context_survey()
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![3, 4],
            "the work in hand must survive the eviction it could not itself be named by"
        );
    }

    #[test]
    fn a_lone_prompt_is_never_planned_away() {
        let mut s = fresh_root();
        complete_round(&mut s, "one", "one");
        assert!(s.plan_eviction(0).is_none());
    }

    #[test]
    fn a_user_ending_import_turn_is_never_torn() {
        let mut s = fresh_root();
        s.import_note(ChatMessage::user("imported user")).unwrap();
        complete_round(&mut s, "normal", "answer");

        let rendered = s.history_rendered();
        assert_eq!(
            rendered
                .iter()
                .filter_map(|message| message.content.first_text())
                .collect::<Vec<_>>(),
            vec!["imported user", "normal", "answer"]
        );
    }

    /// Work abandoned before any reply is closed by the next prompt,
    /// not by a fabricated one: nothing is recorded in its place, and the
    /// model reads it exactly as it lies, the next prompt following.
    #[test]
    fn abandoned_work_is_kept_whole_and_read_whole() {
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
        complete_round(&mut s, "next", "answer");
        let rendered = s.history_rendered();
        let read: Vec<&str> = rendered
            .iter()
            .filter_map(|message| message.content.first_text())
            .collect();
        assert_eq!(read, vec!["interrupted", "next", "answer"]);
    }

    /// The work an interrupted turn did stays in the context: an agentic
    /// run is one prompt and then many tool turns, and an Esc that dropped
    /// them all would leave the model to rediscover its own session.
    #[test]
    fn abandoned_work_keeps_its_tool_turns_in_context() {
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
        complete_round(&mut s, "next", "answer");
        assert_eq!(
            s.history_rendered()
                .iter()
                .map(|message| message.role.clone())
                .collect::<Vec<_>>(),
            vec![
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::Tool,
                ChatRole::User,
                ChatRole::Assistant
            ]
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

    /// A `reply` ends the work it was asked for, so its capstone is earned —
    /// and without one the fold could not tell it from an interruption, and
    /// would read the whole of that work as its note.
    #[test]
    fn a_reply_keeps_its_capstone_and_stays_visible() {
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
        complete_round(&mut s, "follow-up", "answer");
        assert!(
            s.history_rendered().iter().any(|message| {
                message.content.first_text()
                    == Some("[EXARCH // Deliberation ended: replied to parent.]")
            }),
            "a replied turn stays whole in the child's own context"
        );
    }

    /// The recorded set is the resolved set: what the writer worked out is
    /// what the file carries, so replay departs exactly these ids.
    #[test]
    fn record_jsonl_round_trip_carries_the_resolved_cut() {
        let mut s = fresh_root();
        complete_round(&mut s, "one", "one");
        complete_round(&mut s, "two", "two");
        s.evict(
            &[1, 2, 3],
            Some("the parser is fixed".into()),
            EditAuthority::User,
        )
        .unwrap();

        assert!(
            records(&s).iter().any(|record| {
                matches!(
                    record,
                    Record::Protocol(Protocol::Evicted {
                        cut,
                        by: EditAuthority::User,
                    }) if cut.turns == [1, 2] && cut.note.as_deref() == Some("the parser is fixed")
                )
            }),
            "turn 3 is the prompt turn 4 still answers, and never reaches the file"
        );
    }

    /// A record naming a turn no live session could have evicted is the
    /// hand-edited file it is.
    #[test]
    fn resume_refuses_an_eviction_of_a_turn_not_in_the_context() {
        let sessions = sessions_root("resume-foreign-eviction");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_round(&mut live, "one", "one");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let foreign = Record::Protocol(Protocol::Evicted {
            cut: Cut {
                turns: vec![9],
                note: None,
            },
            by: EditAuthority::Model,
        });
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&crate::record::envelope_line(&foreign))
            .unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("an eviction of a turn not in the context is foreign data");
        let text = error.to_string();
        assert!(
            text.contains("evicts turn 9, which is not in the context"),
            "{text}"
        );
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
                Record::Forensic(Forensic::SessionStarted { model, label, .. }) => {
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
    fn clear_rotates_record_jsonl_and_resets_the_table() {
        let mut s = fresh_root();
        complete_round(&mut s, "one", "one");
        complete_round(&mut s, "two", "two");
        s.evict(&answered(&s, &[3]), None, EditAuthority::User)
            .unwrap();

        let record = s.dir().join("record.jsonl");
        s.clear(0, 2).expect("clear");
        assert!(record.with_extension("jsonl.0").exists());
        assert!(s.transcript_index().is_empty());
        assert!(s.is_ready());
    }

    #[test]
    fn a_departed_turn_keeps_no_resident_state() {
        let mut s = fresh_root();
        complete_round(&mut s, "one", "one");
        s.evict(&answered(&s, &[1]), None, EditAuthority::User)
            .unwrap();
        assert_eq!(
            s.event_count(),
            1,
            "no turn's records are owned any more; the hole's marker is what stands"
        );
    }

    #[test]
    fn resume_replays_a_scripted_history_and_preserves_the_context() {
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
        complete_round(&mut live, "one", "answer one");
        complete_round(&mut live, "two", "answer two");
        live.evict(&prefix(&live, 3), None, EditAuthority::Harness)
            .unwrap();
        let expected =
            serde_json::to_vec(&live.history_rendered().iter().collect::<Vec<_>>()).unwrap();
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert!(resumed.is_ready());
        assert_eq!(
            serde_json::to_vec(&resumed.history_rendered().iter().collect::<Vec<_>>()).unwrap(),
            expected
        );
    }

    #[test]
    fn resume_quiesces_a_torn_turn_after_reopening_append_mode() {
        let sessions = sessions_root("resume-mid-turn");
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
            turn: 1,
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
    fn resume_refuses_a_steering_line_with_no_turn_to_steer() {
        let sessions = sessions_root("resume-steering-no-turn");
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
        let orphan = Record::Protocol(Protocol::Steering {
            text: "steer".into(),
        });
        let mut file = fs::OpenOptions::new().append(true).open(&path).unwrap();
        file.write_all(&crate::record::envelope_line(&orphan))
            .unwrap();
        file.write_all(b"\n").unwrap();
        file.flush().unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("a steering line with no turn is foreign data");
        let text = error.to_string();
        assert!(
            text.contains("steers a turn, but no turn has been recorded"),
            "{text}"
        );
    }

    #[test]
    fn resume_refuses_a_stale_id_as_foreign_data() {
        let sessions = sessions_root("resume-stale-id");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_round(&mut live, "one", "one");
        complete_round(&mut live, "two", "two");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let stale = Record::Protocol(Protocol::UserPrompt {
            turn: 1,
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
            .expect("a stale id is foreign data");
        let text = error.to_string();
        assert!(text.contains("names turn 1"), "{text}");
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

    /// A live lineage quiesces its unready tail before it records anything
    /// else, so an interior gap is never something it wrote: the fold refuses
    /// it as the hand-edited file it is, rather than stubbing it shut.
    #[test]
    fn resume_refuses_an_interior_tool_answer_lost_from_disk() {
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
        complete_round(&mut live, "second", "answer");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);

        let data = fs::read(&path).unwrap();
        let mut torn = Vec::with_capacity(data.len());
        for fragment in data.split_inclusive(|byte| *byte == b'\n') {
            let record = crate::record::record_from_line(&fragment[..fragment.len() - 1]).unwrap();
            if matches!(record, Record::Protocol(Protocol::ToolResults { .. })) {
                continue;
            }
            torn.extend_from_slice(fragment);
        }
        fs::write(&path, &torn).unwrap();

        let error = AgentLog::resume(sessions.path(), 0)
            .err()
            .expect("an interior gap is a hand-edited file");
        let text = error.to_string();
        assert!(text.contains("foreign protocol data"), "{text}");
        assert!(
            text.contains("not a seam that quiesce can repair"),
            "{text}"
        );
        assert_eq!(fs::read(&path).unwrap(), torn);
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
            complete_round(&mut live, "one", "one");
            complete_round(&mut live, "two", "two");
            complete_round(&mut live, "three", "three");
            match pattern {
                0 => {}
                1 => {
                    live.evict(&answered(&live, &[3]), None, EditAuthority::User)
                        .unwrap();
                }
                2 => {
                    live.evict(&prefix(&live, 4), None, EditAuthority::Harness)
                        .unwrap();
                }
                3 => {
                    live.evict(&answered(&live, &[1]), None, EditAuthority::Model)
                        .unwrap();
                    live.evict(&answered(&live, &[3]), None, EditAuthority::User)
                        .unwrap();
                }
                4 => {
                    live.evict(
                        &prefix(&live, 4),
                        Some("halfway".into()),
                        EditAuthority::Harness,
                    )
                    .unwrap();
                    live.evict(&answered(&live, &[5]), None, EditAuthority::Model)
                        .unwrap();
                }
                5 => {
                    live.evict(&answered(&live, &[3]), None, EditAuthority::User)
                        .unwrap();
                    complete_round(&mut live, "four", "four");
                }
                6 => {
                    live.evict(&prefix(&live, 2), None, EditAuthority::Harness)
                        .unwrap();
                    live.evict(
                        &prefix(&live, 4),
                        Some("second cut".into()),
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
                serde_json::to_vec(&live.history_rendered().iter().collect::<Vec<_>>()).unwrap();
            drop(live);
            let resumed = AgentLog::resume(sessions.path(), 0).expect("resume edit sequence");
            assert!(resumed.is_ready());
            assert_eq!(
                serde_json::to_vec(&resumed.history_rendered().iter().collect::<Vec<_>>()).unwrap(),
                expected,
                "pattern {pattern}"
            );
        }
    }

    /// The structure is a fold of the log, so replaying the file rebuilds
    /// every row and every note the live session held — weights included, a
    /// misplaced departed record showing up as a marker that had drifted.
    #[test]
    fn resume_rebuilds_the_same_rows_and_notes() {
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
            complete_round(&mut live, prompt, prompt);
        }
        live.evict(
            &prefix(&live, 5),
            Some("the abandoned work is indexed too".into()),
            EditAuthority::Model,
        )
        .unwrap();
        let rows = live.transcript_index();
        let notes = live.notes().to_vec();
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert_eq!(resumed.transcript_index(), rows);
        assert_eq!(resumed.notes(), notes.as_slice());
    }

    /// A departed turn's address is not written down anywhere: the fold mints
    /// it from the loci the records arrived with, so a resumed session reads
    /// the same bytes back with nothing carried over but the file.
    #[test]
    fn a_departed_turn_reads_back_after_resume() {
        let sessions = sessions_root("resume-departed-read");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_round(&mut live, "one", "answer one");
        complete_round(&mut live, "two", "answer two");
        live.evict(&prefix(&live, 2), None, EditAuthority::Harness)
            .unwrap();
        let before = format!("{:?}", live.read_transcript(&[1, 2]).expect("evicted"));
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert_eq!(
            resumed
                .context_survey()
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![3, 4],
            "the cut crossed the resume"
        );
        let after = format!(
            "{:?}",
            resumed
                .read_transcript(&[1, 2])
                .expect("the pointer was rebuilt by the fold alone")
        );
        assert_eq!(after, before);
    }

    /// A `mnemon` child of `parent`: its own log under the same sessions
    /// root, opened with the table and the link the parent hands over.
    fn mnemon(parent: &AgentLog, child_id: AgentId) -> AgentLog {
        let inherited = parent.inherited_context();
        let mut child = parent
            .fork(child_id, 0, "model", &RecordedAccount::for_test("provider"))
            .expect("child log");
        child.import_context(inherited).expect("import");
        child
    }

    fn seed_has_a_tool_call(log: &AgentLog) -> bool {
        log.history_rendered().iter().any(|message| {
            message
                .content
                .iter()
                .any(|part| matches!(part, ContentPart::ToolCall(_)))
        })
    }

    /// An `Evicted` record with no id of its own glues onto whatever turn is
    /// still open, so an eviction run mid-batch lands right after the
    /// dangling assistant frame rather than replacing it. The seed must still
    /// cut at the tool call, not at the eviction that followed it.
    #[test]
    fn a_seed_cut_never_owes_a_tool_result() {
        let mut parent = fresh_root();
        complete_round(&mut parent, "one", "one");
        parent.append_user("two".into(), None).unwrap();
        parent
            .append_assistant(assistant_with_tool("call-1"), vec!["call-1".into()], None)
            .unwrap();
        parent
            .evict(&answered(&parent, &[1]), None, EditAuthority::Model)
            .unwrap();

        let child = mnemon(&parent, 1);
        assert!(
            !seed_has_a_tool_call(&child),
            "a mnemon seed must never carry an unanswered tool call"
        );
        assert!(
            child
                .history_rendered()
                .iter()
                .any(|message| message.content.first_text() == Some("two")),
            "the prompt behind the dangling call must still seed"
        );
    }

    /// The plain in-batch fork: no edit lands after the dangling assistant
    /// frame, so the frame itself is the turn's last record, and the cut
    /// must still land in front of it.
    #[test]
    fn a_seed_cut_never_owes_a_tool_result_with_no_edit() {
        let mut parent = fresh_root();
        complete_round(&mut parent, "one", "one");
        parent.append_user("two".into(), None).unwrap();
        parent
            .append_assistant(assistant_with_tool("call-1"), vec!["call-1".into()], None)
            .unwrap();

        let child = mnemon(&parent, 1);
        assert!(
            !seed_has_a_tool_call(&child),
            "the plain in-batch fork must not regress"
        );
        assert!(
            child
                .history_rendered()
                .iter()
                .any(|message| message.content.first_text() == Some("two")),
        );
    }

    /// A turn's transcript copy is where it was first recorded. The seed cuts
    /// in front of the dangling tool call, so the child's own copy of that
    /// turn is short — and the door still answers the call, off the parent's
    /// file, before and after the child evicts the turn.
    #[test]
    fn a_seeded_turn_reads_back_from_its_origin() {
        let sessions = sessions_root("lineage-origin");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_round(&mut parent, "one", "one");
        parent.append_user("two".into(), None).unwrap();
        parent
            .append_assistant(assistant_with_tool("call-1"), vec!["call-1".into()], None)
            .unwrap();

        let mut child = mnemon(&parent, 1);
        assert!(
            !seed_has_a_tool_call(&child),
            "the child's own copy of the turn stops in front of the call"
        );
        let read = child
            .read_transcript(&[3, 4])
            .expect("the turn is recorded in the parent's file");
        let called = |read: &[TranscriptTurn]| {
            read.iter()
                .flat_map(|turn| &turn.messages)
                .flat_map(|message| &message.parts)
                .any(|part| matches!(part, TranscriptPart::Program { source, .. } if source == "pwd"))
        };
        assert!(
            called(&read),
            "the origin answers the whole turn, not the seed's short copy"
        );

        complete_round(&mut child, "the child's own", "answer");
        child
            .evict(&prefix(&child, 4), None, EditAuthority::Harness)
            .unwrap();
        let departed = child
            .read_transcript(&[3, 4])
            .expect("eviction points at the copy, never at the seed");
        assert_eq!(format!("{departed:?}"), format!("{read:?}"));
        assert!(called(&departed));
    }

    fn openings(read: &[TranscriptTurn]) -> Vec<&str> {
        read.iter()
            .flat_map(|turn| &turn.messages)
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
    fn a_mnemon_child_reads_an_ancestors_evicted_turns_and_mints_ids_above_it() {
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
            complete_round(&mut parent, prompt, prompt);
        }
        parent
            .evict(&prefix(&parent, 6), None, EditAuthority::Harness)
            .unwrap();

        let mut child = mnemon(&parent, 1);
        assert_eq!(
            child
                .context_survey()
                .rows
                .iter()
                .map(|row| row.id)
                .collect::<Vec<_>>(),
            vec![7, 8],
            "the parent's surviving turns cross under the parent's own ids"
        );
        complete_round(&mut child, "the child's own", "answer");
        assert_eq!(child.current_prompt(), Some(9));

        let read = child
            .read_transcript(&[3, 4])
            .expect("an ancestor's evicted turns are in the lineage's transcript");
        assert_eq!(
            read.iter().map(|turn| turn.turn).collect::<Vec<_>>(),
            vec![3, 4]
        );
        assert_eq!(
            read.iter().map(|turn| turn.role).collect::<Vec<_>>(),
            vec![Role::User, Role::Assistant]
        );
        assert_eq!(openings(&read), vec!["two", "two"]);

        let mut grandchild = mnemon(&child, 2);
        let read = grandchild
            .read_transcript(&[3, 4])
            .expect("two links back is still the same transcript");
        assert_eq!(openings(&read), vec!["two", "two"]);
        complete_round(&mut grandchild, "the grandchild's own", "answer");
        assert_eq!(grandchild.current_prompt(), Some(11));
    }

    /// A cut between a prompt and its answers, then a fork, leaves them in
    /// two files: the evicted turn in the ancestor's, the survivors
    /// re-recorded as the child's own. Each turn reads back from wherever its
    /// placement says, so the read answers them all rather than the local half
    /// of them.
    #[test]
    fn turns_split_across_a_fork_read_back_out_of_both_files() {
        let sessions = sessions_root("lineage-split");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        parent
            .append_user("work on the parser".into(), None)
            .unwrap();
        parent
            .append_assistant(assistant_with_tool("call"), vec!["call".into()], None)
            .unwrap();
        parent
            .append_tool_results(vec![ToolResult {
                id: "call".into(),
                content: "result".into(),
            }])
            .unwrap();
        parent
            .append_assistant(ChatMessage::assistant("done"), vec![], None)
            .unwrap();
        parent
            .evict(&prefix(&parent, 2), None, EditAuthority::Harness)
            .unwrap();

        let child = mnemon(&parent, 1);
        let read = child
            .read_transcript(&[1, 2, 3])
            .expect("both halves are in the lineage's transcript");
        assert_eq!(
            read.iter().map(|turn| turn.turn).collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        assert_eq!(
            read.iter()
                .flat_map(|turn| &turn.messages)
                .map(|message| message.role.clone())
                .collect::<Vec<_>>(),
            vec![
                ChatRole::User,
                ChatRole::Assistant,
                ChatRole::Tool,
                ChatRole::Assistant
            ],
            "the evicted turn's records come off the ancestor's file, the survivors off the child's"
        );
    }

    /// A `/rewind` at a ready boundary can leave a parent with no turn in
    /// context at all; the ids it minted are still spent, and the child says
    /// so.
    #[test]
    fn an_empty_parent_context_still_floors_the_childs_first_id() {
        let sessions = sessions_root("lineage-floor");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_round(&mut parent, "one", "one");
        complete_round(&mut parent, "two", "two");
        parent
            .evict(&answered(&parent, &[1, 3]), None, EditAuthority::User)
            .unwrap();
        assert_eq!(parent.context_survey().rows.len(), 0);

        let mut child = mnemon(&parent, 1);
        assert_eq!(child.context_survey().rows.len(), 0);
        child.append_user("first".into(), None).unwrap();
        assert_eq!(child.current_prompt(), Some(5));
    }

    /// Only a fork's opening may link a log to an ancestry: a link found
    /// after the log has a context of its own is foreign data.
    #[test]
    fn admission_refuses_inherited_after_the_first_turn() {
        let sessions = sessions_root("lineage-late-link");
        let mut live = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_round(&mut live, "one", "one");
        let path = sessions.path().join("0/record.jsonl");
        drop(live);
        let late = Record::Protocol(Protocol::Inherited {
            turns: Vec::new(),
            notes: Vec::new(),
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

    /// A pointer whose file will not answer refuses the turn behind it by
    /// path and by the fault the read actually met — a line that will not
    /// read back, or a file that will not open — and never by calling an
    /// turn the lineage recorded unrecorded. Nothing remembers the
    /// break, so the second read tries the file again.
    #[test]
    fn a_broken_ancestry_link_is_refused_by_path_and_fault() {
        let sessions = sessions_root("lineage-broken");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        for prompt in ["one", "two"] {
            complete_round(&mut parent, prompt, prompt);
        }
        parent
            .evict(&prefix(&parent, 2), None, EditAuthority::Harness)
            .unwrap();
        let source = record_path(&parent);
        let child = mnemon(&parent, 1);
        drop(parent);

        let recorded = fs::read_to_string(&source).expect("record.jsonl is utf-8");
        let mut torn = String::new();
        for (line, text) in recorded.lines().enumerate() {
            if line == 1 {
                torn.push_str("{\"not\":\"an envelope\"}\n");
            }
            torn.push_str(text);
            torn.push('\n');
        }
        fs::write(&source, &torn).expect("rewrite the ancestor's log");
        let refusal = child
            .read_transcript(&[1, 2])
            .expect_err("a line the read cannot follow stops the lineage");
        assert!(
            refusal.starts_with(&format!(
                "turn 1 could not be read back from {}: ",
                source.display()
            )) && refusal.contains("did not hash to the locus that named it"),
            "{refusal}"
        );

        fs::remove_file(&source).expect("delete the ancestor's log");
        let refusal = child
            .read_transcript(&[1, 2])
            .expect_err("a deleted ancestor's log stops the lineage");
        assert!(
            refusal.starts_with(&format!(
                "turn 1 could not be read back from {}: ",
                source.display()
            )),
            "{refusal}"
        );
    }

    /// The link is folded, not just recorded: replaying it rebuilds the
    /// parent's rows and its notes, so a resume covers an inherited context
    /// as it covers an evicted one — and the marker at the child's head hole,
    /// being that same projection, carries the parent's own note.
    #[test]
    fn resume_rebuilds_the_same_rows_and_notes_across_a_link() {
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
            complete_round(&mut ancestor, prompt, prompt);
        }
        ancestor
            .evict(
                &prefix(&ancestor, 4),
                Some("the parser is fixed".into()),
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
        assert_eq!(
            live.notes(),
            [Some("the parser is fixed".to_string())],
            "the parent's cut crosses the link"
        );
        let head = live
            .history_rendered()
            .first()
            .expect("an inherited context that was cut renders a head marker")
            .content
            .first_text()
            .unwrap_or_default()
            .to_string();
        assert!(
            head.starts_with("[EXARCH //") && head.contains("the parser is fixed"),
            "the child's marker is the same projection of the same fold: {head}"
        );

        complete_round(&mut live, "own", "own");
        live.evict(&prefix(&live, 6), None, EditAuthority::Harness)
            .unwrap();
        let rows = live.transcript_index();
        let notes = live.notes().to_vec();
        drop(live);

        let resumed = AgentLog::resume(sessions.path(), 0).expect("resume");
        assert_eq!(resumed.transcript_index(), rows);
        assert_eq!(resumed.notes(), notes.as_slice());
    }

    /// The index is the whole lineage's table, in id order: a child inherits
    /// every turn its ancestry recorded, `held` as the parent had it, and its
    /// own turns land above them.
    #[test]
    fn transcript_index_carries_the_inherited_table() {
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
            complete_round(&mut parent, prompt, prompt);
        }
        parent
            .evict(&prefix(&parent, 4), None, EditAuthority::Harness)
            .unwrap();

        let mut child = mnemon(&parent, 1);
        complete_round(&mut child, "four", "four");

        let index = child.transcript_index();
        assert_eq!(
            index
                .iter()
                .map(|row| (row.id, row.kind, row.held))
                .collect::<Vec<_>>(),
            vec![
                (1, TurnKind::Inherited, Held::Evicted { cut: 0 }),
                (2, TurnKind::Inherited, Held::Evicted { cut: 0 }),
                (3, TurnKind::Inherited, Held::Evicted { cut: 0 }),
                (4, TurnKind::Inherited, Held::Evicted { cut: 0 }),
                (5, TurnKind::Inherited, Held::Resident),
                (6, TurnKind::Inherited, Held::Resident),
                (7, TurnKind::Own, Held::Resident),
                (8, TurnKind::Own, Held::Resident),
            ]
        );
        assert_eq!(index[0].label, "one");
    }

    /// One search over the whole lineage: the child's resident turns, its own
    /// freed records, and then the ancestor's file — in transcript order
    /// across all three, and never the same turn twice.
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
        complete_round(&mut parent, "the parser is broken", "I will fix the parser");
        complete_round(&mut parent, "the lexer is fine", "nothing to do");
        parent
            .evict(&prefix(&parent, 2), None, EditAuthority::Harness)
            .unwrap();

        let mut child = mnemon(&parent, 1);
        complete_round(&mut child, "the tests pass", "good");
        child
            .evict(&prefix(&child, 4), None, EditAuthority::Harness)
            .unwrap();

        let answer = child
            .grep_transcript(&regex(r"\bthe\b"), None)
            .expect("the lineage's whole transcript is searchable");
        assert_eq!(answer.total, 4);
        assert_eq!(
            answer
                .hits
                .iter()
                .map(|hit| (hit.turn, hit.role.clone()))
                .collect::<Vec<_>>(),
            vec![
                (1, ChatRole::User),
                (2, ChatRole::Assistant),
                (3, ChatRole::User),
                (5, ChatRole::User),
            ]
        );
        assert_eq!(answer.hits[0].text, "the parser is broken");
    }

    /// A search of the whole transcript answers what it can reach and passes
    /// over what it cannot — the marker at its hole has already told the model
    /// which turns it cannot have back. A narrowing that names one of them is
    /// refused instead, by the path that would not answer.
    #[test]
    fn an_unnarrowed_grep_passes_over_an_unreadable_file() {
        let sessions = sessions_root("lineage-unreadable");
        let mut parent = AgentLog::root(
            sessions.path(),
            0,
            "model",
            &RecordedAccount::for_test("provider"),
            0,
        )
        .unwrap();
        complete_round(&mut parent, "the parser is broken", "I will fix the parser");
        let source = record_path(&parent);
        let mut child = mnemon(&parent, 1);
        drop(parent);
        complete_round(&mut child, "the tests pass", "good");
        fs::remove_file(&source).expect("delete the ancestor's log");

        let answer = child
            .grep_transcript(&regex(r"\bthe\b"), None)
            .expect("an unnarrowed search answers what it can reach");
        assert_eq!(answer.total, 1);
        assert_eq!(answer.hits[0].text, "the tests pass");

        let refusal = child
            .read_transcript(&[1, 2])
            .expect_err("a read that names an unreadable turn is refused");
        assert!(
            refusal.starts_with(&format!(
                "turn 1 could not be read back from {}: ",
                source.display()
            )),
            "{refusal}"
        );
    }
}
