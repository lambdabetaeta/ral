//! The outbound half of the agent / frontend boundary: the closed set of
//! everything a worker can tell a frontend, carried as an [`Event`].  The
//! inbound dual is `post`.

use super::AgentId;
use crate::agent::event::ProviderErrorRecord;
use crate::bus::card::Card;
use crate::bus::post::AgentOutcome;
use crate::provider::{Tuning, Usage};
use ral_core::types::Observation;
use std::path::PathBuf;
use std::time::Duration;

/// One [`Kind`] stamped with the [`AgentId`] that produced it — the unit the
/// channel carries and [`crate::bus::Sink::handle`] consumes.
pub struct Event {
    pub id: AgentId,
    pub kind: Kind,
}

/// Prefix of the [`Kind::Error`] a recovered worker panic emits, so a sink can
/// tell one from a clean completion without matching free text.
pub(crate) const WORKER_PANIC_PREFIX: &str = "worker panicked: ";

/// What an agent is doing — a total state, not a label the next event erases.
///
/// Every moment of a session's life is one of these five, so a frontend can
/// always name the one it is in, including the idle one no label ever announced.
/// The transition is the event ([`Kind::State`]); the states themselves carry no
/// clock and no counter.  A frontend times its own residence in one, which is
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
    /// Summarising the history prefix to win back context.
    Compacting,
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
            Self::Compacting => "compacting",
            Self::WaitingOnAgents => "waiting on agents",
        }
    }

    /// Whether work is outstanding.  [`Self::Ready`] is the one settled state,
    /// so this is what a frontend keys a spinner, a repaint tick, or an
    /// ellipsis on.
    #[must_use]
    pub fn pending(self) -> bool {
        self != Self::Ready
    }
}

