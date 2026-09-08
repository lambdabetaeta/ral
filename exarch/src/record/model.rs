//! The model fold: the provider-facing projection built off the [`Protocol`]
//! records of `record.jsonl` — the same `step`, whether it is applied inline
//! on the attend thread right after [`super::Emitter::emit`] returns, or by
//! [`resume`] from disk.
//!
//! One structure, one fold, everything else a projection. [`Context`] holds
//! every turn the lineage has recorded: a turn is either *here* — its records
//! in memory — or *there* — a [`Pointer`] to the file and byte ranges that
//! hold them. The context sent to the provider, the survey, the transcript
//! index, the head marker and the eviction plan are all pure functions of it.
//!
//! [`Display`](super::Display) and [`Forensic`](super::Forensic) records pass
//! through untouched: this fold projects the protocol subsequence alone.
//!
//! No recorded protocol record is ever discarded: a turn that leaves the
//! context keeps the address its records were measured at, and [`read_at`]
//! reads them back through it one record at a time.

use super::log::Log;
use super::{Fold, Forensic, Locus, Protocol, Record, Recorded, Refusal};
use crate::agent::event::{
    ContextOp, ContextSurvey, GrepAnswer, GrepHit, QuiesceReason, ToolResult, TranscriptExchange,
    TranscriptMessage, TranscriptPart, TurnKind, validate_result_ids,
};
use genai::chat::{
    Binary, BinarySource, ChatMessage, ChatRole, ContentPart, CustomPart, ToolCall, ToolResponse,
};
use regex::Regex;
use serde::{Deserialize, Serialize};
use std::collections::HashSet;
use std::fs::{File, OpenOptions};
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};

/// Marker type implementing [`Fold`] for the model projection; carries no
/// state of its own; [`Context`] is where the projection lives.
pub struct Model;

impl Fold for Model {
    type Memo = Context;

    fn step(memo: &mut Context, record: &Recorded<Record>) -> Result<(), Refusal> {
        match record.value() {
            Record::Protocol(p) => memo.step(Recorded::new(record.locus().clone(), p.clone())),
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

/// Every turn this lineage recorded, and where each of them is.
///
/// The context is the [`Body::Here`] subsequence of `turns`. Invariant: the
/// first resident turn is a user turn.
pub struct Context {
    /// Every turn this lineage recorded, in id order.
    turns: Vec<Turn>,
    /// One note slot per eviction; [`Body::There`]'s `cut` indexes it.
    notes: Vec<Option<String>>,
    state: State,
    /// Own `record.jsonl`: where a turn first recorded here points once it
    /// leaves.
    ///
    /// Reading a completed record by byte range from the file this process is
    /// still appending to is safe: a [`Locus`] exists only once the seam has
    /// written the whole record under its lock.
    source: PathBuf,
    /// Protocol records folded.
    len: usize,
    /// The index of the last context edit — a token measure taken at or
    /// before it is stale.
    newest_edit: Option<usize>,
}

/// One turn: the atom an eviction addresses.
///
/// A *user turn* is a prompt, or an import's opening, and anything before the
/// first reply — it has `exchange == id`. An *assistant turn* is the
/// assistant message, the tool results it called for, and any steering before
/// the next request; it carries its exchange's id beside its own.
struct Turn {
    id: u64,
    exchange: u64,
    kind: TurnKind,
    /// The turn's opening line, clipped to [`OPENING_CHARS`] as it opens.
    label: String,
    /// Serialised bytes, summed as the turn's own records land.
    bytes: usize,
    body: Body,
}

/// Where one turn's records are.
enum Body {
    /// In the context. `origin` is `Some` for a turn first recorded in an
    /// ancestor's file: `records` are what the seed re-recorded here and what
    /// the model is sent; the transcript's copy is at `origin`.
    Here {
        records: Vec<Recorded<Protocol>>,
        origin: Option<Pointer>,
    },
    /// Departed. `cut` indexes [`Context::notes`]; `None` for a drop, which
    /// the marker does not list.
    There { at: Pointer, cut: Option<usize> },
}

/// Where a turn's records lie: a file, and their loci in it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pointer {
    pub source: PathBuf,
    pub loci: Vec<Locus>,
}

/// The one line every view draws for a turn: survey, index, TUI card.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Row {
    pub id: u64,
    pub exchange: u64,
    pub kind: TurnKind,
    pub label: String,
    pub bytes: usize,
    pub held: Held,
}

/// Whether a turn is still in the context, and what took it out. A projection
/// of [`Body`], never stored in the structure.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Held {
    Resident,
    /// `cut` indexes the notes the marker draws, so the row states which
    /// eviction took it rather than leaving a reader to infer it.
    Evicted {
        cut: usize,
    },
    Dropped,
}

impl Held {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Evicted { .. } => "evicted",
            Self::Dropped => "dropped",
        }
    }
}

/// One row of the link a fork carries: the row, and where its transcript copy
/// lies.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Linked {
    pub row: Row,
    pub at: Pointer,
}

impl Turn {
    /// The one way a turn leaves the structure as a row: `held` is projected
    /// off the body, so the two cannot disagree.
    fn row(&self) -> Row {
        Row {
            id: self.id,
            exchange: self.exchange,
            kind: self.kind,
            label: self.label.clone(),
            bytes: self.bytes,
            held: self.held(),
        }
    }

    fn held(&self) -> Held {
        match &self.body {
            Body::Here { .. } => Held::Resident,
            Body::There { cut: Some(cut), .. } => Held::Evicted { cut: *cut },
            Body::There { cut: None, .. } => Held::Dropped,
        }
    }

    /// A user turn opens its exchange, and is the one turn a cut may leave
    /// standing at or below its own reach.
    fn is_user(&self) -> bool {
        self.id == self.exchange
    }

    fn is_resident(&self) -> bool {
        matches!(self.body, Body::Here { .. })
    }

    /// The records this turn holds in memory; empty once it has left.
    fn records(&self) -> &[Recorded<Protocol>] {
        match &self.body {
            Body::Here { records, .. } => records,
            Body::There { .. } => &[],
        }
    }
}

/// The context as an owned value: the messages the provider is sent, and what
/// they weigh by the structure's own per-turn sums.
#[derive(Clone, Default)]
pub struct Rendered {
    messages: Vec<ChatMessage>,
    bytes: usize,
}

impl Rendered {
    fn push(&mut self, messages: Vec<ChatMessage>, bytes: usize) {
        self.bytes = self.bytes.saturating_add(bytes);
        self.messages.extend(messages);
    }

    pub fn messages(&self) -> impl Iterator<Item = &ChatMessage> {
        self.messages.iter()
    }

    pub fn len(&self) -> usize {
        self.messages.len()
    }

    pub fn is_empty(&self) -> bool {
        self.messages.is_empty()
    }

    /// Serialised size, summed from the structure's own per-turn weights.
    pub fn byte_len(&self) -> usize {
        self.bytes
    }

    #[cfg(test)]
    pub(crate) fn for_test(messages: Vec<ChatMessage>) -> Self {
        let bytes = message_bytes(&messages);
        Self { messages, bytes }
    }
}

fn message_bytes(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
        .sum()
}

