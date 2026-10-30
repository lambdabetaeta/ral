//! The one seam, one log: every fact a session records crosses here, once,
//! as a [`Record`]. A record lands in a [`Commit`], one line of the log,
//! alone or beside the records it must not be seen without.
//!
//! A `Record` is one of four classes: [`Transcript`] (verbatim payloads the
//! context folds), [`Control`] (the mail, schedules and model), [`Display`]
//! (presentation), and [`Forensic`] (evidence and diagnostics; it folds nothing). Nothing
//! outside this module tree may mint a fifth class, invent a variant absent
//! from the disposition, or read the log back except through [`replay()`].
//!
//! [`Transient`] is the disjoint, unrecorded half of the channel: deltas, the
//! provisional open thinking line, and chrome that dies with the process.
#![deny(unused_results)]
#![deny(clippy::let_underscore_must_use)]
#![deny(clippy::wildcard_enum_match_arm)]

pub(crate) mod context;
mod control;
pub mod fault;
mod log;
mod mail;
mod model;
mod non_empty;
mod replay;
mod schedules;
mod seam;
mod session;
pub(crate) mod stream;
mod view;

#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod tests;

pub use crate::provider::Selected;
pub use context::{Eviction, ItemKind, Linked, Location, Pointer, Span, SpanRow, TurnRow};
pub(crate) use control::{Folds, Replay};
pub use log::Unavailable;
#[cfg(test)]
pub(crate) use log::{commit_line, parse_line};
pub use mail::Mail;
pub use model::Model;
pub use non_empty::NonEmpty;
pub use replay::{Refusal, replay};
pub use schedules::Schedules;
pub(crate) use seam::Emitter;
pub use seam::Reader;
pub(crate) use session::{AgentLog, role_label};
pub use session::{
    GrepAnswer, GrepHit, Inherited, Resumed, TranscriptMessage, TranscriptPart, TranscriptTurn,
    Uncommitted,
};
pub use view::{BLOCKS_WINDOW, Block, BlockKind, Delta, Verdict, View, context_rows_card};

// Defined in `app`, the layer below, whose `resume_candidates` tells a recorded
// run by it.
pub use crate::app::RECORD;

use crate::card::{Card, Change, DoneOutcome};
use crate::provider::{ProviderError, Tuning, Usage, Via};
use genai::chat::ChatMessage;
use ral_core::data::FOValue;
use ral_core::own::OwnDir;
use serde::{Deserialize, Serialize};
use std::fmt;
use std::ops::Range;
use std::path::PathBuf;

/// The `cmd` recorded for an unparseable tool call.
pub(crate) const INVALID_INPUT: &str = "<invalid input>";

/// The identity of an agent node — the trunk and every forked child alike.
///
/// Opaque, and what crosses the wire: every `exarch-agents` tag and every `Signal`
/// names a node by this.  It is a routing key, not an authority — an `exarch-agents` tag
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

/// The commit number of the record that opened a job.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct JobId(u64);

impl JobId {
    #[must_use]
    pub fn get(self) -> u64 {
        self.0
    }
}

impl From<u64> for JobId {
    fn from(commit: u64) -> Self {
        Self(commit)
    }
}

/// Whether a child starts blank or inherits its parent's conversation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Memory {
    Amnemon,
    Mnemon,
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

/// Where the seam publishes a commit it has just appended, whole, and the
/// transients that ride beside it.
pub(crate) trait Publish: Send {
    fn commit(&self, commit: Commit);
    fn transient(&self, t: Transient);
}

/// One fact a session recorded, in one of four classes.
///
/// Externally tagged, as is each class: an internally tagged enum buffers
/// through serde's `Content`, whose replay recurses natively, so the log
/// could not read back a value as deep as it wrote.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum Record {
    Transcript(Transcript),
    Control(Control),
    Display(Display),
    Forensic(Forensic),
}

