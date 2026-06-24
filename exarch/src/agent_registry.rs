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
//!     the per-agent ceiling, and an `Esc` on the focused subtree;
//!   * a **ceiling** (children only), armed on the shared `process::reaper` as
//!     a `Run` deadline that cancels the worker's subtree, so an abandoned
//!     detached worker is reaped rather than running forever — the trunk,
//!     which is never abandoned, gets none;
//!   * a **generation**, bumped on `/clear`, so a worker that settles into a
//!     rebuilt context has its result rejected instead of delivered.
//!
//! It is *not* the durable-job registry of long-running-work: entries are
//! ephemeral, live only while their agent runs, and a child's result is
//! pushed to its parent's inbox rather than collected by id.

use crate::agent::ProviderHandle;
use crate::bus::{AgentId, Mailbox};
use crate::cancel::Token;
use ral_core::process::{self, Deadline};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, Mutex, MutexGuard};
use std::time::{Duration, Instant};

/// The detached-worker default ceiling: one hour
/// ([[concurrency-primitives-detached-vs-structured]]).  An async agent
/// doing genuinely long work may want a different bound — that is
/// long-running-work's open regime question, surfacing here for a tool
/// rather than a ral verb — but a uniform ceiling keeps an abandoned worker
/// from running unbounded.
pub const AGENT_CEILING: Duration = Duration::from_secs(3600);

/// One live agent, by id, for the `agents` listing.
pub struct AgentInfo {
    pub id: AgentId,
    pub title: String,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
}

/// The fleet's live agents.  Cheap to clone — the inner `Arc` makes a clone
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
    /// The agent's inbox sender, so the frontend can route a focused tab's
    /// typed line into that agent's queue and `wake` it on a focus change.
    /// The registry hands out *senders*, but only the frontend (and the
    /// trunk) ever obtains the registry, so agents still can't reach each
    /// other: the "no inter-agent talking" policy is preserved by who holds
    /// the registry, not by withholding the field.
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

    /// Register an agent under its `parent` (`None` for the trunk).  A child
    /// (`parent` set) arms a ceiling on the reaper that cancels its whole
    /// subtree when it elapses; the trunk gets none.  Returns the birth
    /// generation the worker carries into its result.
    #[allow(clippy::too_many_arguments)] // an agent's registration record, threaded once at birth
    pub fn register(
        &self,
        id: AgentId,
        parent: Option<AgentId>,
        title: String,
        log_dir: PathBuf,
        cancel: Token,
        mailbox: Mailbox,
        provider: ProviderHandle,
    ) -> u64 {
        let ceiling = parent.map(|_| {
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

    /// The live agent's provider handle, for a `/model` swap on the focused
    /// agent.  `None` once it has settled.
    pub fn provider(&self, id: AgentId) -> Option<ProviderHandle> {
        let g = self.lock();
        g.entries.get(&id).map(|e| e.provider.clone())
    }

    /// Settle a worker born under `generation`.  If it is still the live
    /// entry of that generation, remove it — disarming its ceiling — and
    /// return `true` so its result is delivered.  A worker from an older
    /// generation (its context was `/clear`ed) returns `false`: its result
    /// is dropped rather than posted into a rebuilt context.
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
    /// and an `Esc` on the focused subtree all route here.  Each cancelled
    /// worker observes its token, settles `Cancelled`, and removes its own
    /// entry through [`Self::settle`].
    pub fn cancel(&self, id: AgentId) -> bool {
        let g = self.lock();
        let existed = g.entries.contains_key(&id);
        for d in descendants(&g.entries, id, true) {
            if let Some(e) = g.entries.get(&d) {
                e.cancel.cancel();
            }
        }
        existed
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

    /// `/clear` on `root`: cancel and reap `root`'s proper descendants — the
    /// subtree the rebuilt context no longer owns — and bump the generation so
    /// any of their late results are rejected.  `root` itself stays registered
    /// (it is the agent being rebuilt, not torn down).
    pub fn clear_subtree(&self, root: AgentId) {
        let mut g = self.lock();
        let desc = descendants(&g.entries, root, false);
        for d in &desc {
            if let Some(e) = g.entries.get(d) {
                e.cancel.cancel();
            }
        }
        for d in desc {
            g.entries.remove(&d);
        }
        g.generation += 1;
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
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
        for (id, e) in entries.iter() {
            if e.parent == Some(cur) && !out.contains(id) {
                out.push(*id);
                frontier.push(*id);
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ProviderHandle {
        ProviderHandle::new(std::sync::Arc::new(crate::provider::Provider::scripted(
            "test",
            crate::provider::scripted::Script::new(),
        )))
    }

    /// Register `id` under `parent`, returning the birth generation.
    fn entry(reg: &AgentRegistry, id: AgentId, parent: Option<AgentId>) -> u64 {
        reg.register(
            id,
            parent,
            format!("a{id}"),
            PathBuf::from("/tmp"),
            Token::new(),
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

    #[test]
    fn cancel_sets_the_token_and_list_reports_the_subtree() {
        let reg = AgentRegistry::new();
        entry(&reg, 0, None); // the trunk
        let token = Token::new();
        reg.register(
            7,
            Some(0),
            "lint".into(),
            PathBuf::from("/log/7"),
            token.clone(),
            crate::bus::Inbox::new().mailbox(),
            provider(),
        );
        assert_eq!(reg.list(0).len(), 1, "the trunk lists its one child");
        assert_eq!(reg.list(0)[0].title, "lint");
        assert!(reg.cancel(7), "an existing agent is cancellable");
        assert!(token.is_cancelled(), "cancel sets the worker's token");
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
            "r".into(),
            "/l".into(),
            r.clone(),
            mb(),
            provider(),
        );
        reg.register(
            2,
            Some(1),
            "c".into(),
            "/l".into(),
            c.clone(),
            mb(),
            provider(),
        );
        reg.register(
            3,
            Some(2),
            "g".into(),
            "/l".into(),
            g.clone(),
            mb(),
            provider(),
        );
        reg.register(
            4,
            Some(1),
            "s".into(),
            "/l".into(),
            sibling.clone(),
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

    fn mb() -> Mailbox {
        crate::bus::Inbox::new().mailbox()
    }
}
