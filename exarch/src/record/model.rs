//! The model fold: the provider-facing projection built off the [`Protocol`]
//! records of `record.jsonl` — the same `step`, whether it is applied inline
//! on the attend thread right after [`super::Emitter::emit`] returns, or by
//! [`super::replay`] from disk.
//!
//! One growing structure, one fold, everything else a projection. The log is
//! the structure; [`Folded`] is its fold — a table of [`Turn`]s and the
//! [`Cut`]s made in it. The context sent to the provider, the survey, the
//! transcript index, the head marker and the eviction plan are all pure
//! functions of that table.
//!
//! [`Display`](super::Display) and [`Forensic`](super::Forensic) records pass
//! through untouched: this fold projects the protocol subsequence alone, so
//! its slots — and the turns built from them — never see a display or
//! forensic record interleaved on the log between two protocol ones.
//!
//! No recorded protocol record is ever discarded: a departed slot keeps only
//! its [`Stamp`], and [`Ledger::fold_events`] reads it back through that
//! `Stamp`'s byte range one record at a time — never coalesced into a run —
//! so the from-scratch refold in [`resume`] is a bijection with the
//! incrementally maintained table, record by record.

use super::log::Log;
use super::{Fold, Protocol, Record, Recorded, Refusal, Stamp};
use crate::agent::event::{
    ContextOp, ContextSurvey, GrepAnswer, GrepHit, QuiesceReason, ToolResult, TranscriptExchange,
    TranscriptMessage, TranscriptPart, TurnKind, validate_result_ids,
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

/// Marker type implementing [`Fold`] for the model projection; carries no
/// state of its own; [`Memo`] is where the projection lives.
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

/// One turn: the atom an eviction addresses.
///
/// A *user turn* is a prompt, or an import's opening, and anything before the
/// first reply — it has `exchange == id`. An *assistant turn* is the
/// assistant message, the tool results it called for, and any steering before
/// the next request; it carries its exchange's id beside its own.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Turn {
    pub id: u64,
    pub exchange: u64,
    pub kind: TurnKind,
    /// The turn's opening line, clipped to [`OPENING_CHARS`] as it opens.
    pub label: String,
    /// Serialised bytes, summed as the turn's own records land.
    pub bytes: usize,
    pub held: Held,
}

impl Turn {
    /// A user turn opens its exchange, and is the one turn a cut may leave
    /// standing at or below its own reach.
    #[must_use]
    pub fn is_user(&self) -> bool {
        self.id == self.exchange
    }

    fn is_resident(&self) -> bool {
        matches!(self.held, Held::Resident)
    }
}

/// Whether a turn is still in the context, and what took it out.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Held {
    Resident,
    Evicted,
    Dropped,
}

impl Held {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Evicted => "evicted",
            Self::Dropped => "dropped",
        }
    }
}

/// One eviction: the last turn it took, and the model's own line to its
/// future self.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Cut {
    pub through: u64,
    pub note: Option<String>,
}

/// The fold's pure output: the protocol's resting state, every turn this
/// lineage recorded in id order, and the cuts made in them.
///
/// The context is the [`Held::Resident`] subsequence of `turns`. Invariant:
/// the first resident turn is a user turn.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct Folded {
    state: State,
    turns: Vec<Turn>,
    cuts: Vec<Cut>,
}

impl Folded {
    #[must_use]
    pub fn turns(&self) -> &[Turn] {
        &self.turns
    }

    #[must_use]
    pub fn cuts(&self) -> &[Cut] {
        &self.cuts
    }

    /// The context, in id order.
    pub fn resident(&self) -> impl Iterator<Item = &Turn> {
        self.turns.iter().filter(|turn| turn.is_resident())
    }

    /// The highest id the table has reached — every id-bearing record past it
    /// opens a turn.
    fn reach(&self) -> Option<u64> {
        self.turns.last().map(|turn| turn.id)
    }

    fn turn(&self, id: u64) -> Option<&Turn> {
        self.turns.iter().find(|turn| turn.id == id)
    }

    /// Which turns of `exchange` the table holds, in id order.
    fn exchange_turns(&self, exchange: u64) -> Vec<u64> {
        self.turns
            .iter()
            .filter(|turn| turn.exchange == exchange)
            .map(|turn| turn.id)
            .collect()
    }

    /// The context's exchanges, each with its resident turns, in id order.
    fn resident_exchanges(&self) -> Vec<(u64, Vec<u64>)> {
        let mut exchanges: Vec<(u64, Vec<u64>)> = Vec::new();
        for turn in self.resident() {
            match exchanges.last_mut() {
                Some((exchange, turns)) if *exchange == turn.exchange => turns.push(turn.id),
                _ => exchanges.push((turn.exchange, vec![turn.id])),
            }
        }
        exchanges
    }
}

/// The context as a persistent value: one shared segment per turn (the head
/// marker and an abandoned exchange's note included), the growing turn
/// rendered fresh. Clone is Arc bumps; the only routes back to owned messages
/// are the wire door and the mnemon child seed.
#[derive(Clone, Default)]
pub struct Rendered {
    segments: Vec<Arc<[ChatMessage]>>,
    bytes: usize,
}

impl Rendered {
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

    /// Serialised size, summed from the table's own per-turn weights.
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

/// One turn's rendering, cached beside the turn's own end slot: a hit
/// requires `end` to match, so a still-growing turn re-renders and staleness
/// is inexpressible.
struct TurnRender {
    end: usize,
    segment: Arc<[ChatMessage]>,
}

fn message_bytes(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
        .sum()
}

/// One resident protocol record, or the [`Stamp`] to read it back by once a
/// context edit takes its turn out.
struct Ledger {
    len: usize,
    resident: BTreeMap<usize, Recorded<Protocol>>,
    freed: BTreeMap<usize, Stamp>,
    /// The turn each slot belongs to, `None` for the prefix before the first
    /// turn opens. Non-decreasing, so a turn's run is one binary search.
    placement: Vec<Option<u64>>,
    /// `record.jsonl`'s own path — the only thing a freed slot needs to read
    /// itself back.
    ///
    /// Reading a completed record by byte range from the file this process is
    /// still appending to is safe: a [`Stamp`] exists only once the seam has
    /// written the whole record under its lock.
    source: PathBuf,
}

impl Ledger {
    fn append(&mut self, record: Recorded<Protocol>, turn: Option<u64>) -> usize {
        let index = self.len;
        self.len += 1;
        self.placement.push(turn);
        let _ = self.resident.insert(index, record);
        index
    }

    fn len(&self) -> usize {
        self.len
    }

    /// One turn's run of slots. Empty for a turn this log never recorded —
    /// an inherited one the fork did not re-record.
    fn events(&self, turn: u64) -> Range<usize> {
        let start = self.placement.partition_point(|held| *held < Some(turn));
        let end = self.placement.partition_point(|held| *held <= Some(turn));
        start..end
    }

    fn resident_events(&self, range: Range<usize>) -> Option<Vec<&Protocol>> {
        let events: Vec<&Protocol> = self
            .resident
            .range(range.clone())
            .map(|(_, recorded)| recorded.value())
            .collect();
        (events.len() == range.len()).then_some(events)
    }

    /// Every slot in `range`'s [`Stamp`], resident or freed — a whole run
    /// reads back off the file whichever half of the ledger holds it.
    fn stamps(&self, range: Range<usize>) -> Option<Vec<Stamp>> {
        range
            .map(|index| {
                self.resident
                    .get(&index)
                    .map(|recorded| recorded.stamp().clone())
                    .or_else(|| self.freed.get(&index).cloned())
            })
            .collect()
    }

    fn free(&mut self, index: usize) {
        if let Some(recorded) = self.resident.remove(&index) {
            let _ = self.freed.insert(index, recorded.stamp().clone());
        }
    }

    fn free_turn(&mut self, turn: u64) {
        for index in self.events(turn) {
            self.free(index);
        }
    }

