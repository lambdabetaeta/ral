//! The ephemeral, session-owned registry of live async `agent` workers.
//!
//! An async `agent` call (mode `"async"`) forks a child session and runs
//! its turn loop on a *detached* worker thread that outlives the spawning
//! turn — unlike a sync call, which is fork-join on the dispatch
//! `thread::scope`.  That worker needs three things this registry owns:
//!
//!   * a **listing** so an orchestrator can recover ids after context
//!     compaction (`agents`) and stop stragglers (`agent_cancel`) — the
//!     binding-reaping problem the durable-job registry has too;
//!   * a **ceiling**, armed on the shared `process::reaper` as a `Run`
//!     deadline that cancels the worker's token, so an abandoned worker is
//!     reaped rather than running forever;
//!   * a **generation**, bumped on `/clear`, so a worker that settles into a
//!     rebuilt session has its result rejected instead of delivered into a
//!     context that no longer expects it.
//!
//! It is *not* the durable-job registry of long-running-work: entries are
//! ephemeral, live only while their worker runs, and their result is pushed
//! to the session inbox rather than collected by id.

use crate::bus::AgentId;
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

/// One live async agent, by id, for the `agents` listing.
pub struct AgentInfo {
    pub id: AgentId,
    pub title: String,
    pub log_dir: PathBuf,
    pub elapsed: Duration,
}

/// A session's live async agents.  Cheap to clone — the inner `Arc` makes a
/// clone share the same map, so a detached worker holds a handle it can
/// settle through after its turn has ended.
#[derive(Clone)]
pub struct AgentRegistry {
    inner: Arc<Mutex<Inner>>,
}

struct Inner {
    /// Bumped by [`AgentRegistry::clear`]; a worker that settles under an
    /// older generation is rejected.
    generation: u64,
    entries: HashMap<AgentId, Entry>,
}

struct Entry {
    title: String,
    log_dir: PathBuf,
    started: Instant,
    cancel: Token,
    /// Holds the worker's reaper ceiling armed; dropping the entry on settle
    /// disarms it, so a worker that finished before its ceiling is never
    /// reaped by a late pop.
    _ceiling: Deadline,
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

    /// Register a freshly spawned worker.  Arms its ceiling on the reaper (a
    /// `Run` deadline that cancels `cancel` when it elapses) and returns the
    /// birth generation the worker carries into its result.
    pub fn register(
        &self,
        id: AgentId,
        title: String,
        log_dir: PathBuf,
        cancel: Token,
    ) -> u64 {
        let ceiling = process::arm_callback(AGENT_CEILING, {
            let t = cancel.clone();
            move || t.cancel()
        });
        let mut g = self.lock();
        g.entries.insert(
            id,
            Entry {
                title,
                log_dir,
                started: Instant::now(),
                cancel,
                _ceiling: ceiling,
            },
        );
        g.generation
    }

    /// Settle a worker born under `generation`.  If it is still the live
    /// entry of that generation, remove it — disarming its ceiling — and
    /// return `true` so its result is delivered.  A worker from an older
    /// generation (its session was `/clear`ed) returns `false`: its result
    /// is dropped rather than posted into a rebuilt context.
    pub fn settle(&self, id: AgentId, generation: u64) -> bool {
        let mut g = self.lock();
        if g.generation != generation {
            return false;
        }
        g.entries.remove(&id).is_some()
    }

    /// Cancel one live agent by id; `true` if it existed.  The worker
    /// observes the token, settles `Cancelled`, and removes its own entry.
    pub fn cancel(&self, id: AgentId) -> bool {
        let g = self.lock();
        match g.entries.get(&id) {
            Some(e) => {
                e.cancel.cancel();
                true
            }
            None => false,
        }
    }

    /// Snapshot the live agents for the `agents` listing, ordered by id.
    pub fn list(&self) -> Vec<AgentInfo> {
        let g = self.lock();
        let mut v: Vec<AgentInfo> = g
            .entries
            .iter()
            .map(|(id, e)| AgentInfo {
                id: *id,
                title: e.title.clone(),
                log_dir: e.log_dir.clone(),
                elapsed: e.started.elapsed(),
            })
            .collect();
        v.sort_by_key(|a| a.id);
        v
    }

    /// `/clear`: cancel every live agent, bump the generation so their late
    /// results are rejected, and drop the entries (disarming every ceiling).
    pub fn clear(&self) {
        let mut g = self.lock();
        for e in g.entries.values() {
            e.cancel.cancel();
        }
        g.entries.clear();
        g.generation += 1;
    }

    fn lock(&self) -> MutexGuard<'_, Inner> {
        self.inner.lock().unwrap_or_else(|e| e.into_inner())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(reg: &AgentRegistry, id: AgentId) -> u64 {
        reg.register(id, format!("a{id}"), PathBuf::from("/tmp"), Token::new())
    }

    #[test]
    fn settle_delivers_for_current_generation() {
        let reg = AgentRegistry::new();
        let g = entry(&reg, 1);
        assert!(reg.settle(1, g), "a current-generation worker delivers");
        assert!(!reg.settle(1, g), "a settled entry is gone");
    }

    #[test]
    fn clear_bumps_generation_and_rejects_late_results() {
        let reg = AgentRegistry::new();
        let g = entry(&reg, 1);
        reg.clear();
        assert!(
            !reg.settle(1, g),
            "a result from before /clear is rejected by generation"
        );
        assert_ne!(g, reg.generation(), "the generation advanced on clear");
    }

    #[test]
    fn cancel_sets_the_token_and_list_reports_live_agents() {
        let reg = AgentRegistry::new();
        let token = Token::new();
        reg.register(7, "lint".into(), PathBuf::from("/log/7"), token.clone());
        assert_eq!(reg.list().len(), 1);
        assert_eq!(reg.list()[0].title, "lint");
        assert!(reg.cancel(7), "an existing agent is cancellable");
        assert!(token.is_cancelled(), "cancel sets the worker's token");
        assert!(!reg.cancel(99), "an unknown id is not cancellable");
    }
}
