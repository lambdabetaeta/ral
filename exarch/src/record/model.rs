//! The model fold: the provider-facing projection `AgentLog`, built off the
//! [`Protocol`] records of `record.jsonl` — the same `step`, whether it is
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
    ContextOp, ContextSpanKind, ContextSurvey, ContextSurveyItem, EvictionPlan, GrepAnswer,
    GrepHit, QuiesceReason, StoreIndexItem, ToolResult, TranscriptMessage, TranscriptPart,
    TranscriptSpan, validate_result_ids,
};
use genai::chat::{
    Binary, BinarySource, ChatMessage, ChatRole, ContentPart, CustomPart, ToolCall, ToolResponse,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::ops::Range;
use std::path::{Path, PathBuf};
use std::sync::Arc;

/// Marker type implementing [`Fold`] for the model view; carries no state of
/// its own; [`Memo`] is where the projection lives.
pub struct Model;

impl Fold for Model {
    type Memo = Memo;

    fn step(memo: &mut Memo, record: &Recorded<Record>) -> Result<(), Refusal> {
        match record.value() {
            Record::Protocol(p) => {
                step_protocol(memo, record.stamp().clone(), p);
                Ok(())
            }
            Record::Display(_) | Record::Forensic(_) => Ok(()),
        }
    }
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

/// One exchange's (or imported context's) contiguous run of protocol records,
/// named by its exchange id.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Span {
    pub id: u64,
    pub events: Range<usize>,
}

/// The model's addressable window onto the protocol subsequence: the prefix
/// removals the head marker indexes, plus the closed and live spans still
/// readable by exchange id.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct View {
    /// Prefix removals, in the order they landed; the head marker renders
    /// these and nothing else.
    pub evictions: Vec<Eviction>,
    pub spans: Vec<Span>,
}

/// One [`ContextOp::Evict`] as the head marker reads it back.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Eviction {
    /// One row per span the op removed, ascending.
    pub rows: Vec<EvictedRow>,
    /// The model's own line to its future self, if it wrote one.
    pub note: Option<String>,
}

/// What the head marker remembers of one exchange that left the window: its
/// address, and the weight it carried while the model still held it.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct EvictedRow {
    pub exchange: u64,
    pub kind: ContextSpanKind,
    pub opening: String,
    pub steps: usize,
    pub bytes: usize,
}

/// The provider-facing view as a persistent value: one shared segment per
/// closed span (the head marker included), the live tail rendered fresh. Clone is
/// Arc bumps; the only routes back to owned messages are the wire door and
/// the mnemon child seed.
#[derive(Clone, Default)]
pub struct Transcript {
    segments: Vec<Arc<[ChatMessage]>>,
    bytes: usize,
}

impl Transcript {
    fn push(&mut self, segment: Arc<[ChatMessage]>, bytes: usize) {
        self.bytes += bytes;
        self.segments.push(segment);
    }

    pub fn messages(&self) -> impl Iterator<Item = &ChatMessage> {
        self.segments.iter().flat_map(|segment| segment.iter())
    }

    pub fn len(&self) -> usize {
        self.segments.iter().map(|segment| segment.len()).sum()
    }

    pub fn is_empty(&self) -> bool {
        self.segments.iter().all(|segment| segment.is_empty())
    }

    /// Serialised size, from per-segment measures cached at render.
    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn for_test(messages: Vec<ChatMessage>) -> Self {
        let bytes = message_bytes(&messages);
        Self {
            segments: vec![Arc::from(messages)],
            bytes,
        }
    }
}

/// A closed span's rendering, cached beside the span's own end index: a hit
/// requires `end` to match, so a still-growing span re-renders and staleness
/// is inexpressible.
struct SpanRender {
    end: usize,
    segment: Arc<[ChatMessage]>,
    /// Serialised bytes of `segment`, measured once.
    bytes: usize,
}

fn message_bytes(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
        .sum()
}

/// One resident protocol record, or the [`Stamp`] to read it back by once a
/// context edit evicts its span.
#[derive(Default)]
struct Ledger {
    len: usize,
    resident: BTreeMap<usize, Recorded<Protocol>>,
    freed: BTreeMap<usize, Stamp>,
    /// `record.jsonl`'s own path — the only thing a freed index needs to read
    /// itself back.
    ///
    /// Reading a completed record by byte range from the file this process is
    /// still appending to is safe: a [`Stamp`] exists only once the seam has
    /// written the whole record under its lock.
    source: PathBuf,
}

impl Ledger {
    fn append(&mut self, record: Recorded<Protocol>) -> usize {
        let index = self.len;
        self.len += 1;
        let _ = self.resident.insert(index, record);
        index
    }

    fn len(&self) -> usize {
        self.len
    }

    #[allow(
        dead_code,
        reason = "wired in once tui/headless are reborn as printers (P3/P6)"
    )]
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
            let _ = self.freed.insert(index, recorded.stamp().clone());
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
            let stamps: Vec<Stamp> = self.freed.values().cloned().collect();
            let read = read_records(&self.source, &stamps)?;
            events.extend(self.freed.keys().copied().zip(read));
            events.sort_unstable_by_key(|(index, _)| *index);
        }
        Ok(events)
    }
}

/// Read `stamps`' records back off `path`, in the order given — one file
/// opened per call, whoever is asking.
///
/// # Errors
/// Returns `Err` if the file cannot be opened, a range cannot be read, or a
/// stamp names anything but a protocol record.
fn read_records(path: &Path, stamps: &[Stamp]) -> io::Result<Vec<Protocol>> {
    let mut file = open_store(path)?;
    stamps
        .iter()
        .map(|stamp| read_stamped(&mut file, stamp))
        .collect()
}

/// One handle on a record log, carrying the model fold's io-door allow.
fn open_store(path: &Path) -> io::Result<File> {
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:model-fold-freed-read] reads record.jsonl back by Stamp for the fold's own freed slots and the model's `transcript` store door alike; surfaced as a Display::HarnessCall, not the model's own data I/O"
    )]
    File::open(path)
}

fn read_stamped(reader: &mut File, stamp: &Stamp) -> io::Result<Protocol> {
    let bytes = stamp.bytes();
    let length = usize::try_from(bytes.end.saturating_sub(bytes.start))
        .map_err(|_| io::Error::other("a freed record range is too large to read"))?;
    let _ = reader.seek(SeekFrom::Start(bytes.start))?;
    let mut buf = vec![0; length];
    reader.read_exact(&mut buf)?;
    let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
    // Before the parse, not after: a stale range can hold a well-formed
    // record, and refusing it as bad JSON would name the wrong fault.
    if Stamp::digest_of(line) != stamp.digest() {
        return Err(io::Error::other(
            "a record did not hash to the stamp that named it — the log at this \
             path is no longer the file those byte ranges were measured in; a \
             rotated segment, a copied session directory, or an edited log",
        ));
    }
    let entry: super::Entry = serde_json::from_slice(line).map_err(io::Error::other)?;
    match entry.record {
        Record::Protocol(p) => Ok(p),
        Record::Display(_) | Record::Forensic(_) => Err(io::Error::other(
            "a freed model-fold slot pointed at a non-protocol record — the ledger's own invariant broke",
        )),
    }
}

/// The model fold's memo: the running protocol-view projection, per-record
/// residency, the ledger those two are built from, and the render cache
/// under the recompute invariant — a memo of a pure function at immutable
/// arguments, never serialised, rebuilt from nothing by this same fold on
/// resume.
#[derive(Default)]
pub struct Memo {
    ledger: Ledger,
    state: State,
    view: View,
    max_exchange: u64,
    newest_edit: Option<usize>,
    /// Closed-span renderings, keyed by span id — a span id never recurs
    /// (`record_event_span` only opens a span strictly past the running
    /// maximum), so `(id, end)` determines a rendering globally and forever.
    render: HashMap<u64, SpanRender>,
    /// The head marker, recomputed whenever [`View::evictions`] changes and
    /// present exactly when that list is non-empty.
    head_render: Option<(Arc<[ChatMessage]>, usize)>,
    /// Every span that has left the view, by eviction or by drop: where its
    /// records lie in the ledger, and the row it was weighed at while the
    /// model still held it. Memo-only, never serialised, and never part of
    /// the `fold == memo` check.
    departed: BTreeMap<u64, Departed>,
    /// The parent this log's opening context came from, when it is a
    /// `mnemon` child; [`Protocol::Inherited`] is the one thing that sets it.
    ancestry: Option<Ancestry>,
    /// The lineage's own store, walked once on the first read that reaches
    /// past this log's ledger. Memo-only, never serialised.
    ancestry_index: Option<AncestorIndex>,
}

/// [`Memo::departed`]'s value.
struct Departed {
    events: Range<usize>,
    row: EvictedRow,
}

/// One link of a lineage: whose file the ids below it read back from, and
/// how far down they reach.
#[derive(Clone)]
struct Ancestry {
    /// That ancestor's `record.jsonl`.
    source: PathBuf,
    /// Ids at or below this belong to the ancestry; above it they are this
    /// log's own.
    through_exchange: u64,
}

