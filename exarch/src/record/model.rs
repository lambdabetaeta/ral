//! The model fold: the provider-facing projection `AgentLog` used to build
//! by folding `SessionEvent`s off `events.jsonl`, now built the same way off
//! [`Protocol`] records off `record.jsonl` — the same `step`, whether it is
//! applied inline on the attend thread right after [`super::Emitter::emit`]
//! returns, or by [`super::replay`] from disk.
//!
//! [`Display`](super::Display) and [`Forensic`](super::Forensic) records pass
//! through untouched: this fold projects the protocol subsequence alone, so
//! its indices — and the [`Span`]s built from them — never see a display or
//! forensic record interleaved on the log between two protocol ones.
//!
//! No recorded protocol record is ever discarded: an evicted index keeps only
//! its [`Stamp`], and [`Ledger::fold_events`] reads it back through that
//! `Stamp`'s byte range one record at a time — never coalesced into a run —
//! so the from-scratch refold in [`resume`] is a bijection with the
//! incrementally maintained view, record by record.

use super::log::Log;
use super::{Fold, Protocol, Record, Recorded, Refusal, Stamp};
use crate::agent::event::{
    ContextOp, QuiesceReason, ToolResult, summary_prompt, validate_result_ids,
};
use genai::chat::{ChatMessage, ChatRole, ToolResponse};
use std::collections::BTreeMap;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};

/// Marker type implementing [`Fold`] for the model view; carries no state of
/// its own; [`Memo`] is where the projection lives.
pub struct Model;

impl Fold for Model {
    type Memo = Memo;

    fn step(memo: &mut Memo, record: &Recorded<Record>) -> Result<(), Refusal> {
        match record.value() {
            Record::Protocol(p) => {
                step_protocol(memo, record.stamp().clone(), p.clone());
                Ok(())
            }
            Record::Display(_) | Record::Forensic(_) => Ok(()),
        }
    }
}

