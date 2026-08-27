//! The uniform agent node: canonical event log, persistent shell, capability
//! set, hot-swappable provider, and the attend loop every node runs.
//!
//! A run is a tree of these; [`Fleet`](crate::fleet::Fleet) holds what they
//! share.
//!
//! No node is privileged by special-case code — the distinctions reduce to
//! *position*.  The parent-less trunk publishes its cancel token for the
//! OS-signal path; holding `reply` falls out of `returns`, parking out of
//! `interactive`.  A child's result is posted up its parent's mailbox by the
//! spawn site, not by the loop, so the loop is identical for all.
//!
//! This file holds the state; the machinery lives in [`build`], [`attend`],
//! [`deliberate`], [`shell`], and [`probe`], which reach these private fields
//! directly rather than through accessors.

mod attend;
mod build;
pub mod cancel;
pub mod deliberate;
mod dial;
pub mod digest;
pub mod event;
pub mod nudge;
mod probe;
pub mod resources;
pub(crate) mod seat;
mod shell;
#[cfg(test)]
pub(crate) mod testkit;

pub(crate) use attend::quiesce_when_childless;
pub use attend::{Control, NoControl, Verdict};
pub(crate) use build::{Build, fresh_id};
pub use build::{RecordedAccount, RootConfig, RootSeat};
pub use dial::Dial;
pub(crate) use probe::ProbedWorker;
pub(crate) use shell::{LogCell, ReplyCell};

use crate::agent::seat::Seat;
use crate::bus::{AgentId, Inbox, Mailbox};
use crate::fleet::registry::AgentRegistry;
use crate::provider::Provider;
use crate::shell_eval;
use ral_core::serial::FOValue;
use std::sync::{Arc, Mutex};