impl From<Transcript> for Record {
    fn from(p: Transcript) -> Self {
        Self::Transcript(p)
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

impl Record {
    pub(crate) fn is_control(&self) -> bool {
        matches!(self, Self::Control(_))
    }
}

impl Control {
    /// Only a landed switch saves a choice for the next launch.
    pub(crate) fn switched_to(&self) -> Option<crate::provider::state::State> {
        let Self::ModelChanged { selected } = self else {
            return None;
        };
        Some(crate::provider::state::State {
            provider: selected.account.clone(),
            provider_name: selected.label.clone(),
            model: selected.model.clone(),
            via: selected.via.clone(),
        })
    }
}

/// The agent's own state, folded by `Mail`, `Schedules` and `Model`.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Control {
    /// The head of every log and nowhere else.
    SessionStarted(Started),
    SessionResumed {
        selected: Selected,
        /// May change on resume.
        returns: bool,
        at_unix_ms: u64,
    },
    /// Its commit names the job.
    JobOpened,
    JobSettled {
        job: JobId,
        stopped: bool,
    },
    Status {
        status: ChildStatus,
    },
    /// Its commit names the post.
    Posted(Post),
    /// Each became a prompt or a command, and keeps its debt until a `Status`.
    Taken {
        posts: NonEmpty<PostId>,
    },
    /// Left unread, and takes its debt with it.
    Withdrawn {
        posts: NonEmpty<PostId>,
    },
    Scheduled {
        label: String,
        schedule: Schedule,
    },
    Unscheduled {
        label: String,
    },
    ModelChanged {
        selected: Selected,
    },
    /// `model` is the model in force.
    WindowNarrowed {
        model: String,
        context_window: u64,
    },
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Started {
    pub session_id: AgentId,
    pub name: String,
    /// `None` for every root.
    pub parent: Option<AgentId>,
    pub returns: bool,
    pub selected: Selected,
    pub log_dir: PathBuf,
    pub at_unix_ms: u64,
}

/// The commit that posted an item.
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct PostId(u64);

impl PostId {
    /// The post `commit` names: its commit number.
    pub(crate) fn of(commit: &Commit) -> Self {
        Self(commit.number())
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Post {
    /// `None` is a command, never a prompt.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub voice: Option<Voice>,
    pub text: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub from: Option<AgentId>,
}

impl Post {
    /// The word a log note uses for this post's voice.
    pub(crate) fn voice_word(&self) -> &'static str {
        match self.voice {
            Some(Voice::Human) => "human",
            Some(Voice::Peer) => "peer",
            Some(Voice::Harness) => "harness",
            Some(Voice::Wakeup) => "wakeup",
            None => "command",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Schedule {
    pub prompt: String,
    pub trigger: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cron: Option<String>,
    pub next_unix_ms: u64,
    pub fires: u64,
}

/// What was said in the conversation and done to it; the `Context` folds it.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Transcript {
    /// The system prompt every later request goes out under, until the next.
    System {
        text: String,
    },
    /// A prompt opening a user turn of its own, under the id it carries.
    UserPrompt {
        turn: u64,
        text: String,
        voice: Voice,
    },
    /// A user line extending the newest turn: a steering line, or a nudge
    /// continuing the prompt in hand.
    Steering {
        text: String,
        voice: Voice,
    },
    /// A harness-authored message opening a turn of its own: the resume note.
    Import {
        turn: u64,
        message: ChatMessage,
    },
    /// The link a `mnemon` child opens with: the parent's whole turn table,
    /// each row carrying the address its transcript copy lies at, and its
    /// evictions.
    Inherited {
        /// The parent's table at the fork, every row `kind: Inherited` and
        /// `location` as the parent had it, beside where that turn was first
        /// recorded.  The child's context, survey and index are the same
        /// projections of it.
        turns: Vec<context::Linked>,
        /// The parent's evictions in creation order: a resident one is
        /// rendered from its own record, so the link carries nothing rendered.
        evictions: Vec<context::Eviction>,
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
    /// An eviction as the table keeps it: the span it stands for, and its note.
    Evicted(context::Eviction),
    /// `/rewind`: turn `anchor` and every turn after it are withdrawn.
    Rewound {
        anchor: u64,
    },
    /// `/clear`: every turn is withdrawn; the system prompt stays.
    Cleared,
}

/// Who spoke a user-voice message.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Voice {
    Human,
    Wakeup,
    Peer,
    /// Nudges, the harness's own reminders, child results and surface notices:
    /// everything the screen already speaks for elsewhere or not at all.
    Harness,
}

/// How a returning agent answered.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildStatus {
    /// The value its parent reads with `` exarch-agents `read ``.
    Replied(FOValue),
    Failed(String),
    Stopped,
}

impl ChildStatus {
    /// What the parent hears, without the reply value.
    #[must_use]
    pub fn settlement(&self) -> Settlement {
        match self {
            Self::Replied(_) => Settlement::Replied,
            Self::Failed(reason) => Settlement::Failed {
                reason: reason.clone(),
            },
            Self::Stopped => Settlement::Stopped,
        }
    }
}

/// What a parent hears of a child's status, without its value.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "outcome", rename_all = "snake_case", deny_unknown_fields)]
pub enum Settlement {
    Replied,
    Failed { reason: String },
    Stopped,
}

/// The records the view fold needs — cut, coalesced, and reduced *before*
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
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Display {
    /// One line of a reasoning run, or the run's tail where the prose after
    /// it begins.  Consecutive ones grow one block, and the run's own deltas
    /// are superseded line by line as they land.
    Thinking {
        text: String,
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
    /// What a call's result told it; the text itself is `Transcript::ToolResults`'.
    Verdict {
        call: BlockId,
        verdict: Verdict,
    },
    /// A child's landed line.  No body: the reply is a value on the child's own
    /// agent, fetched with `` exarch-agents `read ``, never copied into the parent's
    /// scrollback.
    SubagentDone {
        name: String,
        outcome: Settlement,
        elapsed_ms: u64,
    },
    /// A fact core observed, carried as its total wire form — the
    /// one display content the protocol records cannot supply.
    Observation {
        value: FOValue,
    },
    /// What a write or an edit did to one file.
    Change {
        change: Change,
    },
    /// A render document a ral kit composed for the `surface` builtin: the
    /// mark tree *is* the fact here, so unlike the other records in
    /// this list it is what gets recorded, as the typed card itself.
    Card {
        card: Card,
    },
    Done {
        id: Option<u64>,
        title: String,
        elapsed_ms: Option<u64>,
        outcome: DoneOutcome,
    },
    Context {
        rows: Vec<context::SpanRow>,
    },
}

/// Evidence and diagnostics; no fold derives state from it.
///
/// `Error`, `Nudge`, and `ProviderError` are each the one record their
/// dual-write sites emit for a single fact.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(rename_all = "snake_case", deny_unknown_fields)]
pub enum Forensic {
    /// Tail bookend.
    SessionEnded,
    /// `/close`: the session is over for good, and a resume skips it.
    /// [`Self::SessionEnded`] is written on every drop and says no such thing.
    SessionClosed,
    EngineSevered {
        cause: ral_core::carrier::Severed,
    },
    /// An engine this agent stopped using ended without certified teardown.
    TeardownFailed {
        failure: String,
    },
    /// The meta half of taking a turn: the shape the request goes out under,
    /// sampling and upstreams both.  `turn` is the id the request will produce,
    /// so a retaken request shows the same one twice.
    TurnStarted {
        turn: u64,
        tuning: Tuning,
        /// The upstreams the request asked for, as the selection holds them —
        /// not the one that answered, which is `UsageDelta::provider`.
        #[serde(skip_serializing_if = "Option::is_none")]
        via: Option<Via>,
    },
    /// One call's usage, with who answered it where the backend says: the
    /// response's generation id and the upstream provider it was served by.
    UsageDelta {
        usage: Usage,
        #[serde(skip_serializing_if = "Option::is_none")]
        generation: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        provider: Option<String>,
    },
    /// Ctrl-C or Esc mid-turn.
    Cancelled,
    /// A diagnostic for the user alone — never reaches the model.
    Error {
        text: String,
    },
    /// The agent steering itself: a repair spends the consecutive-repair budget, a
    /// standing-condition reminder spends nothing.
    Nudge {
        cause: String,
        #[serde(skip_serializing_if = "Option::is_none")]
        spent: Option<Spent>,
    },
    ProviderError {
        error: ProviderError,
    },
    /// A provider attempt that failed and was re-issued; the log's alone,
    /// drawn nowhere.
    Retried {
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
    /// A worker policy removed after nobody observed it; `cause` is
    /// `retention`, the one policy left.
    Reap {
        title: String,
        cause: String,
    },
    /// Idle top-level bindings the ledger unset at a ready boundary;
    /// `idle_calls` is parallel to `names`.
    Prune {
        names: Vec<String>,
        idle_calls: Vec<u64>,
    },
}

/// An agent's birth facts, announced whole on the channel by the minting of
/// its emitter (`bus::Emitter::child`), so they precede every signal it sends.
#[derive(Debug, Clone)]
pub struct Born {
    /// What the frontend must still know once the agent has settled: the
    /// row's label and indentation, the gone agent's log directory.
    pub log_dir: OwnDir,
    pub name: String,
    /// `None` for a root, which opens a session of its own.
    pub parent: Option<AgentId>,
    /// What a resumed root picked up, for the frontend to note.
    pub resumed: Option<Resumed>,
    /// The born agent's log, for a frontend to catch up from.
    pub reader: Reader,
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
    Died {
        failures: Vec<String>,
    },
    /// The durable copy already rides [`Transcript::AssistantMessage`].
    StopReason(String),
    /// Drawn, not recorded.
    Resources {
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
    /// A session's terminal recording failure, announced by its owner.
    Fault {
        text: String,
    },
    /// Presentation output was elided by the bus.
    Elided {
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

/// What a repair spent of the consecutive-repair budget, as the record keeps it.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct Spent {
    pub used: u32,
    pub max: u32,
}

/// A record's place in the log: its commit (the line it was appended in,
/// counted from 1) and its index there.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
pub struct Seq {
    // Declaration order is the derived lexicographic order.
    commit: u64,
    index: usize,
}

impl Seq {
    pub(crate) fn new(commit: u64, index: usize) -> Self {
        Self { commit, index }
    }

    pub fn commit(self) -> u64 {
        self.commit
    }

    pub fn index(self) -> usize {
        self.index
    }
}

/// How far a reader has come through one log: the last commit it read, by
/// its number and the byte its line ends at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Cursor {
    commit: u64,
    end: u64,
}

impl Cursor {
    /// The head of the log, before its first commit.
    pub fn start() -> Self {
        Self { commit: 0, end: 0 }
    }

    /// The number the log's next commit carries.
    pub fn next(self) -> u64 {
        self.commit + 1
    }

    /// Past `commit`.
    #[must_use]
    pub fn past(self, commit: &Commit) -> Self {
        Self {
            commit: commit.number(),
            end: commit.bytes().end,
        }
    }
}

/// The first 64 bits of a commit's blake3, as 16 lowercase hex digits.
/// Guards against damage, not an editor who can rewrite the sum too.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Sum(u64);

impl Sum {
    fn of(bytes: &[u8]) -> Self {
        let &[a, b, c, d, e, f, g, h, ..] = blake3::hash(bytes).as_bytes();
        Self(u64::from_be_bytes([a, b, c, d, e, f, g, h]))
    }
}

impl fmt::Display for Sum {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{:016x}", self.0)
    }
}

impl Serialize for Sum {
    fn serialize<S: serde::Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        serializer.collect_str(self)
    }
}

impl<'de> Deserialize<'de> for Sum {
    fn deserialize<D: serde::Deserializer<'de>>(deserializer: D) -> Result<Self, D::Error> {
        use serde::de::{Error as _, Unexpected};
        let text = String::deserialize(deserializer)?;
        let invalid =
            || D::Error::invalid_value(Unexpected::Str(&text), &"16 lowercase hex digits");
        if text.len() != 16
            || !text
                .bytes()
                .all(|byte| matches!(byte, b'0'..=b'9' | b'a'..=b'f'))
        {
            return Err(invalid());
        }
        u64::from_str_radix(&text, 16)
            .map(Self)
            .map_err(|_| invalid())
    }
}

/// Where one record lives: its [`Seq`], and the byte range and [`Sum`] of
/// its line, so a stale range fails rather than answering with a neighbour.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[must_use]
pub struct Locus {
    seq: Seq,
    bytes: Range<u64>,
    sum: Sum,
}

impl Locus {
    /// A locus with no file behind it, for folds that never read back.
    #[cfg(test)]
    pub(crate) fn placeholder(seq: Seq) -> Self {
        Self {
            seq,
            bytes: 0..0,
            sum: Sum::of(&[]),
        }
    }

    pub fn seq(&self) -> Seq {
        self.seq
    }
    pub fn bytes(&self) -> Range<u64> {
        self.bytes.clone()
    }
    pub fn sum(&self) -> Sum {
        self.sum
    }
}

/// One line of the log: records appended together, each named by its [`Locus`].
#[derive(Debug, Clone)]
#[must_use]
pub struct Commit(NonEmpty<Recorded<Record>>);

impl Commit {
    #[cfg(test)]
    pub(crate) fn placeholder(number: u64, records: NonEmpty<Record>) -> Self {
        Self::new(number, 0..0, Sum::of(&[]), records)
    }

    /// The records of line `number`, which spans `bytes` and is sealed by `sum`:
    /// the one place a [`Locus`] is minted.
    fn new(number: u64, bytes: Range<u64>, sum: Sum, records: NonEmpty<Record>) -> Self {
        let locus = |index| Locus {
            seq: Seq::new(number, index),
            bytes: bytes.clone(),
            sum,
        };
        let NonEmpty { first, rest } = records;
        Self(NonEmpty {
            first: Recorded::new(locus(0), first),
            rest: (1..)
                .zip(rest)
                .map(|(index, record)| Recorded::new(locus(index), record))
                .collect(),
        })
    }

    /// The line it occupies, counted from 1.
    pub fn number(&self) -> u64 {
        self.0.first.locus().seq().commit()
    }
    /// Its line's bytes, terminator included.
    pub fn bytes(&self) -> Range<u64> {
        self.0.first.locus().bytes()
    }
    pub fn records(&self) -> &NonEmpty<Recorded<Record>> {
        &self.0
    }
    /// Whether `locus` names this line: its commit, byte range and sum.
    #[must_use]
    pub(crate) fn holds(&self, locus: &Locus) -> bool {
        let own = self.0.first.locus();
        (own.seq.commit(), &own.bytes, own.sum) == (locus.seq.commit(), &locus.bytes, locus.sum)
    }
    pub fn into_records(self) -> NonEmpty<Recorded<Record>> {
        self.0
    }
}

/// A record witnessed by the seam: its [`Locus`] beside the value `emit` was
/// given.
///
/// Its locus comes from a [`Commit`], where a line is appended or read back,
/// or from a [`Pointer`] the transcript reader follows.
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

    pub fn into_parts(self) -> (Locus, R) {
        (self.0, self.1)
    }
}

/// A named block in the view fold's memo, named by
/// the [`Seq`] of the record that opened it.
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
/// class-typed functions; no arm is a wildcard, and a [`Refusal`] during
/// replay refuses the whole session.
pub trait Fold {
    /// No `Default` bound: a memo the fold cannot build from nothing — the
    /// model's, which needs its session — seeds [`replay()`] by value.
    type Memo;

    /// # Errors
    /// Returns [`Refusal`] when this fold does not recognise the record —
    /// during replay that refuses the session rather than skip it silently.
    fn step(memo: &mut Self::Memo, record: Recorded<Record>) -> Result<(), Refusal>;
}

#[cfg(test)]
pub(crate) use testkit::{fixture_dir, read_records};

/// What a test of the record needs that touches the host's directories.
#[cfg(test)]
#[allow(clippy::disallowed_methods, reason = "test code")]
mod testkit {
    use super::{AgentId, Born, Emitter, Record, Recorded, log};
    use ral_core::own::OwnDir;
    use std::path::Path;

    /// A directory for the fixtures that name one and make nothing in it: a
    /// birth's log directory, a bare context's sessions.  It is the host's
    /// temp directory, held.
    pub(crate) fn fixture_dir() -> OwnDir {
        OwnDir::temp_for_test().expect("the temp directory is held")
    }

    impl Born {
        /// A birth with no log and no replay, for a test's tab.
        pub(crate) fn for_test(name: &str, parent: Option<AgentId>) -> Self {
            Self {
                log_dir: fixture_dir(),
                name: name.into(),
                parent,
                resumed: None,
                reader: Emitter::none().reader(),
            }
        }
    }

    /// Read the [`Record`]s of the log file at `path` back, past their `Entry`
    /// envelope, for tests across the crate that assert on the raw log rather
    /// than on a fold's memo — the one sanctioned exception to this module's
    /// own "read the log back only through [`replay()`](super::replay)" rule,
    /// since what these tests exercise is the wire shape itself.
    ///
    /// # Errors
    /// Returns `Err` under exactly [`log::Log::read`]'s own conditions.
    pub(crate) fn read_records(path: &Path) -> std::io::Result<Vec<Record>> {
        log::Log::read(std::fs::File::open(path)?, path)?
            .map(|record| record.map(Recorded::into_value))
            .collect()
    }
}
