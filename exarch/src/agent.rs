//! The uniform agent node: canonical event log, persistent shell, capability
//! set, hot-swappable provider, and the attend loop every node runs.
//!
//! A run is a tree of these; [`Fleet`](crate::fleet::Fleet) holds what they
//! share.
//!
//! No node is privileged by special-case code — the distinctions reduce to
//! *position*.  Holding `reply` falls out of `returns`, parking out of
//! `interactive`.  A child's result is posted up its parent's mailbox by the
//! spawn site, not by the loop, so the loop is identical for all.
//!
//! Two types, split along who may touch what.  [`Agent`] is the public half —
//! identity and immutable config — held behind an `Arc` so the fleet can share
//! it.  [`Avatar`] is the private half — the log, the seat, the inbox, and
//! every other field only the attend thread touches — plus the `Arc<Agent>`
//! it embodies; every method that runs the agent takes `&mut Avatar`.
//!
//! An agent is *live* while its avatar holds the `Arc`; nothing deregisters.
//! The tree that carries that liveness has one direction of strength, and it
//! is the invariant everything else here rests on: **nothing holds an
//! `Arc<Agent>` to a descendant.**  Up is strong ([`Agent::parent`]), so the
//! climb a scope check makes never dangles; down is weak
//! ([`Agent::children`]), so a walk prunes what fails to upgrade, and reaper
//! closures and results addressed upward carry [`std::sync::Weak`].
//!
//! Two per-agent mutexes, [`Agent::status`] and [`Agent::children`].  Rule:
//! hold at most one at a time.  `children` is locked to push or to snapshot,
//! never across a walk; `status` is a single-writer register, and nothing is
//! computed under it.  [`Agent::schedules`] and [`Agent::pins`] are two more
//! cells of public agent state, each already self-locked by its own type;
//! the same rule extends to them, and neither is ever held while pushing
//! into an inbox — a fired schedule drops its registry lock before posting
//! the wakeup.
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
#[cfg(test)]
pub(crate) use build::TestTrunk;
pub(crate) use build::{Build, fresh_id};
pub use build::{RecordedAccount, RootConfig, RootSeat};
pub use dial::Dial;
pub(crate) use probe::ProbedWorker;
pub(crate) use shell::{LogCell, ReplyCell};

use crate::agent::cancel::EvalReach;
use crate::agent::seat::Seat;
use crate::bus::{
    AgentId, AgentMessage, AgentOutcome, AgentResult, Inbox, InboxReject, Mailbox, Post,
};
use crate::fleet::Fleet;
use crate::provider::Provider;
use crate::shell_eval;
use crate::sync::LockExt;
use ral_core::process::CancelCause;
use ral_core::serial::FOValue;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex, Weak};
use std::time::{Duration, Instant};