#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool gates an independent, orthogonal axis (interactive, returns, allow_schedule, tool_enabled, search, disk_warn_latched, context_warn_latched); not a candidate for a combined enum"
)]
pub struct Agent {
    pub id: AgentId,
    /// The resolved prompt that reaches the model on every step.
    pub(crate) system: String,
    /// The template [`Self::system`] came from, still carrying
    /// [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`]: a child inherits *this*,
    /// so it resolves for its own grants rather than its parent's.
    system_base: String,
    /// The fleet-shared builtin index, resolved once at the trunk, so a fork
    /// resolving its own prompt never needs a live [`Shell`](ral_core::Shell).
    index: Arc<crate::prompt::BuiltinIndex>,
    /// Under its own lock so a per-call desk can capture it off `&mut Agent`.
    /// [`LogCell::lock`] panics on contention rather than blocking — the desk
    /// runs only while the attend thread is parked in `run_shell`.
    log: LogCell,
    _run_lock: Option<crate::bootstrap::RunLock>,
    resume_summary: Option<(u64, u64)>,
    /// Every engine-side reach goes through this seat's methods.
    pub(crate) seat: Seat,
    caps: ral_core::types::Capabilities,
    /// `None` for the trunk.  Read with `interactive` wherever position
    /// matters, in place of an `is_root` branch.
    parent: Option<AgentId>,
    /// Spawn generations still available below here.  Bounds depth, not
    /// fan-out: a fork spends none of the parent's, only handing the child one
    /// less, and at zero the desk refuses `` agents `start ``.
    fuel: u32,
    /// A `/model` swaps this handle alone; a fork seeds the child's from a
    /// snapshot, so neither disturbs the other.
    provider: ProviderHandle,
    /// Whether a human is attached (the TUI).  With no parent it makes the
    /// trunk converse rather than run one-shot and headless.  Inherited.
    interactive: bool,
    /// Self-nudges and armed wakeups land here; a child's result lands in its
    /// *parent's*, never reaching across into a sibling's.
    inbox: Inbox,
    /// Its latches reset on a genuine exchange-boundary item, never on a
    /// self-nudge, so a nudge sequence runs to completion within one exchange.
    /// `None` for a toolless (`--chat`) trunk: every nudge steers an agent
    /// toward a tool it does not hold, so such a turn is only ever reported.
    nudges: Option<nudge::Registry>,
    /// One sticky token for this agent's life, registered in the fleet so the
    /// subtree cascade reaches the live exchange.  The attend loop
    /// [`reset`](cancel::Token::reset)s it at each exchange boundary so an Esc
    /// never bleeds into the next; the trunk also [`publish`](cancel::publish)es
    /// it for OS signals.
    cancel: cancel::Token,
    /// `provider.complete` (advertisement) and [`Self::invoke`] (dispatch)
    /// read this one bit, so they cannot disagree about whether the `ral` call
    /// they are handling was ever invited.
    tool_enabled: bool,
    /// Whether this agent may ride the provider's hosted web search — the IT
    /// network policy's verdict ([`crate::egress::Egress`]), inherited verbatim.
    search: bool,
    /// Whether this agent holds `reply`: `!interactive` at the trunk, `true`
    /// for a fork, `false` for a `/branch`.  The desk's refusal and the
    /// prompt's builtin index read this same bit.
    returns: bool,
    /// Whether this agent holds the self-wakeup family.  `pub(crate)` so the
    /// schedule harness test can grant it on a [`Self::for_test`] trunk, which
    /// hardcodes it `false`.
    pub(crate) allow_schedule: bool,
    /// The staged return value, harvested by [`Self::run_shell`] from that
    /// call's [`ReplyCell`] as the desk retires, then lifted into a
    /// [`deliberate::Outcome::Replied`] only once the tool-call batch drains,
    /// so the session settles with every `call_id` answered.
    /// [`Self::deliberate`] clears it on entry, so a cancelled or panicking
    /// deliberation cannot poison the next.
    reply: Option<FOValue>,
    /// The **fleet's** registry, one map cloned to every node: each agent
    /// registers itself and its children, so this is the spawn tree at any
    /// depth.  [`clear_subtree`](crate::fleet::registry::AgentRegistry::clear_subtree)
    /// bumps the cleared session's own generation, so a worker settling after
    /// *that* session's `/clear` drops its result — and one settling after
    /// some other tab's does not.
    pub(crate) agents: AgentRegistry,
    /// Live wakeups (cron / after), posted into this agent's own inbox; the
    /// builtins that arm them gate on `allow_schedule`.
    pub(crate) schedules: crate::fleet::schedule::ScheduleRegistry,
    /// Input tokens and the event-log position at which that measurement
    /// landed — the numerator for the compaction trigger and pressure nudge.
    last_input: (u64, usize),
    /// Pins flow straight past the session to the frontend; the periodic
    /// [`nudge`] reminder reads this mirror to name what the model has pinned.
    pins: shell_eval::PinDigests,
    /// The ral-call clock, bumped at the top of every [`Self::run_shell`] — a
    /// failed eval is still a call.  The settled-worker sweep, the
    /// binding-lease ledger, and [`Self::check_disk_warn`] all read it.  Never
    /// rewound, not even by `/clear`.
    ral_epoch: u64,
    /// The operator's ceiling, shared verbatim by every fork — a host setting.
    /// `None` means [`Self::check_disk_warn`] never walks the dirs at all.
    disk_warn_bytes: Option<u64>,
    /// The [`Self::ral_epoch`] at which the next disk walk falls due, so the
    /// walk amortizes over ral calls rather than over ready boundaries.
    disk_check_epoch: u64,
    /// Latched on crossing the ceiling, so the warning fires once per
    /// excursion rather than once per boundary.
    disk_warn_latched: bool,
    /// Latched on crossing the pressure soft line ahead of auto-compaction
    /// ([`digest::pressure_due`]), so the nudge fires once per excursion
    /// rather than on every clean completion.  Re-armed in `context_pressure`
    /// after the measure falls back under the line.
    context_warn_latched: bool,
    /// The IT-set network policy and its audit ledger — shared verbatim by
    /// every fork, like [`Self::disk_warn_bytes`].
    egress: crate::egress::Egress,
    /// The dial-side capability a wire trunk's `` agents `start `` reaches its
    /// helpers through; `None` on every identity trunk. Shared verbatim by
    /// every fork, so a wire child's own `agent` call dials through the same
    /// seam its parent did.
    dial: Option<Arc<dyn Dial>>,
}

impl Agent {
    pub(crate) fn measured_input(&self) -> Option<u64> {
        let (tokens, measured_at) = self.last_input;
        (!self.log.lock().token_measure_is_stale(measured_at)).then_some(tokens)
    }

    pub(crate) fn token_compaction_due(&self, window: u64) -> bool {
        self.measured_input()
            .is_some_and(|tokens| crate::agent::digest::compaction_due(tokens, window))
    }

    pub(crate) fn token_pressure(&self, window: u64) -> Option<String> {
        self.measured_input()
            .filter(|tokens| crate::agent::digest::pressure_due(*tokens, window))
            .map(|tokens| format!("{tokens} of {window} tokens"))
    }
}

