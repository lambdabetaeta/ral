//! `AgentLog`: one session's handle onto `sessions/<n>/record.jsonl`.
//!
//! It sits over [`Context`], the model fold's authoritative in-memory
//! structure.  Mutations are this log's — each appends the record and
//! advances the fold on what was witnessed; queries are the [`Context`]'s
//! own, reached through [`AgentLog::context`].
//!
//! `tui::scrollback` folds the same log's `Display`/`Forensic` classes into the
//! rendered `user.log`.

use super::model::{Context, Linked, validate_result_ids};
use super::{
    Cut, Display, EditAuthority, Fold as _, Forensic, Protocol, QuiesceReason, Record, Recorded,
    RecordedAccount, RecordedModel, Role, Spent, ToolResult, widen,
};
use crate::provider::{ProviderError, Tuning, Usage};
use crate::record::AgentId;
use genai::chat::{ChatMessage, ChatRole};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

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

/// What a resumed session picked up: its newest turn and the record's size.
#[derive(Clone, Copy, Debug)]
pub struct Resumed {
    pub turn: u64,
    pub bytes: u64,
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
    model: RecordedModel,
    account: RecordedAccount,
    /// `<scratch>/sessions/`, under which `fork` makes each child's dir.
    sessions_root: PathBuf,
    /// The directory a test's log has to itself, dropped with the log.
    #[cfg(test)]
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
        model: &RecordedModel,
        account: &RecordedAccount,
        system_prompt_bytes: usize,
    ) -> io::Result<Self> {
        let mut s = Self::open_fresh(
            sessions_root.to_path_buf(),
            session_id,
            model.clone(),
            account.clone(),
        )?;
        s.record_started(None, system_prompt_bytes, crate::app::now_unix_ms())?;
        Ok(s)
    }

    /// A root log in a directory of its own, deleted when the log is.
    ///
    /// A forked child lives inside that directory without owning it, so a test
    /// keeping a child must keep its root alive too.
    ///
    /// # Errors
    /// Returns `Err` if the directory or the session log cannot be created.
    #[cfg(test)]
    pub(crate) fn for_test(
        session_id: AgentId,
        model: &str,
        account: &RecordedAccount,
    ) -> io::Result<Self> {
        let scratch = tempfile::Builder::new()
            .prefix("exarch-agent-log-test-")
            .tempdir()?;
        let model = RecordedModel::for_test(model);
        let log = Self::root(scratch.path(), session_id, &model, account, 0)?;
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
        model: &RecordedModel,
        account: &RecordedAccount,
    ) -> io::Result<Self> {
        let mut s = Self::open_fresh(
            self.sessions_root.clone(),
            child_id,
            model.clone(),
            account.clone(),
        )?;
        s.record_started(
            Some(self.id),
            system_prompt_bytes,
            crate::app::now_unix_ms(),
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
        if session_id != AgentId::new(0) {
            return Err(io::Error::other(format!(
                "cannot resume session {session_id}: children are transient by design and only session 0 can be resumed"
            )));
        }
        let dir = Self::dir_of(sessions_root, session_id);
        let record_path = dir.join("record.jsonl");
        if !record_path.exists() {
            return Err(io::Error::other(format!(
                "cannot resume {}: no record.jsonl was found here; a session recorded before this exarch's one-seam-one-log change cannot be resumed; was this session started with an older exarch, or is {} the right directory to resume?",
                record_path.display(),
                dir.display()
            )));
        }
        let (context, name, label) =
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
            model: RecordedModel {
                name,
                context_window: None,
            },
            account,
            sessions_root: sessions_root.to_path_buf(),
            #[cfg(test)]
            _scratch: None,
            context,
            seam,
        };
        if !resumed.context.is_ready() {
            resumed.quiesce(QuiesceReason::Aborted);
        }
        Ok(resumed)
    }

    pub fn resumed_summary(&self) -> Resumed {
        let bytes =
            fs::metadata(self.dir.join("record.jsonl")).map_or(0, |metadata| metadata.len());
        Resumed {
            turn: self.context.current_turn().unwrap_or(0),
            bytes,
        }
    }

    /// Record the live model selection and the shared resume boundary stamp.
    ///
    /// # Errors
    /// Returns an error if the resumed breadcrumb cannot be appended.
    pub fn record_resumed(
        &mut self,
        model: &RecordedModel,
        account: &RecordedAccount,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    ) -> io::Result<()> {
        self.model = model.clone();
        self.account = account.clone();
        self.record_forensic(Forensic::SessionResumed {
            model: self.model.name.clone(),
            context_window: self.model.context_window,
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

    /// The model fold this log advances: every query about the context is
    /// its own.
    pub(crate) fn context(&self) -> &Context {
        &self.context
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
        let record = match self.opens(continues) {
            None => Protocol::Steering { text },
            Some(turn) => Protocol::UserPrompt { turn, text },
        };
        self.record_protocol(record).map_err(|e| e.to_string())
    }

    /// The turn a prompt arriving now opens, or `None` where it extends the
    /// one `continues` names: a continuation is honoured only while that
    /// turn is in the context and is the prompt it answers.  Decided here
    /// once, for the record and for the echo the screen draws of it.
    pub fn opens(&self, continues: Option<u64>) -> Option<u64> {
        let extends = continues.is_some_and(|id| self.context.live_prompt() == Some(id));
        (!extends).then(|| self.context.next_id())
    }

    /// Append a user message between a complete tool-result batch and the next
    /// provider request — the only mid-turn user ingress there is.  One record
    /// per message, so a run of arrivals keeps the order it was typed in; the
    /// provider reads consecutive user messages as one turn.
    ///
    /// # Errors
    /// The tool-result batch is incomplete, or recording the prompt failed.
    pub fn append_steering(&mut self, text: String) -> Result<(), String> {
        if !self.context.is_awaiting_assistant() {
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
            self.seam.report_fault(&error);
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
    /// recorded, so replay departs exactly the ids on disk. Returns the
    /// resolved cut; its row on screen is the caller's to author.
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
    ) -> Result<Cut, String> {
        let turns = self.context.resolve_cut(turns)?;
        let cut = Cut { turns, note };
        self.record_protocol(Protocol::Evicted {
            cut: cut.clone(),
            by,
        })
        .map_err(|e| e.to_string())?;
        Ok(cut)
    }

    /// `/rewind`: turn `anchor` and every turn after it leave the structure.
    /// Its row on screen is the caller's to author.
    ///
    /// # Errors
    /// Refuses an anchor not recorded, already departed, or in a batch still
    /// awaiting tool results.
    pub fn rewind(&mut self, anchor: u64) -> Result<(), String> {
        self.context.rewindable(anchor)?;
        self.record_protocol(Protocol::Rewound { anchor })
            .map_err(|e| e.to_string())
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
    pub fn record_usage(&mut self, usage: Usage) -> io::Result<()> {
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
    pub fn record_nudge(&mut self, cause: String, spent: Option<Spent>) -> io::Result<()> {
        self.record_forensic(Forensic::Nudge { cause, spent })
    }

    /// # Errors
    /// See the meta-records note above.
    pub fn record_provider_error(&mut self, error: &ProviderError) -> io::Result<()> {
        self.record_forensic(Forensic::ProviderError {
            error: error.clone(),
        })
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:session-dir] (re)creates the session log dir; event-log infra, not turn-time data I/O"
    )]
    fn open_fresh(
        sessions_root: PathBuf,
        session_id: AgentId,
        model: RecordedModel,
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
            #[cfg(test)]
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
            model: self.model.name.clone(),
            context_window: self.model.context_window,
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
            Err(error) => self.seam.report_fault(&error),
        }
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

#[cfg(test)]
#[allow(
    unused_results,
    clippy::wildcard_enum_match_arm,
    reason = "[test] fixtures discard what they do not assert on"
)]
mod tests;
