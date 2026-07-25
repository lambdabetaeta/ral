//! The uniform agent node: canonical event log, persistent shell, capability
//! set, an owned hot-swappable provider, and the attend loop every node runs.
//!
//! An exarch run is a *fleet* of these arranged in a tree; the
//! [`Fleet`](crate::fleet::Fleet) holds what is shared (the registry, the one
//! event bus, the transport engine), and each [`Agent`] is one node.
//!
//! No node is privileged by special-case code; the distinctions reduce to
//! *position*: the parent-less **trunk** publishes its cancel token for the
//! OS-signal path, holding `reply` falls out of the construction-fixed
//! `returns` bit (`!interactive` at the trunk, `true` for every fork), and
//! parking out of `interactive`/the registry's engagement read. A child's
//! single result is delivered up its parent's mailbox by the spawn site, not
//! by the loop, so the attend loop itself is identical for all.
//!
//! This module holds the node's state — [`Agent`] and its fields — and the
//! projections the rest of the crate reads it through. The machinery lives in
//! sibling modules, each a descendant that reaches those private fields
//! directly rather than through accessors:
//!
//! - [`build`] — construction, the fork/branch descent, `/clear`'s rebuild, and
//!   the [`Drop`] teardown.
//! - [`attend`] — the one loop: pull the next inbox item, take it up, do the
//!   ready-boundary housekeeping, and turn a nudge-worthy outcome into a
//!   self-posted [`Post::Nudge`](crate::bus::Post).
//!   [`Agent::attend_backlog`] is its bounded, per-exchange twin for a
//!   converse session, sharing every per-item [`Agent::take_up`] rather than looping a
//!   second way.
//! - [`deliberate`] — one prompt run to quiescence against the provider: the
//!   step loop under its ceiling, the tool-call batch, the compaction step,
//!   and the outcome constructors behind [`deliberate::Outcome`].
//! - [`shell`] — the desk seam: the host services installed for the extent of one
//!   `ral` call, and the two cells through which the desk handler thread writes
//!   while the attend thread sits parked inside it.
//! - [`probe`] — engine state decoded back across the probe rail, since a live
//!   worker handle is not transportable.

mod attend;
mod build;
pub mod cancel;
pub mod deliberate;
pub mod digest;
pub mod event;
pub mod nudge;
mod probe;
pub mod resources;
pub(crate) mod seat;
mod shell;
#[cfg(test)]
mod testkit;
pub mod transcript;

pub use attend::{Control, NoControl, Verdict};
pub use build::{RootConfig, RootSeat};
pub(crate) use build::{Build, fresh_id};
pub(crate) use probe::ProbedWorker;
pub(crate) use shell::{LogCell, ReplyCell};

use crate::agent::digest::{AGENT_REPLY_CAP, clip};
use crate::agent::seat::Seat;
use crate::agent::transcript::Transcript;
use crate::bus::{AgentId, Inbox, Mailbox};
use crate::fleet::registry::AgentRegistry;
use crate::provider::Provider;
use crate::shell_eval;
use ral_core::Value as RalValue;
use ral_core::serial::FOValue;
use std::sync::{Arc, Mutex};

