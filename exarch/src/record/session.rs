//! `AgentLog`: one session's handle onto `sessions/<n>/record.jsonl`.
//!
//! It sits over [`Context`], the model fold's authoritative in-memory
//! structure.  Mutations are this log's — each appends the record and
//! advances the fold on what was witnessed; queries are the [`Context`]'s
//! own, reached through [`AgentLog::context`].
//!
//! `tui::scrollback` folds the same log's `Display`/`Forensic` classes into the
//! scrollback.

use super::model::{Context, ItemKind, Linked, Model, Residue, Span, validate_result_ids};
use super::{
    EditAuthority, Fold as _, Forensic, Memory, Protocol, QuiesceReason, Record, Recorded,
    RecordedAccount, RecordedModel, Refusal, Role, ToolResult, Voice,
};
use crate::record::AgentId;
use genai::chat::{ChatMessage, ChatRole};
use std::fs;
use std::io;
use std::path::{Path, PathBuf};

/// One line of one message that matched `` exarch-transcript `grep ``'s pattern.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct GrepHit {
    /// The item the line lies in, by `(span, kind)` — the search walks item
    /// by item, so a hit names one outright.
    pub span: Span,
    pub kind: ItemKind,
    pub role: ChatRole,
    /// 1-based, within that message's searched text; a note is one line.
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
    /// `<scratch>/sessions/`, under which `fork` makes each child's dir.
    sessions_root: PathBuf,
    /// The directory a test's log has to itself, dropped with the log.
    #[cfg(test)]
    _scratch: Option<tempfile::TempDir>,
    /// The model fold's own structure over `record.jsonl` — the type every
    /// query in this file answers from; advanced inline through
    /// [`Self::advance`] on every protocol fact this log authors.
    context: Context,
    /// The `(model, window)` a prior run's overflow taught; set by `resume`.
    learned: Option<(String, u64)>,
    /// The one seam: every fact this log authors crosses here as a `Record` —
    /// appended to `sessions/<n>/record.jsonl` and published on whatever bus
    /// `Avatar::couple` has attached, in that order, under one lock.
    seam: crate::record::Emitter,
}

/// What a `mnemon` child is forked with: its parent's turn table and residues.
///
/// The table is turn by turn under the parent's own ids, each row carrying
/// the address that makes it readable whoever recorded it first.
pub struct Inherited {
    /// The parent's table at the fork, each row beside the address its
    /// transcript copy lies at.
    pub turns: Vec<Linked>,
    /// The parent's residues: the child folds them in, so its context is
    /// the same projection the parent's was.
    pub residues: Vec<Residue>,
}

impl AgentLog {
    /// Build a fresh root session log.  Wipes any prior directory with the
    /// same session id, so `record.jsonl` starts empty.
    ///
    /// # Errors
    /// Re-creating the session directory, or writing the opening records,
    /// failed — or `inherited` names a turn its ancestor's file will not read
    /// back.
    pub fn root(
        sessions_root: &Path,
        session_id: AgentId,
        name: &str,
        model: &RecordedModel,
        account: &RecordedAccount,
        system: &str,
        inherited: Option<Inherited>,
    ) -> io::Result<Self> {
        let mut log = Self::open_fresh(sessions_root.to_path_buf(), session_id)?;
        log.open(None, name, model, account, system, inherited)?;
        Ok(log)
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
        let log = Self::root(
            scratch.path(),
            session_id,
            "test",
            &model,
            account,
            "",
            None,
        )?;
        Ok(Self {
            _scratch: Some(scratch),
            ..log
        })
    }

