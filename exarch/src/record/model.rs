//! The model fold: the provider-facing projection built off the [`Protocol`]
//! records of `record.jsonl` — the same `step`, whether it is applied inline
//! on the attend thread right after [`super::Emitter::emit`] returns, or by
//! [`resume()`] from disk.
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
//! context keeps the address its records were measured at, and `read_at`
//! reads them back through it one record at a time.

mod door;
mod fold;
mod render;
mod resume;
mod state;

pub(crate) use door::TranscriptRead;
pub use render::Rendered;
pub use resume::resume;

use super::{Fold, Locus, Protocol, Record, Recorded, Refusal};
use crate::agent::event::{ContextSurvey, TurnKind};
use genai::chat::{ChatMessage, ToolResponse};
use serde::{Deserialize, Serialize};
use state::{State, admissible_prefix};
use std::path::PathBuf;

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

#[cfg(test)]
mod tests {
    use super::render::render_head;
    use super::*;
    use crate::agent::event::ContextOp;

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
        let context = context(
            vec![here(1, 1, "one", 100), here(2, 1, "", 100)],
            Vec::new(),
        );
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
