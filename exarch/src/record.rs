//! The one seam, one log: every fact a session records crosses here, once,
//! as a [`Record`].
//!
//! A `Record` is one of three classes: [`Protocol`] (verbatim payloads the
//! model fold folds), [`Display`] (commits the view fold folds), [`Forensic`]
//! (breadcrumbs neither fold projects but which are worth keeping).  Nothing
//! outside this module tree may mint a fourth class, invent a variant absent
//! from the disposition, or read the log back except through [`replay()`].
//!
//! [`Transient`] is the disjoint, unrecorded half of the channel: deltas, the
//! provisional open thinking line, and chrome that dies with the process.
#![deny(unused_results)]
#![deny(clippy::let_underscore_must_use)]
#![deny(clippy::wildcard_enum_match_arm)]

pub(crate) mod commit;
pub(crate) mod fault;
mod log;
pub(crate) mod model;
mod replay;
mod seam;
mod session;
mod view;

pub use model::{Held, Linked, Pointer, TurnRow};
pub use replay::{Refusal, replay};
pub use seam::Emitter;
pub(crate) use session::role_label;
pub use session::{
    AgentLog, GrepAnswer, GrepHit, Inherited, Resumed, TranscriptMessage, TranscriptPart,
    TranscriptTurn,
};
pub use view::{BLOCKS_WINDOW, Block, BlockKind, Delta, Verdict, View};

use crate::card::{Card, Change, DoneOutcome};
use crate::provider::{Provider, ProviderError, Tuning, Usage};
use genai::chat::ChatMessage;
use ral_core::first_order::FOValue;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;

/// The identity of an agent node — the trunk and every forked child alike.
///
/// Opaque, and what crosses the wire: every `exarch-agents` tag and every `Signal`
/// names a node by this.  It is a routing key, not a door — an `exarch-agents` tag
/// resolves by name through the fleet, and the frontend matches an arriving
/// id against the tab it was handed at that agent's birth.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Debug, Serialize, Deserialize)]
#[serde(transparent)]
pub struct AgentId(u64);

impl AgentId {
    pub const fn new(id: u64) -> Self {
        Self(id)
    }

    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl fmt::Display for AgentId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        self.0.fmt(f)
    }
}

/// What an agent is doing — a total state, not a label the next event erases.
///
/// Every moment of a session's life is one of these five, so a frontend can
/// always name the one it is in, including the idle one no label ever announced.
/// The transition is the delta ([`Transient::State`]); the states themselves
/// carry no clock and no counter.  A frontend times its own residence in one, which is
/// what makes a silent provider stream legible: [`Self::AwaitingModel`] standing
/// for minutes with no token arriving is a stall, where a label reset by each
/// arriving chunk could not tell the two apart.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AgentState {
    /// Parked at a human-input boundary, or between one and the next: nothing
    /// is in flight and the prompt is the agent's own next move.
    Ready,
    /// A step is open — the request in flight, or its response streaming.  The
    /// two are one state because the boundary between them is not the worker's
    /// to know; a frontend tells them apart by whether anything has arrived.
    AwaitingModel,
    /// A `ral` call is evaluating.
    Evaluating,
    /// The momentary state around the eviction edit.
    Evicting,
    /// Parked on a live child's result: a wait on the fleet, not on the human.
    WaitingOnAgents,
}

impl AgentState {
    /// The status-line label — lower case, unpunctuated; a frontend adds its
    /// own continuation mark to the [`Self::pending`] ones.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            Self::Ready => "ready",
            Self::AwaitingModel => "awaiting model",
            Self::Evaluating => "evaluating",
            Self::Evicting => "evicting",
            Self::WaitingOnAgents => "waiting on agents",
        }
    }

    /// Whether work is outstanding.  [`Self::Ready`] is the one settled state,
    /// so this is what a frontend keys a spinner, a repaint tick, or an elapsed
    /// clock on.
    #[must_use]
    pub fn pending(self) -> bool {
        self != Self::Ready
    }
}

/// Where the seam publishes a fact it has just appended, and the transients
/// that ride beside them: the one door onto the live channel, attachable after
/// the log exists because the log outlives any bus.
pub(crate) trait Publish: Send {
    fn fact(&self, recorded: Recorded<Record>);
    fn transient(&self, t: Transient);
}