/// What the fleet knows an agent as.
///
/// Identity and immutable config, fixed at construction — except
/// [`Self::status`], the one register the avatar writes as it runs, and
/// [`Self::children`], which the spawn site pushes onto.  Live exactly while
/// its [`Avatar`] holds the `Arc`: its parent and the fleet's [`Fleet`] hold
/// only [`Weak`], and every walk prunes what fails to upgrade.
#[allow(
    clippy::struct_excessive_bools,
    reason = "each bool gates an independent, orthogonal axis (interactive, returns, allow_schedule, tool_enabled, search); not a candidate for a combined enum"
)]
pub struct Agent {
    pub id: AgentId,
    /// The tab-bar identity [`crate::fleet::check_name`] validates — unique
    /// among live agents, enforced at [`Fleet::enrol`].
    name: String,
    /// Where this agent's own session log is written.
    log_dir: PathBuf,
    started: Instant,
    /// The resolved prompt that reaches the model on every step.  `Arc<str>`
    /// so a fork's per-step read and the desk's own copy are refcount bumps,
    /// never a re-copy of the ~38 KB template.
    pub(crate) system: Arc<str>,
    /// The template [`Self::system`] came from, still carrying
    /// [`crate::prompt::BUILTIN_INDEX_PLACEHOLDER`]: a child inherits *this*,
    /// so it resolves for its own grants rather than its parent's.
    system_base: Arc<str>,
    /// The fleet-shared builtin index, resolved once at the trunk, so a fork
    /// resolving its own prompt never needs a live [`Shell`](ral_core::Shell).
    index: Arc<crate::prompt::BuiltinIndex>,
    caps: ral_core::types::Capabilities,
    /// Strong and upward: `None` ⇔ this agent is a root — the trunk, or a
    /// `/branch` child, which converses and reports to nobody.  A parent
    /// whose avatar has gone is still reachable here, terminated token and
    /// all, so `deposit_reply` and the scope climb never dangle.
    parent: Option<Arc<Self>>,
    /// Weak and downward, so no cycle exists to reason about: a child that
    /// fails to upgrade has settled, and the walk that found it prunes it.
    children: Mutex<Vec<Weak<Self>>>,
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
    /// One sticky token for this agent's life, so the subtree cascade reaches
    /// the live exchange.  The attend loop
    /// [`reset`](cancel::Token::reset)s it at each exchange boundary so an Esc
    /// never bleeds into the next; the process's trunk is additionally
    /// [`publish`](cancel::publish)ed for OS signals by the site that launches
    /// it.
    cancel: cancel::Token,
    /// `provider.complete` (advertisement) and [`Avatar::invoke`] (dispatch)
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
    /// schedule harness test can grant it on a [`Avatar::for_test`] trunk,
    /// which hardcodes it `false`.
    pub(crate) allow_schedule: bool,
    /// The operator's ceiling, shared verbatim by every fork — a host setting.
    /// `None` means [`Avatar::check_disk_warn`] never walks the dirs at all.
    disk_warn_bytes: Option<u64>,
    /// The IT-set network policy and its audit ledger — shared verbatim by
    /// every fork, like [`Self::disk_warn_bytes`].
    egress: crate::egress::Egress,
    /// The dial-side capability a wire trunk's `` agents `start `` reaches its
    /// helpers through; `None` on every identity trunk. Shared verbatim by
    /// every fork, so a wire child's own `agent` call dials through the same
    /// seam its parent did.
    dial: Option<Arc<dyn Dial>>,
    /// The seat's own reach into this agent's running eval, fixed at
    /// construction — a self-registering root states it pre-weakened
    /// ([`EvalReach::interrupt_only`]) precisely because its seat rebuilds in
    /// place on `/clear`, so nothing here ever goes stale enough to need
    /// updating.
    reach: EvalReach,
    /// The sender end of this agent's own inbox; the [`Inbox`] itself stays on
    /// [`Avatar`], reachable only by the attend thread.
    mailbox: Mailbox,
    /// Live wakeups (cron / after), posted into this agent's own inbox; the
    /// builtins that arm them gate on `allow_schedule`. Public agent state:
    /// the desk writes it and other threads read it, so it lives here rather
    /// than on [`Avatar`].
    pub(crate) schedules: crate::fleet::schedule::ScheduleRegistry,
    /// Pins flow straight past the session to the frontend; the periodic
    /// [`nudge`] reminder reads this mirror to name what the model has
    /// pinned. Public agent state, for the same reason as [`Self::schedules`].
    pub(crate) pins: shell_eval::PinDigests,
    /// A process publishing its status: written by the avatar alone
    /// ([`Self::set_resting`], [`Self::deposit_reply`], [`Self::clear_subtree`]'s
    /// generation bump), read by everyone else.  A reader takes one snapshot
    /// and computes nothing under the lock — the mutex buys atomicity, nothing
    /// more.
    status: Mutex<Status>,
    /// The parent's own generation at this agent's birth: the fence a result
    /// addressed upward must survive, since the parent is who reads it.  0 for
    /// a root, which reports to nobody.
    consumer: u64,
}

/// [`Agent`]'s single-writer register — see [`Agent::status`]'s doc for who
/// writes what.
struct Status {
    /// How many times this session has rebuilt its context
    /// ([`Agent::clear_subtree`]).  Work bound for this session is stamped
    /// with the value current at its birth, so a `/clear` here invalidates it
    /// — and a `/clear` anywhere else leaves it alone.
    generation: u64,
    /// When this agent parked waiting for a message, or `None` while it is
    /// working.  The roster's `idle-s`, and the whole of "not busy".
    rest: Option<Instant>,
    /// The value this agent last passed to `reply`, held here for its
    /// parent's `` agents `read `` to fetch.  Kept apart from [`Self::rest`]
    /// because it must survive a wake: a messaged agent is busy again with
    /// its reply still standing, until it replies afresh.
    reply: Option<FOValue>,
}