/// The lineage's closed exchanges, by id — and, when the walk stopped short
/// of the lineage's root, why, so an id it could not reach is refused with
/// the reason rather than called unrecorded.
#[derive(Default)]
struct AncestorIndex {
    spans: BTreeMap<u64, AncestorSpan>,
    broken: Option<Break>,
}

/// One ancestor's exchange: where its records lie, and the row the store
/// index weighs it by. `source` is per-span because a lineage of three
/// spreads its ids over three files.
struct AncestorSpan {
    source: PathBuf,
    stamps: Vec<Stamp>,
    row: EvictedRow,
}

/// Why a lineage walk stopped before the ancestry's root: an ancestor's
/// `record.jsonl` could not be read. `/clear` cancels every descendant before
/// it rotates, so no live child holds a link to a rotated file: the reachable
/// cause is a hand-deleted directory.
struct Break {
    source: PathBuf,
    error: String,
}

impl Break {
    fn refusal(&self, exchange: u64) -> String {
        format!(
            "exchange {exchange} was recorded by an ancestor, but the ancestor's log at {} is gone: {}",
            self.source.display(),
            self.error
        )
    }
}

/// Where one closed exchange's records lie. Spans and [`Stamp`]s rather than
/// the records themselves: resolution is a `&mut self` walk, and a borrow of
/// the ledger taken here would outlive it.
enum Located {
    Resident(Span),
    Freed(Vec<Stamp>),
    Ancestor { source: PathBuf, stamps: Vec<Stamp> },
}

/// One closed exchange's material, or where to read it back from.
enum Sourced {
    /// The render cache's own segment — already in memory.
    Resident(Arc<[ChatMessage]>),
    Stamped {
        source: PathBuf,
        stamps: Vec<Stamp>,
    },
}

/// Where every exchange a store read names lies, owned outright: the read
/// borrows nothing from the memo, so the desk can drop the session lock
/// before it touches a file.
pub(crate) struct StoreRead {
    spans: Vec<(u64, Sourced)>,
}

impl StoreRead {
    /// The named exchanges as material, one [`TranscriptSpan`] each, narrowed
    /// to [`TranscriptPart`]s rather than the provider's own content parts.
    ///
    /// # Errors
    /// Refuses an exchange whose file will not read back.
    pub(crate) fn spans(self) -> Result<Vec<TranscriptSpan>, String> {
        let mut spans = Vec::with_capacity(self.spans.len());
        self.walk(read_back_refusal, |exchange, messages| {
            spans.push(TranscriptSpan {
                exchange,
                messages: messages.iter().map(transcript_message).collect(),
            });
        })?;
        Ok(spans)
    }

    /// One hit per matching line, oldest first and bounded by [`GREP_HITS`].
    ///
    /// # Errors
    /// Refuses a file that will not read back.
    pub(crate) fn grep(self, pattern: &Regex) -> Result<GrepAnswer, String> {
        let mut tally = Tally::default();
        self.walk(
            |_, path, error| {
                format!(
                    "the exchanges recorded in {} could not be read back: {error}",
                    path.display()
                )
            },
            |exchange, messages| grep_messages(pattern, exchange, messages, &mut tally),
        )?;
        Ok(tally.answer())
    }

    /// Hand every named exchange's messages to `visit`, in order, holding at
    /// most one open file: consecutive entries naming one store share it, so
    /// each file is read in a single pass in stamp order, one exchange decoded
    /// at a time.
    fn walk(
        self,
        refusal: impl Fn(u64, &Path, &io::Error) -> String,
        mut visit: impl FnMut(u64, &[ChatMessage]),
    ) -> Result<(), String> {
        let mut open: Option<(PathBuf, File)> = None;
        for (exchange, sourced) in self.spans {
            match sourced {
                Sourced::Resident(segment) => visit(exchange, &segment),
                Sourced::Stamped { source, stamps } => {
                    if open.as_ref().is_none_or(|(path, _)| *path != source) {
                        // Closed before the next is opened, so a lineage of
                        // many files costs one descriptor, never two.
                        drop(open.take());
                        let file = open_store(&source)
                            .map_err(|error| refusal(exchange, &source, &error))?;
                        open = Some((source, file));
                    }
                    let (path, file) = open.as_mut().expect("just opened, or already open");
                    let records = stamps
                        .iter()
                        .map(|stamp| read_stamped(file, stamp))
                        .collect::<io::Result<Vec<_>>>()
                        .map_err(|error| refusal(exchange, path, &error))?;
                    let span: Vec<&Protocol> = records.iter().collect();
                    visit(exchange, &closed_messages(&span));
                }
            }
        }
        Ok(())
    }
}

impl Memo {
    /// The one constructor: `source` is `record.jsonl`'s path, fixed for this
    /// memo's life.
    #[must_use]
    pub fn new(source: PathBuf) -> Self {
        Self {
            ledger: Ledger {
                source,
                ..Ledger::default()
            },
            ..Self::default()
        }
    }

    /// Whether a record that opens a span — a fresh [`Protocol::UserPrompt`],
    /// an imported [`Protocol::ContextMessage`] — is admissible here. Weaker
    /// than [`State::ReadyForUser`]: an exchange that never reached a reply is
    /// abandoned by the next prompt rather than closed by a fabricated one, so
    /// only outstanding tool calls hold the log.
    pub fn is_ready(&self) -> bool {
        admits_new_span(&self.state)
    }

    /// An edit may land at any rest but a batch in flight: outstanding tool
    /// calls name the assistant frame their results answer, so nothing may
    /// come between. What keeps a cut off the *live exchange* is
    /// [`Self::plan_eviction`]'s own shape, not this.
    pub fn can_evict(&self) -> bool {
        self.is_ready()
    }

    pub fn current_exchange(&self) -> Option<u64> {
        (self.max_exchange != 0).then_some(self.max_exchange)
    }

    /// The id the next exchange this log opens is minted above. Ids are
    /// lineage-monotone, so a fork that inherited an empty view — a rewind
    /// at a ready boundary can leave one — still starts past its ancestry
    /// rather than back at 1.
    pub(crate) fn exchange_floor(&self) -> u64 {
        self.max_exchange.max(
            self.ancestry
                .as_ref()
                .map_or(0, |ancestry| ancestry.through_exchange),
        )
    }

    pub fn last_view_exchange(&self) -> Option<u64> {
        self.view.spans.last().map(|span| span.id)
    }

    pub fn log_len(&self) -> usize {
        self.ledger.len()
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.newest_edit.is_some_and(|index| index >= measured_at)
    }

    /// The exchange still owed a reply — one in flight, or one abandoned
    /// without ever getting there. Keyed to [`State::ReadyForUser`] rather
    /// than [`Self::is_ready`], so a context edit can never land on the
    /// exchange a deliberation is still driving.
    pub fn is_live_exchange(&self, id: u64) -> bool {
        !matches!(self.state, State::ReadyForUser) && self.max_exchange == id
    }

    pub(crate) fn is_awaiting_assistant(&self) -> bool {
        matches!(
            self.state,
            State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults
        )
    }

    pub(crate) fn is_awaiting_steering(&self) -> bool {
        matches!(self.state, State::AwaitingAssistantAfterToolResults)
    }

    pub(crate) fn pending_tool_results(&self) -> Option<Vec<String>> {
        match &self.state {
            State::AwaitingToolResults { pending_ids } => Some(pending_ids.clone()),
            State::ReadyForUser
            | State::AwaitingAssistantAfterUser
            | State::AwaitingAssistantAfterToolResults => None,
        }
    }

    /// A human-readable stand-in for `{:?}` on the private [`State`] — used
    /// only in refusal messages, never matched on.
    pub(crate) fn state_description(&self) -> String {
        match &self.state {
            State::ReadyForUser => "ReadyForUser".to_string(),
            State::AwaitingAssistantAfterUser => "AwaitingAssistantAfterUser".to_string(),
            State::AwaitingToolResults { pending_ids } => {
                format!("AwaitingToolResults {{ pending_ids: {pending_ids:?} }}")
            }
            State::AwaitingAssistantAfterToolResults => {
                "AwaitingAssistantAfterToolResults".to_string()
            }
        }
    }

    /// The records a [`QuiesceReason`] quiesce still owes, for the caller to
    /// record through the seam itself — this fold only knows how to compute
    /// them, never to author them.
    pub(crate) fn quiesce_records(&self, reason: QuiesceReason) -> Vec<Protocol> {
        quiesce_records(&self.state, reason)
    }