#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool gates an independent, orthogonal axis (interactive, returns, allow_schedule, tool_enabled, disk_warn_latched); not a candidate for a combined enum"
)]
pub struct Agent {
    pub id: AgentId,
    /// This agent's own system prompt, resolved for its own `returns`/
    /// `allow_schedule` — what actually reaches the model on every step
    /// ([`Self::deliberate`], [`Self::compact`]).
    pub(crate) system: String,
    /// The system prompt template [`Self::system`] was resolved from — still
    /// carrying [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`] rather than a
    /// baked-in builtin index, since the index is filtered per agent and
    /// this template is agent-invariant. Read only by [`Self::fork_with`]
    /// and [`Self::host_services`], which pass it on as a child's own
    /// [`Build::system`] (in-thread and desk-spawned alike) so that child
    /// resolves its own index from its own `returns`/`allow_schedule`,
    /// never its parent's.
    system_base: String,
    /// The fleet-shared builtin-index table ([`crate::prompt::BuiltinIndexes`]),
    /// resolved once at the trunk's construction and inherited by every
    /// fork, so resolving a child's own index never needs a live [`Shell`](ral_core::Shell).
    indexes: Arc<crate::prompt::BuiltinIndexes>,
    /// This session's canonical event log.  Its own lock, never the session
    /// lock, so a per-call desk can capture it off `&mut Agent` — but
    /// [`LogCell::lock`] enforces the non-contention this buys as a checked
    /// law, not a hope: the desk only ever runs while the attend thread sits
    /// parked in `run_shell`, so a collision panics rather than blocking.
    log: LogCell,
    /// This session's operational trace (`transcript.jsonl`), written at the
    /// emit seam through the [`Emitter`](crate::bus::Emitter) the frontend
    /// builds from [`Self::transcript`].  The sibling of [`Self::log`]: the
    /// model view (`events.json`) is what the model saw; the transcript is
    /// what the agent did.
    transcript: Transcript,
    /// Where this agent's engine lives and how the host reaches it: every
    /// engine-side reach — per-call installs, `/clear`'s rebuild, the
    /// registry's cancel reach — goes through seat methods.
    pub(crate) seat: Seat,
    caps: ral_core::types::Capabilities,
    /// This agent's parent, or `None` for the **trunk**.  The sole structural
    /// distinction: the trunk publishes its cancel token for the OS-signal path
    /// and, when `interactive`, converses (withholding `reply` and parking
    /// indefinitely); every asymmetry is read from this field plus
    /// `interactive`, never an `is_root` branch.
    parent: Option<AgentId>,
    /// How many more spawn generations may descend from this agent before
    /// the chain bottoms out.  Bounds depth, not fan-out: [`Self::fork`]
    /// never touches this field on the parent, it only computes the child's
    /// own `fuel` as one less — an agent may start any number of children
    /// without spending its own fuel.  Zero makes the desk refuse
    /// `agent-start` with the exhaustion text
    /// ([`crate::fleet::desk::ExarchDesk`]'s spawn spine), so a chain terminates by
    /// refusal rather than recursing forever.  The trunk starts at whatever
    /// [`RootConfig::fuel`] its launch site chose — [`SPAWN_FUEL`] for
    /// exarch, `0` for synod, which asks and answers, never fleets.
    fuel: u32,
    /// This agent's own hot-swappable provider.  A `/model` on the focused
    /// agent swaps *its* handle alone; a `fork` seeds the child's own handle
    /// from this one's current provider, so neither disturbs the other.  The
    /// attend loop reads it once per item.
    provider: ProviderHandle,
    /// Whether a human is attached to the fleet (the TUI).  With `parent =
    /// None` it makes this the *conversing* trunk; off the TUI (`false`) the
    /// trunk is a one-shot headless agent.  Inherited by every fork.
    interactive: bool,
    /// This agent's own inbox — the consumer side of its message queue.
    /// [`Agent::attend`] pulls inbox items from here; [`Agent::deliberate`]
    /// drains tool-boundary steering from it mid-round-trip.
    /// Self-nudges and self-armed wakeups land here; a child's result lands in
    /// its *parent's* inbox, never reaching across into a sibling's.
    inbox: Inbox,
    /// Per-agent nudge state — the retry budget and the one-shot completion
    /// gates.  Its latches reset on a genuine exchange-boundary item, never on
    /// a self-[`Post::Nudge`](crate::bus::Post), so a multi-step nudge sequence runs to
    /// completion within one exchange.
    nudges: nudge::Registry,
    /// This agent's cancellation token — one sticky token for its life,
    /// registered in the fleet so the subtree cascade (`agent-cancel`, the
    /// ceiling, and `/clear`) always reaches the live exchange.
    /// The attend loop [`reset`](cancel::Token::reset)s its flag at each
    /// genuine exchange boundary, so an Esc on one exchange never bleeds into
    /// the next; the trunk additionally [`publish`](cancel::publish)es it for
    /// OS signals.
    cancel: cancel::Token,
    /// Whether this agent's provider requests advertise the `ral` tool at
    /// all — read by `provider.complete` (advertisement) and [`Self::invoke`]
    /// (dispatch), so the two can never disagree about whether a call was
    /// invited.  `false` only for a `--chat` trunk, which converses with no
    /// tool whatsoever; construction-fixed like `returns`/`allow_schedule`
    /// below.
    tool_enabled: bool,
    /// Whether this agent holds `reply`, fixed at construction from the same
    /// bit that shaped `tools` above, so the two cannot disagree.
    returns: bool,
    /// Whether this agent holds the self-wakeup family, likewise fixed at
    /// construction and inherited verbatim by every fork. `pub(crate)` like
    /// [`Self::agents`]/[`Self::schedules`] above, so a test in another
    /// module (the schedule-family harness builtins' full-stack test) can
    /// grant it directly on a [`Self::for_test`] trunk, which hardcodes it
    /// `false`.
    pub(crate) allow_schedule: bool,
    /// A returning agent's staged return value, harvested by
    /// [`Self::run_shell`] from that call's own [`ReplyCell`] the instant its
    /// installed desk retires. Lifted by [`Self::deliberate`]
    /// into a [`deliberate::Outcome::Replied`] once the current tool-call
    /// batch finishes draining — never mid-batch, so the session reaches a
    /// clean boundary with every `call_id` answered.  Held as the faithful
    /// [`FOValue`] the model passed, so the consuming edge renders it (a null
    /// or empty value settles as the `Empty` variant of
    /// [`AgentOutcome`](crate::bus::AgentOutcome)).  Every returning
    /// agent sets it — a peer and the headless root; `reply` is withheld only
    /// from the interactive root.  Scoped to its own batch: [`Self::deliberate`]
    /// resets it on entry, so a reply staged in a deliberation that then
    /// cancels, errors, or panics before the drain that takes it never
    /// survives to poison the next deliberation.  A plain field, not a
    /// `ReplyCell`: outside the one call's extent a fresh cell is harvested
    /// into, this is touched only by the attend thread under `&mut Agent`.
    reply: Option<FOValue>,
    /// The **fleet's** agent registry — one shared map, cloned to every node,
    /// so "all live agents" is its contents.  Every agent registers itself here
    /// and registers its own children, so the registry is the spawn tree at any
    /// depth.  Survives `/clear`: [`clear_subtree`](crate::fleet::registry::AgentRegistry::clear_subtree)
    /// bumps the generation and reaps the focused agent's descendants, so a
    /// worker that settles after the clear drops its result.
    pub(crate) agents: AgentRegistry,
    /// Live scheduled wakeups (cron / after).  A peer may self-schedule (it
    /// posts wakeups into its own inbox), so this is no longer root-only; the
    /// self-wakeup builtins (`schedule`/`schedules`/`unschedule`) still gate
    /// on `allow_schedule`, refused at the desk without the grant.
    pub(crate) schedules: crate::fleet::schedule::ScheduleRegistry,
    /// Input tokens the model saw on the most recent completion — the live
    /// numerator for the context-pressure compaction trigger
    /// ([`Self::compact`]), the same signal the TUI's fidelity gauge reads.
    /// Set from each step's usage; reset to `0` on `/clear`.
    last_input: u64,
    /// Current pinned-state digests (`key → one-line summary`), mirrored from
    /// the surface sink as the model `` `pin ``s/`` `unpin ``s state.  Pins
    /// otherwise flow past the session to the frontend; this small copy lets
    /// the periodic [`nudge`] reminder describe what the model has pinned.
    /// Cleared on `/clear` like the rest of the session's generation state.
    pins: shell_eval::PinDigests,
    /// The agent's ral-call epoch: incremented once at the top of every
    /// [`Self::run_shell`] call — success or error alike, a failed eval is
    /// still a call.  The integer clock every per-call ledger reads: the
    /// settled-worker retention sweep, the binding-lease ledger, and the
    /// disk-warn check's amortization ([`Self::check_disk_warn`]) all read
    /// this same counter.
    /// Starts at 0, and a fork's child starts its own at 0; deliberately NOT
    /// reset by `/clear` — the registry is emptied there anyway, and a
    /// monotone counter is simpler to reason about than one that rewinds.
    ral_epoch: u64,
    /// The operator's disk-warn ceiling (`config::disk_warn_bytes`), shared
    /// verbatim by every fork — a host setting, not a per-agent choice.
    /// `None` means [`Self::check_disk_warn`] never walks the log/scratch
    /// dirs at all: no configuration, no cost, ever.
    disk_warn_bytes: Option<u64>,
    /// The [`Self::ral_epoch`] at which the next disk-warn check is due;
    /// advances by [`DISK_WARN_CHECK_INTERVAL`] each time
    /// [`Self::check_disk_warn`] actually walks, so the walk is amortized
    /// regardless of how many ready boundaries pass with the epoch
    /// unchanged.
    disk_check_epoch: u64,
    /// Whether the disk-warn ceiling is currently latched over: set on the
    /// crossing, cleared once a later check finds the total back under, so
    /// the warning fires once per excursion, never once per boundary.
    disk_warn_latched: bool,
    /// The IT-set `fetch-url` policy, audit ledger, and rate budget —
    /// shared verbatim by every fork, exactly like [`Self::disk_warn_bytes`]:
    /// a host setting, not a per-agent choice.
    egress: crate::fleet::egress::Egress,
}