/// One fact a session recorded, in one of three classes.
///
/// The outer shape is deliberately not `#[serde(tag = …)]`: each class
/// already tags its own variant, and folding a second tag over it would
/// collide the two keys.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Record {
    Protocol(Protocol),
    Display(Display),
    Forensic(Forensic),
}

/// The line `record.jsonl` actually holds: a [`Record`] with the wall-clock
/// moment it was appended.  Written and read by [`log`] alone — every other
/// module sees a bare `Record`, since a resume, a fold, and a channel
/// publish have no use yet for the moment a line was appended.
///
/// Deliberately not `#[serde(flatten)]`: flattening would swallow the class
/// enums' own `deny_unknown_fields` into this envelope's unknown-key check,
/// which defeats the reason that attribute is on them.
#[derive(Debug, Clone, Serialize, Deserialize)]
struct Entry {
    at_unix_ms: u64,
    record: Record,
}

mod sealed {
    pub trait Sealed {}
}

/// The three record classes a [`Recorded`] may carry, sealed so a fourth
/// cannot be minted outside this module.
///
/// [`emit`](Emitter::emit) is generic over `C: Class`, and folds demand
/// `&Protocol` / `&Display` / `&Forensic` in their own signatures rather
/// than matching `Record` themselves.
pub trait Class: sealed::Sealed + Clone + Into<Record> {}

impl sealed::Sealed for Protocol {}
impl sealed::Sealed for Display {}
impl sealed::Sealed for Forensic {}
impl Class for Protocol {}
impl Class for Display {}
impl Class for Forensic {}

impl From<Protocol> for Record {
    fn from(p: Protocol) -> Self {
        Self::Protocol(p)
    }
}
impl From<Display> for Record {
    fn from(d: Display) -> Self {
        Self::Display(d)
    }
}
impl From<Forensic> for Record {
    fn from(f: Forensic) -> Self {
        Self::Forensic(f)
    }
}

/// Verbatim payloads the model fold needs — the provider's exact `ChatMessage`
/// and tool-call ids.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Protocol {
    /// A prompt opening a user turn of its own, under the id it carries.
    UserPrompt {
        turn: u64,
        text: String,
    },
    /// A user line extending the newest turn: a steering line, or a nudge
    /// continuing the prompt in hand.
    Steering {
        text: String,
    },
    /// A message a `mnemon` child inherits from its parent's context, under
    /// the turn id the parent gave it.
    ContextMessage {
        id: u64,
        message: ChatMessage,
    },
    /// The link a `mnemon` child opens with: the parent's whole turn table,
    /// each row carrying the address its transcript copy lies at.  Written
    /// once, before the [`Protocol::ContextMessage`]s that re-record the
    /// resident ones as this log's own.
    Inherited {
        /// The parent's table at the fork, every row `kind: Inherited` and
        /// `held` as the parent had it, beside where that turn was first
        /// recorded.  The child's marker, survey and index are the same
        /// projections of it.
        turns: Vec<model::Linked>,
        /// The notes made at those evictions, so the child's markers are the
        /// same projection of the same structure.
        notes: Vec<Option<String>>,
    },
    AssistantMessage {
        /// This turn's id, minted by the log.
        turn: u64,
        message: ChatMessage,
        pending_tool_ids: Vec<String>,
        stop_reason: Option<String>,
    },
    ToolResults {
        results: Vec<ToolResult>,
    },
    /// One eviction: the resident turns it took, resolved by the writer, and
    /// the model's note.  Replay departs exactly these ids.
    Evicted {
        cut: Cut,
        by: EditAuthority,
    },
    /// The user's `/rewind`: turn `anchor` and every turn after it leave the
    /// structure outright — no marker, no address, the next prompt minted
    /// `anchor` again.  Their records stay in the file as forensic fact.
    Rewound {
        anchor: u64,
    },
}