    /// The parent context a `mnemon` child inherits, span by span under this
    /// log's own exchange ids: the one other place ownership genuinely
    /// transfers, beside the wire door. The head marker is not among them —
    /// the child re-renders it from the evictions the link carries.
    ///
    /// The last span seeds through [`Self::seed_tail`], cut to the longest
    /// run that owes no tool result — a batch in flight and a dangling edit
    /// left after it both fall out of that same rule.
    pub(crate) fn inherited_context(&mut self) -> Vec<(u64, Vec<ChatMessage>)> {
        let spans = self.view.spans.clone();
        let Some((last, closed)) = spans.split_last() else {
            return Vec::new();
        };
        let mut inherited: Vec<(u64, Vec<ChatMessage>)> = closed
            .iter()
            .map(|span| (span.id, self.render_closed_entry(span).segment.to_vec()))
            .collect();
        inherited.push((last.id, self.seed_tail(last)));
        inherited
    }

    /// The provider-facing transcript — recomputed from the protocol
    /// subsequence on every call, never accumulator state: closed spans hit
    /// the memo, and the tail is rendered fresh because its `omit`/
    /// `repair_end` flags depend on which span is currently last and must
    /// stay free to change retroactively when an edit removes the tail.
    pub fn transcript(&mut self) -> Transcript {
        self.assemble()
    }

    fn assemble(&mut self) -> Transcript {
        let mut transcript = Transcript::default();
        if let Some((segment, bytes)) = &self.head_render {
            transcript.push(Arc::clone(segment), *bytes);
        }
        let spans = self.view.spans.clone();
        if let Some((last, closed)) = spans.split_last() {
            for span in closed {
                let entry = self.render_closed_entry(span);
                transcript.push(Arc::clone(&entry.segment), entry.bytes);
            }
            let tail = self.render_tail(last);
            let bytes = message_bytes(&tail);
            transcript.push(Arc::from(tail), bytes);
        }
        transcript
    }

    /// A closed span's rendering, cached beside its own end index. No flags
    /// parameter exists, so nothing can vary one: the memo's key is the
    /// whole of this function's input.
    fn render_closed_entry(&mut self, span: &Span) -> &SpanRender {
        let stale = self
            .render
            .get(&span.id)
            .is_none_or(|cached| cached.end != span.events.end);
        if stale {
            let messages = closed_messages(&self.span_events(span));
            let bytes = message_bytes(&messages);
            let _ = self.render.insert(
                span.id,
                SpanRender {
                    end: span.events.end,
                    segment: Arc::from(messages),
                    bytes,
                },
            );
        }
        self.render.get(&span.id).expect("just ensured present")
    }

    /// Re-render the head marker from the evictions the view now carries —
    /// the one place [`Self::head_render`] is written, so it cannot drift
    /// from them.
    fn recompute_head(&mut self) {
        self.head_render = (!self.view.evictions.is_empty()).then(|| {
            let message = ChatMessage::user(render_head(&self.view.evictions));
            let bytes = message_bytes(std::slice::from_ref(&message));
            (Arc::from(vec![message]), bytes)
        });
    }

    /// # Panics
    /// Panics if a view span is not resident in the ledger.
    fn span_events(&self, span: &Span) -> Vec<&Protocol> {
        self.ledger
            .resident_events(span.events.clone())
            .expect("view spans are resident in the ledger")
    }

    /// The last span only, never cached and never dropped: the tail is the
    /// exchange in flight, so it renders whatever state it rests in.
    fn render_tail(&self, span: &Span) -> Vec<ChatMessage> {
        self.span_events(span)
            .into_iter()
            .cloned()
            .flat_map(into_chat_messages)
            .collect()
    }

    /// The tail as a `mnemon` child receives it, cut to what admits the
    /// child's own first prompt.
    fn seed_tail(&self, span: &Span) -> Vec<ChatMessage> {
        admissible_prefix(&self.span_events(span))
            .iter()
            .map(|event| (*event).clone())
            .flat_map(into_chat_messages)
            .collect()
    }

    /// The window every read query of the model view answers over — private
    /// to the fold; `AgentLog` reads the shape it needs through the queries
    /// below, never this reference directly.
    pub fn view(&self) -> &View {
        &self.view
    }

    /// The last exchange an eviction has taken out of the view. Evictions are
    /// prefix ops, so every id at or below it is gone for good.
    fn eviction_reach(&self) -> Option<u64> {
        self.view
            .evictions
            .last()
            .and_then(|eviction| eviction.rows.last())
            .map(|row| row.exchange)
    }

    /// Slots still owned by the model view: the append-only ledger retains
    /// what an edit removes, so this is the host's resource probe's own
    /// count, not [`Self::log_len`].
    pub(crate) fn event_count(&self) -> usize {
        let events = self
            .view
            .spans
            .iter()
            .map(|span| span.events.len())
            .sum::<usize>();
        events + usize::from(self.head_render.is_some())
    }

    /// Approximate context size in serialised model-view bytes — the
    /// fallback eviction trigger when the model's context window is
    /// unknown. Closed spans read the memo's cached bytes; only the live
    /// tail is re-serialised.
    pub(crate) fn history_bytes(&mut self) -> usize {
        self.transcript().byte_len()
    }

    /// A span's `bytes` is measured exactly as [`Self::assemble`] measures
    /// it — closed entries from the memo, the last span rendered fresh — so
    /// `total_bytes` is [`Self::history_bytes`] and not a second opinion on
    /// it. `live` reports which span the model is in; it does not decide how
    /// one is weighed.
    ///
    /// # Panics
    /// Panics if a view span is not resident in the ledger.
    pub(crate) fn context_survey(&mut self) -> ContextSurvey {
        // The head marker is in the window but on no row, so it weighs on
        // the total alone: `total_bytes` stays `history_bytes`.
        let mut survey = ContextSurvey {
            evicted: self
                .view
                .evictions
                .iter()
                .map(|eviction| eviction.rows.len())
                .sum(),
            total_bytes: self.head_render.as_ref().map_or(0, |(_, bytes)| *bytes),
            ..ContextSurvey::default()
        };
        let spans = self.view.spans.clone();
        let last = spans.len().saturating_sub(1);
        for (index, span) in spans.iter().enumerate() {
            // The last span is weighed as it lies, never cached as closed.
            let row = if index == last {
                let bytes = message_bytes(&self.render_tail(span));
                self.weighed_row(span, bytes)
            } else {
                self.span_row(span)
            };
            let item = ContextSurveyItem {
                exchange: row.exchange,
                kind: row.kind,
                opening: row.opening,
                bytes: row.bytes,
                steps: row.steps,
                live: self.is_live_exchange(span.id),
            };
            survey.add(item);
        }
        survey
    }

    /// One in-view span weighed as a closed one: what the survey reports, and
    /// what an eviction remembers of the span once its records are freed.
    fn span_row(&mut self, span: &Span) -> EvictedRow {
        let bytes = self.render_closed_entry(span).bytes;
        self.weighed_row(span, bytes)
    }

    fn weighed_row(&self, span: &Span, bytes: usize) -> EvictedRow {
        evicted_row(span.id, &self.span_events(span), bytes)
    }

    /// Read named, closed exchanges in store order — one [`TranscriptSpan`]
    /// each, addressed by its own `exchange` field rather than by argument
    /// position. Every arm of [`Self::sourced`] renders through
    /// [`closed_messages`], so what comes back is what the model was sent,
    /// whether the exchange is still in view, evicted to this log's own file,
    /// or an ancestor's. All are narrowed to [`TranscriptPart`]s rather than
    /// the provider's own content parts.
    ///
    /// Both halves in one call — [`Self::locate_read`] then
    /// [`StoreRead::spans`]; the desk keeps them apart so only the first runs
    /// under the session lock.
    ///
    /// # Errors
    /// Refuses an empty list, a duplicate name, an exchange this lineage never
    /// recorded, the exchange still in progress, or one behind a broken link.
    pub(crate) fn read_context(
        &mut self,
        exchanges: &[u64],
    ) -> Result<Vec<TranscriptSpan>, String> {
        self.locate_read(exchanges)?.spans()
    }

    /// [`Self::read_context`]'s first half: resolve every named exchange under
    /// the caller's lock, borrowing nothing, so the read itself can run once
    /// that lock is gone.
    ///
    /// # Errors
    /// Refuses whatever [`Self::read_context`] refuses.
    pub(crate) fn locate_read(&mut self, exchanges: &[u64]) -> Result<StoreRead, String> {
        self.validate_read(exchanges)?;
        let mut named: Vec<u64> = exchanges.to_vec();
        named.sort_unstable();
        let mut spans = Vec::with_capacity(named.len());
        for exchange in named {
            let sourced = self.sourced(exchange)?;
            spans.push((exchange, sourced));
        }
        Ok(StoreRead { spans })
    }

    /// One closed exchange's material, or the file and stamps to read it back
    /// by — wherever the lineage keeps it.
    fn sourced(&mut self, exchange: u64) -> Result<Sourced, String> {
        Ok(match self.resolve(exchange)? {
            Located::Resident(span) => {
                Sourced::Resident(Arc::clone(&self.render_closed_entry(&span).segment))
            }
            Located::Freed(stamps) => Sourced::Stamped {
                source: self.ledger.source.clone(),
                stamps,
            },
            Located::Ancestor { source, stamps } => Sourced::Stamped { source, stamps },
        })
    }