    /// Every protocol record this fold has ever seen, resident or not — the
    /// freed ones read back one at a time through their own `Stamp`.
    fn fold_events(&self) -> io::Result<Vec<Protocol>> {
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
        Ok(events.into_iter().map(|(_, protocol)| protocol).collect())
    }
}

/// Read `stamps`' records back off `path`, in the order given — one file
/// opened per call, whoever is asking.
///
/// # Errors
/// Returns `Err` if the file cannot be opened, a range cannot be read, or a
/// stamp names anything but a protocol record.
fn read_records(path: &Path, stamps: &[Stamp]) -> io::Result<Vec<Protocol>> {
    let mut file = open_log(path)?;
    stamps
        .iter()
        .map(|stamp| read_stamped(&mut file, stamp))
        .collect()
}

/// One handle on a record log, carrying the model fold's io-door allow.
fn open_log(path: &Path) -> io::Result<File> {
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:model-fold-freed-read] reads record.jsonl back by Stamp for the fold's own freed slots and the model's `transcript` door alike; surfaced as a Display::HarnessCall, not the model's own data I/O"
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

/// The model fold's memo: the ledger, the [`Folded`] table it projects, and
/// the caches under the recompute invariant — a memo of a pure function at
/// immutable arguments, never serialised, rebuilt from nothing by this same
/// fold on resume.
///
/// No `Default`: a memo without its log's path is a memo whose every door
/// read fails, so [`Self::new`] is the only way to one.
pub struct Memo {
    ledger: Ledger,
    folded: Folded,
    newest_edit: Option<usize>,
    /// One rendering per turn, keyed by turn id — a turn id never recurs, so
    /// `(id, end)` determines a rendering globally and forever.
    render: HashMap<u64, TurnRender>,
    /// The head marker, recomputed whenever [`Folded::cuts`] or the table
    /// changes, and present exactly when a cut carries a fragment.
    head_render: Option<(Arc<[ChatMessage]>, usize)>,
    /// The parent this log's opening context came from, when it is a
    /// `mnemon` child; [`Protocol::Inherited`] is the one thing that sets it.
    ancestry: Option<Ancestry>,
    /// Foreign placement for the inherited turns this log never re-recorded:
    /// which file, which stamps. Walked once on the first read that reaches
    /// past this log's ledger. Memo-only, never serialised.
    ancestry_index: Option<AncestorIndex>,
}

/// One link of a lineage: whose file the ids below it read back from, and
/// how far down they reach.
#[derive(Clone)]
struct Ancestry {
    /// That ancestor's `record.jsonl`.
    source: PathBuf,
    /// Ids at or below this belong to the ancestry; above it they are this
    /// log's own.
    through: u64,
}

/// The lineage's foreign placement, by turn — and, when the walk stopped
/// short of the lineage's root, why, so a turn it could not reach is refused
/// with the reason rather than called unrecorded.
#[derive(Default)]
struct AncestorIndex {
    turns: BTreeMap<u64, AncestorTurn>,
    broken: Option<Break>,
}

/// Where one ancestor's turn lies. `source` is per turn because a lineage of
/// three spreads its ids over three files.
struct AncestorTurn {
    source: PathBuf,
    stamps: Vec<Stamp>,
}

/// Why a lineage walk stopped before the ancestry's root: an ancestor's
/// `record.jsonl` would not open, or one of its records would not read back.
/// `/clear` cancels every descendant before it rotates, so no live child
/// holds a link to a rotated file; a hand-deleted directory is what a walk
/// usually meets, but an ancestor is appended to while its children read it,
/// so a record can fail on its own account too.
struct Break {
    source: PathBuf,
    fault: Fault,
    error: String,
}

/// Which half of the walk failed, so a refusal names the fault met rather
/// than the likelier one.
enum Fault {
    /// The file itself.
    Open,
    /// One record within it, by line.
    Record(usize),
}

impl Break {
    fn refusal(&self, turn: u64) -> String {
        let at = self.source.display();
        let error = &self.error;
        match self.fault {
            Fault::Open => format!(
                "turn {turn} was recorded by an ancestor, but the ancestor's log at {at} would not open: {error}"
            ),
            Fault::Record(line) => format!(
                "turn {turn} was recorded by an ancestor, but line {line} of the ancestor's log at {at} would not read back: {error}"
            ),
        }
    }
}

/// [`index_ancestor`]'s failure, before the walk binds it to the file it
/// happened in.
struct Broken {
    fault: Fault,
    error: io::Error,
}

/// Where one turn's records lie: cloned out of this log's ledger, or the file
/// and the stamps to read them back by.
enum Sourced {
    Held(Vec<Protocol>),
    Stamped {
        source: PathBuf,
        stamps: Vec<Stamp>,
    },
}

impl Sourced {
    /// Which file the records came out of; a held run is this log's own.
    fn source<'a>(&'a self, own: &'a Path) -> &'a Path {
        match self {
            Self::Held(_) => own,
            Self::Stamped { source, .. } => source,
        }
    }
}

/// How a located exchange's records become messages.
#[derive(Clone, Copy)]
enum Rendering {
    /// Every turn of a closed exchange at once, through [`closed_messages`] —
    /// what the model was sent, an abandoned exchange's note included.
    Whole,
    /// Turn by turn, each turn's own material: what a read that reaches only
    /// part of an exchange asks for, and the granularity a hit names.
    Turns,
}

impl Rendering {
    fn messages(self, turns: &[(u64, Vec<Protocol>)]) -> Vec<ChatMessage> {
        match self {
            Self::Whole => {
                let events: Vec<&Protocol> = turns.iter().flat_map(|(_, events)| events).collect();
                closed_messages(&events)
            }
            Self::Turns => turns
                .iter()
                .flat_map(|(_, events)| turn_messages(events))
                .collect(),
        }
    }
}

/// One turn's own material, whatever became of the exchange holding it.
fn turn_messages(events: &[Protocol]) -> Vec<ChatMessage> {
    events
        .iter()
        .cloned()
        .flat_map(into_chat_messages)
        .collect()
}

/// One exchange a door read reaches: where each of its turns lies — a fork
/// can leave one exchange's turns in two files, so placement is per turn —
/// and how they render.
struct ExchangeRead {
    exchange: u64,
    turns: Vec<(u64, Sourced)>,
    rendering: Rendering,
}

/// Where every turn a door read names lies, owned outright: the read borrows
/// nothing from the memo, so the desk can drop the session lock before it
/// touches a file.
pub(crate) struct TranscriptRead {
    exchanges: Vec<ExchangeRead>,
}

impl TranscriptRead {
    /// One [`TranscriptExchange`] per exchange the read touched, narrowed to
    /// [`TranscriptPart`]s rather than the provider's own content parts.
    ///
    /// # Errors
    /// Refuses a turn whose file will not read back.
    pub(crate) fn exchanges(self) -> Result<Vec<TranscriptExchange>, String> {
        let mut read = Vec::with_capacity(self.exchanges.len());
        self.walk(read_back_refusal, |exchange, rendering, turns| {
            read.push(TranscriptExchange {
                exchange,
                turns: turns.iter().map(|(turn, _)| *turn).collect(),
                messages: rendering
                    .messages(turns)
                    .iter()
                    .map(transcript_message)
                    .collect(),
            });
        })?;
        Ok(read)
    }

    /// One hit per matching line, oldest first and bounded by [`GREP_HITS`].
    /// Searched turn by turn, so every hit names the turn it lies in.
    ///
    /// # Errors
    /// Refuses a file that will not read back.
    pub(crate) fn grep(self, pattern: &Regex) -> Result<GrepAnswer, String> {
        let mut tally = Tally::default();
        self.walk(
            |_, path, error| {
                format!(
                    "the turns recorded in {} could not be read back: {error}",
                    path.display()
                )
            },
            |exchange, _, turns| {
                for (turn, events) in turns {
                    grep_messages(pattern, exchange, *turn, &turn_messages(events), &mut tally);
                }
            },
        )?;
        Ok(tally.answer())
    }

    /// Hand every located exchange's turns to `visit`, in transcript order,
    /// holding at most one open file: consecutive turns naming one log share
    /// it, so each file is read in a single pass in stamp order.
    fn walk(
        self,
        refusal: impl Fn(u64, &Path, &io::Error) -> String,
        mut visit: impl FnMut(u64, Rendering, &[(u64, Vec<Protocol>)]),
    ) -> Result<(), String> {
        let mut open: Option<(PathBuf, File)> = None;
        for ExchangeRead {
            exchange,
            turns,
            rendering,
        } in self.exchanges
        {
            let mut read = Vec::with_capacity(turns.len());
            for (turn, sourced) in turns {
                let events = match sourced {
                    Sourced::Held(events) => events,
                    Sourced::Stamped { source, stamps } => {
                        if open.as_ref().is_none_or(|(path, _)| *path != source) {
                            // Closed before the next is opened, so a lineage
                            // of many files costs one descriptor, never two.
                            drop(open.take());
                            let file = open_log(&source)
                                .map_err(|error| refusal(turn, &source, &error))?;
                            open = Some((source, file));
                        }
                        let (path, file) = open.as_mut().expect("just opened, or already open");
                        stamps
                            .iter()
                            .map(|stamp| read_stamped(file, stamp))
                            .collect::<io::Result<Vec<_>>>()
                            .map_err(|error| refusal(turn, path, &error))?
                    }
                };
                read.push((turn, events));
            }
            visit(exchange, rendering, &read);
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
                len: 0,
                resident: BTreeMap::new(),
                freed: BTreeMap::new(),
                placement: Vec::new(),
                source,
            },
            folded: Folded::default(),
            newest_edit: None,
            render: HashMap::new(),
            head_render: None,
            ancestry: None,
            ancestry_index: None,
        }
    }

    /// Whether a record that opens an exchange — a fresh
    /// [`Protocol::UserPrompt`], an imported [`Protocol::ContextMessage`] —
    /// is admissible here. Weaker than [`State::ReadyForUser`]: an exchange
    /// that never reached a reply is abandoned by the next prompt rather than
    /// closed by a fabricated one, so only outstanding tool calls hold the
    /// log.
    pub fn is_ready(&self) -> bool {
        admits_new_turn(&self.folded.state)
    }

    /// An edit may land at any rest but a batch in flight: outstanding tool
    /// calls name the assistant frame their results answer, so nothing may
    /// come between. What keeps the work in hand is
    /// [`Self::plan_eviction`]'s own shape and [`Self::validate_edit`], not
    /// this.
    pub fn can_evict(&self) -> bool {
        self.is_ready()
    }

    /// The whole table and its cuts — the one shape every projection below
    /// reads.
    pub fn folded(&self) -> &Folded {
        &self.folded
    }

    pub fn current_exchange(&self) -> Option<u64> {
        self.folded.turns.last().map(|turn| turn.exchange)
    }

