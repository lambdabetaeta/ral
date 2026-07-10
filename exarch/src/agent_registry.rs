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
//!     *and every descendant*, the single primitive behind `agent_cancel`,
//!     the per-agent ceiling, and `/clear`.  Each
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
use ral_core::process::{self, CancelCause, Deadline, DurableRoot};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

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
    /// declaring a new context generation for unrelated agents. Late results
    /// from the removed descendants are rejected because their entries are
    /// gone.
    pub fn cancel_descendants(&self, root: AgentId) {
        let mut g = self.lock();
        remove_descendants(&mut g, root);
    }

    /// Register an agent under its `parent` (`None` for the trunk).  `ceiling`
    /// states whether to arm a reaper deadline that cancels this agent's whole
    /// subtree when it elapses: a child no longer *implies* one — the caller
    /// says so.  A worker gets one (an abandoned detached worker must be
    /// reaped); a branch is a parented child *without* one (a conversation must
    /// not lose a turn at the hour mark); a root never had one.  A `parent`-set
    /// agent still carries its `eval_root` so the cascade reaches its running
    /// eval.  Returns the birth generation the worker carries into its result.
    #[allow(clippy::too_many_arguments)] // an agent's registration record, threaded once at birth
    pub fn register(
        &self,
        id: AgentId,
        parent: Option<AgentId>,
        ceiling: bool,
        title: String,
        log_dir: PathBuf,
        cancel: Token,
        eval_root: Option<DurableRoot>,
        mailbox: Mailbox,
        provider: ProviderHandle,
    ) -> u64 {
        let ceiling = ceiling.then(|| {
            let reg = self.clone();
            process::arm_callback(AGENT_CEILING, move || {
                reg.cancel(id);
            })
        });
        let mut g = self.lock();
        g.entries.insert(
            id,
            Entry {
                parent,
                title,
                log_dir,
                started: Instant::now(),
                cancel,
                eval_root,
                mailbox,
                provider,
                _ceiling: ceiling,
            },
        );
        g.generation
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

    /// Send a marked model-visible message from one live agent to another.
    /// The mailbox is cloned under the registry lock, then posted after the
    /// lock drops; inbox delivery never runs while the registry is held —
    /// the process-wide lock order is inbox → registry (see `bus`'s
    /// module docs), because a park verdict reads this registry under the
    /// consumer's inbox mutex.
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

    /// Settle a worker born under `generation`: if it is still the live
    /// entry of that generation, remove it, disarming its ceiling.  Returns
    /// whether it was.  Delivery is not gated here — the worker posts its
    /// result *before* calling this (deliver-then-retire, so a parent that
    /// observes the entry gone always finds the result already queued), and
    /// a stale result is dropped by the drive loop's generation admission.
    pub fn settle(&self, id: AgentId, generation: u64) -> bool {
        let mut g = self.lock();
        if g.generation != generation {
            return false;
        }
        g.entries.remove(&id).is_some()
    }

    /// Remove an agent from the registry unconditionally — the trunk's
    /// self-removal at the end of its drive (a child is removed by its spawn
    /// site through [`Self::settle`]).
    pub fn deregister(&self, id: AgentId) {
        self.lock().entries.remove(&id);
    }

    /// Cancel an agent **and its whole subtree** by id; `true` if the agent
    /// existed.  The single cascade primitive: `agent_cancel`, the ceiling,
    /// and `/clear` all route here.  Each cancelled worker observes its token,
    /// settles `Cancelled`, and removes its own entry through [`Self::settle`].
    pub fn cancel(&self, id: AgentId) -> bool {
        let g = self.lock();
        let existed = g.entries.contains_key(&id);
        for d in descendants(&g.entries, id, true) {
            if let Some(e) = g.entries.get(&d) {
                cancel_entry(e, CancelCause::Explicit);
            }
        }
        existed
    }

    /// A per-tab turn interrupt: unwind exactly this entry's in-flight turn and
    /// eval, without cascading to descendants or removing the entry. Esc/Ctrl-C
    /// on a focused sub-agent tab route here; the agent drops its turn and re-parks.
    pub fn interrupt(&self, id: AgentId) {
        let g = self.lock();
        if let Some(e) = g.entries.get(&id) {
            e.cancel.cancel(CancelCause::Interrupt);
            if let Some(root) = &e.eval_root {
                root.cancel(CancelCause::Interrupt);
            }
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
        v.sort_by_key(|a| a.id);
        v
    }

    /// Look up one agent'\''s title by id, if it is live.
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
        remove_descendants(&mut g, root);
        g.generation += 1;
    }

    /// Close `root` and its whole subtree: cancel each entry's in-flight turn and
    /// eval, then drop the entries.  The inclusive twin of the `/clear` reap
    /// (`remove_descendants` leaves the root); no generation bump — a branch
    /// pushes no result, so there is nothing to fence.  `/close` routes here.
    pub fn remove_subtree(&self, root: AgentId) {
        let mut g = self.lock();
        let subtree = descendants(&g.entries, root, true);
        for d in &subtree {
            if let Some(e) = g.entries.get(d) {
                cancel_entry(e, CancelCause::Explicit);
            }
        }
        for d in subtree {
            g.entries.remove(&d);
        }
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MessageError {
    UnknownSender(AgentId),
    UnknownRecipient(AgentId),
    /// The recipient's inbox rejected the message at quota — surfaced to
    /// the `message` tool's caller as a user-facing error, never dropped
    /// silently (`decisions/260705_leases-and-budgets`).
    RecipientInboxFull(AgentId, InboxReject),
}

/// The ids in `ancestor`'s subtree, walking the `parent` edges breadth-first.
/// `inclusive` adds `ancestor` itself (for the cascade, which cancels the
/// agent and its descendants); `false` yields only the proper descendants
/// (for the `agents` listing and a `/clear` reap).
fn descendants(
    entries: &HashMap<AgentId, Entry>,
    ancestor: AgentId,
    inclusive: bool,
) -> Vec<AgentId> {
    let mut out = if inclusive {
        vec![ancestor]
    } else {
        Vec::new()
    };
    let mut frontier = vec![ancestor];
    while let Some(cur) = frontier.pop() {
        for (id, e) in entries {
            if e.parent == Some(cur) && !out.contains(id) {
                out.push(*id);
                frontier.push(*id);
            }
        }
    }
    out
}

fn remove_descendants(g: &mut Inner, root: AgentId) {
    let desc = descendants(&g.entries, root, false);
    for d in &desc {
        if let Some(e) = g.entries.get(d) {
            cancel_entry(e, CancelCause::Explicit);
        }
    }
    for d in desc {
        g.entries.remove(&d);
    }
}

/// Cancel one entry across both layers: the cooperative [`Token`] the drive
/// loop polls between steps, and — for a parented agent — its session's
/// durable root, so an in-flight eval unwinds at the evaluator's poll points
/// rather than running to its timeout wall.
fn cancel_entry(e: &Entry, cause: CancelCause) {
    e.cancel.cancel(cause);
    if let Some(root) = &e.eval_root {
        root.cancel(cause);
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
        reg.register(
            id,
            parent,
            parent.is_some(),
            format!("a{id}"),
            PathBuf::from("/tmp"),
            Token::new(),
            parent.map(|_| DurableRoot::default()),
            crate::bus::Inbox::new().mailbox(),
            provider(),
        )
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
    fn register_arms_a_ceiling_only_when_asked() {
        let reg = AgentRegistry::new();
        reg.register(
            1,
            Some(0),
            false,
            "branch".into(),
            PathBuf::from("/tmp"),
            Token::new(),
            Some(DurableRoot::default()),
            mb(),
            provider(),
        );
        reg.register(
            2,
            Some(0),
            true,
            "worker".into(),
            PathBuf::from("/tmp"),
            Token::new(),
            Some(DurableRoot::default()),
            mb(),
            provider(),
        );
        let g = reg.lock();
        assert!(
            g.entries[&1]._ceiling.is_none(),
            "ceiling = false arms no reaper deadline"
        );
        assert!(
            g.entries[&2]._ceiling.is_some(),
            "ceiling = true arms a reaper deadline"
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
    /// `remove_descendants`: each reaped worker is cancelled across both
    /// layers, so an in-flight eval unwinds instead of grinding on as an
    /// orphan whose result nobody will collect.
    #[test]
    fn clear_subtree_cancels_descendant_eval_roots() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let token = Token::new();
        let eval_root = DurableRoot::default();
        reg.register(
            1,
            Some(0),
            true,
            "worker".into(),
            PathBuf::from("/tmp/worker"),
            token.clone(),
            Some(eval_root.clone()),
            mb(),
            provider(),
        );
        reg.clear_subtree(0);
        assert!(token.is_cancelled(), "the reaped worker's token is set");
        assert!(
            eval_root.as_scope().is_cancelled(),
            "the reap cancels the worker's eval layer, not just its token"
        );
    }

    /// `/close` routes to `remove_subtree`, the inclusive twin of the `/clear`
    /// reap: it cancels and drops the whole subtree *including* its root — a
    /// branch and any agents it spawned — where `remove_descendants` would have
    /// left the root live.  The trunk above the closed subtree is untouched.
    #[test]
    fn remove_subtree_cancels_and_removes_the_root_too() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let branch_token = Token::new();
        let branch_root = DurableRoot::default();
        let child_token = Token::new();
        reg.register(
            1,
            Some(0),
            false, // a branch carries no ceiling
            "branch".into(),
            PathBuf::from("/tmp/branch"),
            branch_token.clone(),
            Some(branch_root.clone()),
            mb(),
            provider(),
        );
        reg.register(
            2,
            Some(1),
            true,
            "grandchild".into(),
            PathBuf::from("/tmp/gc"),
            child_token.clone(),
            Some(DurableRoot::default()),
            mb(),
            provider(),
        );

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
    /// trips exactly one entry's token and eval-root with an `Interrupt` cause,
    /// walks no descendants, and deregisters nothing.  So the child's token is
    /// `is_cancelled` but not `terminated` (an interrupt drops the turn, it
    /// does not end the agent), the interrupt reaches the child's eval layer,
    /// the grandchild is untouched, and both entries stay live.  Contrast
    /// `cancel(1)`, which would trip the grandchild too and with a terminate
    /// cause (`Explicit`), settling the whole subtree.
    #[test]
    fn interrupt_unwinds_exactly_one_entry() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let child_token = Token::new();
        let child_root = DurableRoot::default();
        let grandchild_token = Token::new();
        reg.register(
            1,
            Some(0),
            false,
            "child".into(),
            PathBuf::from("/tmp/child"),
            child_token.clone(),
            Some(child_root.clone()),
            mb(),
            provider(),
        );
        reg.register(
            2,
            Some(1),
            true,
            "grandchild".into(),
            PathBuf::from("/tmp/gc"),
            grandchild_token.clone(),
            Some(DurableRoot::default()),
            mb(),
            provider(),
        );

        reg.interrupt(1);

        assert!(child_token.is_cancelled(), "interrupt trips the child's token");
        assert!(
            !child_token.terminated(),
            "an interrupt is not a terminate cause"
        );
        assert!(
            child_root.as_scope().is_cancelled(),
            "the interrupt reaches the child's eval layer, not just its token"
        );
        assert!(
            !grandchild_token.is_cancelled(),
            "no descendant walk: the grandchild is untouched"
        );
        assert!(reg.is_live(1), "interrupt never deregisters the entry");
        assert!(reg.is_live(2), "nor its descendant");
    }

    #[test]
    fn cancel_sets_the_token_and_list_reports_the_subtree() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let token = Token::new();
        let eval_root = DurableRoot::default();
        reg.register(
            7,
            Some(0),
            true,
            "lint".into(),
            PathBuf::from("/log/7"),
            token.clone(),
            Some(eval_root.clone()),
            crate::bus::Inbox::new().mailbox(),
            provider(),
        );
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
        reg.register(
            1,
            None,
            false,
            "r".into(),
            "/l".into(),
            r.clone(),
            None,
            mb(),
            provider(),
        );
        reg.register(
            2,
            Some(1),
            true,
            "c".into(),
            "/l".into(),
            c.clone(),
            Some(DurableRoot::default()),
            mb(),
            provider(),
        );
        reg.register(
            3,
            Some(2),
            true,
            "g".into(),
            "/l".into(),
            g.clone(),
            Some(DurableRoot::default()),
            mb(),
            provider(),
        );
        reg.register(
            4,
            Some(1),
            true,
            "s".into(),
            "/l".into(),
            sibling.clone(),
            Some(DurableRoot::default()),
            mb(),
            provider(),
        );
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
        reg.register(
            2,
            Some(1),
            true,
            "worker".into(),
            PathBuf::from("/tmp/worker"),
            Token::new(),
            Some(DurableRoot::default()),
            inbox.mailbox(),
            provider(),
        );

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

    fn mb() -> Mailbox {
        crate::bus::Inbox::new().mailbox()
    }
}