    /// Where one closed exchange's records lie: own resident, own freed, then
    /// the ancestry — which is consulted only for an id at or below the
    /// fork's reach, every id above it being this log's own.
    ///
    /// # Errors
    /// Refuses an id no lineage record answers for, and one behind a broken
    /// link.
    fn resolve(&mut self, exchange: u64) -> Result<Located, String> {
        if let Some(span) = self.view.spans.iter().find(|span| span.id == exchange) {
            return Ok(Located::Resident(span.clone()));
        }
        if self.departed.contains_key(&exchange) {
            return Ok(Located::Freed(self.departed_stamps(exchange)?));
        }
        let unreached = never_recorded_refusal(exchange, self.last_closed_exchange());
        if self
            .ancestry
            .as_ref()
            .is_none_or(|ancestry| exchange > ancestry.through_exchange)
        {
            return Err(unreached);
        }
        let index = self.ancestor_index();
        match index.spans.get(&exchange) {
            Some(span) => Ok(Located::Ancestor {
                source: span.source.clone(),
                stamps: span.stamps.clone(),
            }),
            None => Err(index
                .broken
                .as_ref()
                .map_or(unreached, |broken| broken.refusal(exchange))),
        }
    }

    /// The lineage's store, re-walked whenever no cached index exists or the
    /// one cached broke: O(ancestor file) then, and free otherwise.
    fn ancestor_index(&mut self) -> &AncestorIndex {
        // A broken walk is never remembered: an unreadable file may be a
        // transient.
        let stale = self
            .ancestry_index
            .as_ref()
            .is_none_or(|index| index.broken.is_some());
        if stale {
            let ancestry = self.ancestry.clone();
            let index = ancestry
                .as_ref()
                .map_or_else(AncestorIndex::default, index_ancestry);
            self.ancestry_index = Some(index);
        }
        self.ancestry_index.as_ref().expect("just ensured present")
    }

    /// One departed exchange's records, by the [`Stamp`]s they were freed
    /// under, in log order.
    fn departed_stamps(&self, exchange: u64) -> Result<Vec<Stamp>, String> {
        let departed = self
            .departed
            .get(&exchange)
            .expect("validated as departed before it is read");
        departed
            .events
            .clone()
            .map(|index| {
                self.ledger.freed.get(&index).cloned().ok_or_else(|| {
                    format!(
                        "exchange {exchange} names record slot {index}, which the ledger has neither resident nor freed — the fold's own invariant broke"
                    )
                })
            })
            .collect()
    }

    /// Every closed exchange the store holds, oldest first: the departed ones
    /// at the weight they were carrying when they left, the rest as the
    /// survey weighs them, and the ancestry's own beneath both. The live
    /// exchange is in no store.
    ///
    /// An id this log recorded itself is listed once, by its own row: an
    /// inherited exchange is an import here, not an ancestor's, whether it is
    /// still in view or long dropped.
    pub(crate) fn store_index(&mut self) -> Vec<StoreIndexItem> {
        let mut items: Vec<StoreIndexItem> = self
            .departed
            .values()
            .map(|departed| store_index_item(&departed.row, false))
            .collect();
        for span in self.view.spans.clone() {
            if !self.is_live_exchange(span.id) {
                let row = self.span_row(&span);
                items.push(store_index_item(&row, true));
            }
        }
        let own: HashSet<u64> = items.iter().map(|item| item.exchange).collect();
        items.extend(
            self.ancestor_index()
                .spans
                .values()
                .filter(|span| !own.contains(&span.row.exchange))
                .map(|span| store_index_item(&span.row, false)),
        );
        items.sort_unstable_by_key(|item| item.exchange);
        items
    }

    /// Search the closed exchanges' text: the named ones, or the whole store.
    /// Resident spans are searched off the render cache, then the departed
    /// ones off `record.jsonl`, then the ancestry's own — each file in one
    /// pass in stamp order.
    ///
    /// An unreadable exchange is refused when it was named and passed over
    /// when the whole store was: the head marker has already told the model
    /// which of its exchanges it cannot have back.
    ///
    /// Both halves in one call — [`Self::locate_grep`] then
    /// [`StoreRead::grep`]; the desk keeps them apart so only the first runs
    /// under the session lock.
    ///
    /// # Errors
    /// Refuses a named list [`Self::validate_read`] refuses, and any named
    /// exchange [`Self::resolve`] cannot reach.
    pub(crate) fn grep_store(
        &mut self,
        pattern: &Regex,
        exchanges: Option<&[u64]>,
    ) -> Result<GrepAnswer, String> {
        self.locate_grep(exchanges)?.grep(pattern)
    }

    /// [`Self::grep_store`]'s first half: where every searchable exchange
    /// lies, resolved under the caller's lock and borrowing nothing.
    ///
    /// # Errors
    /// Refuses whatever [`Self::grep_store`] refuses.
    pub(crate) fn locate_grep(&mut self, exchanges: Option<&[u64]>) -> Result<StoreRead, String> {
        if let Some(named) = exchanges {
            self.validate_read(named)?;
            // Every name is resolved before any is searched: a hit list is no
            // answer to a name the store cannot reach.
            for &exchange in named {
                let _ = self.resolve(exchange)?;
            }
        }
        let wanted = |id: u64| exchanges.is_none_or(|named| named.contains(&id));
        let mut spans: Vec<(u64, Sourced)> = Vec::new();

        for span in self.view.spans.clone() {
            if wanted(span.id) && !self.is_live_exchange(span.id) {
                let segment = Arc::clone(&self.render_closed_entry(&span).segment);
                spans.push((span.id, Sourced::Resident(segment)));
            }
        }

        for exchange in self.departed.keys().copied().filter(|id| wanted(*id)) {
            spans.push((
                exchange,
                Sourced::Stamped {
                    source: self.ledger.source.clone(),
                    stamps: self.departed_stamps(exchange)?,
                },
            ));
        }

        // Never an ancestor's copy of an id this log's own ledger just
        // answered for.
        let own: HashSet<u64> = self
            .view
            .spans
            .iter()
            .map(|span| span.id)
            .chain(self.departed.keys().copied())
            .collect();
        for (exchange, span) in &self.ancestor_index().spans {
            if own.contains(exchange) || !wanted(*exchange) {
                continue;
            }
            spans.push((
                *exchange,
                Sourced::Stamped {
                    source: span.source.clone(),
                    stamps: span.stamps.clone(),
                },
            ));
        }
        Ok(StoreRead { spans })
    }

    /// Every named exchange must be closed, named once, and one this lineage
    /// recorded — its own, in view or departed, or an ancestor's.
    fn validate_read(&self, exchanges: &[u64]) -> Result<(), String> {
        if exchanges.is_empty() {
            return Err("transcript must name at least one exchange".into());
        }
        let reach = self
            .ancestry
            .as_ref()
            .map_or(0, |ancestry| ancestry.through_exchange);
        let mut named = HashSet::with_capacity(exchanges.len());
        for &exchange in exchanges {
            if !named.insert(exchange) {
                return Err(format!("exchange {exchange} was named more than once"));
            }
            if self.is_live_exchange(exchange) {
                return Err(live_exchange_refusal(exchange));
            }
            if exchange > reach
                && !self.departed.contains_key(&exchange)
                && !self.view.spans.iter().any(|span| span.id == exchange)
            {
                return Err(never_recorded_refusal(
                    exchange,
                    self.last_closed_exchange(),
                ));
            }
        }
        Ok(())
    }

    /// The newest exchange the store holds whole: in view or departed, and
    /// never the one still in flight.
    fn last_closed_exchange(&self) -> Option<u64> {
        self.view
            .spans
            .iter()
            .map(|span| span.id)
            .chain(self.departed.keys().copied())
            .filter(|id| !self.is_live_exchange(*id))
            .max()
    }

    /// Resolve a user rewind into the whole visible suffix beginning at its
    /// anchor. The anchor is checked before the suffix is derived.
    ///
    /// # Errors
    /// Refuses an absent anchor or one that has already left the view.
    pub(crate) fn rewind_exchanges(&self, anchor: u64) -> Result<Vec<u64>, String> {
        if !self.view.spans.iter().any(|span| span.id == anchor) {
            if self.eviction_reach().is_some_and(|reach| anchor <= reach) {
                return Err(self.evicted_exchange_refusal(anchor));
            }
            return Err(rewind_unknown_exchange_refusal(
                anchor,
                self.last_view_exchange(),
            ));
        }
        Ok(self
            .view
            .spans
            .iter()
            .filter(|span| span.id >= anchor)
            .map(|span| span.id)
            .collect())
    }

