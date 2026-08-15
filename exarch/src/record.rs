//! The one seam, one log: every fact a session records crosses here, once,
//! as a [`Record`].
//!
//! A `Record` is one of three classes: [`Protocol`] (verbatim payloads the
//! model fold folds), [`Display`] (commits the view fold folds), [`Forensic`]
//! (breadcrumbs neither fold projects but which are worth keeping).  Nothing
//! outside this module tree may mint a fourth class, invent a variant absent
//! from the disposition, or read the log back except through [`replay`].
//!
//! [`Transient`] is the disjoint, unrecorded half of the channel: deltas, the
//! provisional thinking seat, and chrome that dies with the process.
#![allow(
    dead_code,
    reason = "the producers author through this surface, but the printers do not draw it yet: record::view's Blocks and the replay driver stay unconsumed until tui/headless are reborn as printers (P3/P6)"
)]
#![deny(unused_results)]
#![deny(clippy::let_underscore_must_use)]
#![deny(clippy::wildcard_enum_match_arm)]

pub(crate) mod commit;
mod log;
pub(crate) mod model;
mod replay;
mod seam;
mod view;

pub use replay::{Refusal, replay};
pub use seam::Emitter;
pub use view::{Block, BlockKind, View};

pub(crate) use log::FleetSink;

use crate::agent::event::{ContextOp, EditAuthority, ProviderErrorRecord, ToolResult, UsageDelta};
use crate::bus::card::Card;
use crate::bus::{AgentId, AgentState};
use crate::provider::Tuning;
use genai::chat::ChatMessage;
use ral_core::serial::FOValue;
use serde::{Deserialize, Serialize};
use std::ops::Range;
use std::path::PathBuf;

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

/// Verbatim payloads the model fold needs — the provider's exact
/// `ChatMessage`, tool-call ids, and the session bookends.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Protocol {
    /// Head bookend, carrying enough metadata that the file reads on its own.
    SessionStarted {
        session_id: AgentId,
        parent: Option<AgentId>,
        model: String,
        provider: String,
        system_prompt_bytes: usize,
        log_dir: PathBuf,
        at_unix_ms: u64,
    },
    SessionResumed {
        model: String,
        provider: String,
        system_prompt_bytes: usize,
        at_unix_ms: u64,
    },
    /// Tail bookend.
    SessionEnded,
    UserPrompt {
        exchange: u64,
        text: String,
    },
    /// A message a `mnemon` child inherits from its parent's context.
    ContextMessage {
        exchange: u64,
        message: ChatMessage,
    },
    StepStarted {
        n: u32,
        tuning: Tuning,
    },
    AssistantMessage {
        message: ChatMessage,
        pending_tool_ids: Vec<String>,
        stop_reason: Option<String>,
    },
    ToolResults {
        results: Vec<ToolResult>,
    },
    ContextEdited {
        op: ContextOp,
        by: EditAuthority,
    },
}

/// The commits the view fold needs — chopped, coalesced, and reduced
/// *before* they reach the seam, so a resumed scrollback matches what the
/// user actually saw.
///
/// Field shapes here are this parcel's own choice: they carry each fact's
/// "data half" in a form the log can round-trip, never the mark
/// tree a `card` field renders (recomputed by the view fold), with two
/// exceptions this parcel decided on the same rule — `Observation` and
/// `Card` carry the whole fact because there is no other durable trace of it
/// once the mark tree is drawn.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Display {
    /// One reasoning run, sealed where the prose it precedes begins and
    /// superseding that run's streamed deltas.
    Thinking {
        text: String,
    },
    /// Any item entering context — prompt, wakeup, or peer message.
    Prompt {
        text: String,
    },
    /// One chopper-committed block of the assistant's own prose, cut at a
    /// fence-safe paragraph break — so one `AssistantMessage` yields several
    /// of these where the live view streamed many `Token`s. A prefix of the
    /// accumulated delta stream, never the whole answer at once.
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
    /// backward to the nearest resident tail, the mechanism `set_result_size`
    /// used and this plan retires.
    Result {
        text: String,
        call: BlockId,
    },
    /// One read/exec/grep run the producer grouped into a single visual
    /// card — the same dedup and grouping `SurfaceBuffer` already does at
    /// record time, carried as its members' wire forms so the view fold can
    /// rebuild exactly the one card the user saw via `reads_card` /
    /// `execs_card` / `greps_card`, never a mark tree recorded up front.
    ObservationGroup {
        values: Vec<FOValue>,
    },
    SubagentDone {
        name: String,
        text: String,
        error: Option<String>,
        elapsed_ms: u64,
    },
    /// A fact core observed at a door, carried as its total wire form — the
    /// one display content the protocol records cannot supply.
    Observation {
        value: FOValue,
    },
    /// A render document a ral kit composed for the `surface` builtin: the
    /// mark tree *is* the fact here, so unlike the other three commits in
    /// this list it is what gets recorded, opaquely, for the view fold to
    /// decode.
    Card {
        marks: serde_json::Value,
    },
    Done {
        outcome: DoneOutcome,
    },
    Notice {
        notice: NoticeFact,
    },
    Context {
        rows: Vec<ContextRow>,
    },
    /// Beside `Protocol::StepStarted`, whose `tuning` the screen never
    /// showed — the display class never derives from the protocol twin it
    /// duplicates a field of.
    Step {
        n: u32,
    },
    /// Beside `Protocol::ContextEdited`, for the same reason.
    ContextEdited {
        op: ContextOp,
        by: EditAuthority,
    },
}

/// A detached worker's `` `done `` completion, minus the one-line card the
/// view fold rebuilds.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum DoneOutcome {
    Ok,
    Err { message: String, status: i64 },
    Panic { message: String },
}

