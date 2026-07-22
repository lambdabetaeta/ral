//! The ephemeral, fleet-owned registry of live agent nodes.
//!
//! Every agent — the trunk and each forked child alike — registers itself
//! here, so "all live agents" is literally this registry's contents and the
//! fleet is alive exactly while it is non-empty.  Each entry carries the
//! agent's `parent`, so the registry is the spawn *tree*, not a flat set.
//!
//! The registry owns four things the rest of the fleet leans on:
//!
//!   * a **listing** so an orchestrator can recover the ids of the agents it
//!     started after context compaction (`agents`) and stop stragglers
//!     (`agent-cancel`);
//!   * the **subtree cascade**: [`AgentRegistry::cancel`] cancels an agent
//!     *and every descendant*, behind `agent-cancel` and the per-agent
//!     idle lease; `/clear` and `/close` reap a subtree through `clear_subtree`
//!     and `remove_subtree`, and `terminate_entry` is the per-entry primitive
//!     all of them share.  Each
//!     node is cancelled across both layers — the cooperative [`Token`] its
//!     drive loop polls, and its own session's durable root, so an
//!     in-flight `ral` eval unwinds at ral's poll points instead of
//!     grinding to its timeout wall.  Cancelling that root is already the
//!     whole story for the node's own detached `ral` workers, too: a
//!     worker's cancel scope is a child of its shell's durable root
//!     (`decisions/260705_session-ledger`'s one cascade law, "cancelling a
//!     resident cancels what it owns"), and every `CancelScope::is_cancelled`
//!     walks its ancestors, so the cascade reaches them with no extra edge
//!     of its own. A node that is never cancelled — the ordinary `reply`/
//!     settle path, or the trunk's own end-of-`drive` `deregister` — takes
//!     a different route to the same law: [`crate::agent::Agent`]'s `Drop`
//!     cancels its own workers and clears its own schedules unconditionally,
//!     so a settled agent leaks neither. `/clear` is the third route,
//!     explicit and immediate on the outgoing shell (`Agent::clear`), since
//!     it rebuilds the agent in place rather than ending it;
//!   * a **per-turn interrupt**, distinct from the terminate cascade above:
//!     [`AgentRegistry::interrupt`] unwinds exactly one entry's in-flight
//!     turn through [`interrupt_entry`], which cancels the [`Token`] *and*
//!     interrupts through the entry's [`EvalReach`] — which cancels whatever
//!     [`ForegroundScope`](ral_core::process::ForegroundScope) the
//!     [`TurnScope`] cell currently holds, the scope `exarch`'s own
//!     transport captures fresh at the start of every turn. Cancelling that
//!     scope reaches an in-flight eval exactly as terminating would, but
//!     the *next* turn mints an entirely new scope from the untouched
//!     durable root, so an interrupt never poisons a later turn.  The
//!     terminate layer stays [`EvalReach::terminate`]'s alone;
//!   * an **idle lease** (children only), armed on the shared
//!     `process::reaper` as a self-re-arming `Run` deadline against the
//!     agent's exchange clock, so an abandoned child — never engaged, or
//!     engaged and then dropped — is reaped rather than running forever;
//!     the trunk, never abandoned, carries none;
//!   * a **generation**, bumped on `/clear`, so a worker that settles into a
//!     rebuilt context has its result rejected instead of delivered. This is
//!     the one counter every late-settle path in the fleet consults, not a
//!     private counter of this registry's: an async agent's result admits
//!     against it (`Agent::admits`), a detached worker's deferred surface
//!     batch admits against it (`InboxDeferred`, `shell_eval.rs`), and a
//!     schedule takes the stronger route of unconditional removal
//!     (`ScheduleRegistry::clear`, ordered before the inbox sweep) rather
//!     than carrying the generation at all — nothing settles across a
//!     `/clear`, regardless of which edge a chapter uses to say so.
//!
//! It is *not* the durable-job registry of long-running-work: entries are
//! ephemeral, live only while their agent runs, and a child's result is
//! pushed to its parent's inbox rather than collected by id.

use crate::agent::ProviderHandle;
use crate::agent::cancel::Token;
use crate::bus::{AgentId, AgentMessage, InboxMsg, InboxReject, Mailbox};
use ral_core::process::{self, CancelCause, DurableRoot, ForegroundScope};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// A live agent's current-turn foreground scope, refreshed by its own
/// `IdentityTransport` at the start of every turn
/// ([`ral_core::transport::IdentityTransport::observe_foreground`]).  Lives
/// outside the registry's own lock, so [`AgentRegistry::interrupt`] can
/// cancel whatever the *in-flight* turn installed without contending with a
/// dispatch that may run for the turn's whole duration.  `None` until the
/// agent's first turn runs.
pub(crate) type TurnScope = Arc<Mutex<Option<ForegroundScope>>>;

/// How the registry reaches one agent's running eval, per seat kind — the
/// eval-layer half of every cancel path, beside the cooperative [`Token`]
/// each entry also carries.  Two distinct motions, both seat-shaped:
/// [`Self::terminate`] ends the agent's eval for good, [`Self::interrupt`]
/// unwinds exactly the in-flight turn and leaves every later turn clean.
pub(crate) enum EvalReach {
    /// The in-process reach: the durable root of the agent's own `Shell`
    /// (the terminate cascade's handle — cancelling it also reaches the
    /// agent's detached workers through the cancel-scope ancestor chain),
    /// and the turn-scope cell its transport refreshes each turn (the
    /// interrupt's handle — cancelling the held scope unwinds the in-flight
    /// turn alone, since the next turn mints a fresh scope from the
    /// untouched root).
    Identity {
        eval_root: DurableRoot,
        turn_scope: TurnScope,
    },
    /// The wire reach: a wire session's only host-reachable cancel primitive
    /// is `Control::Cancel` on its own in-flight dispatch — there is no
    /// host-side turn scope or durable root to reach separately, so both
    /// [`Self::interrupt`] and [`Self::terminate`] resolve to the same
    /// [`ControlSender::cancel_in_flight`]. A wire-seated agent never
    /// actually has a child today (a wire session's `agent-start` is always
    /// refused, `docs/ral-wiki/decisions/260722_session-is-a-process.md`),
    /// so this variant is unreached in production but must still answer both
    /// calls honestly.
    #[cfg(unix)]
    Wire(ral_core::transport::ControlSender),
}

impl EvalReach {
    /// Unwind the in-flight turn without ending the agent: cancel whatever
    /// [`ForegroundScope`] the turn-scope cell currently holds.  Never
    /// touches the durable root — the next turn is born uncancelled.
    pub(crate) fn interrupt(&self) {
        match self {
            Self::Identity { turn_scope, .. } => {
                if let Some(scope) = turn_scope
                    .lock()
                    .unwrap_or_else(std::sync::PoisonError::into_inner)
                    .as_ref()
                {
                    scope.cancel(CancelCause::Interrupt);
                }
            }
            #[cfg(unix)]
            Self::Wire(ctrl) => ctrl.cancel_in_flight(),
        }
    }

    /// End the agent's eval: cancel its session's durable root, so an
    /// in-flight `ral` eval unwinds at the evaluator's poll points rather
    /// than running to its timeout wall.  Permanently poisons the root by
    /// design — every caller is ending the agent, so there is no later turn
    /// for a poisoned root to break.
    pub(crate) fn terminate(&self, cause: CancelCause) {
        match self {
            Self::Identity { eval_root, .. } => eval_root.cancel(cause),
            #[cfg(unix)]
            Self::Wire(ctrl) => ctrl.cancel_in_flight(),
        }
    }
}