    /// # Errors
    /// Refuses an unnamed edit, an unaddressable target, or a live exchange.
    pub(crate) fn validate_edit(&self, op: &ContextOp) -> Result<(), String> {
        match op {
            ContextOp::Evict {
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

    /// An exchange is addressable iff a span for it is still in the view, and
    /// closed iff it is not the live one. An eviction is a prefix op, so an
    /// id at or below the latest one's reach is told it is gone rather than
    /// told it was never there.
    fn validate_closed_present(&self, exchange: u64) -> Result<(), String> {
        if !self.view.spans.iter().any(|span| span.id == exchange) {
            if self.eviction_reach().is_some_and(|reach| exchange <= reach) {
                return Err(self.evicted_exchange_refusal(exchange));
            }
            return Err(unknown_exchange_refusal(exchange));
        }
        if self.is_live_exchange(exchange) {
            return Err(live_exchange_refusal(exchange));
        }
        Ok(())
    }

    fn evicted_exchange_refusal(&self, exchange: u64) -> String {
        match self.view.spans.first() {
            Some(first) => format!(
                "exchange {exchange} has already left your context — the earliest still in view is {}",
                first.id
            ),
            None => format!(
                "exchange {exchange} has already left your context — no exchange is still in view"
            ),
        }
    }

    /// A plan never names the newest span in view: an eviction exists to keep
    /// the task in hand, and the newest span is also the only one that can be
    /// live, so a cut `validate_edit` would refuse is unrepresentable. Every
    /// span the walk below weighs is therefore strictly older than the
    /// newest, so none of them can still be growing.
    pub(crate) fn plan_eviction(
        &mut self,
        keep_budget_bytes: usize,
        before_exchange: Option<u64>,
    ) -> Option<EvictionPlan> {
        let spans = self.view.spans.clone();
        let (_, older) = spans.split_last()?;
        // A continued exchange and everything after it stay, as does the newest.
        let cap = before_exchange.map_or(older.len(), |id| {
            older
                .iter()
                .position(|span| span.id >= id)
                .unwrap_or(older.len())
        });
        let mut spent = 0usize;
        for span in &spans[cap..] {
            spent = spent.saturating_add(self.render_closed_entry(span).bytes);
        }
        let mut start = cap;
        for index in (0..cap).rev() {
            let bytes = self.render_closed_entry(&spans[index]).bytes;
            let Some(total) = spent.checked_add(bytes) else {
                break;
            };
            if total > keep_budget_bytes {
                break;
            }
            spent = total;
            start = index;
        }
        (start > 0).then(|| EvictionPlan {
            through_exchange: spans[start - 1].id,
        })
    }
}

fn opening_line(text: &str) -> String {
    text.lines().next().unwrap_or_default().to_string()
}

/// A span is an import iff its first record is one; nothing else opens one.
fn span_kind(events: &[&Protocol]) -> ContextSpanKind {
    match events.first() {
        Some(Protocol::ContextMessage { .. }) => ContextSpanKind::Import,
        _ => ContextSpanKind::Exchange,
    }
}

fn step_count(events: &[&Protocol]) -> usize {
    events
        .iter()
        .filter(|event| matches!(event, Protocol::StepStarted { .. }))
        .count()
}

/// A closed span's messages — or, when its own fold never settles, the one
/// note standing for the exchange it abandoned. The single rendering the
/// model view, an eviction's weight, and a read-back all go through, so a
/// record read off disk answers byte-for-byte what the model was sent.
fn closed_messages(events: &[&Protocol]) -> Vec<ChatMessage> {
    let settles = events
        .iter()
        .fold(State::default(), |state, event| advance(&state, event));
    if !matches!(settles, State::ReadyForUser) {
        return vec![ChatMessage::user(abandoned_note(events))];
    }
    events
        .iter()
        .map(|event| (*event).clone())
        .flat_map(into_chat_messages)
        .collect()
}

/// Walk a lineage from one link, indexing every closed exchange it can
/// reach: one pass per ancestor file, each stopping at that link's own
/// `through_exchange`, and following the ancestor's own [`Protocol::Inherited`]
/// on to the grandparent. The nearest copy of an id wins, an ancestor's
/// imported spans being the same exchanges under the same numbers.
fn index_ancestry(ancestry: &Ancestry) -> AncestorIndex {
    let mut index = AncestorIndex::default();
    let mut link = Some(ancestry.clone());
    while let Some(Ancestry {
        source,
        through_exchange,
    }) = link
    {
        match index_ancestor(&source, through_exchange, &mut index.spans) {
            Ok(next) => link = next,
            Err(error) => {
                index.broken = Some(Break {
                    source,
                    error: error.to_string(),
                });
                return index;
            }
        }
    }
    index
}

/// One ancestor file's own exchanges through `through_exchange`, partitioned
/// by the same rule the live fold partitions by on a view that applies no
/// edits: the store is every exchange the file recorded, whatever that
/// session later shed from its window. Answers the link to its own parent,
/// when the walk met one.
fn index_ancestor(
    path: &Path,
    through_exchange: u64,
    spans: &mut BTreeMap<u64, AncestorSpan>,
) -> io::Result<Option<Ancestry>> {
    let mut view = View::default();
    let mut max_exchange = 0u64;
    // Only the last span pushed can still grow, so the records from its start
    // are all a row ever needs.
    let mut buffer: Vec<(Protocol, Stamp)> = Vec::new();
    let mut buffer_start = 0usize;
    let mut index = 0usize;
    let mut link = None;
    for record in Log::read(path)? {
        let record = record?;
        let Record::Protocol(protocol) = record.value() else {
            continue;
        };
        // Everything at or below the link's reach was recorded before the
        // fork; the first record past it ends the pass.
        if record_exchange(protocol).is_some_and(|id| id > through_exchange) {
            break;
        }
        if let Protocol::Inherited {
            source,
            through_exchange,
            ..
        } = protocol
        {
            link = Some(Ancestry {
                source: source.clone(),
                through_exchange: *through_exchange,
            });
        }
        let open = view.spans.len();
        record_event_span(&mut view, &mut max_exchange, protocol, index);
        if view.spans.len() > open {
            // A span begins at `index`; the one before it can no longer grow.
            if let Some(closed) = open.checked_sub(1) {
                index_span(spans, path, &view.spans[closed], &buffer, buffer_start);
            }
            buffer.clear();
            buffer_start = index;
        }
        buffer.push((protocol.clone(), record.stamp().clone()));
        index += 1;
    }
    if let Some(last) = view.spans.last() {
        index_span(spans, path, last, &buffer, buffer_start);
    }
    Ok(link)
}

/// One ancestor span's row and stamps, sliced out of the records buffered
/// from `buffer_start` on. The nearest copy of an id wins, so an id already
/// indexed stands.
fn index_span(
    spans: &mut BTreeMap<u64, AncestorSpan>,
    path: &Path,
    span: &Span,
    buffer: &[(Protocol, Stamp)],
    buffer_start: usize,
) {
    let held = &buffer[span.events.start - buffer_start..span.events.end - buffer_start];
    let _ = spans.entry(span.id).or_insert_with(|| AncestorSpan {
        source: path.to_path_buf(),
        stamps: held.iter().map(|(_, stamp)| stamp.clone()).collect(),
        row: ancestor_row(
            span.id,
            &held.iter().map(|(protocol, _)| protocol).collect::<Vec<_>>(),
        ),
    });
}

/// One ancestor exchange's row, weighed exactly as [`Memo::span_row`] weighs
/// an own one, and named `inherited` however the ancestor itself recorded it.
fn ancestor_row(exchange: u64, events: &[&Protocol]) -> EvictedRow {
    EvictedRow {
        kind: ContextSpanKind::Inherited,
        ..evicted_row(exchange, events, message_bytes(&closed_messages(events)))
    }
}

/// One evicted exchange's row, weighed at `bytes` — the caller's, since an
/// in-view span reads the render cache and a refold re-renders.
fn evicted_row(exchange: u64, events: &[&Protocol], bytes: usize) -> EvictedRow {
    EvictedRow {
        exchange,
        kind: span_kind(events),
        opening: opening_line_for_events(events),
        steps: step_count(events),
        bytes,
    }
}

/// One store-index row, weighed as the row it was built from was.
fn store_index_item(row: &EvictedRow, in_view: bool) -> StoreIndexItem {
    StoreIndexItem {
        exchange: row.exchange,
        kind: row.kind,
        opening: row.opening.clone(),
        steps: row.steps,
        bytes: row.bytes,
        in_view,
    }
}

/// Hits one `` `grep `` answers with; `total` still counts them all.
const GREP_HITS: usize = 100;

/// What a hit's line is clipped to, in bytes.
const GREP_TEXT_BYTES: usize = 200;

/// Store order: exchange, then message within it, then line within that.
type HitKey = (u64, usize, usize);

/// The oldest [`GREP_HITS`] matches in store order, and how many there were
/// in all — bounded, so a pattern that matches everything costs one window
/// rather than the whole store.
#[derive(Default)]
struct Tally {
    hits: Vec<(HitKey, GrepHit)>,
    total: usize,
}

impl Tally {
    fn offer(&mut self, key: HitKey, hit: GrepHit) {
        self.total += 1;
        let at = self.hits.partition_point(|(seen, _)| *seen < key);
        if at < GREP_HITS {
            self.hits.insert(at, (key, hit));
            self.hits.truncate(GREP_HITS);
        }
    }

    fn answer(self) -> GrepAnswer {
        GrepAnswer {
            hits: self.hits.into_iter().map(|(_, hit)| hit).collect(),
            total: self.total,
        }
    }
}

/// One hit per matching line of one exchange's messages.
fn grep_messages(pattern: &Regex, exchange: u64, messages: &[ChatMessage], tally: &mut Tally) {
    for (position, message) in messages.iter().enumerate() {
        for (index, line) in searched_text(message).lines().enumerate() {
            if pattern.is_match(line) {
                tally.offer(
                    (exchange, position, index),
                    GrepHit {
                        exchange,
                        role: message.role.clone(),
                        line: index + 1,
                        text: line[..ral_core::text::floor_char_boundary(line, GREP_TEXT_BYTES)]
                            .to_string(),
                    },
                );
            }
        }
    }
}

/// What a message contributes to a search — read off the same narrowing a
/// `` `read `` answers with, so nothing is searchable that is not readable.
/// A binary payload and a provider extension carry no text a pattern could
/// mean, and contribute none.
fn searched_text(message: &ChatMessage) -> String {
    transcript_message(message)
        .parts
        .into_iter()
        .filter_map(|part| match part {
            TranscriptPart::Text(text)
            | TranscriptPart::Result(text)
            | TranscriptPart::Reasoning(text)
            | TranscriptPart::Program { source: text, .. } => Some(text),
            TranscriptPart::Binary { .. } | TranscriptPart::Custom { .. } => None,
        })
        .filter(|text| !text.is_empty())
        .collect::<Vec<_>>()
        .join("\n")
}

fn opening_line_for_events(events: &[&Protocol]) -> String {
    for event in events {
        match event {
            Protocol::UserPrompt { text, .. } => return opening_line(text),
            Protocol::ContextMessage { message, .. } if message.role == ChatRole::User => {
                if let Some(text) = message.content.first_text() {
                    return opening_line(text);
                }
            }
            Protocol::ContextMessage { .. }
            | Protocol::SessionStarted { .. }
            | Protocol::SessionResumed { .. }
            | Protocol::SessionEnded
            | Protocol::StepStarted { .. }
            | Protocol::AssistantMessage { .. }
            | Protocol::ToolResults { .. }
            | Protocol::ContextEdited { .. }
            | Protocol::Inherited { .. } => {}
        }
    }
    String::new()
}

/// A [`ChatMessage`] as material: role kept verbatim, content narrowed to
/// [`TranscriptPart`]s so a reader never meets a serialization of a Rust
/// struct standing in for content.
fn transcript_message(message: &ChatMessage) -> TranscriptMessage {
    TranscriptMessage {
        role: message.role.clone(),
        parts: message.content.iter().filter_map(transcript_part).collect(),
    }
}

/// One arm per [`ContentPart`] variant — exhaustive, so a variant genai adds
/// later is a compile error here rather than a silent blob dump.
fn transcript_part(part: &ContentPart) -> Option<TranscriptPart> {
    match part {
        ContentPart::Text(text) => Some(TranscriptPart::Text(text.clone())),
        ContentPart::ToolCall(call) => Some(transcript_program(call)),
        ContentPart::ToolResponse(response) => {
            Some(TranscriptPart::Result(response.content.clone()))
        }
        ContentPart::ReasoningContent(text) => Some(TranscriptPart::Reasoning(text.clone())),
        ContentPart::Binary(binary) => Some(transcript_binary(binary)),
        ContentPart::Custom(custom) => Some(transcript_custom(custom)),
        // An opaque provider continuation token, carrying no information of
        // its own — it produces no part.
        ContentPart::ThoughtSignature(_) => None,
    }
}

/// The tool call IS the ral program the agent ran, so for exarch's one tool
/// this carries the script source rather than the raw arguments JSON; any
/// other tool carries its name and argument keys instead of their values.
fn transcript_program(call: &ToolCall) -> TranscriptPart {
    let keys = call
        .fn_arguments
        .as_object()
        .map(|args| args.keys().cloned().collect())
        .unwrap_or_default();
    let source = if call.fn_name == crate::shell_eval::tools::ral::NAME {
        call.fn_arguments
            .get("cmd")
            .and_then(serde_json::Value::as_str)
            .map(str::to_string)
            .unwrap_or_default()
    } else {
        String::new()
    };
    TranscriptPart::Program {
        tool: call.fn_name.clone(),
        source,
        keys,
    }
}

fn transcript_binary(binary: &Binary) -> TranscriptPart {
    TranscriptPart::Binary {
        content_type: binary.content_type.clone(),
        name: binary.name.clone().unwrap_or_default(),
        bytes: binary_payload_bytes(&binary.source),
    }
}

/// The decoded byte length of a binary payload. A URL source names no local
/// bytes at all, so it reports zero rather than the length of the URL text.
fn binary_payload_bytes(source: &BinarySource) -> usize {
    match source {
        BinarySource::Url(_) => 0,
        BinarySource::Base64(data) => {
            let padding = data.chars().rev().take_while(|&c| c == '=').count();
            (data.len() / 4) * 3 - padding
        }
    }
}

fn transcript_custom(custom: &CustomPart) -> TranscriptPart {
    let (provider, model) = custom
        .model_iden
        .as_ref()
        .map(|iden| (iden.adapter_kind.to_string(), iden.model_name.to_string()))
        .unwrap_or_default();
    TranscriptPart::Custom { provider, model }
}

fn live_exchange_refusal(id: u64) -> String {
    format!("exchange {id} is the one you are in — a context edit may only name closed exchanges")
}

fn unknown_exchange_refusal(id: u64) -> String {
    format!("exchange {id} is not present in the current view")
}

fn never_recorded_refusal(id: u64, last: Option<u64>) -> String {
    match last {
        Some(last) => {
            format!("exchange {id} was never recorded — the last closed exchange is {last}")
        }
        None => format!("exchange {id} was never recorded — no exchange has closed yet"),
    }
}

fn read_back_refusal(exchange: u64, path: &Path, error: &io::Error) -> String {
    format!(
        "exchange {exchange} could not be read back from {}: {error}",
        path.display()
    )
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
        | Protocol::ContextEdited { .. }
        | Protocol::Inherited { .. } => state.clone(),
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
        | Protocol::ContextEdited { .. }
        | Protocol::Inherited { .. } => Vec::new(),
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
        | Protocol::ContextEdited { .. }
        | Protocol::Inherited { .. } => None,
    }
}

/// Protocol sequencing legality — a runtime guard for data read off disk,
/// never consulted by [`Fold::step`] itself: live authorship is already
/// typestate-correct, so the only place this can fail is [`Admission`],
/// walking a file this process did not just write.
fn admissible(state: &State, protocol: &Protocol) -> bool {
    match protocol {
        Protocol::UserPrompt { .. } | Protocol::ContextMessage { .. } => admits_new_span(state),
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
        | Protocol::ContextEdited { .. }
        // Sequencing-neutral; where a link may stand is [`Admission::step`]'s
        // own rule, needing more than a [`State`].
        | Protocol::Inherited { .. } => true,
    }
}

/// Whether a record that opens a span may follow. Only outstanding tool calls
/// forbid it: an exchange the model never replied to is abandoned by the next
/// prompt, not closed by a fabricated reply.
fn admits_new_span(state: &State) -> bool {
    !matches!(state, State::AwaitingToolResults { .. })
}

/// The longest prefix of `events` that owes no tool result — the only
/// shape a `mnemon` seed may take, the child's own launch prompt landing
/// behind it.
fn admissible_prefix<'a, 'b>(events: &'b [&'a Protocol]) -> &'b [&'a Protocol] {
    let mut state = State::default();
    let mut end = 0;
    for (index, event) in events.iter().enumerate() {
        state = advance(&state, event);
        if admits_new_span(&state) {
            end = index + 1;
        }
    }
    &events[..end]
}