/// The commits the view fold needs — cut, coalesced, and reduced *before*
/// they reach the seam, so a resumed scrollback matches what the user
/// actually saw.
///
/// Field shapes here are this parcel's own choice: they carry each fact's
/// "data half" in a form the log can round-trip, never the mark
/// tree a `card` field renders (recomputed by the view fold), with two
/// exceptions this parcel decided on the same rule — `Observation` and
/// `Card` carry the whole fact because there is no other durable trace of it
/// once the mark tree is drawn.  A `Change` carries its diff already cut at
/// the source, never the bytes it was taken from.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Display {
    /// One line of a reasoning run, or the run's tail where the prose after
    /// it begins.  Consecutive ones grow one block, and the run's own deltas
    /// are superseded line by line as they land.
    Thinking {
        text: String,
    },
    /// Any item entering context — prompt, wakeup, or peer message — and
    /// the user turn it opens; `None` for a line extending the turn in hand.
    Prompt {
        text: String,
        turn: Option<u64>,
    },
    /// One line of the assistant's own prose, cut at the newline that
    /// completed it — so one `AssistantMessage` yields many of these, and a
    /// reader sees the answer as it is spoken.  Consecutive ones grow one
    /// block, which is why the cut needs no meaning of its own.
    Answer {
        text: String,
    },
    ToolCall {
        tool: String,
        cmd: String,
        summary: Option<String>,
    },
    HarnessCall {
        verb: String,
        subject: Option<String>,
        payload: String,
        failed: bool,
    },
    /// The byte-identical third copy of a result: emitted, drawn, and
    /// recorded from the one clipped string the model saw.
    ///
    /// `call` names the `ToolCall` commit this result belongs to — the
    /// producer already knows it, being the one that just emitted that
    /// commit — so the view fold addresses it directly rather than walking
    /// backward to the nearest resident tail.
    Result {
        text: String,
        failed: bool,
        call: BlockId,
    },
    /// A child's landed line.  No body: the reply is a value on the child's own
    /// agent, fetched with `` exarch-agents `read ``, never copied into the parent's
    /// scrollback.
    SubagentDone {
        name: String,
        error: Option<String>,
        elapsed_ms: u64,
    },
    /// A fact core observed at a door, carried as its total wire form — the
    /// one display content the protocol records cannot supply.
    Observation {
        value: FOValue,
    },
    /// What a write or an edit did to one file.
    Change {
        change: Change,
    },
    /// A render document a ral kit composed for the `surface` builtin: the
    /// mark tree *is* the fact here, so unlike the other three commits in
    /// this list it is what gets recorded, as the typed card itself.
    Card {
        card: Card,
    },
    Done {
        cmd: String,
        outcome: DoneOutcome,
    },
    Context {
        turns: Vec<model::TurnRow>,
    },
    /// Beside `Forensic::TurnStarted`, whose `tuning` the screen never
    /// showed — the display class never derives from the twin it duplicates a
    /// field of.  `id` is the id the request will produce, so a cancelled
    /// request retaken shows the same one twice.
    Turn {
        id: u64,
    },
    /// Beside `Protocol::Evicted`, for the same reason.
    Evicted {
        cut: Cut,
        by: EditAuthority,
    },
    /// Beside `Protocol::Rewound`: the view cuts back to where turn `anchor`
    /// opened.
    Rewound {
        anchor: u64,
    },
}

/// Breadcrumbs that determine no model projection but are worth keeping.
///
/// `Error`, `Nudge`, and `ProviderError` are each the one record their
/// dual-write sites emit for a single fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Forensic {
    /// Head bookend, carrying enough metadata that the file reads on its own.
    SessionStarted {
        session_id: AgentId,
        parent: Option<AgentId>,
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        /// What the account was called when the session started — a
        /// snapshot, the right thing for a log to hold even once a sibling
        /// account arrives or a workspace is renamed.
        #[serde(rename = "provider")]
        label: String,
        /// `service` and `account` join `label` once an account carries a
        /// service name and an id of its own; both absent on a `record.jsonl`
        /// written before this pair existed, which still resumes on `label`
        /// alone.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        system_prompt_bytes: usize,
        log_dir: PathBuf,
        at_unix_ms: u64,
    },
    SessionResumed {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        #[serde(rename = "provider")]
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    },
    /// Tail bookend.
    SessionEnded,
    /// The meta half of taking a turn: the shape the request goes out under,
    /// sampling and routing both.  The id the request will produce is the
    /// display twin's.
    TurnStarted {
        tuning: Tuning,
        /// The serving provider actually pinned, which is not the selection's
        /// stored pin: a service that does not route drops it.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        route: Option<String>,
    },
    UsageDelta {
        usage: Usage,
    },
    /// Ctrl-C or Esc mid-turn.
    Cancelled,
    /// A diagnostic for the user alone — never reaches the model.
    Error {
        text: String,
    },
    /// The agent steering itself: a repair spends the per-exchange budget, a
    /// standing-condition reminder spends nothing.
    Nudge {
        cause: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        spent: Option<Spent>,
    },
    ProviderError {
        error: ProviderError,
    },
    /// An operational note the attend loop issued — the model never saw it.
    SystemNote {
        text: String,
    },
    /// The paired result for a `HarnessCall` — forensic only, since the act
    /// row on screen already says everything.
    HarnessResult {
        text: String,
    },
    /// The history informs the resume note; the live register follows the
    /// shell boundary and is not restored.
    Pin {
        key: String,
    },
    Unpin {
        key: String,
    },
    /// A worker policy removed after nobody observed it; `cause` is one of
    /// `ReapCause`'s spellings (`idle`, `backstop`, `retention`).
    Reap {
        cmd: String,
        cause: String,
    },
    /// Idle top-level bindings the ledger unset at a ready boundary;
    /// `idle_calls` is parallel to `names`.
    Prune {
        names: Vec<String>,
        idle_calls: Vec<u64>,
    },
    /// A mid-session model switch — the context-floor denominator, absent
    /// from `SessionStarted` once the run outlives its first selection.
    ModelChanged {
        model: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        context_window: Option<u64>,
        #[serde(rename = "provider")]
        label: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        service: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        account: Option<String>,
    },
}