/// An agent's embodiment in this process: the thread that thinks and acts
/// for it, owning everything only it touches, so every method is plain
/// `&mut self`.
pub struct Avatar {
    /// The public half this avatar embodies — readable crate-wide, so a
    /// caller outside the `agent` module reaches identity and config at
    /// `.agent.…` rather than through a delegator for every field.
    pub(crate) agent: Arc<Agent>,
    /// Under its own lock so a per-call desk can capture it off `&mut Avatar`.
    /// [`LogCell::lock`] panics on contention rather than blocking — the desk
    /// runs only while the attend thread is parked in `run_shell`.
    log: LogCell,
    _run_lock: Option<crate::bootstrap::RunLock>,
    resume_summary: Option<(u64, u64)>,
    /// Every engine-side reach goes through this seat's methods.
    pub(crate) seat: Seat,
    /// Self-nudges and armed wakeups land here; a child's result lands in its
    /// *parent's*, never reaching across into a sibling's.
    inbox: Inbox,
    /// The repair budget resets on a genuine exchange-boundary item, never on
    /// a self-nudge, so a nudge sequence runs to completion within one
    /// exchange; the whole state is reborn with the context on `/clear` and
    /// `/rewind`.  `None` for a toolless (`--chat`) trunk: every nudge steers
    /// an agent toward a tool it does not hold, so such a turn is only ever
    /// reported.
    nudges: Option<nudge::Nudges>,
    /// The staged return value, harvested by [`Self::run_shell`] from that
    /// call's [`ReplyCell`] as the desk retires, then lifted into a
    /// [`deliberate::Outcome::Replied`] only once the tool-call batch drains,
    /// so the session settles with every `call_id` answered.
    /// [`Self::deliberate`] clears it on entry, so a cancelled or panicking
    /// deliberation cannot poison the next.
    reply: Option<FOValue>,
    /// The fleet itself, `Arc`-shared with every other node: the name a
    /// spawn claims and the by-id door a frontend command arrives through.
    /// Not the tree — that is [`Agent::parent`] and [`Agent::children`].
    pub(crate) fleet: Arc<Fleet>,
    /// Input tokens and the event-log position at which that measurement
    /// landed — the numerator for the compaction trigger and pressure nudge.
    last_input: (u64, usize),
    /// The ral-call clock, bumped at the top of every [`Self::run_shell`] — a
    /// failed eval is still a call.  The settled-worker sweep, the
    /// binding-lease ledger, and [`Self::check_disk_warn`] all read it.  Never
    /// rewound, not even by `/clear`.
    ral_epoch: u64,
    /// The [`Self::ral_epoch`] at which the next disk walk falls due, so the
    /// walk amortizes over ral calls rather than over ready boundaries.
    disk_check_epoch: u64,
    /// Latched on crossing the ceiling, so the warning fires once per
    /// excursion rather than once per boundary.
    disk_warn_latched: bool,
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

    pub(crate) fn cancel_token(&self) -> &cancel::Token {
        &self.cancel
    }

    pub(crate) fn returns(&self) -> bool {
        self.returns
    }

    pub(crate) fn caps(&self) -> &ral_core::types::Capabilities {
        &self.caps
    }

    pub(crate) fn fuel(&self) -> u32 {
        self.fuel
    }

    pub(crate) fn search(&self) -> bool {
        self.search
    }

    pub(crate) fn interactive(&self) -> bool {
        self.interactive
    }

    pub(crate) fn disk_warn_bytes(&self) -> Option<u64> {
        self.disk_warn_bytes
    }

    pub(crate) fn egress(&self) -> &crate::egress::Egress {
        &self.egress
    }

    pub(crate) fn dial(&self) -> Option<&Arc<dyn Dial>> {
        self.dial.as_ref()
    }

    pub(crate) fn system_base(&self) -> &Arc<str> {
        &self.system_base
    }

    pub(crate) fn index(&self) -> &Arc<crate::prompt::BuiltinIndex> {
        &self.index
    }

    pub(crate) fn name(&self) -> &str {
        &self.name
    }

    pub(crate) fn log_dir(&self) -> &Path {
        &self.log_dir
    }

    pub(crate) fn elapsed(&self) -> Duration {
        self.started.elapsed()
    }

    /// The sender end of this agent's own inbox.
    pub(crate) fn mailbox(&self) -> &Mailbox {
        &self.mailbox
    }

    /// The parent's own generation at this agent's birth — the fence a result
    /// addressed upward must survive.
    pub(crate) fn consumer(&self) -> u64 {
        self.consumer
    }

    /// Whom this agent reports to, `None` for a root.
    pub(crate) fn parent(&self) -> Option<&Arc<Self>> {
        self.parent.as_ref()
    }

    /// The same edge as an id, for the birth notice the frontend draws a tab
    /// from.
    pub(crate) fn parent_id(&self) -> Option<AgentId> {
        self.parent.as_ref().map(|p| p.id)
    }

