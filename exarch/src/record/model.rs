//! The model fold: the provider-facing projection built off the [`Protocol`]
//! records of `record.jsonl` — the same `step`, whether it is applied inline
//! on the attend thread right after [`super::Emitter::emit`] returns, or by
//! [`resume()`] from disk.
//!
//! One structure, one fold, everything else a projection. [`Context`] holds
//! every turn the lineage has recorded: a turn is either *here* — its records
//! in memory — or *there* — a [`Pointer`] to the file and byte ranges that
//! hold them. Each eviction leaves a [`Residue`] standing for the span it
//! took. The context sent to the provider, the survey, the transcript index
//! and the eviction plan are all pure functions of it.
//!
//! [`Display`](super::Display) and [`Forensic`](super::Forensic) records pass
//! through untouched: this fold projects the protocol subsequence alone.
//!
//! No recorded protocol record is ever discarded: a turn that leaves the
//! context, by eviction or by withdrawal, keeps the address its records were
//! measured at, and `read_at` reads them back through it one record at a time.
//! Rows are never removed, so ids stay monotone down the lineage.
//!
//! A link ([`Protocol::Inherited`]) is the one record whose meaning lies in
//! another file: its step reads the parent's records through digest-checked
//! pointers, and that file only ever grows.

mod fold;
mod render;
mod resume;
mod span;
mod state;
mod table;
mod transcript;

pub use resume::resume;
pub use span::Span;

use super::{EditAuthority, Fold, Locus, Protocol, Record, Recorded, Refusal, Role, TurnKind};
use genai::chat::{ChatMessage, ChatRole, ContentPart, ToolResponse};
use render::residue_label;
use serde::{Deserialize, Serialize};
use state::State;
pub(super) use state::validate_result_ids;
use std::path::PathBuf;
use std::sync::Arc;
use table::{Item, Table};

/// Marker type implementing [`Fold`] for the model projection; carries no
/// state of its own; [`Context`] is where the projection lives.
pub struct Model;

impl Fold for Model {
    type Memo = Context;

    fn step(memo: &mut Context, record: Recorded<Record>) -> Result<(), Refusal> {
        match record.into_parts() {
            (locus, Record::Protocol(p)) => memo.step(Recorded::new(locus, p)),
            (_, Record::Display(_) | Record::Forensic(_)) => Ok(()),
        }
    }
}

/// `` exarch-context `survey ``'s answer: one row per frontier item, beside
/// the truth about what is sent.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ContextSurvey {
    pub rows: Vec<SpanRow>,
    pub total_bytes: usize,
}

/// Every turn this lineage recorded, every residue its evictions left, and
/// where each turn is.
///
/// The context is the frontier: every item still resident, in id order.
/// Invariant: the frontier opens with a user-role item.
pub struct Context {
    table: Table,
    state: State,
    /// The prompt every request goes out under; empty until the first
    /// [`Protocol::System`].
    system: Arc<str>,
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
    /// ancestor's file: `records` are the admissible prefix read back from it,
    /// which the model is sent; the transcript's copy is at `origin`.
    Here {
        records: Vec<Recorded<Protocol>>,
        origin: Option<Pointer>,
    },
    /// Departed, as stamped when it left.
    There { at: Pointer, departure: Departure },
}

/// One residue as the table keeps it: the record, and where it is — what a
/// turn's [`Body`] says of a turn, without records.
struct Standing {
    residue: Residue,
    /// `None` while the residue is resident, else its stamp, as
    /// [`Body::There`] keeps a turn's.
    departure: Option<Departure>,
}

/// Where a departed item is, stamped as it leaves: a [`Location`] that is
/// never resident.
///
/// A stamp never goes stale. The residue an item departs under covers it, and
/// any later residue covering it covers that one whole, since residues are
/// laminar, so the first is its innermost holder for good. Withdrawal
/// dominates, and is permanent.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Departure {
    Under(Span),
    Withdrawn,
}

impl From<Departure> for Location {
    fn from(departure: Departure) -> Self {
        match departure {
            Departure::Under(span) => Self::Under(span),
            Departure::Withdrawn => Self::Withdrawn,
        }
    }
}

/// One eviction as recorded, and what it leaves standing: the span of turn
/// ids it took and the model's note, `None` for the harness's own. It has no
/// id: its span is its address.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Residue {
    pub span: Span,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub by: EditAuthority,
}

/// Where a turn's records lie: a file, and their loci in it.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pointer {
    pub source: PathBuf,
    pub loci: Vec<Locus>,
}

/// What an item is: a turn of some provenance, or a residue.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ItemKind {
    Turn(TurnKind),
    Residue,
}

impl ItemKind {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Turn(kind) => kind.as_str(),
            Self::Residue => "residue",
        }
    }
}

/// The one line every view draws for an item: survey, index, TUI card. A
/// turn's span is its own id alone; an item is named by `(span, kind)`.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SpanRow {
    pub span: Span,
    pub role: Role,
    pub kind: ItemKind,
    pub label: String,
    pub bytes: usize,
    pub location: Location,
}