    /// The id the next turn this log opens is minted above. Ids are
    /// lineage-monotone, so a fork that inherited an empty context — a
    /// rewind at a ready boundary can leave one — still starts past its
    /// ancestry rather than back at 1.
    pub(crate) fn id_floor(&self) -> u64 {
        self.folded
            .reach()
            .unwrap_or(0)
            .max(self.ancestry.as_ref().map_or(0, |ancestry| ancestry.through))
    }

    /// The id the next prompt or assistant message takes, minted here and
    /// never chosen by a caller.
    pub(crate) fn next_id(&self) -> u64 {
        self.id_floor().saturating_add(1)
    }

    /// The newest exchange still in the context.
    pub fn last_context_exchange(&self) -> Option<u64> {
        self.folded.resident().last().map(|turn| turn.exchange)
    }

    pub fn log_len(&self) -> usize {
        self.ledger.len()
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.newest_edit.is_some_and(|index| index >= measured_at)
    }

    /// The exchange still owed a reply — one in flight, or one abandoned
    /// without ever getting there. An eviction concept no longer: this is the
    /// door's alone, refusing to read the exchange still being written.
    pub fn is_live_exchange(&self, id: u64) -> bool {
        !matches!(self.folded.state, State::ReadyForUser) && self.current_exchange() == Some(id)
    }

    pub(crate) fn is_awaiting_assistant(&self) -> bool {
        matches!(
            self.folded.state,
            State::AwaitingAssistantAfterUser | State::AwaitingAssistantAfterToolResults
        )
    }

    pub(crate) fn is_awaiting_steering(&self) -> bool {
        matches!(self.folded.state, State::AwaitingAssistantAfterToolResults)
    }

    pub(crate) fn pending_tool_results(&self) -> Option<Vec<String>> {
        match &self.folded.state {
            State::AwaitingToolResults { pending_ids } => Some(pending_ids.clone()),
            State::ReadyForUser
            | State::AwaitingAssistantAfterUser
            | State::AwaitingAssistantAfterToolResults => None,
        }
    }