    pub(crate) fn generation(&self) -> u64 {
        self.status.lock_ignore_poison().generation
    }

    pub(crate) fn rest(&self) -> Option<Instant> {
        self.status.lock_ignore_poison().rest
    }

    pub(crate) fn reply(&self) -> Option<FOValue> {
        self.status.lock_ignore_poison().reply.clone()
    }

    pub(crate) fn has_reply(&self) -> bool {
        self.status.lock_ignore_poison().reply.is_some()
    }

    /// Mark this agent parked for a message, or working again.  Setting it
    /// twice running keeps the first stamp, so the roster's `idle-s` measures
    /// from when the agent parked and not from the attend loop's latest pass.
    pub(crate) fn set_resting(&self, resting: bool) {
        let mut status = self.status.lock_ignore_poison();
        if resting {
            status.rest.get_or_insert_with(Instant::now);
        } else {
            status.rest = None;
        }
    }

    /// Stage `reply` for this agent's parent to fetch, and wake it with the
    /// one-line [`AgentOutcome::Replied`] notice — deposit first, so a parent
    /// woken by the notice always finds the value already there.  A root has
    /// nobody to notify: its reply goes to whoever drives its loop.
    ///
    /// # Errors
    /// The parent's inbox was at quota, so the notice was refused; the deposit
    /// stands either way and `` agents `read `` would still find it.
    pub(crate) fn deposit_reply(&self, reply: FOValue) -> Result<(), InboxReject> {
        self.status.lock_ignore_poison().reply = Some(reply);
        let Some(parent) = &self.parent else {
            return Ok(());
        };
        // After the status guard drops: the process-wide lock order is
        // inbox → agent.
        parent.mailbox.push(Post::AgentResult(AgentResult {
            name: self.name.clone(),
            outcome: AgentOutcome::Replied,
            elapsed: self.elapsed(),
            generation: self.consumer,
        }))
    }

    /// Send a marked model-visible note to `to`, stamping its exchange clock
    /// as a steer does, and for the same reason: a message is an exchange, and
    /// one that woke a resting child must not leave it to be reaped
    /// mid-answer.  Scoping is the caller's — [`Self::descendant`].
    ///
    /// # Errors
    /// The recipient's inbox is at quota.
    pub(crate) fn message(&self, to: &Self, text: String) -> Result<(), InboxReject> {
        to.mailbox.stamp_exchange();
        to.mailbox.push(Post::AgentMessage(AgentMessage {
            from: self.id,
            from_name: self.name.clone(),
            text,
        }))
    }

    /// Whether this agent may still be talked to: something has already
    /// spoken to it, or it holds a reply someone may want to follow up on.
    pub(crate) fn messageable(&self) -> bool {
        self.mailbox.last_exchange().is_some() || self.has_reply()
    }

    /// Time since this agent's last human exchange, or since birth if never
    /// engaged.
    pub(crate) fn idle(&self) -> Duration {
        self.mailbox
            .last_exchange()
            .unwrap_or(self.started)
            .elapsed()
    }

    /// Cancel this agent across both terminate-class layers: the cooperative
    /// [`cancel::Token`] the attend loop polls and its eval-layer reach — a
    /// no-op on the eval side for an agent whose reach is interrupt-only.
    pub(crate) fn cancel(&self, cause: CancelCause) {
        self.cancel.cancel(cause);
        self.reach.terminate(cause);
    }

    /// Unwind this agent's in-flight run without ending it: the Esc/Ctrl-C
    /// path, and the `` agents `cancel `` scoped verb's per-target primitive.
    pub(crate) fn interrupt(&self) {
        self.cancel.cancel(CancelCause::Interrupt);
        self.reach.interrupt();
    }

    /// Take `child` under this agent — the one downward edge, weak, written
    /// once at the child's construction.
    pub(crate) fn adopt(&self, child: &Arc<Self>) {
        self.children
            .lock_ignore_poison()
            .push(Arc::downgrade(child));
    }

    /// This agent's live direct children, pruning the settled ones as it
    /// snapshots.  The lock is held for the snapshot alone: every walk runs
    /// outside it.
    pub(crate) fn children(&self) -> Vec<Arc<Self>> {
        let mut kids = self.children.lock_ignore_poison();
        let live: Vec<Arc<Self>> = kids.iter().filter_map(Weak::upgrade).collect();
        if live.len() != kids.len() {
            *kids = live.iter().map(Arc::downgrade).collect();
        }
        live
    }