/// The channel's other passenger: content that dies with the channel and
/// never reaches the log.
///
/// Disjoint from [`Recorded`] by construction, so journaling a delta or
/// publishing an unrecorded fact are both type errors.
#[derive(Debug, Clone)]
pub enum Transient {
    Token(String),
    /// A live reasoning token, superseded line by line by [`Display::Thinking`]
    /// as the worker records each line it completes.
    Thinking(String),
    State(AgentState),
    /// The producer's flush signal ending a streaming step, emitted once the
    /// step's last record has landed: a printer may take it as leave to drop
    /// whatever line each lane still has open, since no record will cover it.
    Boundary,
    Born {
        /// What the frontend must still know once the agent has settled: the row's label and indentation, the tombstone's log path.
        log_dir: PathBuf,
        name: String,
        /// `None` for a `/branch` child: it roots its own tab tree, and that
        /// is what `/close` reads to know what it may kill.
        parent: Option<AgentId>,
    },
    Died,
    /// The durable copy already rides [`Protocol::AssistantMessage`].
    StopReason(String),
    /// The durable fact is the new segment's bookend.
    Cleared,
    /// Drawn, not recorded.
    Resources {
        card: Card,
    },
    /// Drawn, not recorded.
    Limits {
        card: Card,
    },
    /// The live register's copy of a pin — [`Forensic::Pin`] is the durable
    /// breadcrumb, this is the rendered card the process is holding, which a
    /// resume does not restore.
    Pin {
        key: String,
        card: Card,
    },
    Unpin {
        key: String,
    },
    /// A seam append failure, or a channel elision marker: a fact about the
    /// plumbing itself, not about the session, so it has no durable form.
    Fault {
        text: String,
    },
}

/// One tool result as the model sees it: the rendered, per-section-capped
/// string, never the raw stdout or stderr bytes.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResult {
    pub id: String,
    pub content: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EditAuthority {
    Model,
    Harness,
}

impl EditAuthority {
    pub fn name(self) -> &'static str {
        match self {
            Self::Model => "model",
            Self::Harness => "harness",
        }
    }
}

/// One eviction as recorded: the resident turns it took — resolved by the
/// writer, so replay departs exactly these — and the model's note.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Cut {
    pub turns: Vec<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
}

/// Why the work in hand is ending with no real assistant reply: decides whether it
/// is owed a capstone, and whether it earns a `Forensic::Cancelled`
/// breadcrumb.
#[derive(Clone, Copy)]
pub enum QuiesceReason {
    Cancelled,
    /// A surfaced error — transport failure, exhausted retry budget — before
    /// the assistant replied.
    Aborted,
    /// A sub-agent called `reply`: that round-trip never asked for a closing
    /// assistant message, so the machine sits awaiting one on purpose, not in
    /// error.
    Replied,
}

/// Who took a turn: a prompt (or an import's opening) is the user's; the
/// assistant message, its tool results and any steering are the assistant's.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Role {
    User,
    Assistant,
}