/// One turn as a link carries it, `location` kept for the link's judge.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct TurnRow {
    pub id: u64,
    pub role: Role,
    pub kind: TurnKind,
    pub label: String,
    pub bytes: usize,
    pub location: Location,
}

/// Whether an item is in the context, and what holds it if not: kept on the
/// item, stamped as it departs, and never derived.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Location {
    Resident,
    /// Under the residue whose eviction took it, named by its span: the
    /// smallest later residue covering it.
    Under(Span),
    /// Taken out by `/clear` or `/rewind`.
    Withdrawn,
}

impl Location {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Resident => "resident",
            Self::Under(_) => "under",
            Self::Withdrawn => "withdrawn",
        }
    }

    /// Where an item so located has departed to; `None` while resident.
    fn departure(self) -> Option<Departure> {
        match self {
            Self::Resident => None,
            Self::Under(span) => Some(Departure::Under(span)),
            Self::Withdrawn => Some(Departure::Withdrawn),
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
    /// A user turn is a prompt, or an import's opening.
    fn is_user(&self) -> bool {
        self.role == Role::User
    }

    fn location(&self) -> Location {
        match self.body {
            Body::Here { .. } => Location::Resident,
            Body::There { departure, .. } => departure.into(),
        }
    }

    fn is_resident(&self) -> bool {
        self.location() == Location::Resident
    }

    fn is_withdrawn(&self) -> bool {
        self.location() == Location::Withdrawn
    }

    /// The records this turn holds in memory; empty once it has left.
    fn records(&self) -> &[Recorded<Protocol>] {
        match &self.body {
            Body::Here { records, .. } => records,
            Body::There { .. } => &[],
        }
    }
}

impl Standing {
    fn location(&self) -> Location {
        self.departure.map_or(Location::Resident, Location::from)
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
        .flat_map(|recorded| chat_messages(recorded.value()))
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
            system: Arc::default(),
            len: 0,
            newest_edit: None,
        }
    }

    /// The prompt every request goes out under.
    #[must_use]
    pub fn system(&self) -> &Arc<str> {
        &self.system
    }

    /// Every residue the lineage's evictions left, in creation order.
    pub(crate) fn residues(&self) -> Vec<Residue> {
        self.table.residues().cloned().collect()
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
                    id: turn.id,
                    role: turn.role,
                    kind: TurnKind::Inherited,
                    label: turn.label.clone(),
                    bytes: turn.bytes,
                    location: turn.location(),
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
        self.table.turn(id)
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

    /// One row per frontier item, beside the truth about what is sent:
    /// `total_bytes` is [`Self::history_bytes`], not a second opinion on it.
    pub(crate) fn context_survey(&self) -> ContextSurvey {
        ContextSurvey {
            rows: self.table.frontier().iter().map(Item::row).collect(),
            total_bytes: self.history_bytes(),
        }
    }

    /// Every turn and every residue the lineage recorded, each saying what
    /// holds it: by `from`, a wider span first, a later residue first, the
    /// turn last. The whole lineage's, since a fork inherits the table.
    pub(crate) fn transcript_index(&self) -> Vec<SpanRow> {
        self.table.items().iter().map(Item::row).collect()
    }
}

impl Item<'_> {
    /// The item's one line, for the survey and the index alike.
    fn row(&self) -> SpanRow {
        let (role, kind, label) = match *self {
            Item::Turn(turn) => (turn.role, ItemKind::Turn(turn.kind), turn.label.clone()),
            Item::Residue(standing) => (
                Role::User,
                ItemKind::Residue,
                residue_label(&standing.residue),
            ),
        };
        SpanRow {
            span: self.span(),
            role,
            kind,
            label,
            bytes: self.bytes(),
            location: self.location(),
        }
    }
}

/// What a turn's opening line is clipped to as it opens. One measure, so no
/// reader of a row holds a different length.
const OPENING_CHARS: usize = 50;

fn opening_line(text: &str) -> String {
    text.lines()
        .next()
        .unwrap_or_default()
        .chars()
        .take(OPENING_CHARS)
        .collect()
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

fn chat_messages(protocol: &Protocol) -> Vec<ChatMessage> {
    match protocol {
        Protocol::UserPrompt { text, .. } | Protocol::Steering { text, .. } => {
            vec![ChatMessage::user(text.clone())]
        }
        Protocol::Import { message, .. } | Protocol::AssistantMessage { message, .. } => {
            vec![message.clone()]
        }
        Protocol::ToolResults { results } => results
            .iter()
            .map(|r| ChatMessage::from(ToolResponse::new(&r.id, &r.content)))
            .collect(),
        Protocol::System { .. }
        | Protocol::Evicted { .. }
        | Protocol::Rewound { .. }
        | Protocol::Cleared
        | Protocol::Inherited { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests;