/// One handle on a record log, carrying the model fold's io-door allow.
fn open_log(path: &Path) -> io::Result<File> {
    #[allow(
        clippy::disallowed_methods,
        reason = "[io-door:silent:model-fold-pointer-read] reads record.jsonl back by Locus for a departed turn and the model's `transcript` door alike; surfaced as a Display::HarnessCall, not the model's own data I/O"
    )]
    File::open(path)
}

fn read_at(reader: &mut File, locus: &Locus) -> io::Result<Protocol> {
    let bytes = locus.bytes();
    let length = usize::try_from(bytes.end.saturating_sub(bytes.start))
        .map_err(|_| io::Error::other("a record's byte range is too large to read"))?;
    let _ = reader.seek(SeekFrom::Start(bytes.start))?;
    let mut buf = vec![0; length];
    reader.read_exact(&mut buf)?;
    let line = buf.strip_suffix(b"\n").unwrap_or(&buf);
    // Before the parse, not after: a stale range can hold a well-formed
    // record, and refusing it as bad JSON would name the wrong fault.
    if Locus::digest_of(line) != locus.digest() {
        return Err(io::Error::other(
            "a record did not hash to the locus that named it — the log at this \
             path is no longer the file those byte ranges were measured in; a \
             rotated segment, a copied session directory, or an edited log",
        ));
    }
    let entry: super::Entry = serde_json::from_slice(line).map_err(io::Error::other)?;
    match entry.record {
        Record::Protocol(p) => Ok(p),
        Record::Display(_) | Record::Forensic(_) => Err(io::Error::other(
            "a pointer named a non-protocol record — a turn's address no longer \
             names the records it was measured over",
        )),
    }
}

/// One turn's records, or the address to read them back by.
enum Sourced {
    Held(Vec<Protocol>),
    Located(Pointer),
}

/// One turn's own material, whatever became of the exchange holding it.
fn turn_messages(records: &[Protocol]) -> Vec<ChatMessage> {
    records
        .iter()
        .cloned()
        .flat_map(into_chat_messages)
        .collect()
}

/// The same, for a resident turn's witnessed records.
fn resident_messages(records: &[Recorded<Protocol>]) -> Vec<ChatMessage> {
    records
        .iter()
        .map(|recorded| recorded.value().clone())
        .flat_map(into_chat_messages)
        .collect()
}

/// One exchange a door read reaches: where each of its turns lies — a fork
/// can leave one exchange's turns in two files, so placement is per turn.
struct ExchangeRead {
    exchange: u64,
    turns: Vec<(u64, Sourced)>,
}

/// What a read does where a turn's file will not read back: refuse by path,
/// or pass over the turn. A narrowing answers for what it named; a search of
/// the whole transcript passes over what it cannot reach, the head marker
/// having already told the model which turns it cannot have back.
#[derive(Clone, Copy)]
enum Unreadable {
    Skip,
    Refuse,
}

/// Where every turn a door read names lies, owned outright: the read borrows
/// nothing from the structure, so the desk can drop the session lock before
/// it touches a file.
pub(crate) struct TranscriptRead {
    exchanges: Vec<ExchangeRead>,
    unreadable: Unreadable,
}