    /// This agent's live proper descendants at any depth — the pruning
    /// descent.  An agent walks what it spawned, never itself.
    pub(crate) fn walk(&self) -> Vec<Arc<Self>> {
        let mut out = Vec::new();
        let mut frontier = self.children();
        while let Some(node) = frontier.pop() {
            frontier.extend(node.children());
            out.push(node);
        }
        out
    }

    /// `target` if it is a proper descendant of this agent, else `None` — the
    /// scoping `` agents `cancel ``/`` `message ``/`` `read `` share.  A climb
    /// from `target`'s *parent*, so it costs O(depth) and takes no lock, and
    /// so nothing is a proper descendant of itself.
    pub(crate) fn descendant(&self, target: &Arc<Self>) -> Option<Arc<Self>> {
        let mut above = target.parent.as_ref();
        while let Some(node) = above {
            if std::ptr::eq(Arc::as_ptr(node), std::ptr::from_ref(self)) {
                return Some(target.clone());
            }
            above = node.parent.as_ref();
        }
        None
    }

    /// The park signal for a node that launched async agents: a direct child
    /// still *working* is what may yet deliver to this mailbox.  A resting one
    /// has already said what it had to say — it holds its reply and waits to
    /// be messaged — so it must not hold its parent open for the hour its
    /// lease allows.
    pub(crate) fn has_busy_children(&self) -> bool {
        self.children().iter().any(|c| c.rest().is_none())
    }

    /// Cancel this agent and its whole subtree.  Every victim is stamped
    /// across both layers, so an in-flight eval unwinds instead of grinding on
    /// as an orphan whose result nobody will collect.
    pub(crate) fn cancel_tree(&self, cause: CancelCause) {
        self.cancel(cause);
        self.cancel_descendants(cause);
    }

    /// Cancel this agent's proper descendants, leaving it live — a settling
    /// parent abandoning its children, and `/clear`.  Both rebuild in place,
    /// so this must never stamp the root's own [`cancel::Token`]: a terminate
    /// cause there is permanent ([`cancel::Token::reset`] clears only a bare
    /// [`CancelCause::Interrupt`]) and every later run would fail.
    pub(crate) fn cancel_descendants(&self, cause: CancelCause) {
        for node in self.walk() {
            node.cancel(cause);
        }
    }

    /// `/clear`: abandon the subtree the rebuilt context no longer owns, and
    /// bump *this* agent's own generation so any late result addressed to it
    /// is rejected.  Bumping only this agent is what keeps one tab's `/clear`
    /// from dropping work another tab is still waiting on.
    pub(crate) fn clear_subtree(&self) {
        self.cancel_descendants(CancelCause::Explicit);
        self.status.lock_ignore_poison().generation += 1;
    }
}

impl Avatar {
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

    /// This agent's wire identity — the public half's `id`, reachable without
    /// the `agent` field's crate-only visibility.
    pub fn id(&self) -> AgentId {
        self.agent.id
    }

    /// Production reads `self.agent.provider_handle()` directly; this is
    /// test-only convenience for a driven `Avatar` a test does not otherwise
    /// have a handle into.
    #[cfg(test)]
    pub(crate) fn provider_handle(&self) -> ProviderHandle {
        self.agent.provider_handle()
    }

    pub(crate) fn current_provider(&self) -> Arc<Provider> {
        self.agent.current_provider()
    }

    pub(crate) fn cancel_token(&self) -> &cancel::Token {
        self.agent.cancel_token()
    }

    pub(crate) fn returns(&self) -> bool {
        self.agent.returns()
    }

    #[cfg(test)]
    pub(crate) fn caps(&self) -> &ral_core::types::Capabilities {
        self.agent.caps()
    }

    /// Where this agent's own session log is written — `record.jsonl` and its
    /// siblings sit directly inside.
    pub fn log_dir(&self) -> std::path::PathBuf {
        self.agent.log_dir().to_path_buf()
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
        self.agent.mailbox().clone()
    }

    /// So a frontend's emitters mint mailboxes onto the queue the attend loop
    /// is draining, rather than a second one.
    pub(crate) fn inbox(&self) -> Inbox {
        self.inbox.clone()
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
        self.log
            .lock()
            .history_transcript()
            .messages()
            .cloned()
            .collect()
    }

    /// Serialised model-view byte count — the compaction-threshold input.
    ///
    /// # Panics
    /// Panics if the log mutex is poisoned.
    pub fn history_bytes(&self) -> usize {
        self.log.lock().history_bytes()
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