/// Whether a record inadmissible at `state` is one [`quiesce_records`] can
/// still make way for, rather than foreign data no live door could produce.
fn stub_repairable(protocol: &Protocol) -> bool {
    matches!(
        protocol,
        Protocol::UserPrompt { .. } | Protocol::ContextMessage { .. }
    )
}

/// What the model reads in place of an exchange that never reached a reply.
///
/// In the user's voice, never the assistant's: the harness may state a fact
/// about the conversation, but must not put words in the model's mouth — a
/// placeholder in the assistant's own voice is read back as something it chose
/// to say, and imitated. It is cause-neutral because it has to be: a cancel
/// and an abort are told apart only by a `Forensic` record, and this fold
/// projects the protocol subsequence alone.
///
/// Whether tools were called is the fact that changes what to do next, since
/// their effects outlive the context the exchange lost. "Had been called" and
/// "any effects" are both hedged deliberately: a batch answered wholly by
/// [`UNRUN_TOOL_CALL`] is a call made and not run.
fn abandoned_note(events: &[&Protocol]) -> String {
    let effects = if events
        .iter()
        .any(|event| matches!(event, Protocol::ToolResults { .. }))
    {
        "Tools had been called, so any effects on the shell and filesystem stand."
    } else {
        "No tool had been called."
    };
    format!(
        "[EXARCH // An exchange here was interrupted before any reply; its content is not in your context. {effects}]"
    )
}

