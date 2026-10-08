//! The model fold: the provider-facing projection built off the [`Protocol`]
//! records of `record.jsonl` — the same `step`, whether it is applied inline
//! on the attend thread right after [`super::Emitter::emit`] returns, or by
//! [`resume()`] from disk.
//!
//! One structure, one fold, everything else a projection. [`Context`] holds
//! every turn the lineage has recorded: a turn is either *here* — its records
//! in memory — or *there* — a [`Pointer`] to the file and byte ranges that
//! hold them. The context sent to the provider, the survey, the transcript
//! index, the markers standing at its holes and the eviction plan are all
//! pure functions of it.
//!
//! [`Display`](super::Display) and [`Forensic`](super::Forensic) records pass
//! through untouched: this fold projects the protocol subsequence alone.
//!
//! No recorded protocol record is ever discarded: a turn that leaves the
//! context keeps the address its records were measured at, and `read_at`
//! reads them back through it one record at a time.

mod fold;
mod render;
mod resume;
mod state;
mod table;
mod transcript;

pub use resume::resume;

use super::{Fold, Locus, Protocol, Record, Recorded, Refusal, Role, TurnKind};
use genai::chat::{ChatMessage, ChatRole, ContentPart, ToolResponse};
use serde::{Deserialize, Serialize};
pub(super) use state::validate_result_ids;
use state::{State, admissible_prefix};
use std::path::PathBuf;
use table::Table;

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

/// Every turn this lineage recorded, and where each of them is.
///
/// The context is the [`Body::Here`] subsequence of `turns`. Invariant: the
/// first resident turn is a user turn.
pub struct Context {
    table: Table,
    state: State,
    /// Protocol records folded.
    len: usize,
    /// The index of the last context edit — a token measure taken at or
    /// before it is stale.
    newest_edit: Option<usize>,
}

/// One turn: the atom an eviction addresses.
///
/// A *user turn* is a prompt, or an import's opening. An *assistant turn* is
/// the assistant message, the tool results it called for, and any steering
/// before the next request.
struct Turn {
    id: u64,
    role: Role,
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
    /// Departed. `cut` indexes [`Context::notes`].
    There { at: Pointer, cut: usize },
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
pub struct TurnRow {
    pub id: u64,
    pub role: Role,
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
}

impl Held {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Evicted { .. } => "evicted",
        }
    }
}

/// One row of the link a fork carries: the row, and where its transcript copy
/// lies.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Linked {
    pub row: TurnRow,
    pub at: Pointer,
}

impl Turn {
    /// The one way a turn leaves the structure as a row: `held` is projected
    /// off the body, so the two cannot disagree.
    fn row(&self) -> TurnRow {
        TurnRow {
            id: self.id,
            role: self.role,
            kind: self.kind,
            label: self.label.clone(),
            bytes: self.bytes,
            held: self.held(),
        }
    }

    fn held(&self) -> Held {
        match &self.body {
            Body::Here { .. } => Held::Resident,
            Body::There { cut, .. } => Held::Evicted { cut: *cut },
        }
    }