/// A child's idle lease: the wall-clock span since its last human exchange
/// (birth is the epoch) before its whole subtree is reaped.
///
/// ([[concurrency-primitives-detached-vs-structured]]). A human message
/// delivered to the agent renews the clock; nothing else does. A lease that
/// is never renewed therefore fires at exactly this duration from birth —
/// the fixed reap time is this lease's zero-renewal case, not a second
/// bound.
pub const AGENT_LEASE_IDLE: Duration = Duration::from_hours(1);

/// How long a leased child may sit idle-and-parked before the frontend
/// demotes its tab out of the TAB cycle and into the matrix strip.
///
/// Read by the frontend only — the registry owns the exchange clock this
/// threshold measures against, but takes no action of its own at this mark.
pub const AGENT_DEMOTE_IDLE: Duration = Duration::from_mins(5);

/// One live agent, by id, for the `agents` listing.
pub struct AgentInfo {
    pub id: AgentId,
    pub name: String,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
}

/// An agent's registration record, threaded once at birth into
/// [`AgentRegistry::register`].
pub struct Registration {
    pub id: AgentId,
    /// The agent's parent, or `None` for the trunk.
    pub parent: Option<AgentId>,
    /// The idle lease to arm over this agent's whole subtree, or `None` to
    /// arm none — see [`AgentRegistry::register`]. Duration-typed, rather
    /// than a bare flag, so a test can arm a millisecond lease instead of
    /// waiting out the real constant.
    pub lease: Option<Duration>,
    pub name: String,
    pub log_dir: PathBuf,
    pub cancel: Token,
    /// The eval-layer cancel reach for this agent's seat, or `None` for the
    /// trunk, whose session outlives any cancel — an Esc cancels its
    /// *turn*, delivered through the published foreground slot, never its
    /// eval layer.
    pub(crate) reach: Option<EvalReach>,
    pub mailbox: Mailbox,
    pub provider: ProviderHandle,
}

/// The fleet's live agents.
///
/// Cheap to clone — the inner `Arc` makes a clone
/// share the same map, so the trunk, every forked child, and the frontend all
/// hold one registry, and a detached worker holds a handle it can settle
/// through after its turn has ended.
#[derive(Clone)]
pub struct AgentRegistry {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Bumped by [`AgentRegistry::clear_subtree`]; a worker that settles under
    /// an older generation is rejected.
    generation: u64,
    entries: HashMap<AgentId, Entry>,
}

struct Entry {
    /// The agent's parent, or `None` for the trunk.  The edge that makes the
    /// registry a tree and drives the subtree cascade.
    parent: Option<AgentId>,
    name: String,
    log_dir: PathBuf,
    started: Instant,
    cancel: Token,
    /// The eval-layer cancel reach ([`EvalReach`]).  A tripped [`Token`]
    /// alone cannot stop an in-flight `ral` tool call (the drive loop reads
    /// it only between steps, and `run_shell` blocks until the engine
    /// reports), so the cascade also [`terminate`](EvalReach::terminate)s
    /// through this reach, and a per-tab interrupt
    /// [`interrupt`](EvalReach::interrupt)s through it.  `None` for the
    /// trunk, whose session outlives any cancel — an Esc cancels its
    /// *turn*, delivered through the published foreground slot, never its
    /// eval layer.
    reach: Option<EvalReach>,
    /// The agent's inbox sender.  A human exchange delivers through
    /// [`AgentRegistry::steer`], the registry's one steering door; the
    /// `message` tool delivers a marked peer note by id through
    /// [`AgentRegistry::message`].
    mailbox: Mailbox,
    /// The agent's hot-swappable provider handle, so a `/model` on the focused
    /// agent swaps *its* provider without disturbing any other.
    provider: ProviderHandle,
    /// The idle lease armed over this entry's subtree, or `None` if it
    /// carries none.  [`register`](AgentRegistry::register) arms the first
    /// fire from this; the fire itself re-reads it by id through
    /// [`AgentRegistry::lease_state`] rather than closing over a captured
    /// copy, so a settled/removed entry ends the chain simply by not being
    /// found.
    lease: Option<Duration>,
    /// The instant of this agent's last human exchange, or `None` before
    /// the first one — `started` is the epoch until then.  The only writer
    /// is [`AgentRegistry::renew`].
    last_exchange: Option<Instant>,
}

impl Default for AgentRegistry {
    fn default() -> Self {
        Self::new()
    }
}

