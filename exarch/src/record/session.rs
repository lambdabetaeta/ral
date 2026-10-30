//! `AgentLog`: one session's handle onto `sessions/<n>/record.jsonl`.
//!
//! It sits over [`Context`], the model fold's authoritative in-memory
//! structure.  Mutations are this log's — each appends the record and
//! advances the fold on what was witnessed; queries are the [`Context`]'s
//! own, reached through [`AgentLog::context`].
//!
//! `tui::scrollback` folds the same log's `Display`/`Forensic` classes into the
//! scrollback.

use super::context::{Context, Eviction, ItemKind, Linked, Span, validate_result_ids};
use super::{
    Control, EditAuthority, Fold as _, Folds, Forensic, Mail, Memory, Model, NonEmpty, Post,
    QuiesceReason, RECORD, Record, Recorded, Refusal, Role, Schedules, Selected, Started,
    ToolResult, Transcript, Unavailable, Voice,
};
use crate::record::AgentId;
use genai::chat::{ChatMessage, ChatRole};
#[cfg(test)]
use ral_core::own::TestDir;
use ral_core::own::{Entry, OwnDir};
use ral_core::path::Kind;
use std::{fmt, io};

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
    dir: OwnDir,
    /// `<run>/sessions/`, under which `fork` makes each child's dir.
    sessions_root: OwnDir,
    /// The directory a test's log has to itself, dropped with the log.
    #[cfg(test)]
    _scratch: Option<TestDir>,
    /// The model fold's own structure over `record.jsonl` — the type every
    /// query in this file answers from; advanced inline through
    /// [`Self::advance`] on every protocol fact this log authors.
    context: Context,
    /// The one seam: every fact this log authors crosses here as a `Record` —
    /// appended to `sessions/<n>/record.jsonl` and published on whatever bus
    /// `Agent::couple` has attached, in that order, under one lock.
    seam: crate::record::Emitter,
}

/// Why a protocol mutation did not commit.
#[derive(Debug)]
pub enum Uncommitted {
    /// Refused as written: nothing was recorded and the session is usable.
    Refused(String),
    /// The writer failed: the session has ended.
    Unavailable(Unavailable),
}

impl fmt::Display for Uncommitted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Refused(why) => why.fmt(f),
            Self::Unavailable(failure) => failure.fmt(f),
        }
    }
}

impl std::error::Error for Uncommitted {}

impl From<Unavailable> for Uncommitted {
    fn from(failure: Unavailable) -> Self {
        Self::Unavailable(failure)
    }
}