/// Widen a witnessed [`Protocol`] into the [`Record`] enum [`Fold::step`]
/// expects — the bridge from `Emitter::emit::<Protocol>`'s typed return to
/// the class-blind dispatcher, for the attend thread's inline advance.
pub fn widen(recorded: Recorded<Protocol>) -> Recorded<Record> {
    let stamp = recorded.stamp().clone();
    Recorded::new(stamp, Record::Protocol(recorded.into_value()))
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
enum State {
    #[default]
    ReadyForUser,
    AwaitingAssistantAfterUser,
    AwaitingToolResults {
        pending_ids: Vec<String>,
    },
    AwaitingAssistantAfterToolResults,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Digest {
    through_exchange: u64,
    text: String,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct Span {
    id: u64,
    events: Range<usize>,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct View {
    digest: Option<Digest>,
    spans: Vec<Span>,
}

/// One resident protocol record, or the [`Stamp`] to read it back by once a
/// context edit evicts its span.
#[derive(Default)]
struct Ledger {
    len: usize,
    resident: BTreeMap<usize, Recorded<Protocol>>,
    freed: BTreeMap<usize, Stamp>,
    /// `record.jsonl`'s own path, known once this fold is seeded from disk
    /// ([`resume`]) — the only thing a freed index needs to read itself back.
    source: Option<PathBuf>,
}

impl Ledger {
    fn append(&mut self, record: Recorded<Protocol>) -> usize {
        let index = self.len;
        self.len += 1;
        self.resident.insert(index, record);
        index
    }

    fn len(&self) -> usize {
        self.len
    }

    fn event(&self, index: usize) -> Option<&Protocol> {
        self.resident.get(&index).map(Recorded::value)
    }

    fn resident_events(&self, range: Range<usize>) -> Option<Vec<&Protocol>> {
        let events: Vec<&Protocol> = self
            .resident
            .range(range.clone())
            .map(|(_, recorded)| recorded.value())
            .collect();
        (events.len() == range.len()).then_some(events)
    }

    fn free(&mut self, index: usize) {
        if let Some(recorded) = self.resident.remove(&index) {
            self.freed.insert(index, recorded.stamp().clone());
        }
    }

    fn evict(&mut self, ranges: &[Range<usize>]) {
        for index in ranges.iter().cloned().flatten() {
            self.free(index);
        }
    }

    /// Every protocol record this fold has ever seen, resident or not — the
    /// freed ones read back one at a time through their own `Stamp`.
    fn fold_events(&self) -> io::Result<Vec<(usize, Protocol)>> {
        let mut events: Vec<(usize, Protocol)> = self
            .resident
            .iter()
            .map(|(index, recorded)| (*index, recorded.value().clone()))
            .collect();
        if !self.freed.is_empty() {
            let Some(path) = self.source.as_ref() else {
                return Err(io::Error::other(
                    "the freed record ledger has no readable record.jsonl",
                ));
            };
            #[allow(
                clippy::disallowed_methods,
                reason = "[io-door:silent:model-fold-freed-read] reads one evicted protocol record back off record.jsonl by its own Stamp; output infra, not turn-time data I/O"
            )]
            let mut file = File::open(path)?;
            for (index, stamp) in &self.freed {
                events.push((*index, read_freed(&mut file, stamp)?));
            }
            events.sort_unstable_by_key(|(index, _)| *index);
        }
        Ok(events)
    }
}

fn read_freed(reader: &mut File, stamp: &Stamp) -> io::Result<Protocol> {
    let bytes = stamp.bytes();
    let length = usize::try_from(bytes.end.saturating_sub(bytes.start))
        .map_err(|_| io::Error::other("a freed record range is too large to read"))?;
    reader.seek(SeekFrom::Start(bytes.start))?;
    let mut buf = vec![0; length];
    reader.read_exact(&mut buf)?;
    let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
    let record: Record = serde_json::from_slice(line).map_err(io::Error::other)?;
    match record {
        Record::Protocol(p) => Ok(p),
        Record::Display(_) | Record::Forensic(_) => Err(io::Error::other(
            "a freed model-fold slot pointed at a non-protocol record — the ledger's own invariant broke",
        )),
    }
}

/// The model fold's memo: the running protocol-view projection, per-record
/// residency, and the ledger those two are built from.
#[derive(Default)]
pub struct Memo {
    ledger: Ledger,
    state: State,
    view: View,
    max_exchange: u64,
    newest_edit: Option<usize>,
}

impl Memo {
    /// Attach `record.jsonl`'s own path, so a later eviction can read its
    /// freed record back — set once, at resume, by whoever seeded this memo
    /// from disk; a live memo with no eviction ever needs it.
    pub fn attach_source(&mut self, path: PathBuf) {
        self.ledger.source = Some(path);
    }

    pub fn is_ready(&self) -> bool {
        matches!(self.state, State::ReadyForUser)
    }

    pub fn can_compact(&self) -> bool {
        self.is_ready()
    }

    pub fn current_exchange(&self) -> Option<u64> {
        (self.max_exchange != 0).then_some(self.max_exchange)
    }

    pub fn last_view_exchange(&self) -> Option<u64> {
        self.view.spans.last().map(|span| span.id).or_else(|| {
            self.view
                .digest
                .as_ref()
                .map(|digest| digest.through_exchange)
        })
    }

    pub fn log_len(&self) -> usize {
        self.ledger.len()
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.newest_edit.is_some_and(|index| index >= measured_at)
    }

    pub fn is_live_exchange(&self, id: u64) -> bool {
        !self.is_ready() && self.max_exchange == id
    }

    /// The provider-facing message list — recomputed from the protocol
    /// subsequence on every call, never accumulator state: `span_messages`'s
    /// `omit`/`repair_end` flags depend on which span is currently last, so
    /// they must be free to change retroactively when an edit removes the
    /// tail.
    pub fn model_messages(&self) -> Vec<ChatMessage> {
        self.project_view(false)
    }

    fn project_view(&self, omit_tail_assistant: bool) -> Vec<ChatMessage> {
        let spans = &self.view.spans;
        let mut messages = Vec::new();
        if let Some(digest) = &self.view.digest {
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
            .ledger
            .resident_events(span.events.start..end)
            .expect("view spans are resident in the ledger");
        if matches!(events.first(), Some(Protocol::ContextMessage { .. })) {
            return events
                .into_iter()
                .cloned()
                .flat_map(into_chat_messages)
                .collect();
        }

        let mut state = State::default();
        let mut messages = Vec::new();
        for event in events {
            if !admissible(&state, event) && stub_repairable(event) {
                append_aborted_stubs(&mut state, &mut messages);
            }
            messages.extend(into_chat_messages(event.clone()));
            state = advance(&state, event);
        }
        if repair_end && !matches!(state, State::ReadyForUser) {
            append_aborted_stubs(&mut state, &mut messages);
        }
        messages
    }
}

fn advance(state: &State, protocol: &Protocol) -> State {
    match protocol {
        Protocol::UserPrompt { .. } => State::AwaitingAssistantAfterUser,
        Protocol::AssistantMessage {
            pending_tool_ids, ..
        } if !pending_tool_ids.is_empty() => State::AwaitingToolResults {
            pending_ids: pending_tool_ids.clone(),
        },
        Protocol::AssistantMessage { .. } => State::ReadyForUser,
        Protocol::ToolResults { .. } => State::AwaitingAssistantAfterToolResults,
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::ContextMessage { .. }
        | Protocol::StepStarted { .. }
        | Protocol::ContextEdited { .. } => state.clone(),
    }
}

fn into_chat_messages(protocol: Protocol) -> Vec<ChatMessage> {
    match protocol {
        Protocol::UserPrompt { text, .. } => vec![ChatMessage::user(text)],
        Protocol::ContextMessage { message, .. } | Protocol::AssistantMessage { message, .. } => {
            vec![message]
        }
        Protocol::ToolResults { results } => results
            .into_iter()
            .map(|r| ChatMessage::from(ToolResponse::new(&r.id, &r.content)))
            .collect(),
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::StepStarted { .. }
        | Protocol::ContextEdited { .. } => Vec::new(),
    }
}

fn record_exchange(protocol: &Protocol) -> Option<u64> {
    match protocol {
        Protocol::UserPrompt { exchange, .. } | Protocol::ContextMessage { exchange, .. } => {
            Some(*exchange)
        }
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::StepStarted { .. }
        | Protocol::AssistantMessage { .. }
        | Protocol::ToolResults { .. }
        | Protocol::ContextEdited { .. } => None,
    }
}

/// Protocol sequencing legality — a runtime guard for data read off disk,
/// never consulted by [`Fold::step`] itself: live authorship is already
/// typestate-correct, so the only place this can fail is [`validate_resume`],
/// walking a file this process did not just write.
fn admissible(state: &State, protocol: &Protocol) -> bool {
    match protocol {
        Protocol::UserPrompt { .. } => matches!(
            state,
            State::ReadyForUser | State::AwaitingAssistantAfterToolResults,
        ),
        Protocol::ContextMessage { .. } => matches!(state, State::ReadyForUser),
        Protocol::AssistantMessage { message, .. } => {
            message.role == ChatRole::Assistant
                && matches!(
                    state,
                    State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults,
                )
        }
        Protocol::ToolResults { results } => {
            if let State::AwaitingToolResults { pending_ids } = state {
                validate_result_ids(pending_ids, results).is_ok()
            } else {
                false
            }
        }
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::StepStarted { .. }
        | Protocol::ContextEdited { .. } => true,
    }
}

fn stub_repairable(protocol: &Protocol) -> bool {
    matches!(
        protocol,
        Protocol::UserPrompt { .. } | Protocol::ContextMessage { .. }
    )
}

/// The stub records role alternation needs to drive `state` back to
/// [`State::ReadyForUser`] — the one remedy a crash-truncated tail gets on
/// resume, mirroring what a live `quiesce` would have recorded.
fn stubs_to_ready(state: &State, reason: QuiesceReason) -> Vec<Protocol> {
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
        let stub = Protocol::ToolResults { results };
        state = advance(&state, &stub);
        stubs.push(stub);
    }
    if !matches!(state, State::ReadyForUser) {
        stubs.push(Protocol::AssistantMessage {
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
        messages.extend(into_chat_messages(stub));
    }
}

/// One step of the fold: append to the ledger, advance the view, and free
/// whatever no surviving span needs any more — the law that no recorded
/// protocol record is ever truly discarded holds because `free` keeps the
/// evicted index's `Stamp`, never drops it.
fn step_protocol(memo: &mut Memo, stamp: Stamp, protocol: Protocol) {
    let index = memo.ledger.append(Recorded::new(stamp, protocol.clone()));
    memo.state = advance(&memo.state, &protocol);
    record_event_span(&mut memo.view, &mut memo.max_exchange, &protocol, index);
    let removed = if let Protocol::ContextEdited { op, .. } = &protocol {
        let removed = removed_event_ranges(&memo.view, op);
        apply_context_op(&mut memo.view, op);
        memo.newest_edit = Some(index);
        removed
    } else {
        Vec::new()
    };
    memo.ledger.evict(&removed);
    let in_view = memo
        .view
        .spans
        .iter()
        .any(|span| span.events.contains(&index));
    if memo.max_exchange > 0 && !in_view {
        memo.ledger.free(index);
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

/// The partition half of the step: an id-bearing record extends the span
/// with its id at the tail or opens a new one past the running maximum;
/// every other record extends whichever span last grew.
fn record_event_span(view: &mut View, max_exchange: &mut u64, protocol: &Protocol, index: usize) {
    if let Some(id) = record_exchange(protocol) {
        let extended = view
            .spans
            .iter_mut()
            .find(|span| span.id == id && span.events.end == index)
            .map(|span| span.events.end = index + 1)
            .is_some();
        if !extended && id > *max_exchange {
            view.spans.push(Span {
                id,
                events: index..index + 1,
            });
        }
        *max_exchange = (*max_exchange).max(id);
    } else if *max_exchange != 0 {
        let id = *max_exchange;
        if let Some(span) = view
            .spans
            .iter_mut()
            .find(|span| span.id == id && span.events.end == index)
        {
            span.events.end = index + 1;
        }
    }
}

/// Re-derive `(state, view, max_exchange)` from the ledger's own content
/// alone, resident or freed — the `fold == memo` check [`resume`] runs
/// against the incrementally maintained memo, so an eviction that silently
/// lost or misplaced a record is caught rather than trusted.
fn refold(ledger: &Ledger) -> io::Result<(State, View, u64)> {
    let mut state = State::default();
    let mut view = View::default();
    let mut max_exchange = 0u64;
    for (index, protocol) in ledger.fold_events()? {
        state = advance(&state, &protocol);
        record_event_span(&mut view, &mut max_exchange, &protocol, index);
        if let Protocol::ContextEdited { op, .. } = &protocol {
            apply_context_op(&mut view, op);
        }
    }
    Ok((state, view, max_exchange))
}

/// Replay the protocol sequencing law over a file before admitting it:
/// interposing the stub records a live `quiesce` would have written wherever
/// a record is inadmissible but repairable, and refusing outright wherever it
/// is not — foreign data no live door could have produced.
fn validate_resume(records: &[Recorded<Protocol>]) -> Result<(), Refusal> {
    let mut state = State::default();
    let mut view = View::default();
    let mut max_exchange = 0u64;
    for (index, recorded) in records.iter().enumerate() {
        let protocol = recorded.value();
        if !admissible(&state, protocol) {
            if !stub_repairable(protocol) {
                return Err(Refusal::Foreign {
                    record: Box::new(Record::Protocol(protocol.clone())),
                    reason: format!(
                        "record {} is foreign protocol data, not a seam that quiesce can repair; was record.jsonl hand-edited or written by an incompatible exarch?",
                        index + 1
                    ),
                });
            }
            for stub in stubs_to_ready(&state, QuiesceReason::Aborted) {
                state = advance(&state, &stub);
            }
        }
        state = advance(&state, protocol);
        if let Some(id) = record_exchange(protocol) {
            let joins = id > max_exchange
                || view
                    .spans
                    .iter()
                    .any(|span| span.id == id && span.events.end == index);
            if !joins {
                return Err(Refusal::Foreign {
                    record: Box::new(Record::Protocol(protocol.clone())),
                    reason: format!(
                        "record {} names exchange {id}, which the log had already moved past; no live session records a stale exchange id",
                        index + 1
                    ),
                });
            }
        }
        record_event_span(&mut view, &mut max_exchange, protocol, index);
        if let Protocol::ContextEdited { op, .. } = protocol {
            apply_context_op(&mut view, op);
        }
    }
    Ok(())
}

/// The bytes past the last complete JSONL line — a torn write from a session
/// that did not shut down cleanly.
struct CrashTail {
    bytes: Vec<u8>,
    complete_len: u64,
}

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:model-fold-crash-scan] reads record.jsonl whole to find a torn trailing write before resume folds it; output infra, not turn-time data I/O"
)]
fn find_crash_tail(path: &Path) -> io::Result<Option<CrashTail>> {
    let data = std::fs::read(path)?;
    let mut complete_len = 0usize;
    for fragment in data.split_inclusive(|byte| *byte == b'\n') {
        if fragment.last() != Some(&b'\n') {
            break;
        }
        complete_len += fragment.len();
    }
    Ok((complete_len < data.len()).then(|| CrashTail {
        bytes: data[complete_len..].to_vec(),
        complete_len: complete_len as u64,
    }))
}

/// Move a torn tail to `record.jsonl.crash` and trim the live file back to
/// its last complete line, exactly mirroring the retired `events.jsonl`
/// quarantine.
#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:model-fold-crash-quarantine] sidecars and trims a torn record.jsonl tail before resume folds it; output infra, not turn-time data I/O"
)]
fn quarantine_tail(path: &Path, tail: &CrashTail) -> io::Result<()> {
    let mut sidecar_name = path.file_name().unwrap_or_default().to_os_string();
    sidecar_name.push(".crash");
    let sidecar = path.with_file_name(sidecar_name);
    let mut quarantine = OpenOptions::new().create(true).append(true).open(&sidecar)?;
    quarantine.write_all(&tail.bytes)?;
    quarantine.flush()?;
    quarantine.sync_all()?;

    let file = OpenOptions::new().write(true).open(path)?;
    file.set_len(tail.complete_len)?;
    file.sync_all()?;
    eprintln!(
        "exarch: quarantined {} bytes from {} in {} before trimming the live log",
        tail.bytes.len(),
        path.display(),
        sidecar.display()
    );
    Ok(())
}

/// Fold `record.jsonl` into a fresh [`Memo`]: quarantine a torn tail,
/// validate the protocol subsequence's sequencing, fold it, and check the
/// fold against an independent from-scratch refold of the ledger it built —
/// the `fold == memo` law, migrated whole from `AgentLog::resume`.
///
/// # Errors
/// Returns an error if the file cannot be read, quarantined, or refolds to a
/// projection that disagrees with the one built incrementally.
pub fn resume(path: &Path) -> io::Result<Memo> {
    if let Some(tail) = find_crash_tail(path)? {
        quarantine_tail(path, &tail)?;
    }
    let records = Log::read(path)?;
    let mut protocol_records = Vec::new();
    for record in records {
        let record = record?;
        if let Record::Protocol(p) = record.value() {
            protocol_records.push(Recorded::new(record.stamp().clone(), p.clone()));
        }
    }
    validate_resume(&protocol_records).map_err(|refusal| io::Error::other(refusal.to_string()))?;

    let mut memo = Memo::default();
    memo.attach_source(path.to_path_buf());
    for recorded in protocol_records {
        step_protocol(&mut memo, recorded.stamp().clone(), recorded.into_value());
    }

    let (state, view, max_exchange) = refold(&memo.ledger)?;
    if (state, view, max_exchange) != (memo.state.clone(), memo.view.clone(), memo.max_exchange) {
        return Err(io::Error::other(
            "record.jsonl's from-scratch refold disagreed with the ledger's incremental projection",
        ));
    }
    Ok(memo)
}
