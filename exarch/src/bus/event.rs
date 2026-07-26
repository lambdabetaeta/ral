//! The outbound vocabulary of the agent / frontend boundary — the closed
//! set of everything a worker can tell a frontend, and the dual of
//! the inbound `post` module.  Carried as [`Event`]: a [`Kind`] stamped
//! with the [`AgentId`] that produced it.

use super::AgentId;
use crate::agent::event::ProviderErrorRecord;
use crate::bus::card::{Card, IoEvent};
use crate::bus::post::AgentOutcome;
use crate::provider::{Tuning, Usage};
use std::path::PathBuf;
use std::time::Duration;

/// One [`Kind`] stamped with the [`AgentId`] that produced it — the unit
/// [`crate::bus::BusSender`]/[`crate::bus::BusReceiver`] carry and [`crate::bus::Sink::handle`] consumes.
pub struct Event {
    pub id: AgentId,
    pub kind: Kind,
}

/// Prefix of the [`Kind::Error`] message [`crate::bus::pump`] emits when the worker thread
/// unwinds.
///
/// Shared so a sink can recognise a recovered panic without
/// matching on free text (the headless result reports it as an error rather
/// than a clean completion).
pub(crate) const WORKER_PANIC_PREFIX: &str = "worker panicked: ";

pub enum Kind {
    Born {
        log_dir: PathBuf,
        /// This agent's name — its identity everywhere the model can see it
        /// (ASCII alnum / `-` / `_`, 1–24 chars).  The TUI surfaces it in the
        /// tab bar; headless ignores it.
        name: String,
        /// The spawning agent's id — the tab's parent.  The TUI records it so
        /// that when a focused agent ends (`reply`), focus falls back to its
        /// parent, recursing toward the trunk.
        parent: AgentId,
        /// A `/branch` tab (a conversing fork of its parent) rather than a
        /// returning sub-agent.  The TUI records it so `/close` admits only a
        /// branch tab.
        branch: bool,
    },
    Died,
    Token(String),
    /// A live reasoning token, streamed during the model's thinking phase.
    /// Accumulated by the frontend into a provisional deliberation seat until
    /// the final `Reasoning` event commits a real thinking block.
    Thinking(String),
    /// Ends the current streaming step: the frontend flushes whatever
    /// `Token`/`Thinking` text is still open into a committed block
    /// (`tui::viewport::close_boundary`) before the next step or exchange begins.
    /// Interactive-only chrome — no transcript record, since it carries no
    /// content of its own.
    Boundary,
    /// The step's final model reasoning. The frontend commits `text` as a
    /// standalone dialable thinking block; `answer_chars` is the whole turn's
    /// answer mass, the deliberation grain's denominator.
    Reasoning {
        text: String,
        answer_chars: u32,
    },
    Usage(Usage),
    Step {
        /// Steps in the current segment: restarts at 1 each time this
        /// attend loop resumes (e.g. after an async-spawned block settles),
        /// so a run-wide step count is the consumer's own running tally
        /// ([`crate::headless::Headless::steps`]), not this field.
        n: u32,
        tuning: Tuning,
    },
    /// A transient label for the worker's current synchronous phase —
    /// "awaiting model", "compacting".  Emitted before a long op so the
    /// frontend can name what the worker is doing during an otherwise silent
    /// gap (a progress label alongside the spinner); the headless
    /// `events.json` keeps it for post-mortem.  Superseded by the next event
    /// of any kind.
    Phase(String),
    /// A call to `ral` — the one call that genuinely crosses the provider
    /// boundary.  See [`Kind::HarnessCall`] for a desk verb's rail-identical
    /// twin.
    ToolCall {
        tool: &'static str,
        cmd: String,
        /// Short, single-line label the sink shows on the rail — `ral`'s
        /// mandatory `description` — with `cmd` revealed when the user
        /// opens the call. `None` means there is nothing to reveal, so the
        /// call renders statically from `cmd` (the invalid-input rail
        /// header).
        summary: Option<String>,
    },
    ToolResult(String),
    /// A desk verb **acted** — `spawn`, `cancel`, `message`, `reply`,
    /// `schedule`, `unschedule`.  An act changes the world *outside* the
    /// exchange: it starts a process, fills another agent's inbox, arms a wakeup,
    /// ends a run.  A [`Kind::ToolCall`] only observes, so the two never
    /// share a rendered shape — and an act's whole
    /// information content is these three fields, never a host-authored
    /// sentence about them.
    HarnessCall {
        /// The bare imperative the model invoked.
        verb: &'static str,
        /// What the act names: the agent name or schedule label. `None` for
        /// an act that addresses nothing (`reply` answers its parent).
        subject: Option<String>,
        /// The act's argument — the launch prompt, the message text, the
        /// trigger description, the replied value — or, when `failed`, the
        /// short reason it was refused.  Empty when the act carries no
        /// argument and simply landed (`cancel`, `unschedule`).
        payload: String,
        /// Whether the act was refused.  The refusal's *long* form is the
        /// raise, which reaches the model through captured stderr; this bit
        /// only tiers the row.
        failed: bool,
    },
    /// The paired result for a [`Kind::HarnessCall`] — the desk's twin of
    /// [`Kind::ToolResult`], and a *forensic* record only: the act row says
    /// on screen everything the act has to say, and the model's copy of a
    /// refusal travels the raise.  Its one consumer is
    /// [`crate::agent::transcript`].
    HarnessResult(String),
    /// The text of an item as it enters context — a human prompt, a wakeup,
    /// or a peer agent message alike (`agent::attend::announce`), despite the name;
    /// an agent result renders through [`Kind::SubagentDone`] instead.
    /// Interactive-only: no transcript record, and the TUI disarms its
    /// post-cancel drain guard on the first one after a clear.
    UserPromptEcho(String),
    StopReason(String),
    Error(String),
    /// An operational note the agent's attend loop issued — a truncation recovery,
    /// a compaction step.  Names *what happened*, like every other operational
    /// `Kind`; the display renders it dim, but *dimness is the renderer's
    /// choice*, not a fact in the vocabulary.  The trace records it as
    /// `system_note`; it has no `events.json` twin, since it is not a message
    /// the model saw.
    SystemNote(String),
    /// A recovery nudge the attend loop issued between attempts.  The trace records
    /// it; the display surfaces it as it sees fit (a stderr line in headless,
    /// quiet on the TUI rail).  Its `events.json` twin is the model-view
    /// forensic breadcrumb.
    Nudge {
        used: u32,
        max: u32,
        cause: String,
    },
    ProviderError(ProviderErrorRecord),
    /// Emitted by the `agent` tool when a subagent finishes — *after*
    /// the child's own [`Kind::Died`] and *before* the spawn rejoins the
    /// parent's tool result.  The event's session id is the parent
    /// (typically root); the TUI lands the breadcrumb in root's
    /// scrollback regardless of nesting depth, since subagent output
    /// otherwise lives only in its own tab and ages out at `LINGER`.
    SubagentDone {
        name: String,
        /// How the child settled.  The sink reduces this with `text` through
        /// [`AgentOutcome::breadcrumb`] to the body / header-suffix split —
        /// the same reduction an async result makes — so sync and async land
        /// the identical dialable block.
        outcome: AgentOutcome,
        /// The subagent's final assistant text — empty when the run
        /// failed or was cancelled.
        text: String,
        elapsed: Duration,
    },
    /// A render document a ral kit handed to the `surface` builtin: an
    /// ordered stack of Bertin [`Card`] marks (a diff, a measure, a fields
    /// matrix, roled text, raw ink) composed in ral and decoded once by
    /// [`shell_eval`] onto the bus.  Always rendered — a surfaced card is a
    /// deliberate user-facing act.  The open set of cards over a closed set
    /// of marks is what keeps the renderer total while the kit invents new
    /// cards in pure ral.
    ///
    /// [`shell_eval`]: crate::shell_eval
    Card(Card),
    /// A detached worker's `` `done `` completion event, decoded once by
    /// [`shell_eval`] into its typed [`DoneOutcome`](crate::bus::card::DoneOutcome)
    /// and paired with the one-line [`Card`] composed from it — the
    /// [`Kind::Io`] pattern, so `transcript.jsonl` records how the worker
    /// settled (a clean return, a raised error, a panic) rather than only
    /// the card's ink.
    ///
    /// [`shell_eval`]: crate::shell_eval
    Done {
        outcome: crate::bus::card::DoneOutcome,
        card: Card,
    },
    /// A structural I/O event core surfaced (a read, write, exec, or grep),
    /// decoded once by [`shell_eval`] into a typed [`IoEvent`] and paired with
    /// the [`Card`] composed from it.  The bus carries *both*: the rendered
    /// card is what the rail draws, while the raw `event` keeps the structure
    /// the mark tree erases, so `transcript.jsonl` records the effect itself;
    /// the card is a presentation (the rail, the `user.log`) and is not.
    ///
    /// [`shell_eval`]: crate::shell_eval
    Io {
        event: IoEvent,
        card: Card,
    },
    /// Kit-authored *state* pinned to a keyed register slot — the
    /// model-authored dual of the matrix.  The `` `pin [key, body] `` surface
    /// decodes here, writing `card` to slot `key` and **overwriting in place**
    /// on re-pin.  Unlike [`Kind::Card`], a pin is neither logged nor landed in
    /// scrollback: it is what is *currently true*, not a thing that happened, so
    /// it is rendered ambiently in the reserved register column and updated
    /// where it sits.
    Pin {
        key: String,
        card: Card,
    },
    /// Drop a pinned register slot: the `` `unpin [key] `` surface, or a `` `pin ``
    /// whose body is absent or empty.  A finished plan clears its gauge.
    Unpin {
        key: String,
    },
    /// A ready-boundary housekeeping fact core's own engine pushed as a
    /// `` `notice `` surface class — a worker the lease chain reaped
    /// (unobserved past its idle bound, past its absolute backstop, or the
    /// retention sweep expiring a settled entry's unclaimed result), or a run
    /// of idle top-level bindings the ledger pruned. Core emits the fact itself,
    /// at the ready boundary, through the run's surface sink, rather than a
    /// host draining an accessor and composing the event from what it read.
    /// Unlike [`Kind::Card`], the decoded [`Notice`](crate::bus::card::Notice)
    /// rides alongside the rendered `card` (the [`Kind::Io`] pattern) so
    /// `transcript.jsonl` keeps the structural fact the one-liner erases.
    /// Never model-facing — no `events.json` twin, no inbox message: the
    /// idle-observation lease already bounds a forgotten worker's cost to at
    /// most an hour of one seat in the per-agent cap, so nothing is gained
    /// by also telling the model directly.
    Notice {
        notice: crate::bus::card::Notice,
        card: Card,
    },
    /// The `/resources` probe fold: the agent's own accumulator rows,
    /// assembled on its attend thread at the exchange boundary the command
    /// drains at, beside the card rendering them — the raw-fact/rendering
    /// pairing of [`Kind::Io`] and [`Kind::Notice`], so
    /// `transcript.jsonl` records the rows while the card stays a
    /// presentation.  The TUI appends the rows for the accumulators *it*
    /// owns (viewports, views, the bus) to the card at render time; those
    /// stay frontend-side.  Never model-facing: no `events.json` twin, no
    /// inbox reply — probing is for the operator, and it mutates and
    /// renews nothing.
    Resources {
        rows: Vec<crate::agent::resources::ProbeRow>,
        card: Card,
    },
}