/// Evicted exchanges the head marker draws a row each for; everything older
/// collapses into one line naming the range.
const HEAD_ROWS: usize = 40;

/// What a row's opening line is clipped and padded to. One measure for
/// both, so the table cannot be knocked out of column.
const HEAD_OPENING: usize = 50;

/// The head marker: the one user-voice message standing where an evicted
/// prefix was, indexing every exchange that has left so the model can ask for
/// any of them back.
///
/// A pure function of the eviction list — no clock, no path, no
/// ordering-unstable container — so between two evictions the provider's
/// prompt cache sees a byte-stable message 0.
///
/// Voice and bracket as [`abandoned_note`]: the harness may state a fact
/// about the conversation, never speak in the model's own voice.
fn render_head(evictions: &[Eviction]) -> String {
    let rows: Vec<&EvictedRow> = evictions
        .iter()
        .flat_map(|eviction| eviction.rows.iter())
        .collect();
    let (Some(first), Some(last)) = (rows.first(), rows.last()) else {
        return String::new();
    };
    let departed = if rows.len() == 1 {
        format!("Exchange {} has left your context.", first.exchange)
    } else {
        format!(
            "Exchanges {}–{} have left your context.",
            first.exchange, last.exchange
        )
    };
    let whereabouts = "They are still readable: `transcript `read [n]` reads one back as \
                       material, `transcript `grep 're'` searches them all, `transcript \
                       `index` lists every closed exchange.";
    let mut lines = vec![format!("[EXARCH // {departed} {whereabouts}")];
    let collapsed = rows.len().saturating_sub(HEAD_ROWS);
    if collapsed > 0 {
        let range = format!("{}–{}", first.exchange, rows[collapsed - 1].exchange);
        lines.push(format!(
            "{range:>4}  ({collapsed} earlier exchanges — transcript `index)"
        ));
    }
    for drawn in drawn_evictions(evictions, collapsed) {
        for row in drawn.rows {
            lines.push(head_row(row));
        }
        if let Some(note) = drawn.note {
            // `Debug`-quoted, so no note — the model's own, or one inherited
            // off an ancestor's link — can add a line to the marker.
            lines.push(format!("Your note at eviction: {note:?}"));
        }
    }
    format!("{}]", lines.join("\n"))
}

/// What the marker draws: one entry per eviction that still has a row on
/// screen, with its own rows and its own note. One shape, so a note
/// cannot survive the rows it belongs to.
struct Drawn<'a> {
    rows: &'a [EvictedRow],
    note: Option<&'a str>,
}

/// The evictions still on screen past the collapse: each kept to its own
/// surviving suffix of rows, its note carried along only when a row of its
/// own survives.
fn drawn_evictions(evictions: &[Eviction], collapsed: usize) -> Vec<Drawn<'_>> {
    let mut drawn = Vec::new();
    let mut seen = 0usize;
    for eviction in evictions {
        let start = collapsed.saturating_sub(seen).min(eviction.rows.len());
        seen += eviction.rows.len();
        if start < eviction.rows.len() {
            drawn.push(Drawn {
                rows: &eviction.rows[start..],
                note: eviction.note.as_deref(),
            });
        }
    }
    drawn
}

fn head_row(row: &EvictedRow) -> String {
    let opening: String = row.opening.chars().take(HEAD_OPENING).collect();
    format!(
        "{:>4}  {opening:<HEAD_OPENING$}{:>3} steps {:>5} KB",
        row.exchange,
        row.steps,
        row.bytes / 1024
    )
}

/// The answer a tool call that never ran is given.  It does not name what
/// ended the exchange, because it cannot vary on it: a cancel and a `reply`
/// each answer their own batch before they quiesce, so only
/// [`QuiesceReason::Aborted`] ever reaches this.  The cause is
/// `Forensic::Cancelled`'s or `Forensic::ProviderError`'s to carry.
const UNRUN_TOOL_CALL: &str = "[EXARCH // No result: the exchange ended before this call ran.]";

/// The records a quiesce still owes.
///
/// A tool call that never ran is owed an answer whatever ended the exchange:
/// the calls were really made, "not executed" is really the answer, and a
/// dangling tool-call block is not a legal request. A [`QuiesceReason::Replied`]
/// exchange is owed a capstone besides — it ended, and only a record can say
/// so, the fold being unable to tell a reply from an interruption at the
/// resting state the two share.
///
/// Nothing else is synthesised. An exchange that stopped before any reply is
/// left exactly as it lies, and [`Memo::span_messages`] drops it from the
/// model's view once it closes, so the model never reads a turn it never took.
fn quiesce_records(state: &State, reason: QuiesceReason) -> Vec<Protocol> {
    let mut records = Vec::new();
    let mut state = state.clone();
    if let State::AwaitingToolResults { pending_ids } = &state {
        let results = pending_ids
            .iter()
            .map(|id| ToolResult {
                id: id.clone(),
                content: UNRUN_TOOL_CALL.into(),
            })
            .collect();
        let record = Protocol::ToolResults { results };
        state = advance(&state, &record);
        records.push(record);
    }
    if matches!(reason, QuiesceReason::Replied) && !matches!(state, State::ReadyForUser) {
        records.push(Protocol::AssistantMessage {
            message: ChatMessage::assistant("[EXARCH // Exchange ended: replied to parent.]"),
            pending_tool_ids: Vec::new(),
            stop_reason: Some("replied".into()),
        });
    }
    records
}

/// One step of the fold: append to the ledger, advance the view, and free
/// whatever no surviving span needs any more — the law that no recorded
/// protocol record is ever truly discarded holds because `free` keeps the
/// evicted index's `Stamp`, never drops it.
fn step_protocol(memo: &mut Memo, stamp: Stamp, protocol: &Protocol) {
    let index = memo.ledger.append(Recorded::new(stamp, protocol.clone()));
    memo.state = advance(&memo.state, protocol);
    record_event_span(&mut memo.view, &mut memo.max_exchange, protocol, index);
    if let Protocol::Inherited {
        source,
        evictions,
        through_exchange,
    } = protocol
    {
        inherit(&mut memo.view, evictions);
        memo.ancestry = Some(Ancestry {
            source: source.clone(),
            through_exchange: *through_exchange,
        });
        memo.recompute_head();
    }
    let removed = if let Protocol::ContextEdited { op, .. } = protocol {
        let leaving: Vec<Span> = removed_spans(&memo.view, op).into_iter().cloned().collect();
        let mut rows = Vec::with_capacity(leaving.len());
        for span in &leaving {
            let row = memo.span_row(span);
            rows.push(row.clone());
            let _ = memo.departed.insert(
                span.id,
                Departed {
                    events: span.events.clone(),
                    row,
                },
            );
        }
        // A drop leaves the head marker alone: it indexes the prefix that
        // left, and touching message 0 would throw away a cache a drop of a
        // late exchange still holds.
        let evicted = matches!(op, ContextOp::Evict { .. });
        apply_context_op(&mut memo.view, op, if evicted { rows } else { Vec::new() });
        if evicted {
            memo.recompute_head();
        }
        memo.newest_edit = Some(index);
        // Memory hygiene, not correctness: a stale render can never be read
        // back, since a hit demands `end` to match a span still in view.
        let live_ids: HashSet<u64> = memo.view.spans.iter().map(|span| span.id).collect();
        memo.render.retain(|id, _| live_ids.contains(id));
        leaving.iter().map(|span| span.events.clone()).collect()
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

/// The spans `op` takes out of `view`, in view order.
fn removed_spans<'a>(view: &'a View, op: &ContextOp) -> Vec<&'a Span> {
    match op {
        ContextOp::Evict {
            through_exchange, ..
        } => view
            .spans
            .iter()
            .filter(|span| span.id <= *through_exchange)
            .collect(),
        ContextOp::Drop { exchanges } => view
            .spans
            .iter()
            .filter(|span| exchanges.contains(&span.id))
            .collect(),
    }
}

/// A link's own step on the view: the parent's head-marker state becomes the
/// child's, which a fresh log has none of.
fn inherit(view: &mut View, evictions: &[Eviction]) {
    view.evictions = evictions.to_vec();
}