/// The depth budget exarch's own trunks start with, passed as
/// [`RootConfig::fuel`]; each [`Agent::fork_with`] hands its child one less,
/// and a `fuel == 0` agent's `agent-start` calls are refused at the desk
/// ([`crate::fleet::desk::ExarchDesk::launch`]). Bounds how many generations
/// deep a delegation chain may recurse — a few hops covers legitimate
/// delegation — while stopping a runaway spawn-calling chain from exhausting
/// threads instead of the process. Fan-out is a separate, unbounded axis:
/// how many children any one agent starts never spends this budget.
pub(crate) const SPAWN_FUEL: u32 = 3;

/// A shared, swappable handle to the active provider.
///
/// The attend loop reads
/// the current provider once per item through [`Self::current`]; a `/model`
/// switch on the frontend's UI thread swaps it through [`Self::swap`].  Live
/// provider replacement across the UI / attend thread boundary needs a shared
/// cell rather than an owned `Arc` the worker would hold privately; a `Mutex`
/// suffices (the cell is touched at most once per item and on the rare
/// switch).  A peer wraps a *snapshot* of the provider at spawn, so a later
/// root `/model` never disturbs an already-running child.
#[derive(Clone)]
pub struct ProviderHandle(Arc<Mutex<Arc<Provider>>>);