    /// Build a forked child log, inheriting `sessions_root` and recording this
    /// log's id as the child's parent.  A `mnemon` child opens with a link to
    /// this log's whole turn table.
    ///
    /// `model` and `account` are the child's *own*, handed in rather than
    /// copied off this log: a spawn may name another selection, and this log's
    /// pair was fixed at session start, so a `/model` since would make a
    /// copied header a lie.
    ///
    /// # Errors
    /// Creating the child's directory, or writing its opening records, failed
    /// — or this log's turns will not read back for the link.
    pub(crate) fn fork(
        &self,
        child_id: AgentId,
        name: &str,
        model: &RecordedModel,
        account: &RecordedAccount,
        system: &str,
        memory: Memory,
    ) -> io::Result<Self> {
        let mut log = Self::open_fresh(self.sessions_root.clone(), child_id)?;
        let inherited = (memory == Memory::Mnemon).then(|| self.inherited_context());
        log.open(Some(self.id), name, model, account, system, inherited)?;
        Ok(log)
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
        let dir = Self::dir_of(sessions_root, session_id);
        let record_path = dir.join("record.jsonl");
        if !record_path.exists() {
            return Err(io::Error::other(format!(
                "cannot resume {}: no record.jsonl was found here; was this session started with an older exarch, or is {} the right directory to resume?",
                record_path.display(),
                dir.display()
            )));
        }
        let (context, selection) = crate::record::model::resume(&record_path).map_err(|error| {
            io::Error::new(
                error.kind(),
                format!("cannot resume {}: {error}", record_path.display()),
            )
        })?;
        let seam = crate::record::Emitter::append_to(&record_path)?;
        let mut resumed = Self {
            id: session_id,
            dir,
            sessions_root: sessions_root.to_path_buf(),
            #[cfg(test)]
            _scratch: None,
            context,
            seam,
            learned: selection.learned(),
        };
        if !resumed.context.is_ready() {
            resumed.quiesce(QuiesceReason::Aborted);
        }
        Ok(resumed)
    }

    /// The `(model, window)` a prior run's overflow taught its provider.
    pub fn learned_window(&self) -> Option<&(String, u64)> {
        self.learned.as_ref()
    }

    pub fn resumed_summary(&self) -> Resumed {
        let bytes =
            fs::metadata(self.dir.join("record.jsonl")).map_or(0, |metadata| metadata.len());
        Resumed {
            turn: self.context.current_turn().unwrap_or(0),
            bytes,
        }
    }

    /// Record the live model selection and the shared resume boundary stamp,
    /// and the system prompt iff it differs from the one the log last held.
    ///
    /// # Errors
    /// Returns an error if a record cannot be appended.
    pub fn record_resumed(
        &mut self,
        model: &RecordedModel,
        account: &RecordedAccount,
        system: &str,
        at_unix_ms: u64,
    ) -> io::Result<()> {
        self.seam
            .emit(Forensic::SessionResumed {
                model: model.name.clone(),
                context_window: model.context_window,
                label: account.label.clone(),
                service: account.service.clone(),
                account: account.id.clone(),
                at_unix_ms,
            })
            .map(drop)?;
        if *system == **self.context.system() {
            return Ok(());
        }
        self.record_protocol(Protocol::System {
            text: system.to_string(),
        })
    }

    /// Advance the model fold for one witnessed record — called immediately
    /// after every [`crate::record::Emitter::emit`] this log makes itself.
    ///
    /// # Errors
    /// A link's ancestor file will not read back.
    fn advance(&mut self, record: Recorded<Record>) -> io::Result<()> {
        Model::step(&mut self.context, record).map_err(|refusal| match refusal {
            Refusal::Unreadable(error) => error,
            // Live authorship is typestate-correct by construction, so a
            // foreign record here is a caller bug; skipping it would desync
            // the memo from record.jsonl with nothing to show for it.
            Refusal::Foreign { .. } => unreachable!("model fold refused a live record: {refusal}"),
        })
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

    /// Every session directory under `sessions_root`, in id order.
    ///
    /// # Errors
    /// The directory exists but cannot be read.
    pub(crate) fn session_ids(sessions_root: &Path) -> io::Result<Vec<AgentId>> {
        let entries = match fs::read_dir(sessions_root) {
            Ok(entries) => entries,
            Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Vec::new()),
            Err(error) => return Err(error),
        };
        let mut ids: Vec<AgentId> = entries
            .filter_map(Result::ok)
            .filter_map(|entry| {
                let id = entry.file_name().to_str()?.parse().ok()?;
                entry.file_type().ok()?.is_dir().then_some(AgentId::new(id))
            })
            .collect();
        ids.sort_unstable();
        Ok(ids)
    }