impl TranscriptRead {
    /// One [`TranscriptExchange`] per exchange the read touched, narrowed to
    /// [`TranscriptPart`]s rather than the provider's own content parts.
    ///
    /// # Errors
    /// Refuses a turn whose file will not read back.
    pub(crate) fn exchanges(self) -> Result<Vec<TranscriptExchange>, String> {
        let mut read = Vec::with_capacity(self.exchanges.len());
        self.walk(|exchange, turns| {
            read.push(TranscriptExchange {
                exchange,
                turns: turns.iter().map(|(turn, _)| *turn).collect(),
                messages: turns
                    .iter()
                    .flat_map(|(_, records)| turn_messages(records))
                    .map(|message| transcript_message(&message))
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
        self.walk(|exchange, turns| {
            for (turn, records) in turns {
                grep_messages(
                    pattern,
                    exchange,
                    *turn,
                    &turn_messages(records),
                    &mut tally,
                );
            }
        })?;
        Ok(tally.answer())
    }

    /// Hand every located exchange's turns to `visit`, in transcript order,
    /// holding at most one open file: consecutive turns naming one log share
    /// it, so each file is read in a single pass in locus order.
    fn walk(self, mut visit: impl FnMut(u64, &[(u64, Vec<Protocol>)])) -> Result<(), String> {
        let unreadable = self.unreadable;
        let mut open: Option<(PathBuf, File)> = None;
        for ExchangeRead { exchange, turns } in self.exchanges {
            let mut read = Vec::with_capacity(turns.len());
            for (turn, sourced) in turns {
                match sourced {
                    Sourced::Held(records) => read.push((turn, records)),
                    Sourced::Located(Pointer { source, loci }) => {
                        match read_pointer(&mut open, &source, &loci) {
                            Ok(records) => read.push((turn, records)),
                            Err(error) => match unreadable {
                                Unreadable::Refuse => {
                                    return Err(read_back_refusal(turn, &source, &error));
                                }
                                Unreadable::Skip => {}
                            },
                        }
                    }
                }
            }
            visit(exchange, &read);
        }
        Ok(())
    }
}

/// Read one turn's records off `source`, reusing the descriptor already open
/// when consecutive turns name one file — so a lineage of many files costs
/// one descriptor, never two.
fn read_pointer(
    open: &mut Option<(PathBuf, File)>,
    source: &Path,
    loci: &[Locus],
) -> io::Result<Vec<Protocol>> {
    if open
        .as_ref()
        .is_none_or(|(path, _)| path.as_path() != source)
    {
        // Closed before the next is opened.
        drop(open.take());
        let file = open_log(source)?;
        *open = Some((source.to_path_buf(), file));
    }
    let (_, file) = open.as_mut().expect("just opened, or already open");
    loci.iter().map(|locus| read_at(file, locus)).collect()
}

impl Context {
    /// The one constructor: `source` is `record.jsonl`'s path, fixed for this
    /// structure's life.
    #[must_use]
    pub fn new(source: PathBuf) -> Self {
        Self {
            turns: Vec::new(),
            notes: Vec::new(),
            state: State::default(),
            source,
            len: 0,
            newest_edit: None,
        }
    }

    /// The model's own line to its future self, one slot per eviction.
    #[must_use]
    pub fn notes(&self) -> &[Option<String>] {
        &self.notes
    }

    /// The one way a turn leaves the structure as an address: the link a fork
    /// carries, one row per turn the lineage recorded.
    ///
    /// `at` is `origin` where there is one, the departure address where the
    /// turn has left, and this log's own file for a turn first recorded here
    /// and still in the context. Every row's `kind` is `Inherited`, a linked
    /// row being inherited by construction.
    pub(crate) fn linked(&self) -> Vec<Linked> {
        self.turns
            .iter()
            .map(|turn| Linked {
                row: Row {
                    kind: TurnKind::Inherited,
                    ..turn.row()
                },
                at: match &turn.body {
                    Body::Here {
                        origin: Some(at), ..
                    }
                    | Body::There { at, .. } => at.clone(),
                    Body::Here {
                        records,
                        origin: None,
                    } => self.pointer(records),
                },
            })
            .collect()
    }

    /// Where records recorded in this log's own file lie.
    fn pointer(&self, records: &[Recorded<Protocol>]) -> Pointer {
        Pointer {
            source: self.source.clone(),
            loci: records
                .iter()
                .map(|recorded| recorded.locus().clone())
                .collect(),
        }
    }

    /// The context, in id order.
    fn resident(&self) -> impl Iterator<Item = &Turn> {
        self.turns.iter().filter(|turn| turn.is_resident())
    }

    /// The highest id the structure has reached — every id-bearing record
    /// past it opens a turn. Departed rows are kept, so an empty table means
    /// the lineage never minted an id.
    fn reach(&self) -> Option<u64> {
        self.turns.last().map(|turn| turn.id)
    }

    fn turn(&self, id: u64) -> Option<&Turn> {
        self.turns.iter().find(|turn| turn.id == id)
    }

    /// Which turns of `exchange` the structure holds, in id order.
    fn exchange_turns(&self, exchange: u64) -> Vec<u64> {
        self.turns
            .iter()
            .filter(|turn| turn.exchange == exchange)
            .map(|turn| turn.id)
            .collect()
    }

    /// The context's exchanges, each with its resident turns, in id order.
    fn resident_exchanges(&self) -> Vec<(u64, Vec<&Turn>)> {
        let mut exchanges: Vec<(u64, Vec<&Turn>)> = Vec::new();
        for turn in self.resident() {
            match exchanges.last_mut() {
                Some((exchange, turns)) if *exchange == turn.exchange => turns.push(turn),
                _ => exchanges.push((turn.exchange, vec![turn])),
            }
        }
        exchanges
    }

    /// Whether a record that opens an exchange — a fresh
    /// [`Protocol::UserPrompt`], an imported [`Protocol::ContextMessage`] —
    /// is admissible here. Weaker than [`State::ReadyForUser`]: an exchange
    /// that never reached a reply is abandoned by the next prompt rather than
    /// closed by a fabricated one, so only outstanding tool calls hold the
    /// log.
    pub fn is_ready(&self) -> bool {
        admits_new_turn(&self.state)
    }

    /// An edit may land at any rest but a batch in flight: outstanding tool
    /// calls name the assistant frame their results answer, so nothing may
    /// come between. What keeps the work in hand is
    /// [`Self::plan_eviction`]'s own shape and [`Self::validate_edit`], not
    /// this.
    pub fn can_evict(&self) -> bool {
        self.is_ready()
    }

    pub fn current_exchange(&self) -> Option<u64> {
        self.turns.last().map(|turn| turn.exchange)
    }

    /// The id the next turn this log opens is minted above.
    pub(crate) fn id_floor(&self) -> u64 {
        self.reach().unwrap_or(0)
    }

    /// The id the next prompt or assistant message takes, minted here and
    /// never chosen by a caller.
    pub(crate) fn next_id(&self) -> u64 {
        self.id_floor().saturating_add(1)
    }

    /// The newest exchange still in the context.
    pub fn last_context_exchange(&self) -> Option<u64> {
        self.resident().last().map(|turn| turn.exchange)
    }

    pub fn log_len(&self) -> usize {
        self.len
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.newest_edit.is_some_and(|index| index >= measured_at)
    }

    /// The exchange still owed a reply — one in flight, or one abandoned
    /// without ever getting there. The door's concept alone, refusing to read
    /// the exchange still being written.
    pub fn is_live_exchange(&self, id: u64) -> bool {
        !matches!(self.state, State::ReadyForUser) && self.current_exchange() == Some(id)
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
        quiesce_records(&self.state, reason, self.next_id())
    }

    /// The material a `mnemon` child's seed carries, turn by turn under this
    /// log's own ids: the one other place ownership genuinely transfers,
    /// beside the wire door.
    ///
    /// The last turn seeds through [`admissible_prefix`], cut to the longest
    /// run that owes no tool result — a batch in flight and a dangling edit
    /// left after it both fall out of that same rule.
    pub(crate) fn inherited_seed(&self) -> Vec<(u64, u64, Vec<ChatMessage>)> {
        let resident: Vec<&Turn> = self.resident().collect();
        let Some((last, held)) = resident.split_last() else {
            return Vec::new();
        };
        let mut seed: Vec<(u64, u64, Vec<ChatMessage>)> = held
            .iter()
            .map(|turn| (turn.id, turn.exchange, resident_messages(turn.records())))
            .collect();
        let records: Vec<&Protocol> = last.records().iter().map(Recorded::value).collect();
        seed.push((
            last.id,
            last.exchange,
            admissible_prefix(&records)
                .iter()
                .map(|record| (*record).clone())
                .flat_map(into_chat_messages)
                .collect(),
        ));
        seed
    }

    /// The provider-facing context: the marker where a cut has left one, then
    /// the exchanges with a resident turn, in order.
    ///
    /// The last of them is the exchange in hand and renders as it lies,
    /// whatever state it rests in — its last turn may still be growing. Each
    /// earlier exchange renders as its turns' messages if it is settled, else
    /// as its one [`abandoned_note`].
    pub fn rendered(&self) -> Rendered {
        let mut rendered = Rendered::default();
        if let Some(text) = render_head(self) {
            rendered.push_note(text);
        }
        let exchanges = self.resident_exchanges();
        let Some((live, closed)) = exchanges.split_last() else {
            return rendered;
        };
        for (_, turns) in closed {
            if is_settled(turns) {
                for turn in turns {
                    rendered.push(resident_messages(turn.records()), turn.bytes);
                }
            } else {
                rendered.push_note(abandoned_note(turns));
            }
        }
        for turn in &live.1 {
            rendered.push(resident_messages(turn.records()), turn.bytes);
        }
        rendered
    }

    /// Approximate context size in serialised bytes — the fallback eviction
    /// trigger when the model's context window is unknown. Renders no turn:
    /// the marker's weight, plus each resident exchange's own per-turn sums,
    /// or its note's weight where the exchange never settled.
    pub(crate) fn history_bytes(&self) -> usize {
        let mut bytes = render_head(self).map_or(0, note_bytes);
        let exchanges = self.resident_exchanges();
        let Some((live, closed)) = exchanges.split_last() else {
            return bytes;
        };
        for (_, turns) in closed {
            let weight = if is_settled(turns) {
                turns.iter().map(|turn| turn.bytes).sum()
            } else {
                note_bytes(abandoned_note(turns))
            };
            bytes = bytes.saturating_add(weight);
        }
        bytes.saturating_add(live.1.iter().map(|turn| turn.bytes).sum())
    }

    /// Records still owned by the context, for the host's resource probe: the
    /// structure retains what an edit removes, so this is not
    /// [`Self::log_len`].
    pub(crate) fn event_count(&self) -> usize {
        let records: usize = self.resident().map(|turn| turn.records().len()).sum();
        // A marker stands exactly where some turn left by eviction.
        records
            + usize::from(
                self.turns
                    .iter()
                    .any(|turn| matches!(turn.body, Body::There { cut: Some(_), .. })),
            )
    }

    /// One row per resident turn, at the weight the fold summed, beside the
    /// truth about what is sent: `total_bytes` is [`Self::history_bytes`] and
    /// not a second opinion on it, so an abandoned exchange's turns report
    /// their own weights while the context sends only its note.
    pub(crate) fn context_survey(&self) -> ContextSurvey {
        ContextSurvey {
            rows: self.resident().map(Turn::row).collect(),
            evicted: self
                .turns
                .iter()
                .filter(|turn| matches!(turn.held(), Held::Evicted { .. }))
                .count(),
            total_bytes: self.history_bytes(),
        }
    }

    /// Every turn the transcript holds, in id order, each saying whether it
    /// is still in the context. The whole lineage's, since a fork inherits
    /// the table.
    pub(crate) fn transcript_index(&self) -> Vec<Row> {
        self.turns.iter().map(Turn::row).collect()
    }

    /// Read closed turns in transcript order — the exchanges named outright,
    /// the turns a range covers, or both — as one [`TranscriptExchange`] per
    /// exchange touched, addressed by its own `exchange` field rather than by
    /// argument position.
    ///
    /// What comes back is each turn's own material, wherever the lineage
    /// keeps it: in the context, departed to this log's file, or first
    /// recorded in an ancestor's.
    ///
    /// Both halves in one call — [`Self::locate_read`] then
    /// [`TranscriptRead::exchanges`]; the desk keeps them apart so only the
    /// first runs under the session lock.
    ///
    /// # Errors
    /// Refuses a read that names nothing, a duplicate name, an exchange or
    /// range this lineage never recorded, the turn still being written, or a
    /// turn whose file will not read back.
    pub(crate) fn read_transcript(
        &self,
        exchanges: &[u64],
        turns: Option<(u64, u64)>,
    ) -> Result<Vec<TranscriptExchange>, String> {
        self.locate_read(exchanges, turns)?.exchanges()
    }

    /// [`Self::read_transcript`]'s first half: resolve every turn the read
    /// names under the caller's lock, borrowing nothing, so the read itself
    /// can run once that lock is gone.
    ///
    /// # Errors
    /// Refuses whatever [`Self::read_transcript`] refuses at location time.
    pub(crate) fn locate_read(
        &self,
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
            let mut located = Vec::with_capacity(turns.len());
            for turn in turns {
                located.push((turn, self.sourced(turn)?));
            }
            read.push(ExchangeRead {
                exchange,
                turns: located,
            });
        }
        Ok(TranscriptRead {
            exchanges: read,
            unreadable: Unreadable::Refuse,
        })
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
            if Some(to) > self.reach() {
                return Err(not_recorded_refusal_turn(to, self.reach()));
            }
            if let Some(open) = self
                .unclosed_turn()
                .filter(|open| (from..=to).contains(open))
                && let Some(turn) = self.turn(open)
            {
                return Err(self.live_exchange_refusal(turn.exchange));
            }
        }
        let selected = self.select(exchanges, turns);
        // A validated exchange always has turns of its own, so an empty
        // selection means the range itself began past the reach.
        match (selected.is_empty(), turns) {
            (true, Some((from, _))) => Err(not_recorded_refusal_turn(from, self.reach())),
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
        let named = self.turns.iter().filter(|turn| {
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
        (!matches!(self.state, State::ReadyForUser))
            .then(|| self.reach())
            .flatten()
    }

    /// The whole of "seek in the context, then the file": one turn's records
    /// where they are held, or the address to read them back by.
    ///
    /// # Errors
    /// Refuses a turn this lineage never recorded.
    fn sourced(&self, turn: u64) -> Result<Sourced, String> {
        let Some(found) = self.turn(turn) else {
            return Err(not_recorded_refusal_turn(turn, self.reach()));
        };
        Ok(match &found.body {
            Body::Here {
                records,
                origin: None,
            } => Sourced::Held(
                records
                    .iter()
                    .map(|recorded| recorded.value().clone())
                    .collect(),
            ),
            Body::Here {
                origin: Some(at), ..
            }
            | Body::There { at, .. } => Sourced::Located(at.clone()),
        })
    }

    /// Search the closed turns' text: the ones a narrowing names, or the
    /// whole transcript. Each file is read in one pass in locus order, and
    /// every hit names the turn it lies in.
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
        &self,
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
    /// Refuses whatever [`Self::grep_transcript`] refuses at location time.
    pub(crate) fn locate_grep(
        &self,
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
                located.push((turn, self.sourced(turn)?));
            }
            read.push(ExchangeRead {
                exchange,
                turns: located,
            });
        }
        Ok(TranscriptRead {
            exchanges: read,
            unreadable: if narrowed {
                Unreadable::Refuse
            } else {
                Unreadable::Skip
            },
        })
    }

    /// Every exchange the structure holds — how an unnarrowed `` `grep ``
    /// names the whole transcript.
    fn every_exchange(&self) -> Vec<u64> {
        let mut exchanges: Vec<u64> = self.turns.iter().map(|turn| turn.exchange).collect();
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
            if self.exchange_turns(exchange).is_empty() {
                return Err(not_recorded_refusal(exchange, self.reach()));
            }
        }
        Ok(())
    }

    /// The exchange still being written is refused by name — and told which
    /// of its own turns have closed, since those are readable.
    fn live_exchange_refusal(&self, exchange: u64) -> String {
        let turns = self.exchange_turns(exchange);
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
        for turn in self.resident() {
            if turn.exchange >= anchor && exchanges.last() != Some(&turn.exchange) {
                exchanges.push(turn.exchange);
            }
        }
        if exchanges.first() != Some(&anchor) {
            if self.turn(anchor).is_some() {
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
        let Some(turn) = self.turn(through) else {
            return Err(not_recorded_refusal_turn(through, self.reach()));
        };
        if !turn.is_resident() {
            return Err(self.departed_turn_refusal(through));
        }
        if self
            .resident()
            .last()
            .is_some_and(|last| last.id == through)
        {
            return Err(format!(
                "{through} is the newest turn; an eviction keeps the work in hand"
            ));
        }
        Ok(())
    }

    /// An exchange is droppable iff some turn of it is still in the context
    /// and it is not the one being written.
    fn validate_droppable(&self, exchange: u64) -> Result<(), String> {
        if !self.resident().any(|turn| turn.exchange == exchange) {
            if self.exchange_turns(exchange).is_empty() {
                return Err(format!(
                    "exchange {exchange} is not present in your context"
                ));
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
        match self.resident().next() {
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
        match self.resident().next() {
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
        let resident: Vec<&Turn> = self.resident().collect();
        let (last, candidates) = resident.split_last()?;
        let mut spent = last.bytes;
        let paid = (!last.is_user()).then_some(last.exchange);
        if let Some(user) = paid.and_then(|id| self.turn(id)) {
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
                return (!cut_departures(&self.turns, through).is_empty()).then_some(through);
            }
            spent = total;
        }
        None
    }

    /// The one fold, judging each record before applying it — so a
    /// hand-edited or foreign file is refused by the same function that folds
    /// a live one.
    ///
    /// Live authorship is typestate-correct, so a refusal on the live path is
    /// a harness bug and propagates as [`Refusal`] through [`Fold::step`].
    ///
    /// # Errors
    /// Returns [`Refusal::Foreign`] for a record no live session could have
    /// written at this point in the log.
    pub(crate) fn step(&mut self, record: Recorded<Protocol>) -> Result<(), Refusal> {
        let index = self.len;
        self.judge(index, record.value())?;
        self.len = index + 1;
        self.state = advance(&self.state, record.value());
        match record.value() {
            Protocol::Inherited { turns, notes } => {
                self.install(turns, notes);
                return Ok(());
            }
            Protocol::ContextEdited { op, .. } => {
                self.apply_context_op(op);
                self.newest_edit = Some(index);
                return Ok(());
            }
            Protocol::UserPrompt { .. }
            | Protocol::ContextMessage { .. }
            | Protocol::AssistantMessage { .. }
            | Protocol::ToolResults { .. } => {}
        }
        let Some(at) = self.place(record.value()) else {
            return Ok(());
        };
        let bytes = message_bytes(&into_chat_messages(record.value().clone()));
        let turn = &mut self.turns[at];
        turn.bytes = turn.bytes.saturating_add(bytes);
        match &mut turn.body {
            Body::Here { records, .. } => records.push(record),
            // `judge` refuses a record landing on a departed row.
            Body::There { .. } => unreachable!("a judged record lands on a resident turn"),
        }
        Ok(())
    }

    /// What no live session could have written here.
    fn judge(&self, index: usize, protocol: &Protocol) -> Result<(), Refusal> {
        let n = index + 1;
        let foreign = |reason: String| Refusal::Foreign {
            record: Box::new(Record::Protocol(protocol.clone())),
            reason,
        };
        if !admissible(&self.state, protocol) {
            return Err(foreign(format!(
                "record {n} is foreign protocol data, not a seam that quiesce can repair; was record.jsonl hand-edited or written by an incompatible exarch?"
            )));
        }
        if let Some(id) = self.stale(protocol) {
            return Err(foreign(format!(
                "record {n} names turn {id}, which the log had already moved past; no live session records a stale id"
            )));
        }
        if let Protocol::Inherited { turns, notes } = protocol {
            // The position is the rule: a link stands as the first protocol
            // record of a fork's log, and nowhere else.
            if index != 0 {
                return Err(foreign(format!(
                    "record {n} links this log to an ancestor's transcript, which only a fork's opening may do; by here the log has a context of its own, and no live session inherits twice"
                )));
            }
            if let Some(pair) = turns
                .windows(2)
                .find(|pair| pair[0].row.id >= pair[1].row.id)
            {
                return Err(foreign(format!(
                    "record {n} links a transcript whose turn {} does not precede turn {} in id order; was this log's opening record hand-edited?",
                    pair[0].row.id, pair[1].row.id
                )));
            }
            let past = turns.iter().find(
                |linked| matches!(linked.row.held, Held::Evicted { cut } if cut >= notes.len()),
            );
            if let Some(linked) = past {
                return Err(foreign(format!(
                    "record {n} says turn {} left at an eviction the link does not carry a note for, of the {} it carries; was this log's opening record hand-edited?",
                    linked.row.id,
                    notes.len()
                )));
            }
        }
        if let Some(turn) = self.extends(protocol).filter(|turn| !turn.is_resident()) {
            let id = turn.id;
            return Err(foreign(
                if matches!(protocol, Protocol::ContextMessage { .. }) {
                    format!(
                        "record {n} imports turn {id}, which the link says has already left the context — a fork re-records only what its parent still had"
                    )
                } else {
                    format!("record {n} extends turn {id}, which has already left the context")
                },
            ));
        }
        if let Protocol::ContextMessage { id, exchange, .. } = protocol
            && let Some(turn) = self.turn(*id)
            && turn.exchange != *exchange
        {
            return Err(foreign(format!(
                "record {n} imports turn {id} under exchange {exchange}, but the link places that turn in exchange {}",
                turn.exchange
            )));
        }
        Ok(())
    }

    /// An id the log had already moved past: a prompt is either a fresh
    /// exchange or steering on the one in hand, an assistant message is
    /// always freshly minted, and an imported message names a turn the link
    /// brought over.
    fn stale(&self, protocol: &Protocol) -> Option<u64> {
        let reach = self.reach();
        match protocol {
            Protocol::UserPrompt { exchange, .. } => Some(*exchange).filter(|id| {
                Some(*id) <= reach && self.turns.last().map(|turn| turn.exchange) != Some(*id)
            }),
            Protocol::AssistantMessage { turn, .. } => Some(*turn).filter(|id| Some(*id) <= reach),
            Protocol::ContextMessage { id, .. } => {
                Some(*id).filter(|id| Some(*id) <= reach && self.turn(*id).is_none())
            }
            Protocol::ToolResults { .. }
            | Protocol::ContextEdited { .. }
            | Protocol::Inherited { .. } => None,
        }
    }

    /// The turn a record's material would extend, where it opens none — the
    /// newest row, or the row a [`Protocol::ContextMessage`] names.
    fn extends(&self, protocol: &Protocol) -> Option<&Turn> {
        match protocol {
            Protocol::UserPrompt { exchange, .. } => (Some(*exchange) <= self.reach())
                .then(|| self.turns.last())
                .flatten(),
            Protocol::ToolResults { .. } => self.turns.last(),
            Protocol::ContextMessage { id, .. } => self.turn(*id),
            Protocol::AssistantMessage { .. }
            | Protocol::ContextEdited { .. }
            | Protocol::Inherited { .. } => None,
        }
    }

    /// Which row a record's material lands on, opening one where the record
    /// bears an id the structure has not reached.
    ///
    /// A prompt past the reach opens an exchange; one at or below it is
    /// steering, and extends the assistant turn it answers. An imported
    /// message names its turn outright, the table having arrived whole with
    /// the link, and only a note that inherits nothing opens one of its own.
    fn place(&mut self, protocol: &Protocol) -> Option<usize> {
        let reach = self.reach();
        let last = self.turns.len().checked_sub(1);
        match protocol {
            Protocol::UserPrompt { exchange, text } => {
                if Some(*exchange) <= reach {
                    return last;
                }
                Some(self.open(*exchange, *exchange, TurnKind::Exchange, opening_line(text)))
            }
            Protocol::AssistantMessage { turn, message, .. } => {
                let exchange = self.turns.last()?.exchange;
                Some(self.open(*turn, exchange, TurnKind::Exchange, message_label(message)))
            }
            Protocol::ContextMessage {
                id,
                exchange,
                message,
            } => {
                if let Some(at) = self.turns.iter().position(|turn| turn.id == *id) {
                    return Some(at);
                }
                (Some(*id) > reach)
                    .then(|| self.open(*id, *exchange, TurnKind::Import, message_label(message)))
            }
            Protocol::ToolResults { .. } => last,
            Protocol::ContextEdited { .. } | Protocol::Inherited { .. } => None,
        }
    }

    /// Open a fresh turn at the end of the table, and answer its index.
    fn open(&mut self, id: u64, exchange: u64, kind: TurnKind, label: String) -> usize {
        self.turns.push(Turn {
            id,
            exchange,
            kind,
            label,
            bytes: 0,
            body: Body::Here {
                records: Vec::new(),
                origin: None,
            },
        });
        self.turns.len() - 1
    }

    /// A link's own step: the parent's rows and notes become this log's, so
    /// the child's marker, survey and index are the same projections of the
    /// same structure. A resident row's weight is zeroed because the seed's
    /// records follow and the fold sums them as they land.
    fn install(&mut self, turns: &[Linked], notes: &[Option<String>]) {
        self.turns = turns
            .iter()
            .map(|Linked { row, at }| {
                let (bytes, body) = match row.held {
                    Held::Resident => (
                        0,
                        Body::Here {
                            records: Vec::new(),
                            origin: Some(at.clone()),
                        },
                    ),
                    Held::Evicted { cut } => (
                        row.bytes,
                        Body::There {
                            at: at.clone(),
                            cut: Some(cut),
                        },
                    ),
                    Held::Dropped => (
                        row.bytes,
                        Body::There {
                            at: at.clone(),
                            cut: None,
                        },
                    ),
                };
                Turn {
                    id: row.id,
                    exchange: row.exchange,
                    kind: row.kind,
                    label: row.label.clone(),
                    bytes,
                    body,
                }
            })
            .collect();
        self.notes = notes.to_vec();
    }

    /// Move every turn `op` takes from here to there: its address is its
    /// `origin` where it has one, else this log's own file and the loci its
    /// records were measured at.
    fn apply_context_op(&mut self, op: &ContextOp) {
        let (leaving, cut) = match op {
            ContextOp::Evict { through, .. } => (
                cut_departures(&self.turns, *through),
                Some(self.notes.len()),
            ),
            ContextOp::Drop { exchanges } => (
                self.resident()
                    .filter(|turn| exchanges.contains(&turn.exchange))
                    .map(|turn| turn.id)
                    .collect(),
                None,
            ),
        };
        let leaving: HashSet<u64> = leaving.into_iter().collect();
        let source = self.source.clone();
        for turn in &mut self.turns {
            if !leaving.contains(&turn.id) {
                continue;
            }
            let at = match &mut turn.body {
                Body::Here { records, origin } => origin.take().unwrap_or_else(|| Pointer {
                    source: source.clone(),
                    loci: records
                        .iter()
                        .map(|recorded| recorded.locus().clone())
                        .collect(),
                }),
                Body::There { .. } => continue,
            };
            turn.body = Body::There { at, cut };
        }
        if let ContextOp::Evict { note, .. } = op {
            self.notes.push(note.clone());
        }
    }
}

impl Rendered {
    /// One harness-voice message standing for something the context does not
    /// hold: the head marker, or an abandoned exchange's note.
    fn push_note(&mut self, text: String) {
        let message = ChatMessage::user(text);
        let bytes = message_bytes(std::slice::from_ref(&message));
        self.push(vec![message], bytes);
    }
}

/// What a note weighs as the one message it becomes.
fn note_bytes(text: String) -> usize {
    message_bytes(std::slice::from_ref(&ChatMessage::user(text)))
}

/// Whether an exchange's own fold comes to rest: a reply that called no tool,
/// or an import, which advances nothing and so rests where it lies. An
/// interrupted exchange does not, and reads as its note.
///
/// One look at the exchange's last record, since a turn's body holds material
/// alone. An exchange whose resident turns hold nothing — a fork's link left
/// a turn the seed never re-recorded — rests: there is no interruption to
/// report about material that is not there.
fn is_settled(turns: &[&Turn]) -> bool {
    match turns
        .iter()
        .rev()
        .flat_map(|turn| turn.records().iter().rev())
        .next()
        .map(Recorded::value)
    {
        Some(Protocol::AssistantMessage {
            pending_tool_ids, ..
        }) => pending_tool_ids.is_empty(),
        Some(Protocol::ContextMessage { .. }) | None => true,
        Some(
            Protocol::UserPrompt { .. }
            | Protocol::ToolResults { .. }
            | Protocol::ContextEdited { .. }
            | Protocol::Inherited { .. },
        ) => false,
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
        Protocol::ContextMessage { .. }
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
        Protocol::ContextEdited { .. } | Protocol::Inherited { .. } => Vec::new(),
    }
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

/// Protocol sequencing legality.
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
        Protocol::ContextEdited { .. }
        // Sequencing-neutral; where a link may stand is its own rule in
        // [`Context::judge`], needing more than a [`State`].
        | Protocol::Inherited { .. } => true,
    }
}

/// Whether a record that opens an exchange may follow. Only outstanding tool
/// calls forbid it: an exchange the model never replied to is abandoned by
/// the next prompt, not closed by a fabricated reply.
fn admits_new_turn(state: &State) -> bool {
    !matches!(state, State::AwaitingToolResults { .. })
}

/// The longest prefix of `records` that owes no tool result — the only shape
/// a `mnemon` seed may take, the child's own launch prompt landing behind it.
fn admissible_prefix<'a, 'b>(records: &'b [&'a Protocol]) -> &'b [&'a Protocol] {
    let mut state = State::default();
    let mut end = 0;
    for (index, record) in records.iter().enumerate() {
        state = advance(&state, record);
        if admits_new_turn(&state) {
            end = index + 1;
        }
    }
    &records[..end]
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
fn abandoned_note(turns: &[&Turn]) -> String {
    let called = turns
        .iter()
        .flat_map(|turn| turn.records())
        .any(|record| matches!(record.value(), Protocol::ToolResults { .. }));
    let effects = if called {
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

/// One exchange's run of turns inside one cut — the row the head marker
/// draws. One exchange may appear in two fragments across two cuts.
struct Fragment {
    exchange: u64,
    /// Which eviction took these turns, indexing [`Context::notes`].
    cut: usize,
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

/// Every departed turn as the marker groups it: maximal runs of
/// table-adjacent rows sharing an exchange and a cut, in table order.
///
/// Adjacency is in the table, not among one cut's own rows: where a later cut
/// takes a survivor user turn under an exchange an earlier cut took the
/// middle of, the two are separate fragments — so each row is attributed to
/// the cut that actually took it, and no fragment spans a gap.
fn fragments(turns: &[Turn]) -> Vec<Fragment> {
    let mut fragments: Vec<Fragment> = Vec::new();
    let mut previous: Option<usize> = None;
    for (index, turn) in turns.iter().enumerate() {
        let Held::Evicted { cut } = turn.held() else {
            previous = None;
            continue;
        };
        let adjacent = previous.is_some_and(|before| before + 1 == index);
        match fragments.last_mut() {
            Some(open) if adjacent && open.cut == cut && open.exchange == turn.exchange => {
                open.ids.1 = turn.id;
                open.bytes = open.bytes.saturating_add(turn.bytes);
            }
            _ => fragments.push(Fragment {
                exchange: turn.exchange,
                cut,
                ids: (turn.id, turn.id),
                bytes: turn.bytes,
            }),
        }
        previous = Some(index);
    }
    fragments
}

/// The head marker: the one user-voice message standing where an evicted
/// prefix was, indexing every turn that has left so the model can ask for any
/// of them back. `None` when no turn has been evicted, which is the one
/// notion of "no marker" there is.
///
/// Correct by construction: a turn's body says which cut took it, so nothing
/// here re-derives that from an id window. A pure function of the structure —
/// no clock, no path, no ordering-unstable container — so between two edits
/// the provider's prompt cache sees a byte-stable message 0.
///
/// Voice and bracket as [`abandoned_note`]: the harness may state a fact
/// about the conversation, never speak in the model's own voice.
fn render_head(context: &Context) -> Option<String> {
    let fragments = fragments(&context.turns);
    if fragments.is_empty() {
        return None;
    }
    // Grouped by cut, so each note follows the rows it belongs to.
    let by_cut: Vec<Vec<&Fragment>> = (0..context.notes.len())
        .map(|cut| {
            fragments
                .iter()
                .filter(|fragment| fragment.cut == cut)
                .collect()
        })
        .collect();
    let rows: Vec<&Fragment> = by_cut.iter().flatten().copied().collect();
    let departed = departed_sentence(context, &rows);
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
    let mut seen = 0usize;
    for (cut, drawn) in by_cut.iter().enumerate() {
        let start = collapsed.saturating_sub(seen).min(drawn.len());
        seen += drawn.len();
        if start >= drawn.len() {
            continue;
        }
        for row in &drawn[start..] {
            lines.push(head_row(&context.turns, row));
        }
        // `Debug`-quoted, so no note — the model's own, or one inherited off
        // an ancestor's link — can add a line to the marker.
        if let Some(note) = context.notes[cut].as_deref() {
            lines.push(format!("Your note at eviction: {note:?}"));
        }
    }
    Some(format!("{}]", lines.join("\n")))
}

/// The marker's opening: which exchanges left whole, and which left only some
/// of their turns.
fn departed_sentence(context: &Context, rows: &[&Fragment]) -> String {
    let resident: HashSet<u64> = context.resident().map(|turn| turn.exchange).collect();
    let mut whole: Vec<u64> = Vec::new();
    let mut partial: Vec<String> = Vec::new();
    let mut seen: HashSet<u64> = HashSet::new();
    for row in rows {
        if !seen.insert(row.exchange) {
            continue;
        }
        if !resident.contains(&row.exchange) {
            whole.push(row.exchange);
            continue;
        }
        // A partially departed exchange is always a resident user turn over a
        // departed prefix of its assistant turns, so min–max is its range.
        let (from, to) = rows
            .iter()
            .filter(|other| other.exchange == row.exchange)
            .fold(row.ids, |(from, to), other| {
                (from.min(other.ids.0), to.max(other.ids.1))
            });
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
/// left exactly as it lies, and [`Context::rendered`] reads it as its note
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

/// Fold `record.jsonl` into a fresh [`Context`]: quarantine a torn tail, read
/// the session's identity off the file's first record, then hand every
/// protocol record to [`Context::step`].
///
/// There is nothing to compare the result with, and nothing to check it
/// against: the law is that the fold refuses what it cannot admit, and what
/// guards a read-back is [`read_at`]'s digest check, at the moment of the
/// read, where the risk is.
///
/// The pass is a stream rather than a collection, so what a resume holds is
/// the structure it is building and never the log it is building it from.
///
/// Returns the structure alongside the `(model, label)` pair the head record
/// identifies the session by — `AgentLog::resume` reads its own identity from
/// here rather than re-deriving it, since a session's identity is a fact about
/// its first record, not a second thing to keep in step.
///
/// # Errors
/// Returns an error if the file cannot be read, cannot be quarantined, has no
/// `SessionStarted { session_id: 0, parent: None }` head record, or holds a
/// protocol record the fold refuses.
pub fn resume(path: &Path) -> io::Result<(Context, String, String)> {
    if let Some(tail) = find_crash_tail(path)? {
        quarantine_tail(path, &tail)?;
    }
    let mut context = Context::new(path.to_path_buf());
    let mut identity = None;
    for record in Log::read(path)? {
        let record = record?;
        if identity.is_none() {
            identity = Some(match record.value() {
                Record::Forensic(Forensic::SessionStarted {
                    session_id: 0,
                    parent: None,
                    model,
                    label,
                    ..
                }) => (model.clone(), label.clone()),
                Record::Forensic(Forensic::SessionStarted {
                    session_id, parent, ..
                }) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record starts session {session_id:?} with parent {parent:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a child log?",
                        path.display()
                    )));
                }
                other @ (Record::Protocol(_) | Record::Display(_) | Record::Forensic(_)) => {
                    return Err(io::Error::other(format!(
                        "cannot resume {}: the first record is {other:?}; expected SessionStarted {{ session_id: 0, parent: None }} — is this a copied child log?",
                        path.display()
                    )));
                }
            });
        }
        let Record::Protocol(protocol) = record.value() else {
            continue;
        };
        context
            .step(Recorded::new(record.locus().clone(), protocol.clone()))
            .map_err(|refusal| io::Error::other(refusal.to_string()))?;
    }
    let Some((model, label)) = identity else {
        return Err(io::Error::other(format!(
            "cannot resume {}: no complete session records were found; is the file truncated?",
            path.display()
        )));
    };
    Ok((context, model, label))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A turn in the context. Its records are elided: every fold under test
    /// here reads `bytes`, never the material.
    fn here(id: u64, exchange: u64, label: &str, bytes: usize) -> Turn {
        Turn {
            id,
            exchange,
            kind: TurnKind::Exchange,
            label: label.to_string(),
            bytes,
            body: Body::Here {
                records: Vec::new(),
                origin: None,
            },
        }
    }

    /// A departed turn: `cut` indexes the notes, `None` for a drop.
    fn there(id: u64, exchange: u64, label: &str, bytes: usize, cut: Option<usize>) -> Turn {
        Turn {
            body: Body::There {
                at: Pointer {
                    source: PathBuf::from("record.jsonl"),
                    loci: Vec::new(),
                },
                cut,
            },
            ..here(id, exchange, label, bytes)
        }
    }

    fn context(turns: Vec<Turn>, notes: Vec<Option<String>>) -> Context {
        Context {
            turns,
            notes,
            ..Context::new(PathBuf::from("record.jsonl"))
        }
    }

    fn held(context: &Context) -> Vec<(u64, Held)> {
        context
            .turns
            .iter()
            .map(|turn| (turn.id, turn.held()))
            .collect()
    }

    fn resident_ids(context: &Context) -> Vec<u64> {
        context.resident().map(|turn| turn.id).collect()
    }

    /// A marker row's last two columns: the turn range it draws, and its
    /// weight in whole KB.
    fn columns(line: &str) -> (&str, &str) {
        // The marker's last line closes its bracket.
        let mut fields = line.trim_end_matches(']').split_whitespace().rev();
        let unit = fields.next();
        assert_eq!(unit, Some("KB"), "{line}");
        let weight = fields.next().expect("a row ends in a weight");
        let range = fields.next().expect("a row carries a turn range");
        (range, weight)
    }

    /// A cut takes every turn at or below it, and leaves a user turn whose
    /// exchange still has an assistant turn in the context standing with its
    /// survivors.
    #[test]
    fn a_cut_keeps_a_user_turn_with_survivors() {
        let mut context = context(
            vec![
                here(1, 1, "one", 10),
                here(2, 1, "", 10),
                here(3, 3, "three", 10),
                here(4, 3, "", 10),
                here(5, 3, "", 10),
            ],
            Vec::new(),
        );
        context.apply_context_op(&ContextOp::Evict {
            through: 4,
            note: None,
        });
        assert_eq!(
            held(&context),
            vec![
                (1, Held::Evicted { cut: 0 }),
                (2, Held::Evicted { cut: 0 }),
                (3, Held::Resident),
                (4, Held::Evicted { cut: 0 }),
                (5, Held::Resident),
            ]
        );
        assert_eq!(
            resident_ids(&context),
            vec![3, 5],
            "the user turn stays with the assistant turn that survived it"
        );
        assert_eq!(context.notes(), &[None]);
    }

    /// A user turn whose exchange keeps nothing leaves like any other turn.
    #[test]
    fn a_cut_takes_a_user_turn_with_no_survivors() {
        let mut context = context(
            vec![
                here(1, 1, "one", 10),
                here(2, 1, "", 10),
                here(3, 3, "three", 10),
            ],
            Vec::new(),
        );
        context.apply_context_op(&ContextOp::Evict {
            through: 2,
            note: None,
        });
        assert_eq!(
            held(&context),
            vec![
                (1, Held::Evicted { cut: 0 }),
                (2, Held::Evicted { cut: 0 }),
                (3, Held::Resident),
            ]
        );
        assert_eq!(resident_ids(&context), vec![3]);
    }

    /// The marker names the exchanges that left whole and the turns that left
    /// out of one still in the context, draws one row per fragment, and puts
    /// each cut's note after its own rows.
    #[test]
    fn the_head_marker_indexes_fragments_per_cut() {
        let context = context(
            vec![
                there(1, 1, "fix the parser", 12 * 1024, Some(0)),
                there(2, 1, "", 0, Some(0)),
                there(3, 1, "", 0, Some(0)),
                here(4, 4, "add tests for the fold", 0),
                there(5, 4, "", 85 * 1024, Some(1)),
                here(6, 4, "", 10),
            ],
            vec![Some("the parser is fixed".into()), None],
        );
        let marker = render_head(&context).expect("two cuts render one marker");
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
        let context = context(
            vec![
                here(1, 1, "one", 100),
                here(2, 1, "", 100),
                here(3, 3, "three", 100),
                here(4, 3, "", 100),
            ],
            Vec::new(),
        );
        assert_eq!(
            context.plan_eviction(250),
            Some(2),
            "the turn in hand and its user turn are paid first, so turn 2 does not fit"
        );
        assert_eq!(context.plan_eviction(1000), None, "everything fits");
    }

    /// Nothing is old enough to shed when the work in hand alone fills the
    /// budget: a cut that would take no turn is no plan.
    #[test]
    fn a_lone_exchange_is_never_planned_away() {
        let context = context(vec![here(1, 1, "one", 100), here(2, 1, "", 100)], Vec::new());
        assert_eq!(context.plan_eviction(0), None);
    }

    /// A survivor user turn belongs to the cut that finally took it, and the
    /// earlier cut's row for its exchange never gains its weight after the
    /// fact.
    #[test]
    fn the_marker_attributes_a_survivor_user_turn_to_the_cut_that_took_it() {
        let mut context = context(
            vec![
                here(3, 3, "fix the parser", 40 * 1024),
                here(4, 3, "", 12 * 1024),
                here(5, 3, "", 8 * 1024),
                here(6, 3, "", 4 * 1024),
            ],
            Vec::new(),
        );
        context.apply_context_op(&ContextOp::Evict {
            through: 5,
            note: Some("half the parser work".into()),
        });
        assert_eq!(
            resident_ids(&context),
            vec![3, 6],
            "the user turn stays with the assistant turn that survived it"
        );
        let first = render_head(&context).expect("one cut renders one marker");
        let rows: Vec<&str> = first.lines().filter(|line| line.contains(" KB")).collect();
        assert_eq!(rows.len(), 1, "{first}");
        assert_eq!(
            columns(rows[0]),
            ("4–5", "20"),
            "the cut weighs the turns it took: {first}"
        );

        context.apply_context_op(&ContextOp::Evict {
            through: 6,
            note: Some("the parser is fixed".into()),
        });
        assert_eq!(
            held(&context),
            vec![
                (3, Held::Evicted { cut: 1 }),
                (4, Held::Evicted { cut: 0 }),
                (5, Held::Evicted { cut: 0 }),
                (6, Held::Evicted { cut: 1 }),
            ]
        );
        let marker = render_head(&context).expect("two cuts render one marker");
        let lines: Vec<&str> = marker.lines().collect();
        let note = |text: &str| {
            lines
                .iter()
                .position(|line| line.contains(text))
                .unwrap_or_else(|| panic!("{marker}"))
        };
        let (half, fixed) = (note("half the parser work"), note("the parser is fixed"));
        let rows: Vec<(usize, &str)> = lines
            .iter()
            .enumerate()
            .filter(|(_, line)| line.contains(" KB"))
            .map(|(at, line)| (at, *line))
            .collect();
        assert_eq!(rows.len(), 3, "{marker}");
        assert!(
            rows[0].0 < half && columns(rows[0].1) == ("4–5", "20"),
            "the first cut's row weighs turns 4 and 5 alone, as it did before the second cut: {marker}"
        );
        assert!(
            rows[1].0 > half && rows[1].0 < fixed && columns(rows[1].1) == ("3", "40"),
            "the survivor user turn stands under the cut that took it: {marker}"
        );
        assert!(
            rows[2].0 > half && rows[2].0 < fixed && columns(rows[2].1) == ("6", "4"),
            "turn 6 left with it, in a fragment of its own: {marker}"
        );
        assert!(
            marker.starts_with("[EXARCH // Exchange 3 has left your context."),
            "{marker}"
        );
    }

    /// The marker is a function of the structure, so a drop that empties a
    /// partially evicted exchange is spoken of as a whole departure at once.
    #[test]
    fn a_drop_that_empties_a_partially_evicted_exchange_rewrites_its_sentence() {
        let mut context = context(
            vec![
                here(3, 3, "fix the parser", 40 * 1024),
                here(4, 3, "", 12 * 1024),
                here(5, 3, "", 8 * 1024),
            ],
            Vec::new(),
        );
        context.apply_context_op(&ContextOp::Evict {
            through: 4,
            note: None,
        });
        assert_eq!(resident_ids(&context), vec![3, 5]);
        let partial = render_head(&context).expect("one cut renders one marker");
        assert!(
            partial.starts_with("[EXARCH // Turn 4 of exchange 3 has left your context."),
            "{partial}"
        );

        context.apply_context_op(&ContextOp::Drop { exchanges: vec![3] });
        assert_eq!(
            held(&context),
            vec![
                (3, Held::Dropped),
                (4, Held::Evicted { cut: 0 }),
                (5, Held::Dropped),
            ]
        );
        let marker = render_head(&context).expect("the evicted turn is still indexed");
        assert!(
            marker.starts_with("[EXARCH // Exchange 3 has left your context."),
            "{marker}"
        );
        let rows: Vec<&str> = marker.lines().filter(|line| line.contains(" KB")).collect();
        assert_eq!(rows.len(), 1, "a drop adds no row of its own: {marker}");
        assert_eq!(columns(rows[0]), ("4", "12"), "{marker}");
    }
}