/// A `` `notice `` housekeeping fact, minus its rendered card.
///
/// `cause` mirrors `ReapCause::as_str`'s three spellings rather than
/// importing the live enum, keeping this parcel's serde surface
/// self-contained.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "notice", rename_all = "snake_case", deny_unknown_fields)]
pub enum NoticeFact {
    Reap {
        cmd: String,
        cause: String,
    },
    Prune {
        names: Vec<String>,
        idle_calls: Vec<u64>,
    },
}

/// One row of a `/context` survey, minus its rendered card.  `kind` mirrors
/// `ContextSpanKind::as_str`'s three spellings for the same reason.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ContextRow {
    pub exchange: u64,
    pub kind: String,
    pub opening: String,
    pub bytes: usize,
    pub steps: usize,
    pub live: bool,
}

/// Breadcrumbs that determine no model projection but are worth keeping.
///
/// `Error`, `Nudge`, `ProviderError`, and `Stalled` are each the one record
/// their dual-write sites emit for a single fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Forensic {
    UsageDelta {
        usage: UsageDelta,
    },
    /// Ctrl-C or Esc mid-exchange.
    Cancelled,
    /// A diagnostic for the user alone — never reaches the model.
    Error {
        text: String,
    },
    Nudge {
        used: u32,
        max: u32,
        cause: String,
    },
    ProviderError {
        error: ProviderErrorRecord,
    },
    Stalled {
        error: ProviderErrorRecord,
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
    /// A mid-session model switch — the context-floor denominator, absent
    /// from `SessionStarted` once the run outlives its first selection.
    ModelChanged {
        model: String,
        provider: String,
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
    /// A live reasoning token, superseded by [`Display::Thinking`] once the
    /// run lands its authoritative text.
    Thinking(String),
    State(AgentState),
    /// The producer's flush signal ending a streaming step, emitted once the
    /// step's last commit has landed: a printer may take it as leave to retire
    /// whatever is left in the raw streams it drew the live edge from.
    Boundary,
    Born {
        log_dir: PathBuf,
        name: String,
        parent: AgentId,
        branch: bool,
    },
    Died,
    /// The durable copy already rides [`Protocol::AssistantMessage`].
    StopReason(String),
    /// The durable fact is the new segment's bookend.
    Cleared,
    /// Drawn, not recorded.
    Resources {
        rows: Vec<crate::agent::resources::ProbeRow>,
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

/// Where one record lives: its [`Seq`] and the byte range `append` wrote it
/// to.
///
/// The model fold's ledger stores one of these per protocol record, so a
/// freed run reads back through them one record at a time rather than by
/// contiguous span.
#[derive(Debug, Clone, PartialEq, Eq)]
#[must_use]
pub struct Stamp {
    seq: Seq,
    bytes: Range<u64>,
}

impl Stamp {
    pub(crate) fn new(seq: Seq, bytes: Range<u64>) -> Self {
        Self { seq, bytes }
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }

    pub fn bytes(&self) -> Range<u64> {
        self.bytes.clone()
    }
}

/// A record witnessed by the seam: its [`Stamp`] beside the value `emit` was
/// given.
///
/// Built only at append time (by [`Emitter::emit`]) and at replay time
/// (reconstructing history from the file) — nowhere else, since nothing but
/// those two moments has a `Stamp` to attach.
#[derive(Debug, Clone)]
#[must_use]
pub struct Recorded<R>(Stamp, R);

impl<R> Recorded<R> {
    pub(crate) fn new(stamp: Stamp, value: R) -> Self {
        Self(stamp, value)
    }

    pub fn stamp(&self) -> &Stamp {
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
    let stamp = recorded.stamp().clone();
    Recorded::new(stamp, recorded.into_value().into())
}

/// A named commit in the view fold's memo — a block is named by its own
/// commit's [`Seq`].
///
/// A `result_size` patch can carry its call's `BlockId`, and the fold can
/// tolerate a patch whose target it has already evicted.
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
/// (from the channel) or from disk (from [`replay`]) — the same `step`
/// function, two drivers.
///
/// An impl's `step` is one outer-class match over `Record` delegating to
/// class-typed functions (the model fold's takes `&Protocol`; the view
/// fold's take `&Display` / `&Forensic`); no arm is a wildcard, and a
/// [`Refusal`] during replay refuses the whole session.
pub trait Fold {
    type Memo: Default;

    /// # Errors
    /// Returns [`Refusal`] when this fold does not recognise the record —
    /// during replay that refuses the session rather than skip it silently.
    fn step(memo: &mut Self::Memo, record: &Recorded<Record>) -> Result<(), Refusal>;
}

/// A frontend that draws one fold's output — `tui` and `headless` alike —
/// and is never handed a `Record`, so it cannot match on the vocabulary and
/// a third hand-rolled projection cannot compile.
pub trait Printer {
    fn transient(&mut self, t: &Transient);
    fn sync(&mut self, blocks: &Blocks);
}

/// Read `path`'s [`Record`]s back, past their `Entry` envelope, for tests
/// across the crate that assert on the raw log rather than on a fold's
/// memo — the one sanctioned exception to this module's own "read the log
/// back only through [`replay`]" rule, since what these tests exercise is
/// the wire shape itself.
///
/// # Errors
/// Returns `Err` under exactly [`log::Log::read`]'s own conditions.
#[cfg(test)]
pub(crate) fn read_records(path: &std::path::Path) -> std::io::Result<Vec<Record>> {
    Ok(log::Log::read(path)?
        .into_iter()
        .collect::<std::io::Result<Vec<_>>>()?
        .into_iter()
        .map(Recorded::into_value)
        .collect())
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