impl Role {
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::User => "user",
            Self::Assistant => "assistant",
        }
    }
}

/// Where a turn came from: this session's own work, a harness-authored
/// import, or a turn a fork inherited from its parent's table.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnKind {
    Own,
    Import,
    Inherited,
}

impl TurnKind {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Own => "own",
            Self::Import => "import",
            Self::Inherited => "inherited",
        }
    }
}

/// What a repair spent of the per-exchange budget, as the record keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spent {
    pub used: u32,
    pub max: u32,
}

/// A session's provider identity, snapshotted for the record.
///
/// What the account was called, and which service and account it was, at the
/// moment the session began. `Clone` because [`AgentLog::fork`] carries it
/// into every child's own `SessionStarted` bookend unchanged.
#[derive(Clone, Debug)]
pub struct RecordedAccount {
    pub label: String,
    pub service: String,
    pub id: String,
}

impl RecordedAccount {
    /// The snapshot of `account`, labelled as it reads among `available` —
    /// the one place a live account becomes a log header.
    pub fn of(account: &crate::provider::Account, available: &[crate::provider::Account]) -> Self {
        Self {
            label: crate::provider::identity::label(account, available),
            service: account.service.name.as_str().to_string(),
            id: account.id.as_str().to_string(),
        }
    }

    /// A snapshot for tests that only care that *something* is recorded.
    /// Not `#[cfg(test)]`: integration test binaries link the library built
    /// without it, so a fixture they share with the unit tests must be an
    /// ordinary function, as `Avatar::for_test` already is.
    #[doc(hidden)]
    pub fn for_test(name: &str) -> Self {
        Self {
            label: name.to_string(),
            service: name.to_string(),
            id: name.to_string(),
        }
    }
}

/// A session's model, snapshotted for the record: its name and the context
/// window its provider reported when the selection was minted.
#[derive(Clone, Debug)]
pub struct RecordedModel {
    pub name: String,
    pub context_window: Option<u64>,
}

impl RecordedModel {
    /// The one place a live selection becomes a log header's model.
    pub fn of(provider: &Provider) -> Self {
        Self {
            name: provider.model().to_string(),
            context_window: provider.context_window(),
        }
    }

    /// A snapshot for tests that only care that *something* is recorded; not
    /// `#[cfg(test)]` for the reason [`RecordedAccount::for_test`] is not.
    #[doc(hidden)]
    pub fn for_test(name: &str) -> Self {
        Self {
            name: name.to_string(),
            context_window: None,
        }
    }
}

/// A record's position in the log — the line it was assigned at append.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Seq(u64);

impl Seq {
    pub(crate) fn new(n: u64) -> Self {
        Self(n)
    }

    pub fn get(self) -> u64 {
        self.0
    }
}

/// Where one record lives: its [`Seq`], the byte range `append` wrote it to,
/// and a digest of its own bytes that a stale range fails rather than reads
/// past.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[must_use]
pub struct Locus {
    seq: Seq,
    bytes: Range<u64>,
    /// Blake3 of the record's own bytes, truncated: a range read back
    /// against a file it was not measured in — a rotated segment, a
    /// copied directory, a torn write — fails to match rather than
    /// returning a plausible neighbour.
    digest: u64,
}

impl Locus {
    /// `body` is the record's JSON without its line terminator; the range it
    /// occupies includes that terminator.
    pub(crate) fn over(seq: Seq, at: u64, body: &[u8]) -> Self {
        Self {
            seq,
            bytes: at..at + body.len() as u64 + 1,
            digest: Self::digest_of(body),
        }
    }

    /// The digest a body must hash to. Truncated to 64 bits: this guards
    /// against a stale offset, not an adversary.
    pub(crate) fn digest_of(body: &[u8]) -> u64 {
        u64::from_le_bytes(
            blake3::hash(body).as_bytes()[..8]
                .try_into()
                .expect("blake3 digests are 32 bytes"),
        )
    }

    /// A locus that names no bytes. The view fold never reads a record back
    /// through one, so its tests need no file behind it — the one locus not
    /// derived from a record's own bytes, and `cfg(test)` so no production
    /// path can mint one.
    #[cfg(test)]
    pub(crate) fn placeholder(seq: Seq) -> Self {
        Self {
            seq,
            bytes: 0..0,
            digest: Self::digest_of(&[]),
        }
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }

    pub fn bytes(&self) -> Range<u64> {
        self.bytes.clone()
    }

    pub fn digest(&self) -> u64 {
        self.digest
    }
}

/// A record witnessed by the seam: its [`Locus`] beside the value `emit` was
/// given.
///
/// Built only at append time (by [`Emitter::emit`]) and at replay time
/// (reconstructing history from the file) — nowhere else, since nothing but
/// those two moments has a `Locus` to attach.
#[derive(Debug, Clone)]
#[must_use]
pub struct Recorded<R>(Locus, R);

impl<R> Recorded<R> {
    pub(crate) fn new(locus: Locus, value: R) -> Self {
        Self(locus, value)
    }

    pub fn locus(&self) -> &Locus {
        &self.0
    }

    pub fn value(&self) -> &R {
        &self.1
    }

    pub fn into_value(self) -> R {
        self.1
    }
}

/// Widen a witnessed class-typed record into the [`Record`] enum
/// [`Fold::step`] expects.
///
/// The bridge from `Emitter::emit`'s typed return to the class-blind
/// dispatcher, for the attend thread's inline advance.
pub fn widen<C: Class>(recorded: Recorded<C>) -> Recorded<Record> {
    let locus = recorded.locus().clone();
    Recorded::new(locus, recorded.into_value().into())
}

/// A named commit in the view fold's memo — a block is named by its own
/// commit's [`Seq`].
///
/// A result patch carries its call's `BlockId`, and the fold tolerates a
/// patch whose target it has already evicted.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
pub struct BlockId(Seq);

impl BlockId {
    pub(crate) fn new(seq: Seq) -> Self {
        Self(seq)
    }

    pub fn seq(self) -> Seq {
        self.0
    }
}

pub use view::Blocks;

/// One fold over the log, replayed identically whether it is driven live
/// (from the channel) or from disk (from [`replay()`]) — the same `step`
/// function, two drivers.
///
/// An impl's `step` is one outer-class match over `Record` delegating to
/// class-typed functions (the model fold's takes `&Protocol`; the view
/// fold's take `&Display` / `&Forensic`); no arm is a wildcard, and a
/// [`Refusal`] during replay refuses the whole session.
pub trait Fold {
    /// No `Default` bound: a memo the fold cannot build from nothing — the
    /// model's, which needs its log's path — seeds [`replay()`] by value.
    type Memo;

    /// # Errors
    /// Returns [`Refusal`] when this fold does not recognise the record —
    /// during replay that refuses the session rather than skip it silently.
    fn step(memo: &mut Self::Memo, record: &Recorded<Record>) -> Result<(), Refusal>;
}

/// Read `path`'s [`Record`]s back, past their `Entry` envelope, for tests
/// across the crate that assert on the raw log rather than on a fold's
/// memo — the one sanctioned exception to this module's own "read the log
/// back only through [`replay()`]" rule, since what these tests exercise is
/// the wire shape itself.
///
/// # Errors
/// Returns `Err` under exactly [`log::Log::read`]'s own conditions.
#[cfg(test)]
pub(crate) fn read_records(path: &std::path::Path) -> std::io::Result<Vec<Record>> {
    log::Log::read(path)?
        .map(|record| record.map(Recorded::into_value))
        .collect()
}

/// The other half of [`read_records`]: a test that hand-writes or
/// hand-edits `record.jsonl` needs the envelope shape too, without reaching
/// for the private [`Entry`] type itself.
#[cfg(test)]
pub(crate) fn envelope_line(record: &Record) -> Vec<u8> {
    #[derive(Serialize)]
    struct Envelope<'a> {
        at_unix_ms: u64,
        record: &'a Record,
    }
    serde_json::to_vec(&Envelope {
        at_unix_ms: 0,
        record,
    })
    .expect("a Record always serialises")
}

/// The read-side twin of [`envelope_line`], for a test that pulls one line
/// back off a live-written `record.jsonl` to inspect or edit it.
///
/// # Errors
/// Returns `Err` when `line` does not parse as the `Entry` envelope.
#[cfg(test)]
pub(crate) fn record_from_line(line: &[u8]) -> serde_json::Result<Record> {
    #[derive(Deserialize)]
    struct Envelope {
        record: Record,
    }
    serde_json::from_slice::<Envelope>(line).map(|e| e.record)
}