/// The depth budget exarch's trunks start with.
///
/// At zero the desk refuses `` agents `start ``
/// ([`crate::fleet::desk::ExarchDesk::launch`]), so a runaway spawn chain
/// exhausts fuel instead of threads.  Fan-out is unbounded.
pub const SPAWN_FUEL: u32 = 3;

/// A shared, swappable handle to the active provider: live replacement across
/// the UI / attend thread boundary needs a cell, not an `Arc` the worker owns
/// privately.
///
/// A peer wraps a *snapshot* taken at spawn, so a later root `/model` never
/// disturbs a running child.
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

    /// Replace the active provider (a `/model` switch).  An in-flight
    /// deliberation finishes on the provider it started with.
    ///
    /// # Panics
    /// Panics if the provider-handle mutex is poisoned.
    pub fn swap(&self, provider: Arc<Provider>) {
        *self.0.lock().expect("provider handle poisoned") = provider;
    }
}

impl Agent {
    /// Registered by the spawn site, so a `/model` on the child's tab swaps
    /// the child alone.
    pub(crate) fn provider_handle(&self) -> ProviderHandle {
        self.provider.clone()
    }

    pub(crate) fn current_provider(&self) -> Arc<Provider> {
        self.provider.current()
    }

    /// Where this agent's own session log is written — `record.jsonl` and its
    /// siblings sit directly inside.
    pub fn log_dir(&self) -> std::path::PathBuf {
        self.log.lock().dir().to_path_buf()
    }

    pub(crate) fn is_resumed(&self) -> bool {
        self.resume_summary.is_some()
    }

    pub(crate) fn resume_summary(&self) -> Option<(u64, u64)> {
        self.resume_summary
    }

    /// For `schedule` to arm wakeups into its *own* inbox, and for the spawn
    /// site to capture as a child's upward result edge.
    pub(crate) fn mailbox(&self) -> Mailbox {
        self.inbox.mailbox()
    }

    /// So a frontend's emitters mint mailboxes onto the queue the attend loop
    /// is draining, rather than a second one.
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
    }

    /// The spawn site registers a peer's in the parent's [`Self::agents`].
    pub(crate) fn cancel_token(&self) -> &cancel::Token {
        &self.cancel
    }

    /// Every deliberation must hand the session back at a ready boundary.
    ///
    /// # Panics
    /// Panics if the log mutex is poisoned.
    pub fn is_ready(&self) -> bool {
        self.log.lock().is_ready()
    }

    /// The model-view messages the next request would carry.
    ///
    /// # Panics
    /// Panics if the log mutex is poisoned.
    pub fn rendered_messages(&self) -> Vec<genai::chat::ChatMessage> {
        self.log.lock().history_messages()
    }

    /// Serialised model-view byte count — the compaction-threshold input.
    ///
    /// # Panics
    /// Panics if the log mutex is poisoned.
    pub fn history_bytes(&self) -> usize {
        self.log.lock().history_bytes()
    }

    pub(crate) fn returns(&self) -> bool {
        self.returns
    }

    /// The *live* directory, probed so a prior `cd` shows — not the seat's own
    /// `cwd`, which only reseeds the shell on `/clear`.  [`Self::host_services`]
    /// wants the live one so a desk-spawned child starts where the model is.
    pub(crate) fn cwd(&self) -> std::path::PathBuf {
        match self.seat.transport().probe(FOValue::Variant {
            label: "cwd".into(),
            payload: None,
        }) {
            Ok(FOValue::String { value }) => std::path::PathBuf::from(value),
            other => unreachable!("`cwd probe always answers a String, got {other:?}"),
        }
    }

    /// Production spawns read the private field directly or narrow the desk's
    /// captured snapshot ([`crate::policy::narrow`]); this is test-only.
    #[cfg(test)]
    pub(crate) fn caps(&self) -> &ral_core::types::Capabilities {
        &self.caps
    }

    /// For a test polling an async spawn's settle without a full deliberation.
    #[cfg(test)]
    pub(crate) fn next_item_for_test(&self) -> Option<crate::bus::Item> {
        self.inbox.next_item()
    }
}

/// The message of a recovered panic payload, for either string shape.
pub(crate) fn panic_msg(p: &Box<dyn std::any::Any + Send>) -> String {
    p.downcast_ref::<String>()
        .cloned()
        .or_else(|| p.downcast_ref::<&'static str>().map(|s| (*s).into()))
        .unwrap_or_else(|| "non-string payload".into())
}