impl ProviderHandle {
    pub fn new(provider: Arc<Provider>) -> Self {
        Self(Arc::new(Mutex::new(provider)))
    }

    /// The provider in force for the next item.
    ///
    /// # Panics
    /// Panics if the provider-handle mutex is poisoned.
    pub fn current(&self) -> Arc<Provider> {
        self.0.lock().expect("provider handle poisoned").clone()
    }

    /// Replace the active provider (a `/model` switch).  Takes effect on the
    /// attend loop's next item; an in-flight deliberation finishes on the
    /// provider it started with, and any running peer keeps its own
    /// snapshot.
    ///
    /// # Panics
    /// Panics if the provider-handle mutex is poisoned.
    pub fn swap(&self, provider: Arc<Provider>) {
        *self.0.lock().expect("provider handle poisoned") = provider;
    }
}

impl Agent {
    /// This agent's own provider handle — read by the spawn site to register it
    /// (so a `/model` on the child's tab swaps the child alone) and by the
    /// frontend to build the [`Fleet`](crate::fleet::Fleet).
    pub(crate) fn provider_handle(&self) -> ProviderHandle {
        self.provider.clone()
    }

    /// The provider in force for this agent's next item — for a slash
    /// command (`/compact`) the attend loop runs against the agent's own
    /// handle.
    pub(crate) fn current_provider(&self) -> Arc<Provider> {
        self.provider.current()
    }

    pub(crate) fn log_dir(&self) -> std::path::PathBuf {
        self.log.lock().dir().to_path_buf()
    }

    /// A handle to this session's [`Transcript`] — for the frontend to attach
    /// to the emitter it drives this session through (the root) and for the
    /// spawn site to hand a forked child its own trace.
    pub(crate) fn transcript(&self) -> Transcript {
        self.transcript.clone()
    }