pub enum Kind {
    Born {
        log_dir: PathBuf,
        /// ASCII alnum / `-` / `_`, 1–24 chars; the identity the model sees.
        name: String,
        /// The spawning agent: focus falls back along this when a focused agent
        /// ends, recursing toward the trunk.
        parent: AgentId,
        /// A `/branch` tab — a conversing fork of its parent, and the only kind
        /// `/close` may kill.
        branch: bool,
    },
    Died,
    Token(String),
    /// A live reasoning token, accumulated into a provisional seat until
    /// [`Kind::Reasoning`] commits the real thinking block.
    Thinking(String),
    /// Ends the streaming step: the frontend commits whatever `Token`/`Thinking`
    /// text is still open.  Chrome — untraced, and carries no content itself.
    Boundary,
    /// The step's final model reasoning, committed as a standalone dialable
    /// block.  `answer_chars` is *this step's* answer length, which the frontend
    /// accumulates into the block's deliberation-grain denominator.
    Reasoning {
        text: String,
        answer_chars: u32,
    },
    Usage(Usage),
    Step {
        /// Restarts at 1 on every entry to `deliberate`, so a run-wide step
        /// count is the consumer's own tally, not this field.
        n: u32,
        tuning: Tuning,
    },
    /// The worker entered a new [`AgentState`].  Untraced, and emitted only on
    /// a transition, so a frontend's clock measures time in state.
    State(AgentState),
    /// A call to `ral` — the one call that genuinely crosses the provider
    /// boundary.  [`Kind::HarnessCall`] is a desk verb's rail-identical twin.
    ToolCall {
        tool: &'static str,
        cmd: String,
        /// `ral`'s mandatory `description`: the rail's one-line label, `cmd`
        /// revealed on opening.  `None` renders `cmd` statically instead.
        summary: Option<String>,
    },
    ToolResult(String),
    /// A desk verb **acted** — `spawn`, `cancel`, `message`, `reply`,
    /// `schedule`, `unschedule`.  It changes the world *outside* the exchange
    /// where a [`Kind::ToolCall`] only observes, so the two never render alike.
    HarnessCall {
        verb: &'static str,
        /// The agent name or schedule label; `None` for an act that addresses
        /// nothing (`reply` answers its parent).
        subject: Option<String>,
        /// The launch prompt, message text, trigger, or replied value — when
        /// `failed`, the short reason instead.  Empty for `cancel`/`unschedule`.
        payload: String,
        /// Whether the act was refused.  The long form is the raise reaching the
        /// model through captured stderr; this bit only tiers the row.
        failed: bool,
    },
    /// The paired result for a [`Kind::HarnessCall`], *forensic* only: the act
    /// row says everything on screen, so its one consumer is the trace.
    HarnessResult(String),
    /// The text of *any* item as it enters context — human prompt, wakeup, or
    /// peer agent message alike (`agent::attend::announce`), despite the name;
    /// an agent result goes through [`Kind::SubagentDone`].  Untraced, and the
    /// first one after a clear disarms the TUI's post-cancel drain guard.
    UserPromptEcho(String),
    StopReason(String),
    Error(String),
    /// An operational note the attend loop issued — a truncation recovery, a
    /// compaction step.  Traced, but no `events.json` twin: the model never saw it.
    SystemNote(String),
    /// A recovery nudge the attend loop issued between attempts.  Unlike
    /// [`Kind::SystemNote`] it does have an `events.json` twin.
    Nudge {
        used: u32,
        max: u32,
        cause: String,
    },
    ProviderError(ProviderErrorRecord),
    /// The stream broke mid-turn, past the first chunk.  Distinct from
    /// [`Kind::ProviderError`], which ends an exchange: the streamed prefix is
    /// committed and the nudge re-drives the turn, so this reports a failure the
    /// run survived.  Carries the same record, rendered under its own headline.
    Stalled(ProviderErrorRecord),
    /// An async agent settled and its result drained into a parent's context
    /// (`agent::attend::announce`).  Stamped with the *draining* agent's id, yet
    /// the TUI lands the breadcrumb in root's scrollback whatever the nesting
    /// depth — a subagent's own tab ages out at `LINGER`.
    SubagentDone {
        name: String,
        /// Reduced with `text` through [`AgentOutcome::breadcrumb`] into the
        /// body / header-suffix split every settled agent renders through.
        outcome: AgentOutcome,
        /// Empty when the run failed or was cancelled.
        text: String,
        elapsed: Duration,
    },
    /// A render document a ral kit composed for the `surface` builtin: Bertin
    /// [`Card`] marks decoded once by [`crate::shell_eval`].  Rendered but never
    /// traced — a rendering is no effect — and an open set of cards over a
    /// closed set of marks keeps the renderer total.
    Card(Card),
    /// A detached worker's `` `done `` completion: its typed
    /// [`DoneOutcome`](crate::bus::card::DoneOutcome) beside the one-line
    /// [`Card`] made from it, so the trace records how the worker settled.
    Done {
        outcome: crate::bus::card::DoneOutcome,
        card: Card,
    },
    /// A fact core observed at a door (a command settling, a write landing, a
    /// redirect read, a grep, a capability denial): core's one [`Observation`]
    /// — carrying the call site, the observation window, and the acting
    /// principal — beside its [`Card`].  The card is what the rail draws;
    /// `event` keeps the structure the mark tree erases, and that is what traces.
    Io {
        event: Observation,
        card: Card,
    },
    /// State pinned to a keyed register slot: the model's `` `pin [key, body] ``
    /// surface and the host's reconciled `services` slot.  Untraced and never in
    /// scrollback — it is what is *currently true*, so a re-pin overwrites.
    Pin {
        key: String,
        card: Card,
    },
    /// Drop a pinned slot: the `` `unpin [key] `` surface, or a `` `pin `` whose
    /// body is absent or empty.  A finished plan clears its gauge.
    Unpin {
        key: String,
    },
    /// A ready-boundary housekeeping fact core's own engine pushed as a
    /// `` `notice `` surface — a reaped worker, or idle top-level bindings the
    /// ledger pruned — emitted through the run's surface sink, not by a host
    /// draining an accessor.  Never model-facing; the decoded `Notice` rides
    /// beside `card` so the trace keeps what the one-liner erases.
    Notice {
        notice: crate::bus::card::Notice,
        card: Card,
    },
    /// The `/resources` probe fold: the agent's own accumulator rows beside the
    /// card rendering them.  The TUI appends rows for the accumulators *it* owns
    /// at render time, so those never reach the trace.  Never model-facing.
    Resources {
        rows: Vec<crate::agent::resources::ProbeRow>,
        card: Card,
    },
}
