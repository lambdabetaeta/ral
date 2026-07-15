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
//!     (`agent_cancel`);
//!   * the **subtree cascade**: [`AgentRegistry::cancel`] cancels an agent
//!     *and every descendant*, behind `agent_cancel` and the per-agent
//!     ceiling; `/clear` and `/close` reap a subtree through `clear_subtree`
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
//!     whatever [`ForegroundScope`](ral_core::process::ForegroundScope) the
//!     entry's [`TurnScope`] cell currently holds — the scope `exarch`'s own
//!     transport captures fresh at the start of every turn
//!     ([`crate::agent::Agent::turn_scope`]). Cancelling that scope reaches
//!     an in-flight eval exactly as cancelling `eval_root` would, but the
//!     *next* turn mints an entirely new scope from the untouched durable
//!     root, so an interrupt never poisons a later turn.  `eval_root` stays
//!     the terminate cascade's alone;
//!   * a **ceiling** (children only), armed on the shared `process::reaper` as
//!     a `Run` deadline that cancels the worker's subtree, so an abandoned
//!     detached worker is reaped rather than running forever — the trunk,
//!     which is never abandoned, gets none;
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
use crate::bus::{AgentId, AgentMessage, InboxMsg, InboxReject, Mailbox};
use crate::cancel::Token;
use ral_core::process::{self, CancelCause, Deadline, DurableRoot, ForegroundScope};
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

/// The detached-worker default ceiling: one hour
/// ([[concurrency-primitives-detached-vs-structured]]).
///
/// An async agent
/// doing genuinely long work may want a different bound — that is
/// long-running-work's open regime question, surfacing here for a tool
/// rather than a ral verb — but a uniform ceiling keeps an abandoned worker
/// from running unbounded.
pub const AGENT_CEILING: Duration = Duration::from_hours(1);

/// One live agent, by id, for the `agents` listing.
pub struct AgentInfo {
    pub id: AgentId,
    pub title: String,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
}

/// An agent's registration record, threaded once at birth into
/// [`AgentRegistry::register`].
pub struct Registration {
    pub id: AgentId,
    /// The agent's parent, or `None` for the trunk.
    pub parent: Option<AgentId>,
    /// Whether to arm a reaper deadline over this agent's whole subtree —
    /// see [`AgentRegistry::register`].
    pub ceiling: bool,
    pub title: String,
    pub log_dir: PathBuf,
    pub cancel: Token,
    pub eval_root: Option<DurableRoot>,
    pub turn_scope: Option<TurnScope>,
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
    title: String,
    log_dir: PathBuf,
    started: Instant,
    cancel: Token,
    /// The eval-layer cancel handle — the durable root of the agent's own
    /// `Shell`.  A tripped [`Token`] alone cannot stop an in-flight `ral`
    /// tool call (the drive loop reads it only between steps, and
    /// `run_shell` blocks until the engine reports), so the cascade also
    /// cancels the agent's session root: the eval unwinds at the
    /// evaluator's own poll points.  `None` for the trunk, whose session
    /// outlives any cancel — an Esc cancels its *turn*, delivered through
    /// the published foreground slot, never its root.
    eval_root: Option<DurableRoot>,
    /// The current-turn foreground scope [`AgentRegistry::interrupt`]
    /// cancels, distinct from `eval_root`: refreshed every turn by the
    /// agent's own transport, so cancelling it unwinds the in-flight turn
    /// alone — the next turn mints a fresh scope from the untouched
    /// `eval_root`.  `None` for the trunk, whose Esc routes through the
    /// published foreground slot instead.
    turn_scope: Option<TurnScope>,
    /// The agent's inbox sender, so the frontend can route a focused tab's
    /// typed line into that agent's queue, `wake` it on a focus change, and
    /// the `message` tool can deliver a marked peer note by id.
    mailbox: Mailbox,
    /// The agent's hot-swappable provider handle, so a `/model` on the focused
    /// agent swaps *its* provider without disturbing any other.
    provider: ProviderHandle,
    /// Holds the worker's reaper ceiling armed; dropping the entry on settle
    /// disarms it, so a worker that finished before its ceiling is never
    /// reaped by a late pop.  `None` for the trunk, which has no ceiling.
    _ceiling: Option<Deadline>,
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
    /// trunk).  [`Registration::ceiling`] states whether to arm a reaper
    /// deadline that cancels this agent's whole subtree when it elapses: a
    /// child no longer *implies* one — the caller says so.  A worker gets one
    /// (an abandoned detached worker must be reaped); a branch is a parented
    /// child *without* one (a conversation must not lose a turn at the hour
    /// mark); a root never had one.  A `parent`-set agent still carries its
    /// `eval_root` and `turn_scope` so the cascade and an interrupt each
    /// reach its running eval.
    ///
    /// Refuses (`None`) when `parent` is `Some` and is not, at this instant,
    /// a live and un-terminated entry — a spawn racing an `agent_cancel`/
    /// `/clear` on its own parent, closed by checking under the very lock the
    /// cascade holds for its whole walk, so the check can never observe a
    /// parent mid-teardown: either the cascade has not reached the lock yet
    /// (parent live, registration proceeds normally) or it already has
    /// (parent gone or terminated, registration refused) — there is no third
    /// state. Otherwise returns the birth generation the worker carries into
    /// its result.
    pub fn register(&self, reg: Registration) -> Option<u64> {
        let Registration {
            id,
            parent,
            ceiling,
            title,
            log_dir,
            cancel,
            eval_root,
            turn_scope,
            mailbox,
            provider,
        } = reg;
        let ceiling_guard = ceiling.then(|| {
            let reg = self.clone();
            process::arm_callback(AGENT_CEILING, move || {
                reg.cancel_cause(id, CancelCause::Deadline);
            })
        });
        let mut g = self.lock();
        if let Some(p) = parent
            && g.entries.get(&p).is_none_or(|e| e.cancel.terminated())
        {
            drop(g);
            drop(ceiling_guard);
            return None;
        }
        g.entries.insert(
            id,
            Entry {
                parent,
                title,
                log_dir,
                started: Instant::now(),
                cancel,
                eval_root,
                turn_scope,
                mailbox,
                provider,
                _ceiling: ceiling_guard,
            },
        );
        Some(g.generation)
    }