/// What a `mnemon` child is forked with: its parent's turn table and evictions.
///
/// The table is turn by turn under the parent's own ids, each row carrying
/// the address that makes it readable whoever recorded it first.
pub struct Inherited {
    /// The parent's table at the fork, each row beside the address its
    /// transcript copy lies at.
    pub turns: Vec<Linked>,
    /// The parent's evictions: the child folds them in, so its context is
    /// the same projection the parent's was.
    pub evictions: Vec<Eviction>,
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
        sessions_root: &OwnDir,
        session_id: AgentId,
        name: &str,
        returns: bool,
        selected: &Selected,
        system: &str,
        inherited: Option<Inherited>,
    ) -> io::Result<(Self, Mail, Schedules, Model)> {
        let mut log = Self::open_fresh(sessions_root.clone(), session_id)?;
        let (mail, schedules, model) =
            log.open(None, name, returns, selected, system, inherited)?;
        Ok((log, mail, schedules, model))
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
    ) -> io::Result<(Self, Mail, Schedules, Model)> {
        let scratch = OwnDir::for_test("agent-log")?;
        let selected = Selected::for_test(model);
        let (log, mail, schedules, model) =
            Self::root(&scratch, session_id, "test", false, &selected, "", None)?;
        Ok((
            Self {
                _scratch: Some(scratch),
                ..log
            },
            mail,
            schedules,
            model,
        ))
    }

    /// Build a forked child log, inheriting `sessions_root` and recording this
    /// log's id as the child's parent.  A `mnemon` child opens with a link to
    /// this log's whole turn table.
    ///
    /// `selected` is the child's own choice, not this log's opening choice.
    ///
    /// # Errors
    /// Creating the child's directory, or writing its opening records, failed
    /// — or this log's turns will not read back for the link.
    pub(crate) fn fork(
        &self,
        child_id: AgentId,
        name: &str,
        selected: &Selected,
        system: &str,
        memory: Memory,
    ) -> io::Result<(Self, Mail, Schedules, Model)> {
        self.seam.check_available()?;
        let mut log = Self::open_fresh(self.sessions_root.clone(), child_id)?;
        let inherited = (memory == Memory::Mnemon).then(|| self.inherited_context());
        let (mail, schedules, model) =
            log.open(Some(self.id), name, true, selected, system, inherited)?;
        Ok((log, mail, schedules, model))
    }

    /// Fold the context and the three control folds from `record.jsonl`,
    /// quiesce an unready context, and commit the resume line reconciling
    /// them, then `System` if it changed.
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
    pub fn reopen(
        sessions_root: &OwnDir,
        session_id: AgentId,
        selected: &Selected,
        returns: bool,
        system: &str,
        at_unix_ms: u64,
    ) -> io::Result<(Self, Mail, Schedules, Model)> {
        let dir = sessions_root.dir(&session_id.to_string())?;
        let record_path = dir.path().join(RECORD);
        if dir.stat(RECORD).is_none() {
            return Err(io::Error::other(format!(
                "cannot resume {}: no record.jsonl was found here; was this session started with an older exarch, or is {} the right directory to resume?",
                ral_core::path::shown(&record_path),
                dir
            )));
        }
        let (context, (mut mail, mut schedules, mut model)) =
            crate::record::context::resume(sessions_root, session_id).map_err(|error| {
                io::Error::new(
                    error.kind(),
                    format!(
                        "cannot resume {}: {error}",
                        ral_core::path::shown(&record_path)
                    ),
                )
            })?;
        let seam = crate::record::Emitter::append_to(&dir)?;
        let mut resumed = Self {
            id: session_id,
            dir,
            sessions_root: sessions_root.clone(),
            #[cfg(test)]
            _scratch: None,
            context,
            seam,
        };
        if !resumed.context.is_ready() {
            resumed.quiesce(QuiesceReason::Aborted)?;
        }
        let pending: Vec<_> = mail.pending().collect();
        let mut line = NonEmpty::one(Control::SessionResumed {
            selected: selected.clone(),
            returns,
            at_unix_ms,
        });
        let withdrawn = if let Some(posts) = NonEmpty::collect(pending.iter().map(|&(id, _)| id)) {
            line.rest.push(Control::Withdrawn { posts });
            Some(withdrawn_note(
                &pending.iter().map(|&(_, post)| post).collect::<Vec<_>>(),
            ))
        } else {
            None
        };
        if let Some(job) = mail.job() {
            line.rest.push(Control::JobSettled {
                job,
                stopped: false,
            });
        }
        line.rest
            .extend(schedules.iter().map(|(label, _)| Control::Unscheduled {
                label: label.to_string(),
            }));
        drop(resumed.seam.commit_to(
            Folds {
                mail: &mut mail,
                schedules: &mut schedules,
                model: &mut model,
            },
            line,
            withdrawn.into_iter().collect(),
        )?);
        if *system != **resumed.context.system() {
            resumed.record_transcript(Transcript::System {
                text: system.to_string(),
            })?;
        }
        Ok((resumed, mail, schedules, model))
    }

    pub fn resumed_summary(&self) -> Resumed {
        let bytes = self.dir.stat(RECORD).map_or(0, |stat| stat.len);
        Resumed {
            turn: self.context.current_turn().unwrap_or(0),
            bytes,
        }
    }

    /// Advance the model fold for one witnessed record — called immediately
    /// after every [`crate::record::Emitter::emit`] this log makes itself.
    ///
    /// # Errors
    /// A link's ancestor file will not read back.
    fn advance(&mut self, record: Recorded<Record>) -> io::Result<()> {
        Context::step(&mut self.context, record).map_err(|refusal| match refusal {
            Refusal::Unreadable(error) => error,
            // Live authorship is typestate-correct by construction, so a
            // foreign record here is a caller bug; skipping it would desync
            // the memo from record.jsonl with nothing to show for it.
            Refusal::Foreign { .. } => unreachable!("model fold refused a live record: {refusal}"),
        })
    }

    /// The record seam this session authors through — a cheap clone whoever
    /// couples a live bus, records a display record, or feeds the chopper
    /// takes for themselves.
    pub fn record_emitter(&self) -> crate::record::Emitter {
        self.seam.clone()
    }

    pub fn id(&self) -> AgentId {
        self.id
    }

    /// Every session directory under `sessions_root`, in id order.
    ///
    /// # Errors
    /// The directory cannot be read.
    pub(crate) fn session_ids(sessions_root: &OwnDir) -> io::Result<Vec<AgentId>> {
        let mut ids: Vec<AgentId> = sessions_root
            .list()?
            .into_iter()
            .filter_map(|Entry { name, stat }| {
                let id = name.parse().ok()?;
                (stat.kind == Kind::Dir).then_some(AgentId::new(id))
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
    pub fn live_roots(sessions_root: &OwnDir) -> io::Result<Vec<(AgentId, String)>> {
        Self::session_ids(sessions_root)?
            .into_iter()
            .filter_map(|id| {
                live_root(sessions_root, id)
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
    pub fn dir(&self) -> &OwnDir {
        &self.dir
    }

    /// The run's `sessions/` directory this log's session is in, for a test that
    /// folds a child of it.
    #[cfg(test)]
    pub(crate) fn sessions_root(&self) -> &OwnDir {
        &self.sessions_root
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
            evictions: self.context.evictions(),
        }
    }

    /// One harness-authored message imported as a turn of this log's own —
    /// the resume note, which inherits nothing and links to no one.
    ///
    /// # Errors
    /// Refused as not ready (`Refused`), or the writer failed (`Unavailable`).
    pub fn import_note(&mut self, message: ChatMessage) -> Result<(), Uncommitted> {
        if !self.context.is_ready() {
            return Err(Uncommitted::Refused(format!(
                "cannot import context while the session is {}",
                self.context.waiting_for()
            )));
        }
        let turn = self.context.next_id();
        self.record_transcript(Transcript::Import { turn, message })
            .map_err(Uncommitted::from)
    }

    // ── Transcript mutations ────────────────────────────────────────────────

    /// Commit a top-level user prompt, opening a turn of its own — or
    /// extending the turn `continues` names, where that prompt is still the
    /// live one.
    ///
    /// # Errors
    /// Refused as mid-turn (`Refused`), or the writer failed (`Unavailable`).
    pub fn append_user(
        &mut self,
        text: String,
        continues: Option<u64>,
        voice: Voice,
    ) -> Result<(), Uncommitted> {
        if !self.context.is_ready() {
            return Err(Uncommitted::Refused(format!(
                "cannot accept a new user prompt while the session is {}",
                self.context.waiting_for()
            )));
        }
        // A continuation is honoured only while the turn it names is in the
        // context and is the prompt it answers.
        let record = if continues.is_some_and(|id| self.context.live_prompt() == Some(id)) {
            Transcript::Steering { text, voice }
        } else {
            Transcript::UserPrompt {
                turn: self.context.next_id(),
                text,
                voice,
            }
        };
        self.record_transcript(record).map_err(Uncommitted::from)
    }

    /// Append a user message between a complete tool-result batch and the next
    /// provider request — the only mid-turn user ingress there is.  One record
    /// per message, so a run of arrivals keeps the order it was typed in; the
    /// provider reads consecutive user messages as one turn.
    ///
    /// # Errors
    /// Refused as incomplete or turnless (`Refused`), or the writer failed (`Unavailable`).
    pub fn append_steering(&mut self, text: String, voice: Voice) -> Result<(), Uncommitted> {
        if !self.context.is_awaiting_assistant() {
            return Err(Uncommitted::Refused(format!(
                "tool results must be complete before accepting a steering prompt; the session is {}",
                self.context.waiting_for()
            )));
        }
        if self.context.unclosed_turn().is_none() {
            return Err(Uncommitted::Refused(
                "cannot accept a steering prompt: no turn is being written".into(),
            ));
        }
        self.record_transcript(Transcript::Steering { text, voice })
            .map_err(Uncommitted::from)
    }

    /// Commit an assistant reply, moving the turn on to await tool results
    /// or — when no tools were called — back to a ready boundary.
    ///
    /// # Errors
    /// Refused as unexpected or wrong-role (`Refused`), or the writer failed (`Unavailable`).
    pub fn append_assistant(
        &mut self,
        message: ChatMessage,
        pending_tool_ids: Vec<String>,
        stop_reason: Option<String>,
    ) -> Result<(), Uncommitted> {
        if !self.context.is_awaiting_assistant() {
            return Err(Uncommitted::Refused(format!(
                "assistant message is not expected while the session is {}",
                self.context.waiting_for()
            )));
        }
        if message.role != ChatRole::Assistant {
            return Err(Uncommitted::Refused(format!(
                "a {} message arrived where the assistant's reply was expected",
                role_label(&message.role)
            )));
        }
        let turn = self.context.next_id();
        self.record_transcript(Transcript::AssistantMessage {
            turn,
            message,
            pending_tool_ids,
            stop_reason,
        })
        .map_err(Uncommitted::from)
    }

    /// Commit the tool-result batch answering the pending tool calls.
    ///
    /// # Errors
    /// Refused as unexpected or mismatched (`Refused`), or the writer failed (`Unavailable`).
    pub fn append_tool_results(&mut self, results: Vec<ToolResult>) -> Result<(), Uncommitted> {
        let Some(pending_ids) = self.context.pending_tool_results() else {
            return Err(Uncommitted::Refused(format!(
                "tool results are not expected while the session is {}",
                self.context.waiting_for()
            )));
        };
        validate_result_ids(&pending_ids, &results).map_err(Uncommitted::Refused)?;
        self.record_transcript(Transcript::ToolResults { results })
            .map_err(Uncommitted::from)
    }

    /// End work that produced no real assistant reply, recording only what
    /// the log still owes: an answer to tool calls that never ran, and a
    /// capstone for work that ended on `reply`.  Work abandoned short of a
    /// reply is left as it lies, and the next prompt opens a turn after it.
    ///
    /// # Errors
    /// A required record failed: the writer has failed and the session has ended.
    pub fn quiesce(&mut self, reason: QuiesceReason) -> Result<(), Unavailable> {
        for record in self.context.quiesce_records(reason) {
            self.record_transcript(record)?;
        }
        // Only a cancellation earns its own breadcrumb; an abort's marker is
        // the `ProviderError` already on disk.
        if matches!(reason, QuiesceReason::Cancelled) {
            self.seam.emit_or_end(Forensic::Cancelled);
        }
        Ok(())
    }

    /// Take the run `span` out of the context, leaving one eviction standing
    /// for it with `note`. Its row on screen is the caller's to author.
    ///
    /// # Errors
    /// Refused as an invalid cut (`Refused`), or the writer failed (`Unavailable`).
    pub fn evict(
        &mut self,
        span: Span,
        note: Option<String>,
        by: EditAuthority,
    ) -> Result<(), Uncommitted> {
        self.context
            .resolve_cut(span)
            .map_err(Uncommitted::Refused)?;
        self.record_transcript(Transcript::Evicted(Eviction { span, note, by }))
            .map_err(Uncommitted::from)
    }

    /// `/rewind`: turn `anchor` and every turn after it are withdrawn from
    /// the context, and stay readable in the transcript. Its row on screen is
    /// the caller's to author.
    ///
    /// # Errors
    /// Refused as an invalid anchor (`Refused`), or the writer failed (`Unavailable`).
    pub fn rewind(&mut self, anchor: u64) -> Result<(), Uncommitted> {
        self.context
            .rewindable(anchor)
            .map_err(Uncommitted::Refused)?;
        self.record_transcript(Transcript::Rewound { anchor })
            .map_err(Uncommitted::from)
    }

    /// Preflight `/clear` before preparing its replacement engine.
    ///
    /// # Errors
    /// Refused as not ready (`Refused`), or the writer failed (`Unavailable`).
    pub(crate) fn prepare_clear(&self) -> Result<(), Uncommitted> {
        self.seam.check_available()?;
        if !self.context.is_ready() {
            return Err(Uncommitted::Refused(format!(
                "cannot clear while the session is {}",
                self.context.waiting_for()
            )));
        }
        Ok(())
    }

    /// `/clear`: withdraw every turn; the transcript and system prompt stay.
    ///
    /// # Errors
    /// Refused as not ready (`Refused`), or the writer failed (`Unavailable`).
    pub(crate) fn clear(&mut self) -> Result<(), Uncommitted> {
        self.prepare_clear()?;
        self.record_transcript(Transcript::Cleared)
            .map_err(Uncommitted::from)
    }

    // ── Internal helpers ──────────────────────────────────────────────────

    fn open_fresh(sessions_root: OwnDir, session_id: AgentId) -> io::Result<Self> {
        let name = session_id.to_string();
        sessions_root.remove(&name)?;
        let dir = sessions_root.make_dir(&name)?;
        let seam = crate::record::Emitter::create(&dir)?;
        Ok(Self {
            id: session_id,
            dir,
            context: Context::new(session_id, sessions_root.clone()),
            sessions_root,
            #[cfg(test)]
            _scratch: None,
            seam,
        })
    }

    /// A log's opening, in one fixed order: `SessionStarted`, the link where
    /// there is one, then the system prompt.
    fn open(
        &mut self,
        parent: Option<AgentId>,
        name: &str,
        returns: bool,
        selected: &Selected,
        system: &str,
        inherited: Option<Inherited>,
    ) -> io::Result<(Mail, Schedules, Model)> {
        let folds = self.seam.begin(Started {
            session_id: self.id,
            name: name.to_string(),
            parent,
            returns,
            selected: selected.clone(),
            log_dir: self.dir.path().to_path_buf(),
            at_unix_ms: crate::app::now_unix_ms(),
        })?;
        if let Some(Inherited { turns, evictions }) = inherited {
            let link = self.seam.emit(Transcript::Inherited { turns, evictions })?;
            self.advance(link)?;
        }
        self.record_transcript(Transcript::System {
            text: system.to_string(),
        })?;
        Ok(folds)
    }

    /// Emit a protocol fact and advance the fold on its witnessed result.
    ///
    /// # Errors
    /// The writer failed, or the fold refused a durable record, breaking agreement.
    fn record_transcript(&mut self, p: Transcript) -> Result<(), Unavailable> {
        let recorded = self.seam.emit(p)?;
        self.advance(recorded)
            .map_err(|error| self.seam.fail(error))
    }
}

/// What a restart tells the log about the inputs it withdrew unread.
fn withdrawn_note(pending: &[&Post]) -> Forensic {
    let descriptions = pending
        .iter()
        .map(|post| {
            let line: String = post
                .text
                .lines()
                .next()
                .unwrap_or_default()
                .chars()
                .take(80)
                .collect();
            format!("{}: {line}", post.voice_word())
        })
        .collect::<Vec<_>>()
        .join("; ");
    Forensic::SystemNote {
        text: format!(
            "the restart withdrew {} queued input(s) no one had read: {descriptions}",
            pending.len()
        ),
    }
}

/// The name session `id`'s record was opened under if it heads a root and was
/// never closed.  A directory with no record is no session, hence not live.
///
/// # Errors
/// The record exists but cannot be opened, or holds a line that is not a
/// record.
fn live_root(sessions: &OwnDir, id: AgentId) -> io::Result<Option<String>> {
    let opened = sessions
        .dir(&id.to_string())
        .and_then(|session| session.open(RECORD).map(|file| (session, file)));
    let (session, file) = match opened {
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        opened => opened?,
    };
    let mut records = super::log::Log::read(file, &session.path().join(RECORD))?;
    let Some(head) = records.next().transpose()? else {
        return Ok(None);
    };
    let Record::Control(Control::SessionStarted(Started {
        parent: None, name, ..
    })) = head.value()
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
#[allow(clippy::disallowed_methods, reason = "test code")]
#[allow(
    unused_results,
    clippy::wildcard_enum_match_arm,
    reason = "[test] fixtures discard what they do not assert on"
)]
mod tests;