    /// A human-readable stand-in for `{:?}` on the private [`State`] — used
    /// only in refusal messages, never matched on.
    pub(crate) fn state_description(&self) -> String {
        match &self.folded.state {
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
        quiesce_records(&self.folded.state, reason, self.next_id())
    }

    /// The parent context a `mnemon` child inherits, turn by turn under this
    /// log's own ids: the one other place ownership genuinely transfers,
    /// beside the wire door. The head marker is not among them — the child
    /// re-renders it from the table the link carries.
    ///
    /// The last turn seeds through [`admissible_prefix`], cut to the longest
    /// run that owes no tool result — a batch in flight and a dangling edit
    /// left after it both fall out of that same rule.
    pub(crate) fn inherited_seed(&self) -> Vec<(u64, u64, Vec<ChatMessage>)> {
        let resident: Vec<&Turn> = self.folded.resident().collect();
        let Some((last, held)) = resident.split_last() else {
            return Vec::new();
        };
        let mut seed: Vec<(u64, u64, Vec<ChatMessage>)> = held
            .iter()
            .map(|turn| (turn.id, turn.exchange, self.turn_messages(turn.id)))
            .collect();
        let events = self.turn_events(last.id);
        seed.push((
            last.id,
            last.exchange,
            admissible_prefix(&events)
                .iter()
                .map(|event| (*event).clone())
                .flat_map(into_chat_messages)
                .collect(),
        ));
        seed
    }

    /// The provider-facing context — recomputed from the protocol
    /// subsequence on every call, never accumulator state: settled turns hit
    /// the memo, and the exchange in hand is rendered as it lies.
    pub fn rendered(&mut self) -> Rendered {
        let mut rendered = Rendered::default();
        if let Some((segment, bytes)) = &self.head_render {
            rendered.push(Arc::clone(segment), *bytes);
        }
        let exchanges = self.folded.resident_exchanges();
        let Some((live, closed)) = exchanges.split_last() else {
            return rendered;
        };
        for (_, turns) in closed {
            self.push_closed(&mut rendered, turns);
        }
        // The exchange in hand renders whatever state it rests in: its last
        // turn may still be growing, and an abandoned exchange is not yet
        // abandoned while it is the one in hand.
        for turn in &live.1 {
            self.push_turn(&mut rendered, *turn);
        }
        rendered
    }

    /// A closed exchange: the one [`abandoned_note`] where its own fold never
    /// settles, otherwise its turns' renders in order.
    fn push_closed(&mut self, rendered: &mut Rendered, turns: &[u64]) {
        if self.is_settled(turns) {
            for turn in turns {
                self.push_turn(rendered, *turn);
            }
            return;
        }
        let message = ChatMessage::user(abandoned_note(&self.exchange_events(turns)));
        let bytes = message_bytes(std::slice::from_ref(&message));
        rendered.push(Arc::from(vec![message]), bytes);
    }

    /// One turn, at the weight the fold summed as its records landed — so a
    /// resident weight, a departed weight and a marker fragment's are one
    /// sum.
    fn push_turn(&mut self, rendered: &mut Rendered, turn: u64) {
        let bytes = self.folded.turn(turn).map_or(0, |row| row.bytes);
        let segment = Arc::clone(&self.render_turn(turn).segment);
        rendered.push(segment, bytes);
    }

    /// One turn's rendering, cached beside its own end slot. No flags
    /// parameter exists, so nothing can vary one: the memo's key is the whole
    /// of this function's input.
    fn render_turn(&mut self, turn: u64) -> &TurnRender {
        let events = self.ledger.events(turn);
        let stale = self
            .render
            .get(&turn)
            .is_none_or(|cached| cached.end != events.end);
        if stale {
            let messages = self.turn_messages(turn);
            let _ = self.render.insert(
                turn,
                TurnRender {
                    end: events.end,
                    segment: Arc::from(messages),
                },
            );
        }
        self.render.get(&turn).expect("just ensured present")
    }

    /// One turn's own material.
    fn turn_messages(&self, turn: u64) -> Vec<ChatMessage> {
        self.turn_events(turn)
            .into_iter()
            .cloned()
            .flat_map(into_chat_messages)
            .collect()
    }

    /// Re-render the head marker from the table the fold now holds — the one
    /// place [`Self::head_render`] is written, so it cannot drift from it.
    /// Whether there is a marker at all is [`render_head`]'s to say, so the
    /// two cannot disagree about it.
    fn recompute_head(&mut self) {
        self.head_render =
            render_head(&self.folded.turns, &self.folded.cuts).map(|text| {
                let message = ChatMessage::user(text);
                let bytes = message_bytes(std::slice::from_ref(&message));
                (Arc::from(vec![message]), bytes)
            });
    }

    /// # Panics
    /// Panics if a resident turn's slots are not resident in the ledger.
    fn turn_events(&self, turn: u64) -> Vec<&Protocol> {
        self.ledger
            .resident_events(self.ledger.events(turn))
            .expect("a resident turn's records are resident in the ledger")
    }

    fn exchange_events(&self, turns: &[u64]) -> Vec<&Protocol> {
        turns.iter().flat_map(|turn| self.turn_events(*turn)).collect()
    }

    /// Whether an exchange's own fold comes to rest — an interrupted one does
    /// not, and reads as its note.
    fn is_settled(&self, turns: &[u64]) -> bool {
        let settles = self
            .exchange_events(turns)
            .iter()
            .fold(State::default(), |state, event| advance(&state, event));
        matches!(settles, State::ReadyForUser)
    }

    /// Slots still owned by the context: the append-only ledger retains what
    /// an edit removes, so this is the host's resource probe's own count, not
    /// [`Self::log_len`].
    pub(crate) fn event_count(&self) -> usize {
        let events: usize = self
            .folded
            .resident()
            .map(|turn| self.ledger.events(turn.id).len())
            .sum();
        events + usize::from(self.head_render.is_some())
    }

    /// Approximate context size in serialised bytes — the fallback eviction
    /// trigger when the model's context window is unknown.
    pub(crate) fn history_bytes(&mut self) -> usize {
        self.rendered().byte_len()
    }

    /// One row per resident turn, at the weight the fold summed, beside the
    /// truth about what is sent: `total_bytes` is [`Self::history_bytes`] and
    /// not a second opinion on it, so an abandoned exchange's turns report
    /// their own weights while the context sends only its note.
    pub(crate) fn context_survey(&mut self) -> ContextSurvey {
        let total_bytes = self.history_bytes();
        ContextSurvey {
            rows: self.folded.resident().cloned().collect(),
            evicted: self
                .folded
                .turns
                .iter()
                .filter(|turn| matches!(turn.held, Held::Evicted))
                .count(),
            total_bytes,
        }
    }

    /// Every turn the transcript holds, in id order, each saying whether it
    /// is still in the context. The whole lineage's, since a fork inherits
    /// the table.
    pub(crate) fn transcript_index(&self) -> Vec<Turn> {
        self.folded.turns.clone()
    }

    /// Read closed turns in transcript order — the exchanges named outright,
    /// the turns a range covers, or both — as one [`TranscriptExchange`] per
    /// exchange touched, addressed by its own `exchange` field rather than by
    /// argument position.
    ///
    /// An exchange the read reaches whole renders through [`closed_messages`],
    /// so what comes back is what the model was sent, whether its turns are
    /// still in the context, departed to this log's own file, or an
    /// ancestor's; a read that reaches only part of one answers those turns'
    /// own material.
    ///
    /// Both halves in one call — [`Self::locate_read`] then
    /// [`TranscriptRead::exchanges`]; the desk keeps them apart so only the
    /// first runs under the session lock.
    ///
    /// # Errors
    /// Refuses a read that names nothing, a duplicate name, an exchange or
    /// range this lineage never recorded, the turn still being written, or a
    /// turn behind a broken link.
    pub(crate) fn read_transcript(
        &mut self,
        exchanges: &[u64],
        turns: Option<(u64, u64)>,
    ) -> Result<Vec<TranscriptExchange>, String> {
        self.locate_read(exchanges, turns)?.exchanges()
    }

    /// [`Self::read_transcript`]'s first half: resolve every turn the read names
    /// under the caller's lock, borrowing nothing, so the read itself can run
    /// once that lock is gone.
    ///
    /// # Errors
    /// Refuses whatever [`Self::read_transcript`] refuses.
    pub(crate) fn locate_read(
        &mut self,
        exchanges: &[u64],
        turns: Option<(u64, u64)>,
    ) -> Result<TranscriptRead, String> {
        if exchanges.is_empty() && turns.is_none() {
            return Err(
                "transcript `read` must name what to read — `exchanges: [n, …]`, \
                 `turns: [from, to]`, or both"
                    .into(),
            );
        }
        let selected = self.select_read(exchanges, turns)?;
        let mut read = Vec::with_capacity(selected.len());
        for (exchange, turns) in selected {
            let reached_whole = turns.len() == self.folded.exchange_turns(exchange).len();
            let mut located = Vec::with_capacity(turns.len());
            for turn in turns {
                located.push((turn, self.sourced(turn)?));
            }
            // A whole exchange renders as it was sent, but only where one file
            // holds it: a fork's imported records do not advance the protocol
            // its ancestor's own records do, so a mixture of the two would
            // fold as abandoned. Split across a fork, each turn answers its
            // own material.
            let own = self.ledger.source.clone();
            let one_file = located
                .windows(2)
                .all(|pair| pair[0].1.source(&own) == pair[1].1.source(&own));
            read.push(ExchangeRead {
                exchange,
                turns: located,
                rendering: if reached_whole && one_file {
                    Rendering::Whole
                } else {
                    Rendering::Turns
                },
            });
        }
        Ok(TranscriptRead { exchanges: read })
    }

    /// Which turns a read names, grouped by exchange in transcript order: the
    /// named exchanges' own turns, and every turn a range covers.
    ///
    /// # Errors
    /// Refuses an exchange [`Self::validate_read`] refuses, a range reaching
    /// past what is recorded, and one reaching the turn still being written.
    fn select_read(
        &self,
        exchanges: &[u64],
        turns: Option<(u64, u64)>,
    ) -> Result<Vec<(u64, Vec<u64>)>, String> {
        self.validate_read(exchanges)?;
        if let Some((from, to)) = turns {
            if Some(to) > self.folded.reach() {
                return Err(not_recorded_refusal_turn(to, self.folded.reach()));
            }
            if let Some(open) = self.unclosed_turn().filter(|open| (from..=to).contains(open))
                && let Some(turn) = self.folded.turn(open)
            {
                return Err(self.live_exchange_refusal(turn.exchange));
            }
        }
        let selected = self.select(exchanges, turns);
        // A validated exchange always has turns of its own, so an empty
        // selection means the range itself began past the reach.
        match (selected.is_empty(), turns) {
            (true, Some((from, _))) => {
                Err(not_recorded_refusal_turn(from, self.folded.reach()))
            }
            _ => Ok(selected),
        }
    }

    /// Every turn a door names — one whose exchange is named, or that a range
    /// covers — grouped by exchange in id order. The turn still being written
    /// is never selected: its exchange's earlier turns have closed, and it
    /// has not.
    fn select(&self, exchanges: &[u64], turns: Option<(u64, u64)>) -> Vec<(u64, Vec<u64>)> {
        let unclosed = self.unclosed_turn();
        let mut selected: Vec<(u64, Vec<u64>)> = Vec::new();
        let named = self.folded.turns.iter().filter(|turn| {
            Some(turn.id) != unclosed
                && (exchanges.contains(&turn.exchange)
                    || turns.is_some_and(|(from, to)| (from..=to).contains(&turn.id)))
        });
        for turn in named {
            match selected.last_mut() {
                Some((exchange, ids)) if *exchange == turn.exchange => ids.push(turn.id),
                _ => selected.push((turn.exchange, vec![turn.id])),
            }
        }
        selected
    }

    /// The turn still being written, when the protocol is mid-exchange: the
    /// one turn no door reads back, its exchange's earlier turns having
    /// closed.
    fn unclosed_turn(&self) -> Option<u64> {
        (!matches!(self.folded.state, State::ReadyForUser))
            .then(|| self.folded.reach())
            .flatten()
    }

    /// One turn's records, or the file and stamps to read them back by —
    /// wherever the lineage keeps them. This log's own ledger answers for
    /// every turn it recorded; the ancestry answers for one it never did, so
    /// an exchange a fork left in two files reads back out of both.
    ///
    /// # Errors
    /// Refuses a turn no lineage record answers for, and one behind a broken
    /// link.
    fn sourced(&mut self, turn: u64) -> Result<Sourced, String> {
        let range = self.ledger.events(turn);
        if !range.is_empty() {
            // Cloned here rather than read back when every slot is still
            // resident: the records are already in memory.
            if let Some(events) = self.ledger.resident_events(range.clone()) {
                return Ok(Sourced::Held(events.into_iter().cloned().collect()));
            }
            let stamps = self.ledger.stamps(range).ok_or_else(|| {
                format!(
                    "turn {turn} names a record slot the ledger has neither resident nor freed — the fold's own invariant broke"
                )
            })?;
            return Ok(Sourced::Stamped {
                source: self.ledger.source.clone(),
                stamps,
            });
        }
        self.ancestral(turn)
    }

    /// One inherited turn this log never re-recorded, off the ancestor's own
    /// file.
    fn ancestral(&mut self, turn: u64) -> Result<Sourced, String> {
        let unreached = not_recorded_refusal_turn(turn, self.folded.reach());
        let index = self.ancestor_index();
        let Some(held) = index.turns.get(&turn) else {
            return Err(index
                .broken
                .as_ref()
                .map_or(unreached, |broken| broken.refusal(turn)));
        };
        Ok(Sourced::Stamped {
            source: held.source.clone(),
            stamps: held.stamps.clone(),
        })
    }

    /// The lineage's foreign placement, re-walked whenever no cached index
    /// exists or the one cached broke: O(ancestor file) then, and free
    /// otherwise.
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

    /// Search the closed turns' text: the ones a narrowing names, or the
    /// whole transcript. What the ledger holds is searched first, then the
    /// ancestry's own — each file in one pass in stamp order — and every hit
    /// names the turn it lies in.
    ///
    /// An unreadable turn is refused when a narrowing named it and passed
    /// over when the whole transcript was searched: the head marker has
    /// already told the model which of its turns it cannot have back.
    ///
    /// Both halves in one call — [`Self::locate_grep`] then
    /// [`TranscriptRead::grep`]; the desk keeps them apart so only the first
    /// runs under the session lock.
    ///
    /// # Errors
    /// Refuses a narrowing [`Self::validate_read`] or [`Self::select_read`]
    /// refuses, and any turn it names and cannot reach.
    pub(crate) fn grep_transcript(
        &mut self,
        pattern: &Regex,
        exchanges: Option<&[u64]>,
        turns: Option<(u64, u64)>,
    ) -> Result<GrepAnswer, String> {
        self.locate_grep(exchanges, turns)?.grep(pattern)
    }

    /// [`Self::grep_transcript`]'s first half: where every searchable turn
    /// lies, resolved under the caller's lock and borrowing nothing.
    ///
    /// A turn is searched when a narrowing names its exchange or covers its
    /// id; with no narrowing at all, the whole transcript is.
    ///
    /// # Errors
    /// Refuses whatever [`Self::grep_transcript`] refuses.
    pub(crate) fn locate_grep(
        &mut self,
        exchanges: Option<&[u64]>,
        turns: Option<(u64, u64)>,
    ) -> Result<TranscriptRead, String> {
        if exchanges.is_some_and(<[u64]>::is_empty) && turns.is_none() {
            return Err(
                "transcript `grep`: `exchanges` names no exchange — omit it to search the \
                 whole transcript"
                    .into(),
            );
        }
        let narrowed = exchanges.is_some() || turns.is_some();
        let searchable = if narrowed {
            self.select_read(exchanges.unwrap_or_default(), turns)?
        } else {
            self.select(&self.every_exchange(), None)
        };
        let mut read = Vec::with_capacity(searchable.len());
        for (exchange, turns) in searchable {
            let mut located = Vec::with_capacity(turns.len());
            for turn in turns {
                match self.sourced(turn) {
                    Ok(sourced) => located.push((turn, sourced)),
                    // A narrowing answers for itself; the whole transcript
                    // passes over what a broken link put out of reach.
                    Err(refusal) if narrowed => return Err(refusal),
                    Err(_) => {}
                }
            }
            if !located.is_empty() {
                read.push(ExchangeRead {
                    exchange,
                    turns: located,
                    rendering: Rendering::Turns,
                });
            }
        }
        Ok(TranscriptRead { exchanges: read })
    }

    /// Every exchange the table holds — how an unnarrowed `` `grep `` names
    /// the whole transcript.
    fn every_exchange(&self) -> Vec<u64> {
        let mut exchanges: Vec<u64> = self
            .folded
            .turns
            .iter()
            .map(|turn| turn.exchange)
            .collect();
        exchanges.dedup();
        exchanges
    }

    /// Every named exchange must be closed, named once, and one this lineage
    /// recorded.
    fn validate_read(&self, exchanges: &[u64]) -> Result<(), String> {
        let mut named = HashSet::with_capacity(exchanges.len());
        for &exchange in exchanges {
            if !named.insert(exchange) {
                return Err(format!("exchange {exchange} was named more than once"));
            }
            if self.is_live_exchange(exchange) {
                return Err(self.live_exchange_refusal(exchange));
            }
            if self.folded.exchange_turns(exchange).is_empty() {
                return Err(not_recorded_refusal(exchange, self.folded.reach()));
            }
        }
        Ok(())
    }

    /// The exchange still being written is refused by name — and told which
    /// of its own turns have closed, since those are readable.
    fn live_exchange_refusal(&self, exchange: u64) -> String {
        let turns = self.folded.exchange_turns(exchange);
        let closed = &turns[..turns.len().saturating_sub(1)];
        match (closed.first(), closed.last()) {
            (Some(from), Some(to)) => format!(
                "exchange {exchange} is still in progress — its closed turns {from}–{to} are readable with `transcript `read [turns: [{from}, {to}]]`"
            ),
            _ => format!("exchange {exchange} is still in progress — no turn of it has closed yet"),
        }
    }

    /// Resolve a user rewind into the whole visible suffix beginning at its
    /// anchor. The anchor is checked before the suffix is derived.
    ///
    /// # Errors
    /// Refuses an absent anchor or one that has already left the context.
    pub(crate) fn rewind_exchanges(&self, anchor: u64) -> Result<Vec<u64>, String> {
        let mut exchanges: Vec<u64> = Vec::new();
        for turn in self.folded.resident() {
            if turn.exchange >= anchor && exchanges.last() != Some(&turn.exchange) {
                exchanges.push(turn.exchange);
            }
        }
        if exchanges.first() != Some(&anchor) {
            if self.folded.turn(anchor).is_some() {
                return Err(self.departed_refusal(anchor));
            }
            return Err(rewind_unknown_refusal(anchor, self.last_context_exchange()));
        }
        Ok(exchanges)
    }

    /// # Errors
    /// Refuses an unnamed edit, an unaddressable target, or the work in hand.
    pub(crate) fn validate_edit(&self, op: &ContextOp) -> Result<(), String> {
        match op {
            ContextOp::Evict { through, .. } => self.validate_cut(*through),
            ContextOp::Drop { exchanges } => {
                if exchanges.is_empty() {
                    return Err("a context edit must name at least one exchange".into());
                }
                let mut named = HashSet::with_capacity(exchanges.len());
                for &exchange in exchanges {
                    if !named.insert(exchange) {
                        return Err(format!("exchange {exchange} was named more than once"));
                    }
                    self.validate_droppable(exchange)?;
                }
                Ok(())
            }
        }
    }

    /// A cut names a turn still in the context, and never the newest one: an
    /// eviction exists to keep the work in hand.
    fn validate_cut(&self, through: u64) -> Result<(), String> {
        let Some(turn) = self.folded.turn(through) else {
            return Err(not_recorded_refusal_turn(through, self.folded.reach()));
        };
        if !turn.is_resident() {
            return Err(self.departed_turn_refusal(through));
        }
        if self.folded.resident().last().is_some_and(|last| last.id == through) {
            return Err(format!(
                "{through} is the newest turn; an eviction keeps the work in hand"
            ));
        }
        Ok(())
    }

    /// An exchange is droppable iff some turn of it is still in the context
    /// and it is not the one being written.
    fn validate_droppable(&self, exchange: u64) -> Result<(), String> {
        if !self
            .folded
            .resident()
            .any(|turn| turn.exchange == exchange)
        {
            if self.folded.exchange_turns(exchange).is_empty() {
                return Err(format!("exchange {exchange} is not present in your context"));
            }
            return Err(self.departed_refusal(exchange));
        }
        if self.is_live_exchange(exchange) {
            return Err(format!(
                "exchange {exchange} is the one you are in — a context edit may only name closed exchanges"
            ));
        }
        Ok(())
    }

    fn departed_refusal(&self, exchange: u64) -> String {
        match self.folded.resident().next() {
            Some(first) => format!(
                "exchange {exchange} has already left your context — the earliest still in it is {}",
                first.exchange
            ),
            None => format!(
                "exchange {exchange} has already left your context — no exchange is still in it"
            ),
        }
    }

    fn departed_turn_refusal(&self, turn: u64) -> String {
        match self.folded.resident().next() {
            Some(first) => format!(
                "turn {turn} has already left your context — the earliest still in it is {}",
                first.id
            ),
            None => {
                format!("turn {turn} has already left your context — no turn is still in it")
            }
        }
    }

    /// The cut an eviction would make to spend no more than `keep` bytes on
    /// what stays, `None` when nothing is old enough to shed.
    ///
    /// Candidates are every turn in the context but the last, so the work in
    /// hand is unnameable rather than merely unlikely. The last turn's own
    /// weight is spent up front, and its user turn's with it: a user turn
    /// stays while its exchange has an assistant turn in context, so that one
    /// is never a saving. For the same reason a user turn that overshoots
    /// cannot leave alone — its whole exchange does, the cut falling through
    /// the exchange's newest resident turn.
    pub(crate) fn plan_eviction(&self, keep: usize) -> Option<u64> {
        let resident: Vec<&Turn> = self.folded.resident().collect();
        let (last, candidates) = resident.split_last()?;
        let mut spent = last.bytes;
        let paid = (!last.is_user()).then_some(last.exchange);
        if let Some(user) = paid.and_then(|id| self.folded.turn(id)) {
            spent = spent.saturating_add(user.bytes);
        }
        for turn in candidates.iter().rev() {
            if paid == Some(turn.id) {
                continue;
            }
            let total = spent.saturating_add(turn.bytes);
            if total > keep {
                let through = if turn.is_user() {
                    candidates
                        .iter()
                        .rev()
                        .find(|t| t.exchange == turn.id)
                        .map_or(turn.id, |t| t.id)
                } else {
                    turn.id
                };
                // A cut that would take nothing is no plan: the work in hand
                // and the user turn it belongs to already fill the budget.
                return (!cut_departures(&self.folded.turns, through).is_empty())
                    .then_some(through);
            }
            spent = total;
        }
        None
    }
}

/// What a turn's opening line is clipped to as it opens, and the column the
/// head marker pads that label to. One measure, so no reader of a row — the
/// marker's own table, a survey, the transcript index, or an ancestor's table
/// as it lands in a child's log — holds or aligns a different length.
const OPENING_CHARS: usize = 50;

fn opening_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(OPENING_CHARS)
        .collect()
}