impl AgentRegistry {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Mutex::new(Inner {
                generation: 0,
                entries: HashMap::new(),
            })),
        }
    }

    /// The current generation — captured by a worker at birth so a result
    /// that arrives after a `/clear` can be rejected.
    pub fn generation(&self) -> u64 {
        self.lock().generation
    }

    /// Whether any agent remains registered — the fleet's liveness.
    pub fn is_empty(&self) -> bool {
        self.lock().entries.is_empty()
    }

    /// Whether `parent` has any live direct child still registered.  The park
    /// signal for a headless root (or any node) that launched async agents and
    /// must stay alive to receive their results: a child delivers only to its
    /// own parent's inbox, so a live direct child is exactly what keeps this
    /// agent's mailbox from being permanently empty.  Cheap — a scan, no
    /// allocation, unlike [`Self::list`].
    pub fn has_children(&self, parent: AgentId) -> bool {
        self.lock()
            .entries
            .values()
            .any(|e| e.parent == Some(parent))
    }

    /// True while `id` is a live registered agent — the fleet still lists it.
    /// A conversing agent reads this at each park: once its entry is reaped
    /// (`/clear`, `/close`), it must quiesce rather than park Held as a
    /// zombie.
    pub fn is_live(&self, id: AgentId) -> bool {
        self.lock().entries.contains_key(&id)
    }

    /// Cancel and reap `root`'s proper descendants, leaving `root` live.
    /// A settling parent (`reply`) uses this to abandon live children without
    /// declaring a new context generation for unrelated agents; the `/clear`
    /// UI gesture uses it for the same reason on the trunk. `Agent::clear`
    /// rebuilds the trunk's context in place rather than ending it, so the
    /// reap must never reach the trunk's own entry: a terminate-class cause
    /// stamped on its sticky [`Token`] would be permanent ([`Token::reset`]
    /// only ever clears a bare [`CancelCause::Interrupt`]), which is exactly
    /// why the UI thread must route through here and never through
    /// [`Self::cancel`]'s inclusive cascade. Late results from the removed
    /// descendants are rejected because their entries are gone.
    pub fn cancel_descendants(&self, root: AgentId) {
        let mut g = self.lock();
        cancel_and_remove(&mut g, root, false);
    }

    /// Register an agent under [`Registration::parent`] (`None` for the
    /// trunk).  [`Registration::lease`] states the idle lease to arm over
    /// this agent's whole subtree, or `None` to arm none: a child no longer
    /// *implies* one — the caller says so.  A worker gets one (an abandoned
    /// detached worker must be reaped); a branch is a parented child
    /// *without* one (a conversation must not lose a turn at the hour mark);
    /// a root never had one.  A `parent`-set agent still carries its
    /// [`EvalReach`] so the cascade and an interrupt each reach its running
    /// eval.
    ///
    /// # Errors
    /// Returns [`RegisterError::SessionDead`] when `parent` is `Some` and is
    /// not, at this instant, a live and un-terminated entry — a spawn racing
    /// an `agent-cancel`/`/clear` on its own parent, closed by checking under
    /// the very lock the cascade holds for its whole walk, so the check can
    /// never observe a parent mid-teardown: either the cascade has not
    /// reached the lock yet (parent live, registration proceeds normally) or
    /// it already has (parent gone or terminated, registration refused) —
    /// there is no third state.
    ///
    /// Returns [`RegisterError::NameTaken`] when [`Registration::name`] is
    /// already borne by another *live* entry — any entry but `id`'s own
    /// current one, so a re-register under the same id (`register_self`'s
    /// "idempotent enough" re-register) always overwrites in place rather
    /// than colliding with the very entry it replaces — checked under the
    /// same lock, so two same-name spawns racing within one turn cannot both
    /// succeed: whichever reaches the lock first claims the name, and the
    /// second observes it already taken. This is the authoritative half of
    /// the spawn-uniqueness rule; `agent-start`'s own pre-check
    /// ([`crate::fleet::desk::ExarchDesk::launch`]) is the cheap, didactic
    /// half that refuses the ordinary case before ever forking a nursery
    /// session.
    ///
    /// Otherwise returns the birth generation the worker carries into its
    /// result.
    pub fn register(&self, reg: Registration) -> Result<u64, RegisterError> {
        let Registration {
            id,
            parent,
            lease,
            name,
            log_dir,
            cancel,
            reach,
            mailbox,
            provider,
        } = reg;
        if let Some(ttl) = lease {
            let reg = self.clone();
            process::arm_callback(ttl, move || lease_fire(&reg, id)).keep();
        }
        let mut g = self.lock();
        if let Some(p) = parent
            && g.entries.get(&p).is_none_or(|e| e.cancel.terminated())
        {
            drop(g);
            return Err(RegisterError::SessionDead);
        }
        // Excludes `id`'s own current entry: a re-register under the same id
        // (`register_self`'s "idempotent enough" re-register, `Agent::register_self`)
        // must overwrite in place, not be refused for "colliding" with the
        // very entry it is about to replace.
        if g.entries
            .iter()
            .any(|(&other, e)| other != id && e.name == name)
        {
            drop(g);
            return Err(RegisterError::NameTaken(name));
        }
        g.entries.insert(
            id,
            Entry {
                parent,
                name,
                log_dir,
                started: Instant::now(),
                cancel,
                reach,
                mailbox,
                provider,
                lease,
                last_exchange: None,
            },
        );
        Ok(g.generation)
    }

    /// Whether any live entry bears `name` — the cheap, didactic half of the
    /// spawn-uniqueness rule [`Self::register`] enforces authoritatively;
    /// `agent-start`'s own pre-check reads this before ever forking a
    /// nursery session, so the ordinary duplicate-name spawn refuses without
    /// the cost of a fork it would only have to unwind.
    pub fn name_live(&self, name: &str) -> bool {
        self.lock().entries.values().any(|e| e.name == name)
    }

    /// The live entry bearing `name`, if any. Names are unique among live
    /// entries by construction ([`Self::register`]), so at most one match
    /// ever exists. `agent-cancel` and `message` resolve their `name`
    /// argument to an [`AgentId`] through this before reaching the
    /// id-scoped primitives below.
    pub fn resolve_name(&self, name: &str) -> Option<AgentId> {
        let g = self.lock();
        g.entries
            .iter()
            .find(|(_, e)| e.name == name)
            .map(|(id, _)| *id)
    }

    /// The live agent's inbox sender, or `None` once it has settled/cleared.
    /// The frontend reads this to resolve whether a tab is steerable and
    /// whether it is waiting for input; [`Self::steer`] is the actual
    /// delivery door.  A missing entry means the tab is dead or lingering.
    pub fn mailbox(&self, id: AgentId) -> Option<Mailbox> {
        let g = self.lock();
        g.entries.get(&id).map(|e| e.mailbox.clone())
    }

    /// Deliver a human message to `id`: the registry's one steering door.
    /// Under one lock acquisition, stamps the exchange clock — renew before
    /// push, so the woken agent's own park verdict already reads `engaged`
    /// (verdict-before-pop, [`crate::bus::Inbox::next_or_idle`]) — and
    /// clones the mailbox out; the push itself runs after the guard drops,
    /// the same clone-mailbox-out/drop-guard/push shape [`Self::message`]
    /// follows for the process-wide inbox-before-registry lock order
    /// (`bus`'s module docs). `false` once the entry is gone, and the line
    /// is dropped rather than reviving anything.
    pub fn steer(&self, id: AgentId, text: String) -> bool {
        let mailbox = {
            let mut g = self.lock();
            let Some(e) = g.entries.get_mut(&id) else {
                return false;
            };
            renew_entry(e);
            let mailbox = e.mailbox.clone();
            drop(g);
            mailbox
        };
        mailbox.push_user(text);
        true
    }

    /// Renew `id`'s idle lease: stamp its exchange clock to now.  The
    /// registry's write itself lives in [`renew_entry`], shared with
    /// [`Self::steer`]; this entry point exists for a test to exercise the
    /// write directly, without going through a mailbox delivery. Neither
    /// focusing, nor the model-facing [`Self::message`], nor a `/resources`
    /// probe renews, so enumeration and attention alone can never
    /// immortalise a child. `false` once the entry is gone.
    pub fn renew(&self, id: AgentId) -> bool {
        self.lock().entries.get_mut(&id).map(renew_entry).is_some()
    }

    /// Whether a human has ever exchanged with `id` — `false` for an entry
    /// never renewed, or one already gone.
    pub fn engaged(&self, id: AgentId) -> bool {
        self.lock()
            .entries
            .get(&id)
            .is_some_and(|e| e.last_exchange.is_some())
    }

    /// Elapsed time since `id`'s last human exchange, or since birth if it
    /// was never engaged.  `None` once the entry is gone.
    pub fn idle(&self, id: AgentId) -> Option<Duration> {
        let g = self.lock();
        g.entries
            .get(&id)
            .map(|e| e.last_exchange.unwrap_or(e.started).elapsed())
    }

    /// The nearest time-to-reap across every live leased entry — `ttl -
    /// idle`, saturating at zero, minimised over the fleet.  `None` when no
    /// live entry carries a lease.  Read by `/resources`; a pure survey,
    /// like every other probe figure — it renews nothing.
    pub fn nearest_reap(&self) -> Option<Duration> {
        let g = self.lock();
        g.entries
            .values()
            .filter_map(|e| {
                let ttl = e.lease?;
                let idle = e.last_exchange.unwrap_or(e.started).elapsed();
                Some(ttl.saturating_sub(idle))
            })
            .min()
    }

    /// The lease chain's own accessor, read by [`lease_fire`]: for a live
    /// entry carrying a lease, `(idle, ttl)`; `None` once the entry is gone
    /// (settled, `/clear`'d, `/close`'d) or carries no lease — either way
    /// the chain has nothing left to arm.
    fn lease_state(&self, id: AgentId) -> Option<(Duration, Duration)> {
        self.lock().entries.get(&id).and_then(|e| {
            let ttl = e.lease?;
            Some((e.last_exchange.unwrap_or(e.started).elapsed(), ttl))
        })
    }

    /// Send a marked model-visible message from one live agent to another —
    /// only to a proper descendant of `from` (self excluded), the same
    /// contract [`Self::cancel_scoped`] enforces: an agent may reach the
    /// agents it started, never a sibling, an ancestor, or itself.  The
    /// mailbox is cloned under the registry lock, then posted after the
    /// lock drops; inbox delivery never runs while the registry is held —
    /// the process-wide lock order is inbox → registry (see `bus`'s
    /// module docs), because a park verdict reads this registry under the
    /// consumer's inbox mutex.
    ///
    /// # Errors
    /// Returns `Err` if `from` or `to` is not a live agent (`UnknownSender` /
    /// `UnknownRecipient`), if `to` is not a proper descendant of `from`
    /// (`NotADescendant`), or if the recipient's inbox is at quota
    /// (`RecipientInboxFull`).
    pub fn message(&self, from: AgentId, to: AgentId, text: String) -> Result<(), MessageError> {
        let (mailbox, from_name) = {
            let g = self.lock();
            let from_name = g
                .entries
                .get(&from)
                .ok_or(MessageError::UnknownSender(from))?
                .name
                .clone();
            let mailbox = g
                .entries
                .get(&to)
                .ok_or(MessageError::UnknownRecipient(to))?
                .mailbox
                .clone();
            if !is_descendant_of(&g.entries, from, to) {
                return Err(MessageError::NotADescendant(to));
            }
            drop(g);
            (mailbox, from_name)
        };
        mailbox
            .push(InboxMsg::AgentMessage(AgentMessage {
                from,
                from_name,
                text,
            }))
            .map_err(|reject| MessageError::RecipientInboxFull(to, reject))
    }

    /// The live agent's provider handle, for a `/model` swap on the focused
    /// agent.  `None` once it has settled.
    pub fn provider(&self, id: AgentId) -> Option<ProviderHandle> {
        let g = self.lock();
        g.entries.get(&id).map(|e| e.provider.clone())
    }

    /// Settle a worker born under `generation`: unconditionally reap its own
    /// entry, so its lease chain's next fire finds nothing and ends
    /// silently, whether or not `generation` still matches the live one. A
    /// worker's settle is authoritative over its
    /// *own* entry's lifetime — restructured so a stale-generation worker can
    /// never leave a zombie entry behind, rather than relying on `/clear`
    /// being trunk-only (and so bumping the generation in the same stroke
    /// that reaps every worker it could ever have made stale) to make that
    /// case unreachable in practice. Returns whether the result is admitted:
    /// `true` only when the entry existed *and* `generation` was current.
    pub fn settle(&self, id: AgentId, generation: u64) -> bool {
        let mut g = self.lock();
        let current = g.generation;
        let existed = g.entries.remove(&id).is_some();
        drop(g);
        existed && current == generation
    }

    /// Remove an agent from the registry unconditionally — the trunk's
    /// self-removal at the end of its drive (a child is removed by its spawn
    /// site through [`Self::settle`]).
    pub fn deregister(&self, id: AgentId) {
        self.lock().entries.remove(&id);
    }

    /// Cancel an agent **and its whole subtree** by id, unscoped; `true` if
    /// the agent existed. The per-agent lease routes here (with
    /// [`CancelCause::Deadline`]); `/clear` and `/close` reap through
    /// [`Self::clear_subtree`]/[`Self::remove_subtree`] instead. `pub(crate)`
    /// because it trusts its caller to already have decided `id` is a
    /// legitimate target — the model-facing `agent-cancel` tool goes through
    /// [`Self::cancel_scoped`], which checks that before ever reaching here.
    pub(crate) fn cancel(&self, id: AgentId) -> bool {
        self.cancel_cause(id, CancelCause::Explicit)
    }

    /// [`Self::cancel`]'s shared implementation, parameterised on the cause
    /// so an expired lease can report `Deadline` ("timed out") rather than
    /// `Explicit` ("cancelled") without a second cascade to maintain.
    fn cancel_cause(&self, id: AgentId, cause: CancelCause) -> bool {
        let g = self.lock();
        let existed = g.entries.contains_key(&id);
        for d in descendants(&g.entries, id, true) {
            if let Some(e) = g.entries.get(&d) {
                terminate_entry(e, cause);
            }
        }
        existed
    }

    /// `agent-cancel`'s model-facing entry point: cancel `target`'s whole
    /// subtree, but only if `target` is a proper descendant of `caller` — an
    /// agent may stop what it started, never a sibling, an ancestor, or
    /// itself. `Ok(false)` for an unknown `target` (the tool's existing
    /// no-op-report contract); `Err` only for a genuine scope violation.
    ///
    /// # Errors
    /// Returns `Err(NotADescendant(target))` if `target` is live but is not
    /// a proper descendant of `caller`.
    pub fn cancel_scoped(&self, caller: AgentId, target: AgentId) -> Result<bool, NotADescendant> {
        let live = {
            let g = self.lock();
            if !g.entries.contains_key(&target) {
                return Ok(false);
            }
            is_descendant_of(&g.entries, caller, target)
        };
        if !live {
            return Err(NotADescendant(target));
        }
        Ok(self.cancel(target))
    }

    /// A per-tab turn interrupt: unwind exactly this entry's in-flight turn,
    /// without cascading to descendants or removing the entry. Esc/Ctrl-C on
    /// a focused sub-agent tab route here; the agent drops its turn and
    /// re-parks — its *next* turn runs uncancelled, since [`interrupt_entry`]
    /// never touches the terminate layer.
    pub fn interrupt(&self, id: AgentId) {
        let g = self.lock();
        if let Some(e) = g.entries.get(&id) {
            interrupt_entry(e);
        }
    }

    /// Snapshot the live agents in `ancestor`'s subtree (its proper
    /// descendants, at any depth), ordered by id — the `agents` listing for
    /// the agent that started them.  `ancestor` itself is excluded: an agent
    /// lists the ones *it* spawned, not itself.
    pub fn list(&self, ancestor: AgentId) -> Vec<AgentInfo> {
        let g = self.lock();
        let mut v: Vec<AgentInfo> = descendants(&g.entries, ancestor, false)
            .into_iter()
            .filter_map(|id| {
                g.entries.get(&id).map(|e| AgentInfo {
                    id,
                    name: e.name.clone(),
                    log_dir: e.log_dir.clone(),
                    elapsed: e.started.elapsed(),
                })
            })
            .collect();
        drop(g);
        v.sort_by_key(|a| a.id);
        v
    }

    /// Look up one agent's name by id, if it is live.
    pub fn name_for(&self, id: AgentId) -> Option<String> {
        let g = self.lock();
        g.entries.get(&id).map(|e| e.name.clone())
    }
    /// `/clear` on `root`: cancel and reap `root`'s proper descendants — the
    /// subtree the rebuilt context no longer owns — and bump the generation so
    /// any late result or deferred surface batch from the old context is
    /// rejected. `root` itself stays registered (it is the agent being rebuilt,
    /// not torn down).
    pub fn clear_subtree(&self, root: AgentId) {
        let mut g = self.lock();
        cancel_and_remove(&mut g, root, false);
        g.generation += 1;
    }

    /// Close `root` and its whole subtree: cancel each entry's in-flight turn and
    /// eval, then drop the entries.  The inclusive twin of the `/clear` reap
    /// (which leaves the root live); no generation bump — a branch pushes no
    /// result, so there is nothing to fence.  `/close` routes here.
    pub fn remove_subtree(&self, root: AgentId) {
        let mut g = self.lock();
        cancel_and_remove(&mut g, root, true);
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Why [`AgentRegistry::register`] refused a registration.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// `parent` is `Some` and is not, at this instant, a live and
    /// un-terminated entry — the parent has already vanished or been
    /// terminated out from under the racing spawn.
    SessionDead,
    /// A live entry already bears this name — carries the offending name so
    /// the caller can render a didactic refusal.
    NameTaken(String),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageError {
    UnknownSender(AgentId),
    UnknownRecipient(AgentId),
    /// `to` is live but is not a proper descendant of `from` — the same
    /// scoping [`NotADescendant`] names for [`AgentRegistry::cancel_scoped`].
    NotADescendant(AgentId),
    /// The recipient's inbox rejected the message at quota — surfaced to
    /// the `message` tool's caller as a user-facing error, never dropped
    /// silently (`decisions/260705_leases-and-budgets`).
    RecipientInboxFull(AgentId, InboxReject),
}

/// A model-facing scoping violation shared by `agent-cancel` and `message`.
///
/// Each takes a target id but may only reach a proper descendant of the
/// caller — an ancestor, a sibling, or the caller's own id is refused.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct NotADescendant(pub AgentId);

/// True if `id` is a proper descendant of `ancestor` — walks `id`'s own
/// parent chain to the root rather than scanning `ancestor`'s whole subtree,
/// so a scoping check costs one walk to the root, not a tree-wide BFS.
/// `id == ancestor` is never a descendant of itself: the walk starts at
/// `id`'s *parent*, so it can only ever match an ancestor strictly above it.
fn is_descendant_of(entries: &HashMap<AgentId, Entry>, ancestor: AgentId, id: AgentId) -> bool {
    let mut cur = id;
    while let Some(parent) = entries.get(&cur).and_then(|e| e.parent) {
        if parent == ancestor {
            return true;
        }
        cur = parent;
    }
    false
}

/// The ids in `ancestor`'s subtree, at any depth.  `inclusive` adds
/// `ancestor` itself (for the cascade, which cancels the agent and its
/// descendants); `false` yields only the proper descendants (for the
/// `agents` listing and a `/clear` reap).  Builds the child adjacency once
/// and walks it, rather than rescanning the whole map per frontier pop.
fn descendants(
    entries: &HashMap<AgentId, Entry>,
    ancestor: AgentId,
    inclusive: bool,
) -> Vec<AgentId> {
    let mut children_of: HashMap<AgentId, Vec<AgentId>> = HashMap::new();
    for (id, e) in entries {
        if let Some(p) = e.parent {
            children_of.entry(p).or_default().push(*id);
        }
    }
    let mut out = if inclusive {
        vec![ancestor]
    } else {
        Vec::new()
    };
    let mut frontier = vec![ancestor];
    while let Some(cur) = frontier.pop() {
        if let Some(kids) = children_of.get(&cur) {
            for &k in kids {
                out.push(k);
                frontier.push(k);
            }
        }
    }
    out
}

/// Cancel and drop a subtree.  `inclusive` reaps `root` itself (the `/close`
/// twin, [`AgentRegistry::remove_subtree`]); `false` leaves `root` live and
/// reaps only its proper descendants (the `/clear` reap and a settling
/// `reply`).  Each reaped entry is cancelled across both layers before removal
/// so an in-flight eval unwinds instead of grinding on as an orphan.
fn cancel_and_remove(g: &mut Inner, root: AgentId, inclusive: bool) {
    let victims = descendants(&g.entries, root, inclusive);
    for d in &victims {
        if let Some(e) = g.entries.get(d) {
            terminate_entry(e, CancelCause::Explicit);
        }
    }
    for d in victims {
        g.entries.remove(&d);
    }
}

/// One firing of an agent's idle lease, on the reaper daemon thread.
///
/// A gone entry — settled, `/clear`'d, `/close`'d — or one carrying no
/// lease ends the chain silently: no cancel, no notice, no re-arm.  A live
/// leased entry past its bound is reaped through the ordinary cascade
/// ([`CancelCause::Deadline`]); otherwise the chain re-arms itself for
/// exactly the remaining margin.  Renewal ([`AgentRegistry::renew`]) never
/// touches the reaper — this lazy re-arm on the next fire is the whole
/// mechanism.
fn lease_fire(reg: &AgentRegistry, id: AgentId) {
    let Some((idle, ttl)) = reg.lease_state(id) else {
        return;
    };
    if idle >= ttl {
        reg.cancel_cause(id, CancelCause::Deadline);
    } else {
        let reg = reg.clone();
        process::arm_callback(ttl.checked_sub(idle).unwrap(), move || lease_fire(&reg, id)).keep();
    }
}

/// Stamp one entry's exchange clock to now — the single write
/// [`AgentRegistry::renew`] and [`AgentRegistry::steer`] both perform, each
/// under its own lock scope shaped by what else it must do at the same
/// acquisition.
fn renew_entry(e: &mut Entry) {
    e.last_exchange = Some(Instant::now());
}

/// Cancel one entry across both terminate-class layers: the cooperative
/// [`Token`] the drive loop polls between steps, and — for a parented
/// agent — its eval layer through [`EvalReach::terminate`], so an in-flight
/// eval unwinds at the evaluator's poll points rather than running to its
/// timeout wall.
fn terminate_entry(e: &Entry, cause: CancelCause) {
    e.cancel.cancel(cause);
    if let Some(reach) = &e.reach {
        reach.terminate(cause);
    }
}

/// Unwind one entry's in-flight turn without ending the agent: cancel the
/// [`Token`] and interrupt through the entry's [`EvalReach`], which never
/// touches the terminate layer — the next turn is born clean regardless of
/// what this call cancels.
fn interrupt_entry(e: &Entry) {
    e.cancel.cancel(CancelCause::Interrupt);
    if let Some(reach) = &e.reach {
        reach.interrupt();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ProviderHandle {
        ProviderHandle::new(std::sync::Arc::new(crate::provider::Provider::scripted(
            "test",
            crate::provider::ProviderKind::Openai,
            crate::provider::scripted::Script::new(),
        )))
    }

    /// Poll `pred` until it holds or `timeout` elapses — the reaper fires on
    /// its own daemon thread, so a lease test asserts against wall time
    /// rather than a synchronous call.
    fn eventually(timeout: Duration, pred: impl Fn() -> bool) -> bool {
        let start = Instant::now();
        while start.elapsed() < timeout {
            if pred() {
                return true;
            }
            std::thread::sleep(Duration::from_millis(5));
        }
        false
    }

    /// Register `id` under `parent`, returning the birth generation.
    fn entry(reg: &AgentRegistry, id: AgentId, parent: Option<AgentId>) -> u64 {
        reg.register(Registration {
            id,
            parent,
            lease: parent.is_some().then_some(AGENT_LEASE_IDLE),
            name: format!("a{id}"),
            log_dir: PathBuf::from("/tmp"),
            cancel: Token::new(),
            reach: parent.map(|_| EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: crate::bus::Inbox::new().mailbox(),
            provider: provider(),
        })
        .expect("a fresh trunk, or a child of one, always registers")
    }

    #[test]
    fn settle_delivers_for_current_generation() {
        let reg = AgentRegistry::new();
        let g = entry(&reg, 1, None);
        assert!(reg.settle(1, g), "a current-generation worker delivers");
        assert!(!reg.settle(1, g), "a settled entry is gone");
    }

    /// A lease is an explicit knob, not a consequence of being parented: a
    /// `lease: None` entry's eval layer is never reached by the reaper,
    /// while a `lease: Some(short)` entry's is cancelled once its bound
    /// elapses — asserted behaviourally (the reap stamps the entry's
    /// cancel layers; a live drive loop, absent here, is what would go on
    /// to settle the entry out of the map), since there is no guard field
    /// left to peek at — liveness gating lives in the fire itself.
    #[test]
    fn register_arms_a_lease_only_when_asked() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk: a live parent for 1 and 2
        let branch_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "branch".into(),
            log_dir: PathBuf::from("/tmp"),
            cancel: branch_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let worker_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(0),
            lease: Some(Duration::from_millis(50)),
            name: "worker".into(),
            log_dir: PathBuf::from("/tmp"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: worker_root.clone(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        assert!(
            eventually(Duration::from_secs(2), || worker_root
                .as_scope()
                .is_cancelled()),
            "lease: Some(short) is reaped once its bound elapses"
        );
        std::thread::sleep(Duration::from_millis(300));
        assert!(
            !branch_token.is_cancelled(),
            "lease: None arms no reaper deadline"
        );
    }

    #[test]
    fn clear_subtree_bumps_generation_and_rejects_late_results() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let g = entry(&reg, 1, Some(0)); // its child
        reg.clear_subtree(0);
        assert!(
            !reg.settle(1, g),
            "a result from before /clear is rejected by generation"
        );
        assert!(reg.mailbox(1).is_none(), "the cleared child is reaped");
        assert!(reg.mailbox(0).is_some(), "the cleared agent itself stays");
        assert_ne!(g, reg.generation(), "the generation advanced on clear");
    }

    /// `/clear` and a settling `reply` reap descendants through
    /// `cancel_and_remove`: each reaped worker is cancelled across both
    /// layers, so an in-flight eval unwinds instead of grinding on as an
    /// orphan whose result nobody will collect.
    #[test]
    fn clear_subtree_cancels_descendant_eval_roots() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let token = Token::new();
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        reg.clear_subtree(0);
        assert!(token.is_cancelled(), "the reaped worker's token is set");
        assert!(
            eval_root.as_scope().is_cancelled(),
            "the reap cancels the worker's eval layer, not just its token"
        );
    }

    /// The invariant the `/clear` UI handler leans on: `Agent::clear`
    /// rebuilds the trunk's context in place rather than ending it, and a
    /// sticky [`Token`] never forgets a terminate-class cause once one lands
    /// — [`Token::reset`] only ever clears a bare [`CancelCause::Interrupt`].
    /// So no gesture that leaves the trunk live may stamp a terminate cause
    /// on the trunk's own token: `cancel_descendants` is the primitive that
    /// guarantees it, in contrast to [`AgentRegistry::cancel`]'s inclusive
    /// cascade, which would stamp `Explicit` on the trunk too and fail every
    /// turn after it forever.
    #[test]
    fn clear_gesture_reaps_descendants_but_spares_the_trunks_token() {
        let reg = AgentRegistry::new();
        let trunk_token = Token::new();
        let _ = reg.register(Registration {
            id: 0,
            parent: None,
            lease: None,
            name: "trunk".into(),
            log_dir: PathBuf::from("/tmp/trunk"),
            cancel: trunk_token.clone(),
            reach: None,
            mailbox: mb(),
            provider: provider(),
        });
        let child_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: child_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.cancel_descendants(0);

        assert!(
            child_token.terminated(),
            "the descendant's token is terminate-stamped"
        );
        assert!(!reg.is_live(1), "the descendant's entry is reaped");
        assert!(reg.is_live(0), "the trunk's entry survives the gesture");
        assert!(
            !trunk_token.is_cancelled(),
            "cancel_descendants never touches the trunk's own token"
        );

        // A stamped terminate cause would survive this round trip; an
        // uncancelled token does not, so this proves the trunk's token
        // carries no terminate cause at all, not merely that this call
        // happened to skip it.
        trunk_token.cancel(CancelCause::Interrupt);
        trunk_token.reset();
        assert!(
            !trunk_token.is_cancelled(),
            "the trunk's token round-trips through an interrupt and reset \
             uncancelled, so the next turn after /clear would run"
        );
    }

    /// `/close` routes to `remove_subtree`, the inclusive twin of the `/clear`
    /// reap: it cancels and drops the whole subtree *including* its root — a
    /// branch and any agents it spawned — where the `/clear` reap would have
    /// left the root live.  The trunk above the closed subtree is untouched.
    #[test]
    fn remove_subtree_cancels_and_removes_the_root_too() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let branch_token = Token::new();
        let branch_root = DurableRoot::default();
        let child_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None, // a branch carries no lease
            name: "branch".into(),
            log_dir: PathBuf::from("/tmp/branch"),
            cancel: branch_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: branch_root.clone(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/gc"),
            cancel: child_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.remove_subtree(1);

        assert!(
            branch_token.is_cancelled(),
            "the closed branch's token is set"
        );
        assert!(
            branch_root.as_scope().is_cancelled(),
            "close reaches the branch's eval layer, not just its token"
        );
        assert!(child_token.is_cancelled(), "a spawned descendant cascades");
        assert!(
            !reg.is_live(1),
            "the branch itself is gone, unlike a /clear reap"
        );
        assert!(!reg.is_live(2), "and so is its descendant");
        assert!(
            reg.is_live(0),
            "the trunk above the closed subtree survives"
        );
    }

    /// `interrupt` is the per-tab turn interrupt, not the subtree cascade: it
    /// trips exactly one entry's token and whatever `ForegroundScope` its
    /// `turn_scope` cell holds — simulating the transport's per-turn
    /// capture — walks no descendants, and deregisters nothing.  So the
    /// child's token is `is_cancelled` but not `terminated` (an interrupt
    /// drops the turn, it does not end the agent), the interrupt reaches the
    /// cell's scope, the grandchild is untouched, and both entries stay
    /// live.  Contrast `cancel(1)`, which would trip the grandchild too and
    /// with a terminate cause (`Explicit`), settling the whole subtree.
    ///
    /// `eval_root` itself is never touched, so it stays uncancelled — pinned
    /// again by [`interrupt_never_poisons_the_next_turn`].
    #[test]
    fn interrupt_unwinds_exactly_one_entry() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let child_token = Token::new();
        let child_root = DurableRoot::default();
        // Simulates what `IdentityTransport::observe_foreground` writes at
        // the start of the child's in-flight turn.
        let child_turn_scope: TurnScope = Arc::new(Mutex::new(Some(child_root.child())));
        let grandchild_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: child_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: child_root.clone(),
                turn_scope: child_turn_scope.clone(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/gc"),
            cancel: grandchild_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.interrupt(1);

        assert!(
            child_token.is_cancelled(),
            "interrupt trips the child's token"
        );
        assert!(
            !child_token.terminated(),
            "an interrupt is not a terminate cause"
        );
        assert!(
            child_turn_scope
                .lock()
                .unwrap()
                .as_ref()
                .expect("the cell holds the in-flight turn's scope")
                .is_cancelled(),
            "the interrupt reaches whatever scope the turn-scope cell holds"
        );
        assert!(
            !child_root.as_scope().is_cancelled(),
            "eval_root itself is never touched by an interrupt"
        );
        assert!(
            !grandchild_token.is_cancelled(),
            "no descendant walk: the grandchild is untouched"
        );
        assert!(reg.is_live(1), "interrupt never deregisters the entry");
        assert!(reg.is_live(2), "nor its descendant");
    }

    /// An interrupted agent's *next* turn must run uncancelled: `interrupt`
    /// reaches only the turn-scope cell, never `eval_root`, and core's cancel
    /// scopes are monotone — so a foreground scope minted from `eval_root`
    /// for the next turn (exactly what `IdentityTransport::observe_foreground`
    /// captures at the start of every dispatch) starts clean.
    #[test]
    fn interrupt_never_poisons_the_next_turn() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let eval_root = DurableRoot::default();
        let turn_scope: TurnScope = Arc::new(Mutex::new(Some(eval_root.child())));
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                turn_scope: turn_scope.clone(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.interrupt(1);
        assert!(
            turn_scope.lock().unwrap().as_ref().unwrap().is_cancelled(),
            "the in-flight turn did unwind"
        );

        // The next turn mints a fresh scope from the same `eval_root` and the
        // transport re-captures it into the same cell, exactly as
        // `IdentityTransport::observe_foreground` does at every dispatch.
        let next_turn = eval_root.child();
        *turn_scope.lock().unwrap() = Some(next_turn.clone());

        assert!(
            !next_turn.is_cancelled(),
            "the next turn is born uncancelled — the interrupt never poisoned eval_root"
        );
    }

    #[test]
    fn cancel_sets_the_token_and_list_reports_the_subtree() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let token = Token::new();
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 7,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "lint".into(),
            log_dir: PathBuf::from("/log/7"),
            cancel: token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: crate::bus::Inbox::new().mailbox(),
            provider: provider(),
        });
        assert_eq!(reg.list(0).len(), 1, "the trunk lists its one child");
        assert_eq!(reg.list(0)[0].name, "lint");
        assert!(reg.cancel(7), "an existing agent is cancellable");
        assert!(token.is_cancelled(), "cancel sets the worker's token");
        assert!(
            eval_root.as_scope().is_cancelled(),
            "cancel reaches the worker's eval layer through its session root"
        );
        assert!(!reg.cancel(99), "an unknown id is not cancellable");
    }

    /// The subtree cascade: cancelling a mid-tree agent cancels it and every
    /// descendant, at any depth.
    #[test]
    fn cancel_cascades_to_the_whole_subtree() {
        let reg = AgentRegistry::new();
        let r = Token::new();
        let c = Token::new();
        let g = Token::new();
        let sibling = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: None,
            lease: None,
            name: "r".into(),
            log_dir: "/l".into(),
            cancel: r.clone(),
            reach: None,
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "c".into(),
            log_dir: "/l".into(),
            cancel: c.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 3,
            parent: Some(2),
            lease: Some(AGENT_LEASE_IDLE),
            name: "g".into(),
            log_dir: "/l".into(),
            cancel: g.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let _ = reg.register(Registration {
            id: 4,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "s".into(),
            log_dir: "/l".into(),
            cancel: sibling.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        // Cancel the mid-tree node 2: it and its descendant 3 go; the sibling
        // 4 and the root 1 are untouched.
        assert!(reg.cancel(2));
        assert!(c.is_cancelled(), "the cancelled node");
        assert!(g.is_cancelled(), "its descendant cascades");
        assert!(!sibling.is_cancelled(), "a sibling is untouched");
        assert!(!r.is_cancelled(), "the parent is untouched");
    }

    #[test]
    fn mailbox_resolves_only_while_an_agent_is_live() {
        let reg = AgentRegistry::new();
        let g = entry(&reg, 1, None);
        assert!(
            reg.mailbox(1).is_some(),
            "a live entry hands out its sender"
        );
        assert!(reg.mailbox(99).is_none(), "an unknown id has no sender");
        assert!(reg.settle(1, g));
        assert!(reg.mailbox(1).is_none(), "a settled entry hands out none");
    }

    #[test]
    fn message_posts_a_marked_note_to_a_live_agent() {
        let reg = AgentRegistry::new();
        entry(&reg, 1, None);
        let inbox = crate::bus::Inbox::new();
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: inbox.mailbox(),
            provider: provider(),
        });

        reg.message(1, 2, "check the lexer".into())
            .expect("message to a live agent succeeds");
        let Some(crate::bus::Turn::Message(msg)) = inbox.drain_turn() else {
            panic!("expected a peer message");
        };
        assert_eq!(msg.from, 1);
        assert_eq!(msg.from_name, "a1");
        assert_eq!(msg.text, "check the lexer");
    }

    #[test]
    fn message_reports_unknown_sender_or_recipient() {
        let reg = AgentRegistry::new();
        entry(&reg, 1, None);
        assert_eq!(
            reg.message(1, 99, "hello".into()),
            Err(MessageError::UnknownRecipient(99))
        );
        assert_eq!(
            reg.message(99, 1, "hello".into()),
            Err(MessageError::UnknownSender(99))
        );
    }

    /// A1: `message` may only reach a proper descendant of the sender — an
    /// ancestor, a sibling, or the sender's own id is refused, even though
    /// all three are live agents the existence checks alone would admit.
    #[test]
    fn message_refuses_self_and_non_descendant_targets() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        entry(&reg, 1, Some(0)); // trunk's child
        entry(&reg, 2, Some(0)); // trunk's other child, 1's sibling

        assert_eq!(
            reg.message(1, 1, "hi".into()),
            Err(MessageError::NotADescendant(1)),
            "a self-message is refused"
        );
        assert_eq!(
            reg.message(1, 2, "hi".into()),
            Err(MessageError::NotADescendant(2)),
            "a sibling is refused"
        );
        assert_eq!(
            reg.message(1, 0, "hi".into()),
            Err(MessageError::NotADescendant(0)),
            "an ancestor is refused"
        );

        let inbox = crate::bus::Inbox::new();
        let _ = reg.register(Registration {
            id: 3,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/3"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: inbox.mailbox(),
            provider: provider(),
        });
        assert!(
            reg.message(1, 3, "ok".into()).is_ok(),
            "a proper descendant is reachable"
        );
    }

    /// A1: `agent-cancel`'s scoped entry point may only reach a proper
    /// descendant of the caller.  A leaf cannot cancel the trunk (an
    /// ancestor), a sibling, or itself; a parent can cancel its own
    /// subtree at any depth; an unknown id is reported, not an error.
    #[test]
    fn cancel_scoped_reaches_only_a_proper_descendant() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "leaf".into(),
            log_dir: PathBuf::from("/tmp/1"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let sibling_token = Token::new();
        let _ = reg.register(Registration {
            id: 2,
            parent: Some(0),
            lease: None,
            name: "sibling".into(),
            log_dir: PathBuf::from("/tmp/2"),
            cancel: sibling_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        let grandchild_token = Token::new();
        let _ = reg.register(Registration {
            id: 3,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/3"),
            cancel: grandchild_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        assert_eq!(
            reg.cancel_scoped(1, 0),
            Err(NotADescendant(0)),
            "a leaf cannot cancel an ancestor (the trunk)"
        );
        assert_eq!(
            reg.cancel_scoped(1, 2),
            Err(NotADescendant(2)),
            "nor a sibling"
        );
        assert_eq!(
            reg.cancel_scoped(1, 1),
            Err(NotADescendant(1)),
            "nor itself"
        );
        assert!(
            !sibling_token.is_cancelled(),
            "none of the refused calls touched anything"
        );

        assert_eq!(
            reg.cancel_scoped(0, 3),
            Ok(true),
            "the trunk can cancel its own grandchild"
        );
        assert!(grandchild_token.is_cancelled());

        assert_eq!(
            reg.cancel_scoped(0, 999),
            Ok(false),
            "an unknown id is reported, not an error"
        );
    }

    /// L3: a registration whose `parent` has already vanished from the map —
    /// the tail of a spawn racing a `/clear`/`agent-cancel` on that parent —
    /// is refused outright rather than landing orphaned and uncancellable.
    #[test]
    fn register_refuses_when_the_parent_is_gone() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let outcome = reg.register(Registration {
            id: 1,
            parent: Some(99), // never registered
            lease: None,
            name: "orphan".into(),
            log_dir: PathBuf::from("/tmp/orphan"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert_eq!(
            outcome,
            Err(RegisterError::SessionDead),
            "a parent absent from the map refuses the registration"
        );
        assert!(!reg.is_live(1), "the refused entry is never inserted");
    }

    /// L3, the other half: a parent whose entry still lingers but whose own
    /// token already carries a terminate-class cause (cancelled, not yet
    /// self-settled) refuses a new child exactly as an absent parent would —
    /// the bare `cancel()` cascade never removes entries, so "absent from
    /// the map" alone would miss this window.
    #[test]
    fn register_refuses_when_the_parent_is_already_terminated() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let parent_token = Token::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: None,
            name: "parent".into(),
            log_dir: PathBuf::from("/tmp/parent"),
            cancel: parent_token.clone(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        parent_token.cancel(CancelCause::Explicit);
        assert!(reg.is_live(1), "the cancelled parent's entry still lingers");

        let outcome = reg.register(Registration {
            id: 2,
            parent: Some(1),
            lease: Some(AGENT_LEASE_IDLE),
            name: "late-child".into(),
            log_dir: PathBuf::from("/tmp/late-child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert_eq!(
            outcome,
            Err(RegisterError::SessionDead),
            "a parent already carrying a terminate cause refuses new children"
        );
        assert!(!reg.is_live(2));
    }

    /// The authoritative half of the spawn-uniqueness rule: two
    /// registrations bearing the same name never both succeed, even when
    /// nothing has settled between them — the desk's own pre-check
    /// (`name_live`) is only the cheap, didactic half; this is the check
    /// that actually closes the race.
    #[test]
    fn register_refuses_a_name_already_borne_by_a_live_entry() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "helper".into(),
            log_dir: PathBuf::from("/tmp/helper"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert!(reg.name_live("helper"), "the first spawn's name is live");

        let outcome = reg.register(Registration {
            id: 2,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "helper".into(),
            log_dir: PathBuf::from("/tmp/helper-2"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert_eq!(
            outcome,
            Err(RegisterError::NameTaken("helper".to_string())),
            "a second live agent may not bear the same name"
        );
        assert!(
            !reg.is_live(2),
            "the refused registration is never inserted"
        );
        assert_eq!(
            reg.resolve_name("helper"),
            Some(1),
            "the name still resolves to the entry that actually holds it"
        );
    }

    /// A lease reap's cascade must report `Deadline` ("timed out"), not
    /// `Explicit` ("cancelled") — a reaped agent and an `agent-cancel`ed one
    /// hit the same shared cascade (`cancel_cause`), distinguished only by
    /// which cause each caller passes.  This is the cascade the lease chain
    /// itself uses ([`lease_fire`]); this test pins the cause directly,
    /// independent of the chain's own timing.
    #[test]
    fn lease_reap_reports_deadline_not_explicit() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });
        assert!(reg.cancel_cause(1, CancelCause::Deadline));
        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Deadline),
            "a reaped worker's eval layer reports it timed out, not that it was explicitly cancelled"
        );
    }

    /// `settle` is restructured so a stale-generation worker can never leave
    /// a zombie entry: it always reaps its own entry, whether or not the
    /// generation still matches, delivering only when it does.  In
    /// production `/clear`'s own cascade already removes every worker it
    /// could make stale in the same stroke as its generation bump; this
    /// pins the invariant directly, independent of that coincidence.
    #[test]
    fn settle_always_reaps_its_entry_even_across_a_bare_generation_bump() {
        let reg = AgentRegistry::new();
        let g = entry(&reg, 1, None);
        reg.lock().generation += 1;
        assert!(
            !reg.settle(1, g),
            "a stale-generation result is not delivered"
        );
        assert!(
            !reg.is_live(1),
            "but its entry is reaped regardless, never left dangling"
        );
    }

    /// `renew` resets the exchange clock, deferring the fire: a lease
    /// renewed partway through its span survives past the original bound
    /// and is reaped only once the *renewed* span elapses.
    #[test]
    fn renew_defers_the_fire() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let ttl = Duration::from_millis(300);
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(ttl),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        std::thread::sleep(ttl / 2);
        assert!(reg.renew(1), "a live entry renews");

        std::thread::sleep(ttl / 2 + Duration::from_millis(50));
        assert!(
            !eval_root.as_scope().is_cancelled(),
            "renewed at half the ttl, still alive past the original bound"
        );

        assert!(
            eventually(Duration::from_secs(2), || eval_root
                .as_scope()
                .is_cancelled()),
            "reaped once the renewed span elapses"
        );
    }

    /// Settling before the lease ever fires ends the chain silently: its
    /// late fire finds no entry and never reaches the settled token.
    #[test]
    fn settle_before_the_fire_ends_the_chain_silently() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let ttl = Duration::from_millis(80);
        let token = Token::new();
        let g = reg
            .register(Registration {
                id: 1,
                parent: Some(0),
                lease: Some(ttl),
                name: "child".into(),
                log_dir: PathBuf::from("/tmp/child"),
                cancel: token.clone(),
                reach: Some(EvalReach::Identity {
                    eval_root: DurableRoot::default(),
                    turn_scope: TurnScope::default(),
                }),
                mailbox: mb(),
                provider: provider(),
            })
            .expect("registers");

        assert!(reg.settle(1, g), "settles well before the lease would fire");

        std::thread::sleep(ttl * 4);
        assert!(
            !token.is_cancelled(),
            "the chain's late fire found no entry and never touched the settled token"
        );
    }

    /// `clear_subtree` outranks a still-armed lease: the reap is immediate
    /// and the lease's later fire, finding no entry, never overwrites the
    /// cause `/clear` already stamped.
    #[test]
    fn clear_subtree_outranks_the_lease() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let ttl = Duration::from_millis(80);
        let eval_root = DurableRoot::default();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(ttl),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: eval_root.clone(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: mb(),
            provider: provider(),
        });

        reg.clear_subtree(0);
        assert!(!reg.is_live(1), "the reap removes the entry immediately");
        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Explicit),
            "clear_subtree cancels through the ordinary /clear cause"
        );

        std::thread::sleep(ttl * 4);
        assert_eq!(
            eval_root.as_scope().cause(),
            Some(CancelCause::Explicit),
            "the lease's late fire finds no entry and never overwrites the cause"
        );
    }

    /// `engaged`/`idle` read the exchange clock directly: a fresh entry has
    /// never been engaged and its idle span runs off birth; a raw push onto
    /// the entry's own mailbox — bypassing `renew` — is not an exchange;
    /// only `renew` itself flips `engaged` and resets `idle`.
    #[test]
    fn engaged_and_idle_read_the_exchange_clock() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let inbox = crate::bus::Inbox::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: inbox.mailbox(),
            provider: provider(),
        });

        assert!(!reg.engaged(1), "a fresh entry has never been engaged");
        assert!(
            reg.idle(1).is_some(),
            "idle reads off birth before any exchange"
        );

        inbox.push(InboxMsg::UserSteering("hello".into())).unwrap();
        assert!(!reg.engaged(1), "a raw mailbox push is not an exchange");

        assert!(reg.renew(1), "renew stamps a live entry");
        assert!(reg.engaged(1), "renewed at least once");
        let idle_after_renew = reg.idle(1).expect("still live");
        assert!(
            idle_after_renew < Duration::from_secs(1),
            "idle resets to (near) zero right after a renewal"
        );
    }

    /// `steer` is the registry's one human-delivery door: it renews the idle
    /// lease (`engaged` flips true, `idle` resets) and the text actually
    /// lands on the agent's own queue, in one call.  A dead id is refused
    /// without panicking, and delivers nothing.
    #[test]
    fn steer_renews_and_delivers() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let inbox = crate::bus::Inbox::new();
        let _ = reg.register(Registration {
            id: 1,
            parent: Some(0),
            lease: Some(AGENT_LEASE_IDLE),
            name: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            reach: Some(EvalReach::Identity {
                eval_root: DurableRoot::default(),
                turn_scope: TurnScope::default(),
            }),
            mailbox: inbox.mailbox(),
            provider: provider(),
        });

        assert!(!reg.engaged(1), "not yet engaged");
        assert!(
            reg.steer(1, "hello".into()),
            "a live entry accepts steering"
        );
        assert!(reg.engaged(1), "steer renews the exchange clock");
        assert!(
            reg.idle(1).expect("still live") < Duration::from_secs(1),
            "idle resets to (near) zero right after a steer"
        );

        let Some(crate::bus::Turn::Human(text)) = inbox.drain_turn() else {
            panic!("expected the steered text to have actually landed");
        };
        assert_eq!(text, "hello");

        assert!(
            !reg.steer(99, "nobody".into()),
            "a dead id is refused, not panicked"
        );
    }

    fn mb() -> Mailbox {
        crate::bus::Inbox::new().mailbox()
    }
}
