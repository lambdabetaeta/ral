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
pub(crate) use transcript::TranscriptRead;

use super::{Fold, Locus, Protocol, Record, Recorded, Refusal};
use crate::agent::event::{ContextSurvey, Role, TurnKind};
use genai::chat::{ChatMessage, ChatRole, ToolResponse};
use serde::{Deserialize, Serialize};
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

fn message_label(message: &ChatMessage) -> String {
    opening_line(message.content.first_text().unwrap_or_default())
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
        Some(reach) => format!("turn {turn} is not recorded — the latest is {reach}"),
        None => format!("turn {turn} is not recorded — nothing has been recorded yet"),
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
        Protocol::Evicted { .. } | Protocol::Inherited { .. } => Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::render::render_marker;
    use super::*;
    use crate::record::Seq;

    /// A turn in the context. Its records are elided: every fold under test
    /// here reads `bytes`, never the material.
    fn here(id: u64, role: Role, label: &str, bytes: usize) -> Turn {
        Turn {
            id,
            role,
            kind: TurnKind::Own,
            label: label.to_string(),
            bytes,
            body: Body::Here {
                records: Vec::new(),
                origin: None,
            },
        }
    }

    /// A departed turn: `cut` indexes the notes.
    fn there(id: u64, role: Role, label: &str, bytes: usize, cut: usize) -> Turn {
        Turn {
            body: Body::There {
                at: Pointer {
                    source: PathBuf::from("record.jsonl"),
                    loci: Vec::new(),
                },
                cut,
            },
            ..here(id, role, label, bytes)
        }
    }

    fn context(turns: Vec<Turn>, notes: Vec<Option<String>>) -> Context {
        Context {
            table: Table::seeded(PathBuf::from("record.jsonl"), turns, notes),
            ..Context::new(PathBuf::from("record.jsonl"))
        }
    }

    /// One record through the live fold. The table is what these tests read,
    /// so a placeholder locus is address enough.
    fn step(context: &mut Context, protocol: Protocol) -> Result<(), Refusal> {
        let seq = Seq::new(context.log_len() as u64 + 1);
        context.step(Recorded::new(Locus::placeholder(seq), protocol))
    }

    fn held(context: &Context) -> Vec<(u64, Held)> {
        context
            .table
            .turns()
            .iter()
            .map(|turn| (turn.id, turn.held()))
            .collect()
    }

    fn resident_ids(context: &Context) -> Vec<u64> {
        context.resident().map(|turn| turn.id).collect()
    }

    /// Every hole's marker, in the order [`Context::rendered`] stands them.
    fn markers(context: &Context) -> Vec<String> {
        let turns = context.table.turns();
        let mut markers = Vec::new();
        let mut at = 0;
        while at < turns.len() {
            if turns[at].is_resident() {
                at += 1;
                continue;
            }
            let end = at + turns[at..].iter().take_while(|t| !t.is_resident()).count();
            markers.push(render_marker(context, &turns[at..end]));
            at = end;
        }
        markers
    }

    /// The one marker a table with a single hole draws.
    fn marker(context: &Context) -> String {
        let drawn = markers(context);
        let [marker] = drawn.as_slice() else {
            panic!("one hole draws one marker, got {drawn:?}")
        };
        marker.clone()
    }

    /// Evict through the fold, as a recorded cut does: resolve, then depart
    /// exactly what was resolved.
    fn evict(context: &mut Context, turns: &[u64], note: Option<&str>) {
        let resolved = context.resolve_cut(turns).expect("a legal cut");
        context.table.evict(&resolved, note.map(str::to_string));
    }

    /// A marker row's turn id and its weight in whole KB.
    fn columns(line: &str) -> (&str, &str) {
        // The marker's last line closes its bracket.
        let row = line.trim_end_matches(']');
        let id = row
            .split_whitespace()
            .next()
            .expect("a row opens with its turn id");
        let mut fields = row.split_whitespace().rev();
        assert_eq!(fields.next(), Some("KB"), "{line}");
        let weight = fields.next().expect("a row ends in a weight");
        (id, weight)
    }

    /// Sorted ids read as en-dash runs — the one spelling a turn set has.
    #[test]
    fn runs_reads_ids_as_en_dash_runs() {
        assert_eq!(runs(&[41, 42, 43, 50]), "41–43, 50");
        assert_eq!(runs(&[7]), "7");
        assert_eq!(runs(&[]), "");
        assert_eq!(runs(&[3, 1, 2]), "1–3", "the ids are sorted first");
        assert_eq!(runs(&[5, 5, 6]), "5–6", "a repeat is no run of its own");
    }

    /// The grouping a turn's role and order imply: the prompt a turn answers,
    /// and the turns answering a prompt — a prompt with none included.
    #[test]
    fn a_prompt_and_its_answers_are_read_off_role_and_order() {
        let context = context(
            vec![
                here(1, Role::User, "one", 10),
                here(2, Role::Assistant, "", 10),
                here(3, Role::Assistant, "", 10),
                here(4, Role::User, "four", 10),
                here(5, Role::Assistant, "", 10),
                here(6, Role::User, "six", 10),
            ],
            Vec::new(),
        );
        let answers = |prompt| context.answers(prompt).map(|t| t.id).collect::<Vec<_>>();
        assert_eq!(
            (1..=6).map(|id| context.prompt_of(id)).collect::<Vec<_>>(),
            vec![Some(1), Some(1), Some(1), Some(4), Some(4), Some(6)]
        );
        assert_eq!(context.prompt_of(9), None, "turn 9 is not recorded");
        assert_eq!(answers(1), vec![2, 3]);
        assert_eq!(answers(4), vec![5]);
        assert_eq!(answers(6), Vec::<u64>::new(), "the newest prompt has none");
        assert_eq!(answers(9), Vec::<u64>::new());
        assert_eq!(context.current_prompt(), Some(6));
        assert_eq!(context.current_turn(), Some(6));
    }

    /// A log that has minted no id has no newest turn to name.
    #[test]
    fn an_empty_log_has_no_current_turn() {
        let empty = Context::new(PathBuf::from("record.jsonl"));
        assert_eq!(empty.current_turn(), None);
        assert_eq!(empty.current_prompt(), None);
    }

    /// An imported message opens a turn in its own voice: the link carries no
    /// role of its own, and the message is the one datum that says whose it
    /// was.
    #[test]
    fn an_import_opens_a_turn_in_its_messages_voice() {
        let mut context = Context::new(PathBuf::from("record.jsonl"));
        step(
            &mut context,
            Protocol::ContextMessage {
                id: 1,
                message: ChatMessage::user("the parent's prompt"),
            },
        )
        .expect("an import at a ready boundary");
        step(
            &mut context,
            Protocol::ContextMessage {
                id: 2,
                message: ChatMessage::assistant("the parent's answer"),
            },
        )
        .expect("an import at a ready boundary");
        assert_eq!(
            context
                .transcript_index()
                .iter()
                .map(|row| (row.id, row.role, row.kind))
                .collect::<Vec<_>>(),
            vec![
                (1, Role::User, TurnKind::Import),
                (2, Role::Assistant, TurnKind::Import),
            ]
        );
        assert_eq!(context.current_prompt(), Some(1));
        assert_eq!(context.current_turn(), Some(2));
    }

    /// A prompt one of whose answers the set does not name stays with it,
    /// silently.
    #[test]
    fn a_cut_keeps_a_user_turn_with_survivors() {
        let mut context = context(
            vec![
                here(1, Role::User, "one", 10),
                here(2, Role::Assistant, "", 10),
                here(3, Role::User, "three", 10),
                here(4, Role::Assistant, "", 10),
                here(5, Role::Assistant, "", 10),
            ],
            Vec::new(),
        );
        assert_eq!(
            context.resolve_cut(&[1, 2, 3, 4]).expect("a legal cut"),
            vec![1, 2, 4],
            "turn 3 is the prompt turn 5 still answers"
        );
        evict(&mut context, &[1, 2, 3, 4], None);
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
        assert_eq!(resident_ids(&context), vec![3, 5]);
        assert_eq!(context.notes(), &[None]);
    }

    /// A prompt that keeps no answer leaves like any other turn.
    #[test]
    fn a_cut_takes_a_user_turn_with_no_survivors() {
        let mut context = context(
            vec![
                here(1, Role::User, "one", 10),
                here(2, Role::Assistant, "", 10),
                here(3, Role::User, "three", 10),
            ],
            Vec::new(),
        );
        evict(&mut context, &[1, 2], None);
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

    /// The survivor rule refuses rather than takes nothing, and says which
    /// answers would have to be named too.
    #[test]
    fn a_cut_that_would_take_nothing_names_what_to_add() {
        let context = context(
            vec![
                here(30, Role::User, "the prompt", 10),
                here(31, Role::Assistant, "", 10),
                here(32, Role::Assistant, "", 10),
            ],
            Vec::new(),
        );
        assert_eq!(
            context.resolve_cut(&[30]).unwrap_err(),
            "turn 30 is the prompt whose turns 31–32 are still in your context, and a prompt \
             stays with them — name them too, or leave it"
        );
        assert_eq!(
            context.resolve_cut(&[]).unwrap_err(),
            "an eviction must name at least one turn"
        );
    }

    /// The one turn no eviction takes is the one being written.
    #[test]
    fn a_cut_refuses_the_unclosed_turn() {
        let mut context = context(
            vec![
                here(1, Role::User, "one", 10),
                here(2, Role::Assistant, "", 10),
            ],
            Vec::new(),
        );
        context.state = State::AwaitingAssistantAfterUser;
        assert_eq!(
            context.resolve_cut(&[2]).unwrap_err(),
            "turn 2 is being written now — an eviction keeps the work in hand"
        );
        assert_eq!(
            context.resolve_cut(&[9]).unwrap_err(),
            "turn 9 is not recorded — the latest is 2"
        );
        assert_eq!(
            context.resolve_cut(&[1]).unwrap_err(),
            "turn 1 is the prompt whose turn 2 is still in your context, and a prompt stays with \
             it — name it too, or leave it",
            "the prompt stays with the unclosed turn, which the set may not name"
        );
    }

    /// A detour inside the work in hand: the good work stands, the detour
    /// leaves as one hole, and the turn in hand is untouched.
    #[test]
    fn a_cut_inside_the_work_in_hand_leaves_one_hole() {
        let mut context = context(
            vec![
                here(30, Role::User, "fix the parser", 10),
                here(31, Role::Assistant, "", 10),
                here(32, Role::Assistant, "", 10),
                here(33, Role::Assistant, "", 10),
                here(34, Role::Assistant, "", 20 * 1024),
                here(35, Role::Assistant, "", 20 * 1024),
                here(36, Role::Assistant, "", 20 * 1024),
                here(37, Role::Assistant, "", 10),
            ],
            Vec::new(),
        );
        context.state = State::AwaitingAssistantAfterUser;
        assert_eq!(
            context.resolve_cut(&[34, 35, 36]).expect("a legal cut"),
            vec![34, 35, 36]
        );
        evict(
            &mut context,
            &[34, 35, 36],
            Some("that read was a dead end"),
        );
        assert_eq!(resident_ids(&context), vec![30, 31, 32, 33, 37]);
        let marker = marker(&context);
        assert!(
            marker.starts_with("[EXARCH // Turns 34–36 have left your context."),
            "{marker}"
        );
        assert!(
            marker.contains("`exarch-transcript `read [turns: !{range 34 37}]` reads turns 34 through 36 back as material"),
            "{marker}"
        );
        assert!(
            marker.contains("Your note: \"that read was a dead end\""),
            "{marker}"
        );
    }

    /// A row states the turn's id, whose it was, its opening line and its
    /// weight — and a hole of one turn says so in the singular.
    #[test]
    fn a_marker_row_sets_id_role_label_and_weight() {
        let context = context(
            vec![
                here(1, Role::User, "fix the parser", 10),
                there(2, Role::Assistant, "I will fix the parser", 12 * 1024, 0),
                here(3, Role::User, "next", 10),
            ],
            vec![Some("the parser is fixed".into())],
        );
        let marker = marker(&context);
        assert!(
            marker.starts_with("[EXARCH // Turn 2 has left your context."),
            "{marker}"
        );
        let row = marker
            .lines()
            .find(|line| line.contains(" KB"))
            .expect("one departed turn, one row");
        assert_eq!(
            row,
            format!(
                "{:>4}  {:<9} {:<OPENING_CHARS$}{:>5} KB",
                2, "assistant", "I will fix the parser", 12
            )
        );
    }

    /// A marker names every turn that left, draws one row for each, and puts
    /// each cut's note after its own rows.
    #[test]
    fn a_marker_draws_a_row_per_departed_turn() {
        let context = context(
            vec![
                there(1, Role::User, "fix the parser", 12 * 1024, 0),
                there(2, Role::Assistant, "", 0, 0),
                there(3, Role::Assistant, "", 0, 0),
                here(4, Role::User, "add tests for the fold", 0),
                there(5, Role::Assistant, "the fold reads", 85 * 1024, 1),
                here(6, Role::Assistant, "", 10),
            ],
            vec![Some("the parser is fixed".into()), None],
        );
        let [first, second] = markers(&context).try_into().expect("two holes");
        assert!(
            first.starts_with("[EXARCH // Turns 1–3 have left your context."),
            "{first}"
        );
        assert!(
            first.contains("`exarch-transcript `index` lists them."),
            "{first}"
        );
        let rows: Vec<&str> = first.lines().filter(|line| line.contains(" KB")).collect();
        assert_eq!(rows.len(), 3, "one row per departed turn, got {first}");
        assert!(
            rows[0].contains("user") && rows[0].contains("fix the parser"),
            "{first}"
        );
        assert_eq!(columns(rows[0]), ("1", "12"));
        assert!(
            first.contains("Your note: \"the parser is fixed\""),
            "{first}"
        );
        assert!(
            second.starts_with("[EXARCH // Turn 5 has left your context."),
            "{second}"
        );
        let rows: Vec<&str> = second.lines().filter(|line| line.contains(" KB")).collect();
        assert_eq!(rows.len(), 1, "{second}");
        assert!(
            rows[0].contains("assistant") && rows[0].contains("the fold reads"),
            "{second}"
        );
        assert_eq!(columns(rows[0]), ("5", "85"));
    }

    /// Two holes with a resident turn between them render two markers, each
    /// at its own place, and both weigh and count.
    #[test]
    fn two_holes_render_two_markers_in_place() {
        let context = context(
            vec![
                here(1, Role::User, "one", 10),
                there(2, Role::Assistant, "", 4 * 1024, 0),
                here(3, Role::User, "three", 10),
                there(4, Role::Assistant, "", 8 * 1024, 1),
                here(5, Role::User, "five", 10),
            ],
            vec![None, None],
        );
        assert_eq!(markers(&context).len(), 2);
        assert_eq!(
            context.event_count(),
            2,
            "no resident turn holds a record here, so the count is the two holes"
        );
        let weight: usize = markers(&context)
            .iter()
            .map(|text| message_bytes(std::slice::from_ref(&ChatMessage::user(text.clone()))))
            .sum();
        assert_eq!(
            context.history_bytes(),
            weight + 30,
            "each marker weighs beside the resident turns' own bytes"
        );
        let rendered = context.rendered();
        assert_eq!(
            rendered.len(),
            2,
            "the resident turns hold no records, so only the markers render"
        );
    }

    /// A suffix cut at a ready boundary may take the whole context, and the
    /// marker then stands alone.
    #[test]
    fn a_suffix_cut_may_empty_the_context() {
        let mut context = context(
            vec![
                here(1, Role::User, "one", 10),
                here(2, Role::Assistant, "", 10),
                here(3, Role::User, "three", 10),
            ],
            Vec::new(),
        );
        assert_eq!(
            context.suffix_from(1).expect("a resident anchor"),
            vec![1, 2, 3]
        );
        evict(&mut context, &[1, 2, 3], None);
        assert!(resident_ids(&context).is_empty());
        assert_eq!(markers(&context).len(), 1, "one hole, one marker");
        assert_eq!(context.rendered().len(), 1, "the marker stands alone");
        assert_eq!(
            context.suffix_from(1).unwrap_err(),
            "turn 1 has already left your context — no turn is still in it"
        );
        assert_eq!(
            context.suffix_from(9).unwrap_err(),
            "turn 9 is not recorded — the latest is 3"
        );
    }

    /// The plan spends the last turn's weight and its prompt's up front, then
    /// walks back until a turn does not fit — and answers the resolved set, a
    /// prompt with a surviving answer kept back.
    #[test]
    fn a_plan_walks_back_from_the_turn_in_hand() {
        let context = context(
            vec![
                here(1, Role::User, "one", 100),
                here(2, Role::Assistant, "", 100),
                here(3, Role::User, "three", 100),
                here(4, Role::Assistant, "", 100),
            ],
            Vec::new(),
        );
        assert_eq!(
            context.plan_eviction(250),
            Some(vec![1, 2]),
            "the turn in hand and its prompt are paid first, so turn 2 does not fit"
        );
        assert_eq!(context.plan_eviction(1000), None, "everything fits");
    }

    /// Nothing is old enough to shed when the work in hand alone fills the
    /// budget: a cut that would take no turn is no plan.
    #[test]
    fn a_lone_prompt_is_never_planned_away() {
        let context = context(
            vec![
                here(1, Role::User, "one", 100),
                here(2, Role::Assistant, "", 100),
            ],
            Vec::new(),
        );
        assert_eq!(context.plan_eviction(0), None);
    }

    /// A survivor user turn belongs to the cut that finally took it, and the
    /// earlier cut's rows never gain its weight after the fact.
    #[test]
    fn a_marker_attributes_a_survivor_user_turn_to_the_cut_that_took_it() {
        let mut context = context(
            vec![
                here(3, Role::User, "fix the parser", 40 * 1024),
                here(4, Role::Assistant, "", 12 * 1024),
                here(5, Role::Assistant, "", 8 * 1024),
                here(6, Role::Assistant, "", 4 * 1024),
            ],
            Vec::new(),
        );
        evict(&mut context, &[3, 4, 5], Some("half the parser work"));
        assert_eq!(
            resident_ids(&context),
            vec![3, 6],
            "the prompt stays with the answer that survived it"
        );
        let first = marker(&context);
        let rows: Vec<&str> = first.lines().filter(|line| line.contains(" KB")).collect();
        assert_eq!(rows.len(), 2, "{first}");
        assert_eq!(columns(rows[0]), ("4", "12"), "{first}");
        assert_eq!(columns(rows[1]), ("5", "8"), "{first}");

        evict(&mut context, &[3, 6], Some("the parser is fixed"));
        assert_eq!(
            held(&context),
            vec![
                (3, Held::Evicted { cut: 1 }),
                (4, Held::Evicted { cut: 0 }),
                (5, Held::Evicted { cut: 0 }),
                (6, Held::Evicted { cut: 1 }),
            ]
        );
        let marker = marker(&context);
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
        assert_eq!(rows.len(), 4, "{marker}");
        assert!(
            rows[1].0 < half
                && columns(rows[0].1) == ("4", "12")
                && columns(rows[1].1) == ("5", "8"),
            "the first cut's rows weigh turns 4 and 5 alone, as they did before the second cut: {marker}"
        );
        assert!(
            rows[2].0 > half && rows[2].0 < fixed && columns(rows[2].1) == ("3", "40"),
            "the survivor user turn stands under the cut that took it: {marker}"
        );
        assert!(
            rows[3].0 > half && rows[3].0 < fixed && columns(rows[3].1) == ("6", "4"),
            "turn 6 left with it, under the same cut: {marker}"
        );
        assert!(
            marker.starts_with("[EXARCH // Turns 3–6 have left your context."),
            "{marker}"
        );
    }
}