    /// This session's mailbox — for the `schedule` tool to arm wakeups into
    /// its *own* inbox, and the spawn site to capture the parent's box as a
    /// child's upward result edge.
    pub(crate) fn mailbox(&self) -> Mailbox {
        self.inbox.mailbox()
    }

    /// A handle to this session's inbox — for a frontend to build the bus
    /// (whose emitters mint mailboxes onto this same queue) over the session's
    /// own queue, so producers and the attend loop share one inbox.
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// This session's cancellation token (read by the spawn site to register
    /// a peer's sticky token in the parent's [`Self::agents`]).
    pub(crate) fn cancel_token(&self) -> &cancel::Token {
        &self.cancel
    }

    /// Whether the session is at a settled ready boundary — every
    /// deliberation must hand it back here.  Exposed for the harness to
    /// assert the transcript-admission invariant after a deliberation.
    ///
    /// # Panics
    /// Panics if the log mutex is poisoned.
    pub fn is_ready(&self) -> bool {
        self.log.lock().is_ready()
    }

    /// The model-view messages the next request would carry.  Exposed
    /// for the harness to assert no malformed / empty message survives
    /// into the committed transcript.
    ///
    /// # Panics
    /// Panics if the log mutex is poisoned.
    pub fn rendered_messages(&self) -> Vec<genai::chat::ChatMessage> {
        self.log.lock().history_messages()
    }

    /// Serialised model-view byte count — the compaction-threshold
    /// input.  Exposed so the harness can assert compaction fired.
    ///
    /// # Panics
    /// Panics if the log mutex is poisoned.
    pub fn history_bytes(&self) -> usize {
        self.log.lock().history_bytes()
    }

    /// A returning agent holds `reply`; a conversing one had it withheld —
    /// both the desk's refusal and the prompt's builtin index read this same
    /// construction-fixed bit, so the two cannot disagree.
    pub(crate) fn returns(&self) -> bool {
        self.returns
    }

    /// This session's *live* working directory — read through the probe so
    /// a prior `cd` is reflected. The seat's own `cwd` field ([`Seat::Identity`])
    /// is not this: it is fixed at construction and read only to reseed the
    /// shell on `/clear`, so it never moves with the model's own `cd`s. The
    /// one caller, [`Self::host_services`], needs exactly the live value: a
    /// desk-spawned child should start where the model actually is now.
    pub(crate) fn cwd(&self) -> std::path::PathBuf {
        match self.seat.transport().probe(FOValue::Variant {
            label: "cwd".into(),
            payload: None,
        }) {
            Ok(FOValue::String { value }) => std::path::PathBuf::from(value),
            other => unreachable!("`cwd probe always answers a String, got {other:?}"),
        }
    }

    /// This session's ambient authority.  Production spawns read the private
    /// `caps` field directly ([`Self::fork_with`], [`Self::branch`]) or narrow
    /// the desk's own captured snapshot ([`crate::policy::narrow`]); this accessor is
    /// exercised only by tests building a fork off a live agent's own
    /// capabilities.
    #[cfg(test)]
    pub(crate) fn caps(&self) -> &ral_core::types::Capabilities {
        &self.caps
    }

    /// Non-blocking pop of the next inbox item, for a test polling for an
    /// async spawn's settle without running a full deliberation (and its
    /// provider round-trip) on the parent session.
    #[cfg(test)]
    pub(crate) fn next_item_for_test(&self) -> Option<crate::bus::Item> {
        self.inbox.next_item()
    }
}

/// Render a reply payload to the text a model parent reads: the shared
/// value→text rule ([`FOValue`] → [`RalValue`] → [`shell_eval::ral_value_to_text`]),
/// clipped to the reply cap.  The peer edge (the spawn tool settle site) and
/// the `drive_peer` test helper share it; the headless edge instead projects
/// the value faithfully through [`shell_eval::user_json`].
pub(crate) fn render_reply(v: &FOValue) -> String {
    clip(
        &shell_eval::ral_value_to_text(&RalValue::from(v.clone())).unwrap_or_default(),
        AGENT_REPLY_CAP,
    )
}

/// The message of a recovered panic payload, for either string shape.
pub(crate) fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
}