fn message_label(message: &ChatMessage) -> String {
    opening_line(message.content.first_text().unwrap_or_default())
}

/// A closed exchange's messages — or, when its own fold never settles, the
/// one note standing for the exchange it abandoned. The single rendering a
/// read-back goes through, so a record read off disk answers byte-for-byte
/// what the model was sent.
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

/// Walk a lineage from one link, indexing where every turn it can reach
/// lies: one pass per ancestor file, each stopping at that link's own
/// `through`, and following the ancestor's own [`Protocol::Inherited`] on to
/// the grandparent. The nearest copy of an id wins, an ancestor's imported
/// turns being the same turns under the same numbers.
fn index_ancestry(ancestry: &Ancestry) -> AncestorIndex {
    let mut index = AncestorIndex::default();
    let mut link = Some(ancestry.clone());
    while let Some(Ancestry { source, through }) = link {
        match index_ancestor(&source, through, &mut index.turns) {
            Ok(next) => link = next,
            Err(Broken { fault, error }) => {
                index.broken = Some(Break {
                    source,
                    fault,
                    error: error.to_string(),
                });
                return index;
            }
        }
    }
    index
}

/// One ancestor file's own turns through `through`, partitioned by the same
/// rule the live fold partitions by: the transcript is every turn the file
/// recorded, whatever that session later shed from its context. Answers the
/// link to its own parent, when the walk met one.
fn index_ancestor(
    path: &Path,
    through: u64,
    turns: &mut BTreeMap<u64, AncestorTurn>,
) -> Result<Option<Ancestry>, Broken> {
    let mut folded = Folded::default();
    let mut link = None;
    let records = Log::read(path).map_err(|error| Broken {
        fault: Fault::Open,
        error,
    })?;
    for (line, record) in records.enumerate() {
        let record = record.map_err(|error| Broken {
            fault: Fault::Record(line + 1),
            error,
        })?;
        let Record::Protocol(protocol) = record.value() else {
            continue;
        };
        // Everything at or below the link's reach was recorded before the
        // fork; the first turn past it ends the pass.
        if borne_id(protocol).is_some_and(|id| id > through) {
            break;
        }
        let placed = record_turn(&mut folded, protocol);
        if let Protocol::Inherited {
            source,
            through,
            turns: table,
            cuts,
        } = protocol
        {
            inherit(&mut folded, table, cuts);
            link = Some(Ancestry {
                source: source.clone(),
                through: *through,
            });
        }
        if let Some(id) = placed {
            let held = turns.entry(id).or_insert_with(|| AncestorTurn {
                source: path.to_path_buf(),
                stamps: Vec::new(),
            });
            // The nearest copy of an id wins, so a farther file adds nothing
            // to an id one already answers for.
            if held.source == path {
                held.stamps.push(record.stamp().clone());
            }
        }
    }
    Ok(link)
}