    /// The live agent's inbox sender, or `None` once it has settled/cleared.
    /// The frontend looks one up by id to route a steering line into that
    /// agent's queue or to `wake` it on a focus change; a missing entry means
    /// the tab is dead or lingering, and the line is dropped rather than
    /// reviving anything.
    pub fn mailbox(&self, id: AgentId) -> Option<Mailbox> {
        let g = self.lock();
        g.entries.get(&id).map(|e| e.mailbox.clone())
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
        let (mailbox, from_title) = {
            let g = self.lock();
            let from_title = g
                .entries
                .get(&from)
                .ok_or(MessageError::UnknownSender(from))?
                .title
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
            (mailbox, from_title)
        };
        mailbox
            .push(InboxMsg::AgentMessage(AgentMessage {
                from,
                from_title,
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
    /// entry (disarming its ceiling), whether or not `generation` still
    /// matches the live one. A worker's settle is authoritative over its
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
    /// the agent existed. The per-agent ceiling routes here (with
    /// [`CancelCause::Deadline`]); `/clear` and `/close` reap through
    /// [`Self::clear_subtree`]/[`Self::remove_subtree`] instead. `pub(crate)`
    /// because it trusts its caller to already have decided `id` is a
    /// legitimate target — the model-facing `agent_cancel` tool goes through
    /// [`Self::cancel_scoped`], which checks that before ever reaching here.
    pub(crate) fn cancel(&self, id: AgentId) -> bool {
        self.cancel_cause(id, CancelCause::Explicit)
    }

    /// [`Self::cancel`]'s shared implementation, parameterised on the cause
    /// so the ceiling can report `Deadline` ("timed out") rather than
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

    /// `agent_cancel`'s model-facing entry point: cancel `target`'s whole
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
    /// never touches `eval_root`.
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
                    title: e.title.clone(),
                    log_dir: e.log_dir.clone(),
                    elapsed: e.started.elapsed(),
                })
            })
            .collect();
        drop(g);
        v.sort_by_key(|a| a.id);
        v
    }

    /// Look up one agent's title by id, if it is live.
    pub fn title_for(&self, id: AgentId) -> Option<String> {
        let g = self.lock();
        g.entries.get(&id).map(|e| e.title.clone())
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
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
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

/// A model-facing scoping violation shared by `agent_cancel` and `message`.
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

/// Cancel one entry across both terminate-class layers: the cooperative
/// [`Token`] the drive loop polls between steps, and — for a parented
/// agent — its session's durable root, so an in-flight eval unwinds at the
/// evaluator's poll points rather than running to its timeout wall. This
/// entry's `eval_root` is permanently poisoned by design: every caller of
/// this function is ending the agent (or reaping it as abandoned), so there
/// is no later turn for a poisoned root to break.
fn terminate_entry(e: &Entry, cause: CancelCause) {
    e.cancel.cancel(cause);
    if let Some(root) = &e.eval_root {
        root.cancel(cause);
    }
}

/// Unwind one entry's in-flight turn without ending the agent: cancel the
/// [`Token`] and whatever [`ForegroundScope`] its `turn_scope` cell currently
/// holds. Never touches `eval_root` — the next turn mints a fresh scope from
/// that untouched root regardless of what this call cancels.
fn interrupt_entry(e: &Entry) {
    e.cancel.cancel(CancelCause::Interrupt);
    if let Some(cell) = &e.turn_scope
        && let Some(scope) = cell
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .as_ref()
    {
        scope.cancel(CancelCause::Interrupt);
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

    /// Register `id` under `parent`, returning the birth generation.
    fn entry(reg: &AgentRegistry, id: AgentId, parent: Option<AgentId>) -> u64 {
        reg.register(Registration {
            id,
            parent,
            ceiling: parent.is_some(),
            title: format!("a{id}"),
            log_dir: PathBuf::from("/tmp"),
            cancel: Token::new(),
            eval_root: parent.map(|_| DurableRoot::default()),
            turn_scope: parent.map(|_| TurnScope::default()),
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

    /// The ceiling is an explicit knob, not a consequence of being parented:
    /// two children of the same parent arm a reaper deadline iff their
    /// `ceiling` flag is set (a branch passes `false`, a worker `true`).
    #[test]
    #[allow(
        clippy::used_underscore_binding,
        reason = "RAII guard field; the leading underscore marks it as held for Drop, not read — the test deliberately peeks at whether it is armed"
    )]
    fn register_arms_a_ceiling_only_when_asked() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk: a live parent for 1 and 2
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: false,
            title: "branch".into(),
            log_dir: PathBuf::from("/tmp"),
            cancel: Token::new(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        reg.register(Registration {
            id: 2,
            parent: Some(0),
            ceiling: true,
            title: "worker".into(),
            log_dir: PathBuf::from("/tmp"),
            cancel: Token::new(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        let g = reg.lock();
        let one_has_no_ceiling = g.entries[&1]._ceiling.is_none();
        let two_has_ceiling = g.entries[&2]._ceiling.is_some();
        drop(g);
        assert!(one_has_no_ceiling, "ceiling = false arms no reaper deadline");
        assert!(two_has_ceiling, "ceiling = true arms a reaper deadline");
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
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: true,
            title: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: token.clone(),
            eval_root: Some(eval_root.clone()),
            turn_scope: None,
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
        reg.register(Registration {
            id: 0,
            parent: None,
            ceiling: false,
            title: "trunk".into(),
            log_dir: PathBuf::from("/tmp/trunk"),
            cancel: trunk_token.clone(),
            eval_root: None,
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        let child_token = Token::new();
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: true,
            title: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: child_token.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
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
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: false, // a branch carries no ceiling
            title: "branch".into(),
            log_dir: PathBuf::from("/tmp/branch"),
            cancel: branch_token.clone(),
            eval_root: Some(branch_root.clone()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        reg.register(Registration {
            id: 2,
            parent: Some(1),
            ceiling: true,
            title: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/gc"),
            cancel: child_token.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });

        reg.remove_subtree(1);

        assert!(branch_token.is_cancelled(), "the closed branch's token is set");
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
        assert!(reg.is_live(0), "the trunk above the closed subtree survives");
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
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: false,
            title: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: child_token.clone(),
            eval_root: Some(child_root.clone()),
            turn_scope: Some(child_turn_scope.clone()),
            mailbox: mb(),
            provider: provider(),
        });
        reg.register(Registration {
            id: 2,
            parent: Some(1),
            ceiling: true,
            title: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/gc"),
            cancel: grandchild_token.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });

        reg.interrupt(1);

        assert!(child_token.is_cancelled(), "interrupt trips the child's token");
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
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: false,
            title: "child".into(),
            log_dir: PathBuf::from("/tmp/child"),
            cancel: Token::new(),
            eval_root: Some(eval_root.clone()),
            turn_scope: Some(turn_scope.clone()),
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
        reg.register(Registration {
            id: 7,
            parent: Some(0),
            ceiling: true,
            title: "lint".into(),
            log_dir: PathBuf::from("/log/7"),
            cancel: token.clone(),
            eval_root: Some(eval_root.clone()),
            turn_scope: None,
            mailbox: crate::bus::Inbox::new().mailbox(),
            provider: provider(),
        });
        assert_eq!(reg.list(0).len(), 1, "the trunk lists its one child");
        assert_eq!(reg.list(0)[0].title, "lint");
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
        reg.register(Registration {
            id: 1,
            parent: None,
            ceiling: false,
            title: "r".into(),
            log_dir: "/l".into(),
            cancel: r.clone(),
            eval_root: None,
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        reg.register(Registration {
            id: 2,
            parent: Some(1),
            ceiling: true,
            title: "c".into(),
            log_dir: "/l".into(),
            cancel: c.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        reg.register(Registration {
            id: 3,
            parent: Some(2),
            ceiling: true,
            title: "g".into(),
            log_dir: "/l".into(),
            cancel: g.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        reg.register(Registration {
            id: 4,
            parent: Some(1),
            ceiling: true,
            title: "s".into(),
            log_dir: "/l".into(),
            cancel: sibling.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
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
        reg.register(Registration {
            id: 2,
            parent: Some(1),
            ceiling: true,
            title: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: Token::new(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: inbox.mailbox(),
            provider: provider(),
        });

        reg.message(1, 2, "check the lexer".into())
            .expect("message to a live agent succeeds");
        let Some(crate::bus::Turn::Message(msg)) = inbox.drain_turn() else {
            panic!("expected a peer message");
        };
        assert_eq!(msg.from, 1);
        assert_eq!(msg.from_title, "a1");
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
        reg.register(Registration {
            id: 3,
            parent: Some(1),
            ceiling: true,
            title: "child".into(),
            log_dir: PathBuf::from("/tmp/3"),
            cancel: Token::new(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: inbox.mailbox(),
            provider: provider(),
        });
        assert!(
            reg.message(1, 3, "ok".into()).is_ok(),
            "a proper descendant is reachable"
        );
    }

    /// A1: `agent_cancel`'s scoped entry point may only reach a proper
    /// descendant of the caller.  A leaf cannot cancel the trunk (an
    /// ancestor), a sibling, or itself; a parent can cancel its own
    /// subtree at any depth; an unknown id is reported, not an error.
    #[test]
    fn cancel_scoped_reaches_only_a_proper_descendant() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: false,
            title: "leaf".into(),
            log_dir: PathBuf::from("/tmp/1"),
            cancel: Token::new(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        let sibling_token = Token::new();
        reg.register(Registration {
            id: 2,
            parent: Some(0),
            ceiling: false,
            title: "sibling".into(),
            log_dir: PathBuf::from("/tmp/2"),
            cancel: sibling_token.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        let grandchild_token = Token::new();
        reg.register(Registration {
            id: 3,
            parent: Some(1),
            ceiling: true,
            title: "grandchild".into(),
            log_dir: PathBuf::from("/tmp/3"),
            cancel: grandchild_token.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
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
    /// the tail of a spawn racing a `/clear`/`agent_cancel` on that parent —
    /// is refused outright rather than landing orphaned and uncancellable.
    #[test]
    fn register_refuses_when_the_parent_is_gone() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let outcome = reg.register(Registration {
            id: 1,
            parent: Some(99), // never registered
            ceiling: false,
            title: "orphan".into(),
            log_dir: PathBuf::from("/tmp/orphan"),
            cancel: Token::new(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        assert!(
            outcome.is_none(),
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
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: false,
            title: "parent".into(),
            log_dir: PathBuf::from("/tmp/parent"),
            cancel: parent_token.clone(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        parent_token.cancel(CancelCause::Explicit);
        assert!(reg.is_live(1), "the cancelled parent's entry still lingers");

        let outcome = reg.register(Registration {
            id: 2,
            parent: Some(1),
            ceiling: true,
            title: "late-child".into(),
            log_dir: PathBuf::from("/tmp/late-child"),
            cancel: Token::new(),
            eval_root: Some(DurableRoot::default()),
            turn_scope: None,
            mailbox: mb(),
            provider: provider(),
        });
        assert!(
            outcome.is_none(),
            "a parent already carrying a terminate cause refuses new children"
        );
        assert!(!reg.is_live(2));
    }

    /// The ceiling reaper's cascade must report `Deadline` ("timed out"), not
    /// `Explicit` ("cancelled") — a reaped worker and an `agent_cancel`ed one
    /// hit the same shared cascade (`cancel_cause`), distinguished only by
    /// which cause each caller passes.
    #[test]
    fn ceiling_reap_reports_deadline_not_explicit() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let eval_root = DurableRoot::default();
        reg.register(Registration {
            id: 1,
            parent: Some(0),
            ceiling: true,
            title: "worker".into(),
            log_dir: PathBuf::from("/tmp/worker"),
            cancel: Token::new(),
            eval_root: Some(eval_root.clone()),
            turn_scope: None,
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

    fn mb() -> Mailbox {
        crate::bus::Inbox::new().mailbox()
    }
}