    /// The roots a resume reopens: every session whose record opens with a
    /// root's `SessionStarted` and holds no `SessionClosed`, in id order, with
    /// the name it was recorded under.
    ///
    /// # Errors
    /// `sessions_root` exists but cannot be read, or a session's record
    /// cannot: one unreadable record is named rather than skipped, since
    /// skipping it would resume the others as if it had never been open.
    pub fn live_roots(sessions_root: &Path) -> io::Result<Vec<(AgentId, String)>> {
        Self::session_ids(sessions_root)?
            .into_iter()
            .filter_map(|id| {
                let record = Self::dir_of(sessions_root, id).join("record.jsonl");
                live_root(&record)
                    .map_err(|error| {
                        io::Error::new(
                            error.kind(),
                            format!(
                                "cannot read sessions/{id}/record.jsonl: {error}; move that session aside to resume the others?"
                            ),
                        )
                    })
                    .transpose()
                    .map(|name| name.map(|name| (id, name)))
            })
            .collect()
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

    /// The parent half of a `mnemon` fork: the link, which the child's fold
    /// reads back through its pointers.
    pub fn inherited_context(&self) -> Inherited {
        Inherited {
            turns: self.context.linked(),
            residues: self.context.residues(),
        }
    }

    /// One harness-authored message imported as a turn of this log's own —
    /// the resume note, which inherits nothing and links to no one.
    ///
    /// # Errors
    /// The session is not at a ready boundary, or recording failed.
    pub fn import_note(&mut self, message: ChatMessage) -> Result<(), String> {
        if !self.context.is_ready() {
            return Err(format!(
                "cannot import context while the session is {}",
                self.context.waiting_for()
            ));
        }
        let turn = self.context.next_id();
        self.record_protocol(Protocol::Import { turn, message })
            .map_err(|e| e.to_string())
    }

    // ── Protocol mutations ────────────────────────────────────────────────

    /// Commit a top-level user prompt, opening a turn of its own — or
    /// extending the turn `continues` names, where that prompt is still the
    /// live one.
    ///
    /// # Errors
    /// The session is mid-turn, or recording the prompt failed.
    pub fn append_user(
        &mut self,
        text: String,
        continues: Option<u64>,
        voice: Voice,
    ) -> Result<(), String> {
        if !self.context.is_ready() {
            return Err(format!(
                "cannot accept a new user prompt while the session is {}",
                self.context.waiting_for()
            ));
        }
        // A continuation is honoured only while the turn it names is in the
        // context and is the prompt it answers.
        let record = if continues.is_some_and(|id| self.context.live_prompt() == Some(id)) {
            Protocol::Steering { text, voice }
        } else {
            Protocol::UserPrompt {
                turn: self.context.next_id(),
                text,
                voice,
            }
        };
        self.record_protocol(record).map_err(|e| e.to_string())
    }

    /// Append a user message between a complete tool-result batch and the next
    /// provider request — the only mid-turn user ingress there is.  One record
    /// per message, so a run of arrivals keeps the order it was typed in; the
    /// provider reads consecutive user messages as one turn.
    ///
    /// # Errors
    /// The tool-result batch is incomplete, or recording the prompt failed.
    pub fn append_steering(&mut self, text: String, voice: Voice) -> Result<(), String> {
        if !self.context.is_awaiting_assistant() {
            return Err(format!(
                "tool results must be complete before accepting a steering prompt; the session is {}",
                self.context.waiting_for()
            ));
        }
        if self.context.unclosed_turn().is_none() {
            return Err("cannot accept a steering prompt: no turn is being written".into());
        }
        self.record_protocol(Protocol::Steering { text, voice })
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
        if matches!(reason, QuiesceReason::Cancelled) {
            self.seam.emit_or_report(Forensic::Cancelled);
        }
    }

    /// The span a forced eviction would take to spend no more
    /// than `keep_budget_bytes` on what stays; `None` when nothing is old
    /// enough to shed.
    pub fn plan_forced_eviction(&self, keep_budget_bytes: usize) -> Option<Span> {
        self.context.plan_forced_eviction(keep_budget_bytes)
    }

    /// Take the run `span` out of the context, leaving one residue standing
    /// for it with `note`. Its row on screen is the caller's to author.
    ///
    /// # Errors
    /// Refuses what [`Context::resolve_cut`] refuses.
    pub fn evict(
        &mut self,
        span: Span,
        note: Option<String>,
        by: EditAuthority,
    ) -> Result<(), String> {
        self.context.resolve_cut(span)?;
        self.record_protocol(Protocol::Evicted {
            residue: Residue { span, note, by },
        })
        .map_err(|e| e.to_string())
    }

    /// `/rewind`: turn `anchor` and every turn after it are withdrawn from
    /// the context, and stay readable in the transcript. Its row on screen is
    /// the caller's to author.
    ///
    /// # Errors
    /// Refuses an anchor not recorded, already withdrawn, or in a batch still
    /// awaiting tool results.
    pub fn rewind(&mut self, anchor: u64) -> Result<(), String> {
        self.context.rewindable(anchor)?;
        self.record_protocol(Protocol::Rewound { anchor })
            .map_err(|e| e.to_string())
    }

    /// `/clear`: every turn is withdrawn from the context and stays readable
    /// in the transcript; the system prompt stays.
    ///
    /// # Errors
    /// The session is awaiting tool results, or recording failed.
    pub(crate) fn clear(&mut self) -> io::Result<()> {
        if !self.context.is_ready() {
            return Err(io::Error::other(format!(
                "cannot clear while the session is {}",
                self.context.waiting_for()
            )));
        }
        self.record_protocol(Protocol::Cleared)
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    #[allow(
        clippy::disallowed_methods,
        reason = "[silent:session-dir] (re)creates the session log dir; event-log infra, not turn-time data I/O"
    )]
    fn open_fresh(sessions_root: PathBuf, session_id: AgentId) -> io::Result<Self> {
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
            sessions_root,
            #[cfg(test)]
            _scratch: None,
            context: Context::new(source),
            learned: None,
            seam,
        })
    }

    /// A log's opening, in one fixed order: `SessionStarted`, the link where
    /// there is one, then the system prompt.
    fn open(
        &mut self,
        parent: Option<AgentId>,
        name: &str,
        model: &RecordedModel,
        account: &RecordedAccount,
        system: &str,
        inherited: Option<Inherited>,
    ) -> io::Result<()> {
        self.seam
            .emit(Forensic::SessionStarted {
                session_id: self.id,
                name: name.to_string(),
                parent,
                model: model.name.clone(),
                context_window: model.context_window,
                label: account.label.clone(),
                service: account.service.clone(),
                account: account.id.clone(),
                log_dir: self.dir.clone(),
                at_unix_ms: crate::app::now_unix_ms(),
            })
            .map(drop)?;
        if let Some(Inherited { turns, residues }) = inherited {
            self.record_protocol(Protocol::Inherited { turns, residues })?;
        }
        self.record_protocol(Protocol::System {
            text: system.to_string(),
        })
    }

    /// Emit a protocol fact through the seam and advance the model fold on
    /// the witnessed result — the one funnel every protocol mutation takes.
    ///
    /// # Errors
    /// A failed append to the one authoritative log is a session error, and so
    /// is a link whose ancestor file will not read back.
    fn record_protocol(&mut self, p: Protocol) -> io::Result<()> {
        let recorded = self.seam.emit(p)?;
        self.advance(recorded)
    }

    /// [`Self::record_protocol`] with its failure reported as a fault: its
    /// remit is exactly the harness-synthesized records where refusing would
    /// wedge the session — what [`Self::quiesce`] owes.
    fn record_protocol_lossy(&mut self, p: Protocol) {
        if let Err(error) = self.record_protocol(p) {
            self.seam.report_fault(&error);
        }
    }
}

/// The name `record` was opened under if it heads a root and was never closed.
/// A directory with no record is no session, hence not live.
///
/// # Errors
/// The record exists but cannot be opened, or holds a line that is not a
/// record.
fn live_root(record: &Path) -> io::Result<Option<String>> {
    let mut records = match super::log::Log::read(record) {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        records => records?,
    };
    let Some(head) = records.next().transpose()? else {
        return Ok(None);
    };
    let Record::Forensic(Forensic::SessionStarted {
        parent: None, name, ..
    }) = head.value()
    else {
        return Ok(None);
    };
    for r in records {
        if matches!(r?.value(), Record::Forensic(Forensic::SessionClosed)) {
            return Ok(None);
        }
    }
    Ok(Some(name.clone()))
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