/// Hits one `` `grep `` answers with; `total` still counts them all.
const GREP_HITS: usize = 100;

/// What a hit's line is clipped to, in bytes.
const GREP_TEXT_BYTES: usize = 200;

/// Transcript order: turn, then message within it, then line within that.
type HitKey = (u64, usize, usize);

/// The oldest [`GREP_HITS`] matches in transcript order, and how many there
/// were in all — bounded, so a pattern that matches everything costs one
/// answer rather than the whole transcript.
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

/// One hit per matching line of one turn's messages.
fn grep_messages(
    pattern: &Regex,
    exchange: u64,
    turn: u64,
    messages: &[ChatMessage],
    tally: &mut Tally,
) {
    for (position, message) in messages.iter().enumerate() {
        for (index, line) in searched_text(message).lines().enumerate() {
            if pattern.is_match(line) {
                tally.offer(
                    (turn, position, index),
                    GrepHit {
                        exchange,
                        turn,
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

fn not_recorded_refusal(exchange: u64, reach: Option<u64>) -> String {
    match reach {
        Some(reach) => format!("exchange {exchange} is not recorded — the latest turn is {reach}"),
        None => format!("exchange {exchange} is not recorded — nothing has been recorded yet"),
    }
}

fn not_recorded_refusal_turn(turn: u64, reach: Option<u64>) -> String {
    match reach {
        Some(reach) => format!("turn {turn} is not recorded — the latest is {reach}"),
        None => format!("turn {turn} is not recorded — nothing has been recorded yet"),
    }
}

fn read_back_refusal(turn: u64, path: &Path, error: &io::Error) -> String {
    format!(
        "turn {turn} could not be read back from {}: {error}",
        path.display()
    )
}

fn rewind_unknown_refusal(id: u64, last: Option<u64>) -> String {
    match last {
        Some(last) => {
            format!("exchange {id} is not present in your context — the last exchange is {last}")
        }
        None => format!(
            "exchange {id} is not present in your context — there is no last exchange to rewind"
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
        | Protocol::TurnStarted { .. }
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
        | Protocol::TurnStarted { .. }
        | Protocol::ContextEdited { .. }
        | Protocol::Inherited { .. } => Vec::new(),
    }
}

/// The id a record bears, whatever it names: a prompt's exchange, an
/// assistant message's turn, an imported message's turn.
fn borne_id(protocol: &Protocol) -> Option<u64> {
    match protocol {
        Protocol::UserPrompt { exchange, .. } => Some(*exchange),
        Protocol::AssistantMessage { turn, .. } => Some(*turn),
        Protocol::ContextMessage { id, .. } => Some(*id),
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::TurnStarted { .. }
        | Protocol::ToolResults { .. }
        | Protocol::ContextEdited { .. }
        | Protocol::Inherited { .. } => None,
    }
}

/// The partition half of the step: which turn a record's slot belongs to,
/// opening one where the record bears an id the table has not reached.
///
/// A prompt past the reach opens an exchange; one at or below it is steering,
/// and extends the assistant turn it answers. An imported message names its
/// turn outright, the table having arrived whole with the link, and only a
/// note that inherits nothing opens one of its own.
fn record_turn(folded: &mut Folded, protocol: &Protocol) -> Option<u64> {
    let reach = folded.reach();
    let past = |id: u64| Some(id) > reach;
    let opened = match protocol {
        Protocol::UserPrompt { exchange, text } => past(*exchange)
            .then(|| (*exchange, *exchange, TurnKind::Exchange, opening_line(text))),
        Protocol::AssistantMessage { turn, message, .. } => past(*turn)
            .then(|| {
                folded.turns.last().map(|held| {
                    (
                        *turn,
                        held.exchange,
                        TurnKind::Exchange,
                        message_label(message),
                    )
                })
            })
            .flatten(),
        Protocol::ContextMessage {
            id,
            exchange,
            message,
        } => {
            if folded.turn(*id).is_some() {
                return Some(*id);
            }
            past(*id).then(|| (*id, *exchange, TurnKind::Import, message_label(message)))
        }
        Protocol::SessionStarted { .. }
        | Protocol::SessionResumed { .. }
        | Protocol::SessionEnded
        | Protocol::TurnStarted { .. }
        | Protocol::ToolResults { .. }
        | Protocol::ContextEdited { .. }
        | Protocol::Inherited { .. } => None,
    };
    match opened {
        Some((id, exchange, kind, label)) => {
            folded.turns.push(Turn {
                id,
                exchange,
                kind,
                label,
                bytes: 0,
                held: Held::Resident,
            });
            Some(id)
        }
        None => reach,
    }
}

/// A link's own step on the table: the parent's turns and cuts become this
/// log's, so the child's head marker is the same projection of the same fold.
/// A resident turn's weight is zeroed because its own records follow, and
/// the fold sums them as they land.
fn inherit(folded: &mut Folded, turns: &[Turn], cuts: &[Cut]) {
    folded.turns = turns
        .iter()
        .map(|turn| Turn {
            bytes: if turn.is_resident() { 0 } else { turn.bytes },
            ..turn.clone()
        })
        .collect();
    folded.cuts = cuts.to_vec();
}

/// A cut at `through` takes every turn in the context at or below it, but a
/// user turn whose exchange still has an assistant turn above the cut: that
/// one stays with its survivors.
fn cut_departures(turns: &[Turn], through: u64) -> Vec<u64> {
    turns
        .iter()
        .filter(|turn| turn.is_resident() && turn.id <= through)
        .filter(|turn| !(turn.is_user() && has_survivor(turns, turn.exchange, through)))
        .map(|turn| turn.id)
        .collect()
}

fn has_survivor(turns: &[Turn], exchange: u64, through: u64) -> bool {
    turns
        .iter()
        .any(|turn| turn.is_resident() && turn.exchange == exchange && turn.id > through)
}

/// The turns `op` takes out of the context, marked as they leave. Shared by
/// [`step_protocol`] and [`refold`], so a resume cannot disagree with the
/// session it replays.
fn apply_context_op(folded: &mut Folded, op: &ContextOp) -> Vec<u64> {
    let (left, held) = match op {
        ContextOp::Evict { through, .. } => {
            (cut_departures(&folded.turns, *through), Held::Evicted)
        }
        ContextOp::Drop { exchanges } => (
            folded
                .turns
                .iter()
                .filter(|turn| turn.is_resident() && exchanges.contains(&turn.exchange))
                .map(|turn| turn.id)
                .collect(),
            Held::Dropped,
        ),
    };
    let leaving: HashSet<u64> = left.iter().copied().collect();
    for turn in &mut folded.turns {
        if leaving.contains(&turn.id) {
            turn.held = held;
        }
    }
    if let ContextOp::Evict { through, note } = op {
        folded.cuts.push(Cut {
            through: *through,
            note: note.clone(),
        });
    }
    left
}

/// Protocol sequencing legality — a runtime guard for data read off disk,
/// never consulted by [`Fold::step`] itself: live authorship is already
/// typestate-correct, so the only place this can fail is [`Admission`],
/// walking a file this process did not just write.
fn admissible(state: &State, protocol: &Protocol) -> bool {
    match protocol {
        Protocol::UserPrompt { .. } | Protocol::ContextMessage { .. } => admits_new_turn(state),
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
        | Protocol::TurnStarted { .. }
        | Protocol::ContextEdited { .. }
        // Sequencing-neutral; where a link may stand is [`Admission::step`]'s
        // own rule, needing more than a [`State`].
        | Protocol::Inherited { .. } => true,
    }
}

/// Whether a record that opens an exchange may follow. Only outstanding tool
/// calls forbid it: an exchange the model never replied to is abandoned by
/// the next prompt, not closed by a fabricated reply.
fn admits_new_turn(state: &State) -> bool {
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
        if admits_new_turn(&state) {
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

/// Exchange fragments the head marker draws a row each for; everything older
/// collapses into one line naming the range.
const HEAD_ROWS: usize = 40;

/// The exchange-id column both a row and the collapse line are set in.
const HEAD_ID: usize = 4;

/// The turn-range column a row's assistant turns are set in.
const HEAD_TURNS: usize = 9;

/// One exchange's contiguous run of turns inside one cut — the row the head
/// marker draws. One exchange may appear in two fragments across two cuts.
struct Fragment {
    exchange: u64,
    ids: (u64, u64),
    bytes: usize,
}

impl Fragment {
    /// The assistant turns' id range, or the user turn's own id where the
    /// fragment is that turn alone.
    fn range(&self) -> String {
        let (mut from, to) = self.ids;
        if from == self.exchange && to > self.exchange {
            from = self.exchange + 1;
        }
        if from == to {
            return from.to_string();
        }
        format!("{from}–{to}")
    }
}

/// What the marker draws: one entry per cut that still has a fragment on
/// screen, with its own fragments and its own note. One shape, so a note
/// cannot survive the rows it belongs to.
struct Drawn<'a> {
    rows: &'a [Fragment],
    note: Option<&'a str>,
}

/// Each cut's fragments, in order: the turns it evicted, grouped into
/// contiguous runs sharing an exchange. A cut's own reach bounds it below by
/// the previous cut's.
fn cut_fragments(turns: &[Turn], cuts: &[Cut]) -> Vec<(Vec<Fragment>, Option<String>)> {
    let mut fragments = Vec::with_capacity(cuts.len());
    let mut lower = 0u64;
    for cut in cuts {
        let mut rows: Vec<Fragment> = Vec::new();
        for turn in turns.iter().filter(|turn| {
            matches!(turn.held, Held::Evicted) && turn.id > lower && turn.id <= cut.through
        }) {
            match rows.last_mut() {
                Some(row) if row.exchange == turn.exchange => {
                    row.ids.1 = turn.id;
                    row.bytes = row.bytes.saturating_add(turn.bytes);
                }
                _ => rows.push(Fragment {
                    exchange: turn.exchange,
                    ids: (turn.id, turn.id),
                    bytes: turn.bytes,
                }),
            }
        }
        lower = cut.through;
        fragments.push((rows, cut.note.clone()));
    }
    fragments
}

/// The head marker: the one user-voice message standing where an evicted
/// prefix was, indexing every turn that has left so the model can ask for any
/// of them back. `None` when no cut carries a fragment, which is the one
/// notion of "no marker" there is.
///
/// A pure function of the table and its cuts — no clock, no path, no
/// ordering-unstable container — so between two cuts the provider's prompt
/// cache sees a byte-stable message 0.
///
/// Voice and bracket as [`abandoned_note`]: the harness may state a fact
/// about the conversation, never speak in the model's own voice.
fn render_head(turns: &[Turn], cuts: &[Cut]) -> Option<String> {
    let fragments = cut_fragments(turns, cuts);
    let rows: Vec<&Fragment> = fragments
        .iter()
        .flat_map(|(rows, _)| rows.iter())
        .collect();
    if rows.is_empty() {
        return None;
    }
    let departed = departed_sentence(turns, &rows);
    let whereabouts = "They are still readable: `transcript `read [turns: [a, b]]` or \
                       `[exchanges: [n]]` reads them back as material, `transcript `grep \
                       [pattern: 're']` searches them all, `transcript `index` lists every \
                       turn the transcript holds.";
    let mut lines = vec![format!("[EXARCH // {departed} {whereabouts}")];
    let collapsed = rows.len().saturating_sub(HEAD_ROWS);
    if collapsed > 0 {
        let range = format!("{}–{}", rows[0].exchange, rows[collapsed - 1].exchange);
        lines.push(format!(
            "{range:>HEAD_ID$}  ({collapsed} earlier exchanges — transcript `index)"
        ));
    }
    for drawn in drawn_cuts(&fragments, collapsed) {
        for row in drawn.rows {
            lines.push(head_row(turns, row));
        }
        if let Some(note) = drawn.note {
            // `Debug`-quoted, so no note — the model's own, or one inherited
            // off an ancestor's link — can add a line to the marker.
            lines.push(format!("Your note at eviction: {note:?}"));
        }
    }
    Some(format!("{}]", lines.join("\n")))
}

/// The cuts still on screen past the collapse: each kept to its own surviving
/// suffix of fragments, its note carried along only when a fragment of its
/// own survives.
fn drawn_cuts(
    fragments: &[(Vec<Fragment>, Option<String>)],
    collapsed: usize,
) -> Vec<Drawn<'_>> {
    let mut drawn = Vec::new();
    let mut seen = 0usize;
    for (rows, note) in fragments {
        let start = collapsed.saturating_sub(seen).min(rows.len());
        seen += rows.len();
        if start < rows.len() {
            drawn.push(Drawn {
                rows: &rows[start..],
                note: note.as_deref(),
            });
        }
    }
    drawn
}

/// The marker's opening: which exchanges left whole, and which left only some
/// of their turns.
fn departed_sentence(turns: &[Turn], rows: &[&Fragment]) -> String {
    let mut whole: Vec<u64> = Vec::new();
    let mut partial: Vec<String> = Vec::new();
    let mut seen: Vec<u64> = Vec::new();
    for row in rows {
        if seen.contains(&row.exchange) {
            continue;
        }
        seen.push(row.exchange);
        if !turns
            .iter()
            .any(|turn| turn.exchange == row.exchange && turn.is_resident())
        {
            whole.push(row.exchange);
            continue;
        }
        let gone: Vec<u64> = rows
            .iter()
            .filter(|other| other.exchange == row.exchange)
            .flat_map(|other| [other.ids.0, other.ids.1])
            .collect();
        let (from, to) = (
            gone.iter().min().copied().unwrap_or(row.ids.0),
            gone.iter().max().copied().unwrap_or(row.ids.1),
        );
        partial.push(if from == to {
            format!("turn {from} of exchange {}", row.exchange)
        } else {
            format!("turns {from}–{to} of exchange {}", row.exchange)
        });
    }
    if let (Some(first), Some(last)) = (whole.first(), whole.last()) {
        let clause = if first == last {
            format!("Exchange {first} has")
        } else {
            format!("Exchanges {first}–{last} have")
        };
        let mut sentence = format!("{clause} left your context");
        if !partial.is_empty() {
            sentence += ", and ";
            sentence += &join_and(&partial);
        }
        sentence.push('.');
        return sentence;
    }
    let mut opening = join_and(&partial);
    let verb = if partial.len() == 1 && opening.starts_with("turn ") {
        "has"
    } else {
        "have"
    };
    if let Some(first) = opening.get(..1).map(str::to_uppercase) {
        opening.replace_range(..1, &first);
    }
    format!("{opening} {verb} left your context.")
}

fn join_and(phrases: &[String]) -> String {
    match phrases.split_last() {
        None => String::new(),
        Some((last, [])) => last.clone(),
        Some((last, head)) => format!("{}, and {last}", head.join(", ")),
    }
}

fn head_row(turns: &[Turn], fragment: &Fragment) -> String {
    let label = turns
        .iter()
        .find(|turn| turn.id == fragment.exchange)
        .map_or("", |turn| turn.label.as_str());
    format!(
        "{:>HEAD_ID$}  {:<OPENING_CHARS$}{:>HEAD_TURNS$} {:>5} KB",
        fragment.exchange,
        label,
        fragment.range(),
        fragment.bytes / 1024
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
/// left exactly as it lies, and [`Memo::push_closed`] reads it as its note
/// once it closes, so the model never reads a turn it never took.
fn quiesce_records(state: &State, reason: QuiesceReason, turn: u64) -> Vec<Protocol> {
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
            turn,
            message: ChatMessage::assistant("[EXARCH // Exchange ended: replied to parent.]"),
            pending_tool_ids: Vec::new(),
            stop_reason: Some("replied".into()),
        });
    }
    records
}

/// One step of the fold: append to the ledger under the turn the record
/// belongs to, sum its weight onto that turn, and free whatever no surviving
/// turn needs any more — the law that no recorded protocol record is ever
/// truly discarded holds because `free` keeps the departed slot's `Stamp`,
/// never drops it.
fn step_protocol(memo: &mut Memo, stamp: Stamp, protocol: &Protocol) {
    memo.folded.state = advance(&memo.folded.state, protocol);
    // Before `inherit`, so the link's own slot belongs to no turn: the
    // parent's table arrives with it, and the imported messages that follow
    // are what make its turns this log's.
    let placed = record_turn(&mut memo.folded, protocol);
    let index = memo
        .ledger
        .append(Recorded::new(stamp, protocol.clone()), placed);
    if let Protocol::Inherited {
        source,
        through,
        turns,
        cuts,
    } = protocol
    {
        inherit(&mut memo.folded, turns, cuts);
        memo.ancestry = Some(Ancestry {
            source: source.clone(),
            through: *through,
        });
        memo.recompute_head();
    }
    if let Some(id) = placed {
        let bytes = message_bytes(&into_chat_messages(protocol.clone()));
        if let Some(turn) = memo.folded.turns.iter_mut().find(|turn| turn.id == id) {
            turn.bytes = turn.bytes.saturating_add(bytes);
        }
    }
    if let Protocol::ContextEdited { op, .. } = protocol {
        for id in apply_context_op(&mut memo.folded, op) {
            memo.ledger.free_turn(id);
        }
        if matches!(op, ContextOp::Evict { .. }) {
            memo.recompute_head();
        }
        memo.newest_edit = Some(index);
        // Memory hygiene, not correctness: a stale render can never be read
        // back, since a hit demands `end` to match a turn still in context.
        let held: HashSet<u64> = memo.folded.resident().map(|turn| turn.id).collect();
        memo.render.retain(|id, _| held.contains(id));
    }
    // An edit can take out the very turn its own record landed on.
    if placed.is_some_and(|id| memo.folded.turn(id).is_some_and(|turn| !turn.is_resident())) {
        memo.ledger.free(index);
    }
}

/// Re-derive the [`Folded`] table from the ledger's own content alone,
/// resident or freed — the `fold == memo` law [`resume`] runs against the
/// incrementally maintained memo, so an eviction that silently lost or
/// misplaced a record is caught rather than trusted. Every turn's weight is
/// part of that check, so a misplaced record shows up as a byte mismatch.
fn refold(ledger: &Ledger) -> io::Result<Folded> {
    let mut folded = Folded::default();
    for protocol in ledger.fold_events()? {
        folded.state = advance(&folded.state, &protocol);
        let placed = record_turn(&mut folded, &protocol);
        if let Protocol::Inherited { turns, cuts, .. } = &protocol {
            inherit(&mut folded, turns, cuts);
        }
        if let Some(id) = placed {
            let bytes = message_bytes(&into_chat_messages(protocol.clone()));
            if let Some(turn) = folded.turns.iter_mut().find(|turn| turn.id == id) {
                turn.bytes = turn.bytes.saturating_add(bytes);
            }
        }
        if let Protocol::ContextEdited { op, .. } = &protocol {
            let _ = apply_context_op(&mut folded, op);
        }
    }
    Ok(folded)
}

/// The protocol sequencing law, replayed record by record beside the fold that
/// [`resume`] is building: interposing the records a live `quiesce` would have
/// written wherever a record is inadmissible but repairable — a tool-result
/// batch lost from the file — and refusing outright wherever it is not:
/// foreign data no live door could have produced.
///
/// Its state and table are its own, never the fold's, so admission stays an
/// independent judgement on the file rather than a self-agreement of the memo.
#[derive(Default)]
struct Admission {
    folded: Folded,
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
        let foreign = |reason: String| Refusal::Foreign {
            record: Box::new(Record::Protocol(protocol.clone())),
            reason,
        };
        if !admissible(&self.folded.state, protocol) {
            if !stub_repairable(protocol) {
                return Err(foreign(format!(
                    "record {} is foreign protocol data, not a seam that quiesce can repair; was record.jsonl hand-edited or written by an incompatible exarch?",
                    index + 1
                )));
            }
            for record in quiesce_records(
                &self.folded.state,
                QuiesceReason::Aborted,
                self.folded.reach().unwrap_or(0).saturating_add(1),
            ) {
                self.folded.state = advance(&self.folded.state, &record);
            }
        }
        if matches!(protocol, Protocol::Inherited { .. }) {
            if self.inherited
                || self.folded.reach().is_some()
                || !matches!(self.folded.state, State::ReadyForUser)
            {
                return Err(foreign(format!(
                    "record {} links this log to an ancestor's transcript, which only a fork's opening may do; by here the log has a context of its own, and no live session inherits twice",
                    index + 1
                )));
            }
            self.inherited = true;
        }
        if let Some(id) = self.stale(protocol) {
            return Err(foreign(format!(
                "record {} names turn {id}, which the log had already moved past; no live session records a stale id",
                index + 1
            )));
        }
        self.folded.state = advance(&self.folded.state, protocol);
        let _ = record_turn(&mut self.folded, protocol);
        if let Protocol::Inherited { turns, cuts, .. } = protocol {
            inherit(&mut self.folded, turns, cuts);
        }
        if let Protocol::ContextEdited { op, .. } = protocol {
            let _ = apply_context_op(&mut self.folded, op);
        }
        Ok(())
    }

    /// An id the log had already moved past: a prompt is either a fresh
    /// exchange or steering on the one in hand, an assistant message is
    /// always freshly minted, and an imported message names a turn the link
    /// brought over.
    fn stale(&self, protocol: &Protocol) -> Option<u64> {
        let reach = self.folded.reach();
        match protocol {
            Protocol::UserPrompt { exchange, .. } => Some(*exchange).filter(|id| {
                Some(*id) <= reach
                    && self.folded.turns.last().map(|turn| turn.exchange) != Some(*id)
            }),
            Protocol::AssistantMessage { turn, .. } => {
                Some(*turn).filter(|id| Some(*id) <= reach)
            }
            Protocol::ContextMessage { id, .. } => {
                Some(*id).filter(|id| Some(*id) <= reach && self.folded.turn(*id).is_none())
            }
            Protocol::SessionStarted { .. }
            | Protocol::SessionResumed { .. }
            | Protocol::SessionEnded
            | Protocol::TurnStarted { .. }
            | Protocol::ToolResults { .. }
            | Protocol::ContextEdited { .. }
            | Protocol::Inherited { .. } => None,
        }
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
/// the memo it is building — residency at the addressed context — and never
/// the log it is building it from.
///
/// Returns the folded memo alongside the `(model, label)` pair the head
/// record identifies the session by — `AgentLog::resume` reads its own
/// identity from here rather than re-deriving it, since a session's identity
/// is a fact about its first record, not a second thing to keep in step.
///
/// # Errors
/// Returns an error if the file cannot be read, quarantined, has no
/// `SessionStarted { session_id: 0, parent: None }` head record, or refolds
/// to a table that disagrees with the one built incrementally.
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
                | Protocol::TurnStarted { .. }
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

    if refold(&memo.ledger)? != memo.folded {
        return Err(io::Error::other(
            "record.jsonl's from-scratch refold disagreed with the ledger's incremental table",
        ));
    }
    Ok((memo, model, label))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn user(id: u64, label: &str, bytes: usize) -> Turn {
        Turn {
            id,
            exchange: id,
            kind: TurnKind::Exchange,
            label: label.to_string(),
            bytes,
            held: Held::Resident,
        }
    }

    fn assistant(id: u64, exchange: u64, bytes: usize) -> Turn {
        Turn {
            id,
            exchange,
            kind: TurnKind::Exchange,
            label: String::new(),
            bytes,
            held: Held::Resident,
        }
    }

    /// A cut takes every turn at or below it, and leaves a user turn whose
    /// exchange still has an assistant turn in the context standing with its
    /// survivors.
    #[test]
    fn a_cut_keeps_a_user_turn_with_survivors() {
        let mut folded = Folded {
            turns: vec![
                user(1, "one", 10),
                assistant(2, 1, 10),
                user(3, "three", 10),
                assistant(4, 3, 10),
                assistant(5, 3, 10),
            ],
            ..Folded::default()
        };
        let left = apply_context_op(
            &mut folded,
            &ContextOp::Evict {
                through: 4,
                note: None,
            },
        );
        assert_eq!(left, vec![1, 2, 4]);
        assert_eq!(
            folded.resident().map(|turn| turn.id).collect::<Vec<_>>(),
            vec![3, 5],
            "the user turn stays with the assistant turn that survived it"
        );
        assert_eq!(folded.cuts(), &[Cut { through: 4, note: None }]);
    }

    /// A user turn whose exchange keeps nothing leaves like any other turn.
    #[test]
    fn a_cut_takes_a_user_turn_with_no_survivors() {
        let mut folded = Folded {
            turns: vec![user(1, "one", 10), assistant(2, 1, 10), user(3, "three", 10)],
            ..Folded::default()
        };
        let left = apply_context_op(
            &mut folded,
            &ContextOp::Evict {
                through: 2,
                note: None,
            },
        );
        assert_eq!(left, vec![1, 2]);
        assert_eq!(
            folded.resident().map(|turn| turn.id).collect::<Vec<_>>(),
            vec![3]
        );
    }

    /// The marker names the exchanges that left whole and the turns that left
    /// out of one still in the context, draws one row per fragment, and puts
    /// each cut's note after its own rows.
    #[test]
    fn the_head_marker_indexes_fragments_per_cut() {
        let turns = vec![
            Turn {
                held: Held::Evicted,
                ..user(1, "fix the parser", 12 * 1024)
            },
            Turn {
                held: Held::Evicted,
                ..assistant(2, 1, 0)
            },
            Turn {
                held: Held::Evicted,
                ..assistant(3, 1, 0)
            },
            user(4, "add tests for the fold", 0),
            Turn {
                held: Held::Evicted,
                ..assistant(5, 4, 85 * 1024)
            },
            assistant(6, 4, 10),
        ];
        let cuts = vec![
            Cut {
                through: 3,
                note: Some("the parser is fixed".into()),
            },
            Cut {
                through: 5,
                note: None,
            },
        ];
        let marker = render_head(&turns, &cuts).expect("two cuts render one marker");
        assert!(
            marker.starts_with(
                "[EXARCH // Exchange 1 has left your context, and turn 5 of exchange 4."
            ),
            "{marker}"
        );
        assert!(
            marker.contains("transcript `index` lists every turn the transcript holds."),
            "{marker}"
        );
        let rows: Vec<&str> = marker
            .lines()
            .filter(|line| line.starts_with("   1  ") || line.starts_with("   4  "))
            .collect();
        assert_eq!(rows.len(), 2, "one row per fragment, got {marker}");
        assert!(rows[0].contains("fix the parser"), "{marker}");
        assert!(rows[0].contains("2–3"), "{marker}");
        assert!(rows[0].contains("12 KB"), "{marker}");
        assert!(
            rows[1].contains("add tests for the fold") && rows[1].contains("85 KB"),
            "the fragment draws its exchange's user-turn label: {marker}"
        );
        assert!(
            marker.contains("Your note at eviction: \"the parser is fixed\""),
            "{marker}"
        );
    }

    /// The plan spends the last turn's weight and its user turn's up front,
    /// then walks back until a turn does not fit.
    #[test]
    fn a_plan_walks_back_from_the_turn_in_hand() {
        let folded = Folded {
            turns: vec![
                user(1, "one", 100),
                assistant(2, 1, 100),
                user(3, "three", 100),
                assistant(4, 3, 100),
            ],
            ..Folded::default()
        };
        let memo = Memo {
            folded,
            ..Memo::new(PathBuf::from("record.jsonl"))
        };
        assert_eq!(
            memo.plan_eviction(250),
            Some(2),
            "the turn in hand and its user turn are paid first, so turn 2 does not fit"
        );
        assert_eq!(memo.plan_eviction(1000), None, "everything fits");
    }

    /// Nothing is old enough to shed when the work in hand alone fills the
    /// budget: a cut that would take no turn is no plan.
    #[test]
    fn a_lone_exchange_is_never_planned_away() {
        let folded = Folded {
            turns: vec![user(1, "one", 100), assistant(2, 1, 100)],
            ..Folded::default()
        };
        let memo = Memo {
            folded,
            ..Memo::new(PathBuf::from("record.jsonl"))
        };
        assert_eq!(memo.plan_eviction(0), None);
    }
}