/// `rows` is what an [`ContextOp::Evict`] remembers of the spans it removes;
/// a drop indexes nothing and passes none.
fn apply_context_op(view: &mut View, op: &ContextOp, rows: Vec<EvictedRow>) {
    match op {
        ContextOp::Evict {
            through_exchange,
            note,
        } => {
            view.evictions.push(Eviction {
                rows,
                note: note.clone(),
            });
            view.spans.retain(|span| span.id > *through_exchange);
        }
        ContextOp::Drop { exchanges } => {
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
///
/// An eviction's rows are part of that check, so they are recomputed here
/// from the records the op removed, rendered through [`closed_messages`] —
/// the same rendering the live fold weighed them by.
fn refold(ledger: &Ledger) -> io::Result<(State, View, u64)> {
    let events = ledger.fold_events()?;
    let mut state = State::default();
    let mut view = View::default();
    let mut max_exchange = 0u64;
    for (index, protocol) in &events {
        state = advance(&state, protocol);
        record_event_span(&mut view, &mut max_exchange, protocol, *index);
        if let Protocol::Inherited { evictions, .. } = protocol {
            inherit(&mut view, evictions);
        }
        if let Protocol::ContextEdited { op, .. } = protocol {
            let rows = match op {
                ContextOp::Evict { .. } => removed_spans(&view, op)
                    .into_iter()
                    .map(|span| refolded_row(span, &events))
                    .collect(),
                ContextOp::Drop { .. } => Vec::new(),
            };
            apply_context_op(&mut view, op, rows);
        }
    }
    Ok((state, view, max_exchange))
}

/// One evicted span's row, weighed from the records the refold holds rather
/// than from a render cache it has none of.
fn refolded_row(span: &Span, events: &[(usize, Protocol)]) -> EvictedRow {
    let start = events.partition_point(|(index, _)| *index < span.events.start);
    let end = events.partition_point(|(index, _)| *index < span.events.end);
    let records: Vec<&Protocol> = events[start..end]
        .iter()
        .map(|(_, protocol)| protocol)
        .collect();
    evicted_row(span.id, &records, message_bytes(&closed_messages(&records)))
}

/// The protocol sequencing law, replayed record by record beside the fold that
/// [`resume`] is building: interposing the records a live `quiesce` would have
/// written wherever a record is inadmissible but repairable — a tool-result
/// batch lost from the file — and refusing outright wherever it is not:
/// foreign data no live door could have produced.
///
/// Its `state` and `view` are its own, never the fold's, so admission stays an
/// independent judgement on the file rather than a self-agreement of the memo.
#[derive(Default)]
struct Admission {
    state: State,
    view: View,
    max_exchange: u64,
    index: usize,
    /// A log has at most one ancestry, fixed at the fork that opened it.
    inherited: bool,
}

impl Admission {
    /// # Errors
    /// Returns [`Refusal::Foreign`] for a record no live session could have
    /// written at this point in the log.
    fn step(&mut self, protocol: &Protocol) -> Result<(), Refusal> {
        let index = self.index;
        self.index += 1;
        if !admissible(&self.state, protocol) {
            if !stub_repairable(protocol) {
                return Err(Refusal::Foreign {
                    record: Box::new(Record::Protocol(protocol.clone())),
                    reason: format!(
                        "record {} is foreign protocol data, not a seam that quiesce can repair; was record.jsonl hand-edited or written by an incompatible exarch?",
                        index + 1
                    ),
                });
            }
            for record in quiesce_records(&self.state, QuiesceReason::Aborted) {
                self.state = advance(&self.state, &record);
            }
        }
        if matches!(protocol, Protocol::Inherited { .. }) {
            if self.inherited
                || self.max_exchange != 0
                || !matches!(self.state, State::ReadyForUser)
            {
                return Err(Refusal::Foreign {
                    record: Box::new(Record::Protocol(protocol.clone())),
                    reason: format!(
                        "record {} links this log to an ancestor's store, which only a fork's opening may do; by here the log has a context of its own, and no live session inherits twice",
                        index + 1
                    ),
                });
            }
            self.inherited = true;
        }
        self.state = advance(&self.state, protocol);
        if let Some(id) = record_exchange(protocol) {
            let joins = id > self.max_exchange
                || self
                    .view
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
        record_event_span(&mut self.view, &mut self.max_exchange, protocol, index);
        if let Protocol::ContextEdited { op, .. } = protocol {
            // Rows are the fold's business; this walk judges sequencing, and
            // never reads `View::evictions` back.
            apply_context_op(&mut self.view, op, Vec::new());
        }
        Ok(())
    }
}

/// The bytes past the last complete JSONL line — a torn write from a session
/// that did not shut down cleanly.
struct CrashTail {
    bytes: Vec<u8>,
    complete_len: u64,
}

/// Scanned backwards from the end, a window at a time, rather than forwards
/// through the whole file: the tail is one torn record long however long the
/// log behind it is.
const CRASH_SCAN_WINDOW: u64 = 8 * 1024;

#[allow(
    clippy::disallowed_methods,
    reason = "[io-door:silent:model-fold-crash-scan] scans record.jsonl's tail for a torn trailing write before resume folds it; output infra, not turn-time data I/O"
)]
fn find_crash_tail(path: &Path) -> io::Result<Option<CrashTail>> {
    let mut file = File::open(path)?;
    let len = file.metadata()?.len();
    let mut complete_len = 0;
    let mut cursor = len;
    while cursor > 0 {
        let window = cursor.saturating_sub(CRASH_SCAN_WINDOW);
        let mut chunk = vec![0; usize::try_from(cursor - window).unwrap_or(usize::MAX)];
        let _ = file.seek(SeekFrom::Start(window))?;
        file.read_exact(&mut chunk)?;
        if let Some(last) = chunk.iter().rposition(|byte| *byte == b'\n') {
            complete_len = window + last as u64 + 1;
            break;
        }
        cursor = window;
    }
    if complete_len == len {
        return Ok(None);
    }
    let mut bytes = Vec::new();
    let _ = file.seek(SeekFrom::Start(complete_len))?;
    let _ = file.read_to_end(&mut bytes)?;
    Ok(Some(CrashTail {
        bytes,
        complete_len,
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
    let mut quarantine = OpenOptions::new()
        .create(true)
        .append(true)
        .open(&sidecar)?;
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

/// Fold `record.jsonl` into a fresh [`Memo`]: quarantine a torn tail, then
/// stream the log once — admitting each protocol record's sequencing and
/// folding it in the same pass — and check the fold against an independent
/// from-scratch refold of the ledger it built: the `fold == memo` law,
/// migrated whole from `AgentLog::resume`.
///
/// The pass is a stream rather than a collection, so what a resume holds is
/// the memo it is building — residency at the addressed view — and never the
/// log it is building it from.
///
/// Returns the folded memo alongside the `(model, label)` pair the head
/// record identifies the session by — `AgentLog::resume` reads its own
/// identity from here rather than re-deriving it, since a session's identity
/// is a fact about its first record, not a second thing to keep in step.
///
/// # Errors
/// Returns an error if the file cannot be read, quarantined, has no
/// `SessionStarted { session_id: 0, parent: None }` head record, or refolds
/// to a projection that disagrees with the one built incrementally.
pub fn resume(path: &Path) -> io::Result<(Memo, String, String)> {
    if let Some(tail) = find_crash_tail(path)? {
        quarantine_tail(path, &tail)?;
    }
    let mut memo = Memo::new(path.to_path_buf());
    let mut admission = Admission::default();
    let mut identity = None;
    for record in Log::read(path)? {
        let record = record?;
        let Record::Protocol(protocol) = record.value() else {
            continue;
        };
        if identity.is_none() {
            identity = Some(match protocol {
                Protocol::SessionStarted {
                    session_id: 0,
                    parent: None,
                    model,
                    label,
                    ..
                } => (model.clone(), label.clone()),
                Protocol::SessionStarted {
                    session_id, parent, ..
                } => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record starts session {session_id:?} with parent {parent:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a child log?",
                        path.display()
                    )));
                }
                other @ (Protocol::SessionResumed { .. }
                | Protocol::SessionEnded
                | Protocol::UserPrompt { .. }
                | Protocol::ContextMessage { .. }
                | Protocol::StepStarted { .. }
                | Protocol::AssistantMessage { .. }
                | Protocol::ToolResults { .. }
                | Protocol::ContextEdited { .. }
                | Protocol::Inherited { .. }) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record is {other:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a copied child log?",
                        path.display()
                    )));
                }
            });
        }
        admission
            .step(protocol)
            .map_err(|refusal| io::Error::other(refusal.to_string()))?;
        step_protocol(&mut memo, record.stamp().clone(), protocol);
    }
    let Some((model, label)) = identity else {
        return Err(io::Error::other(format!(
            "cannot resume {}: no complete session records were found; is the file truncated?",
            path.display()
        )));
    };

    let (state, view, max_exchange) = refold(&memo.ledger)?;
    if (state, view, max_exchange) != (memo.state.clone(), memo.view.clone(), memo.max_exchange) {
        return Err(io::Error::other(
            "record.jsonl's from-scratch refold disagreed with the ledger's incremental projection",
        ));
    }
    Ok((memo, model, label))
}