    /// A user turn is the one turn a cut may keep silently — see
    /// [`Context::resolve_cut`]'s survivor rule.
    fn is_user(&self) -> bool {
        self.role == Role::User
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

fn message_bytes(messages: &[ChatMessage]) -> usize {
    messages
        .iter()
        .map(|m| serde_json::to_string(m).map_or(0, |s| s.len()))
        .sum()
}

/// The same, for a resident turn's witnessed records.
fn resident_messages(records: &[Recorded<Protocol>]) -> Vec<ChatMessage> {
    records
        .iter()
        .map(|recorded| recorded.value().clone())
        .flat_map(into_chat_messages)
        .collect()
}

impl Context {
    /// The one constructor: `source` is `record.jsonl`'s path, fixed for this
    /// structure's life.
    #[must_use]
    pub fn new(source: PathBuf) -> Self {
        Self {
            table: Table::new(source),
            state: State::default(),
            len: 0,
            newest_edit: None,
        }
    }

    /// The model's own line to its future self, one slot per eviction.
    #[must_use]
    pub fn notes(&self) -> &[Option<String>] {
        self.table.notes()
    }

    /// The one way a turn leaves the structure as an address: the link a fork
    /// carries, one row per turn the lineage recorded.
    ///
    /// `at` is `origin` where there is one, the departure address where the
    /// turn has left, and this log's own file for a turn first recorded here
    /// and still in the context. Every row's `kind` is `Inherited`, a linked
    /// row being inherited by construction.
    pub(crate) fn linked(&self) -> Vec<Linked> {
        self.table
            .turns()
            .iter()
            .map(|turn| Linked {
                row: TurnRow {
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
            source: self.table.source().to_path_buf(),
            loci: records
                .iter()
                .map(|recorded| recorded.locus().clone())
                .collect(),
        }
    }

    /// The context, in id order.
    fn resident(&self) -> impl Iterator<Item = &Turn> {
        self.table.turns().iter().filter(|turn| turn.is_resident())
    }

    /// The highest id the structure has reached — every id-bearing record
    /// past it opens a turn. Departed rows are kept, so an empty table means
    /// the lineage never minted an id.
    fn reach(&self) -> Option<u64> {
        self.table.turns().last().map(|turn| turn.id)
    }

    fn turn(&self, id: u64) -> Option<&Turn> {
        self.table.turns().iter().find(|turn| turn.id == id)
    }

    /// The newest turn's id — what a tool result's `TURN:` stamp names, and
    /// the turn a steering line or a continuation extends.
    pub fn current_turn(&self) -> Option<u64> {
        self.reach()
    }

    /// The newest user turn's id: the prompt the work in hand answers.
    pub fn current_prompt(&self) -> Option<u64> {
        self.table
            .turns()
            .iter()
            .rev()
            .find(|turn| turn.is_user())
            .map(|turn| turn.id)
    }

    /// The prompt a continuation may extend: the newest turn's, while that
    /// turn is still in the context.
    pub(crate) fn live_prompt(&self) -> Option<u64> {
        let last = self
            .table
            .turns()
            .last()
            .filter(|turn| turn.is_resident())?;
        self.prompt_of(last.id)
    }

    /// The nearest user turn at or before `id` in table order — the prompt
    /// `id` belongs to.
    pub(crate) fn prompt_of(&self, id: u64) -> Option<u64> {
        let turns = self.table.turns();
        let at = turns.iter().position(|turn| turn.id == id)?;
        turns[..=at]
            .iter()
            .rev()
            .find(|turn| turn.is_user())
            .map(|turn| turn.id)
    }

    /// The turns answering `prompt`: those after it in table order, up to the
    /// next user turn. With [`Self::prompt_of`], the one place the grouping a
    /// turn's role and order imply is derived.
    fn answers(&self, prompt: u64) -> impl Iterator<Item = &Turn> {
        let turns = self.table.turns();
        let from = turns
            .iter()
            .position(|turn| turn.id == prompt)
            .map_or(turns.len(), |at| at + 1);
        turns[from..].iter().take_while(|turn| !turn.is_user())
    }

    /// The id the next turn this log opens is minted above.
    fn id_floor(&self) -> u64 {
        self.reach().unwrap_or(0)
    }

    /// The id the next prompt or assistant message takes, minted here and
    /// never chosen by a caller.
    pub(crate) fn next_id(&self) -> u64 {
        self.id_floor().saturating_add(1)
    }

    pub fn log_len(&self) -> usize {
        self.len
    }

    pub fn token_measure_is_stale(&self, measured_at: usize) -> bool {
        self.newest_edit.is_some_and(|index| index >= measured_at)
    }

    /// The material a `mnemon` child's seed carries, turn by turn under this
    /// log's own ids: the one other place ownership genuinely transfers,
    /// beside the wire door.
    ///
    /// The last turn seeds through [`admissible_prefix`], cut to the longest
    /// run that owes no tool result — a batch in flight and a dangling edit
    /// left after it both fall out of that same rule.
    pub(crate) fn inherited_seed(&self) -> Vec<(u64, Vec<ChatMessage>)> {
        let resident: Vec<&Turn> = self.resident().collect();
        let Some((last, held)) = resident.split_last() else {
            return Vec::new();
        };
        let mut seed: Vec<(u64, Vec<ChatMessage>)> = held
            .iter()
            .map(|turn| (turn.id, resident_messages(turn.records())))
            .collect();
        let records: Vec<&Protocol> = last.records().iter().map(Recorded::value).collect();
        seed.push((
            last.id,
            admissible_prefix(&records)
                .iter()
                .map(|record| (*record).clone())
                .flat_map(into_chat_messages)
                .collect(),
        ));
        seed
    }

    /// One row per resident turn, at the weight the fold summed, beside the
    /// truth about what is sent: `total_bytes` is [`Self::history_bytes`],
    /// not a second opinion on it, so each marker's weight is counted once
    /// and never as a row.
    pub(crate) fn context_survey(&self) -> ContextSurvey {
        ContextSurvey {
            rows: self.resident().map(Turn::row).collect(),
            total_bytes: self.history_bytes(),
        }
    }

    /// Every turn the transcript holds, in id order, each saying whether it
    /// is still in the context. The whole lineage's, since a fork inherits
    /// the table.
    pub(crate) fn transcript_index(&self) -> Vec<TurnRow> {
        self.table.turns().iter().map(Turn::row).collect()
    }
}

/// What a turn's opening line is clipped to as it opens, and the column a
/// marker pads that label to. One measure, so no reader of a row — a
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

/// Sorted ids as en-dash runs: `[41, 42, 43, 50]` reads `41–43, 50`. The one
/// spelling of a turn set anything shows a reader.
#[must_use]
pub fn runs(ids: &[u64]) -> String {
    let mut sorted = ids.to_vec();
    sorted.sort_unstable();
    let mut runs: Vec<(u64, u64)> = Vec::new();
    for id in sorted {
        match runs.last_mut() {
            Some(open) if open.1 + 1 == id => open.1 = id,
            Some(open) if open.1 == id => {}
            _ => runs.push((id, id)),
        }
    }
    runs.iter()
        .map(|(from, to)| {
            if from == to {
                from.to_string()
            } else {
                format!("{from}–{to}")
            }
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// A turn's label: the message's opening line, or — where a call opened the
/// message and no prose did — the intent that call declared.
fn message_label(message: &ChatMessage) -> String {
    let line = opening_line(message.content.first_text().unwrap_or_default());
    if line.is_empty() {
        opening_line(&message_intent(message))
    } else {
        line
    }
}

/// The intent a call-first message declared: the first tool call's `description`
/// argument — the one field a tool schema reserves for what the call is for.
fn message_intent(message: &ChatMessage) -> String {
    message
        .content
        .iter()
        .find_map(|part| match part {
            ContentPart::ToolCall(call) => call
                .fn_arguments
                .get("description")
                .and_then(serde_json::Value::as_str),
            ContentPart::Text(_)
            | ContentPart::ToolResponse(_)
            | ContentPart::ReasoningContent(_)
            | ContentPart::Binary(_)
            | ContentPart::Custom(_)
            | ContentPart::ThoughtSignature(_) => None,
        })
        .unwrap_or_default()
        .to_string()
}

/// Whose turn an imported message opens: a tool result answers the assistant's
/// own work, so only a user message is the user's.
fn message_role(message: &ChatMessage) -> Role {
    if message.role == ChatRole::User {
        Role::User
    } else {
        Role::Assistant
    }
}

fn not_recorded_refusal_turn(turn: u64, reach: Option<u64>) -> String {
    match reach {
        Some(reach) => format!("turn {turn} is not recorded: the latest is {reach}"),
        None => format!("turn {turn} is not recorded: nothing has been recorded yet"),
    }
}

fn into_chat_messages(protocol: Protocol) -> Vec<ChatMessage> {
    match protocol {
        Protocol::UserPrompt { text, .. } | Protocol::Steering { text } => {
            vec![ChatMessage::user(text)]
        }
        Protocol::ContextMessage { message, .. } | Protocol::AssistantMessage { message, .. } => {
            vec![message]
        }
        Protocol::ToolResults { results } => results
            .into_iter()
            .map(|r| ChatMessage::from(ToolResponse::new(&r.id, &r.content)))
            .collect(),
        Protocol::Evicted { .. } | Protocol::Rewound { .. } | Protocol::Inherited { .. } => {
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests;
